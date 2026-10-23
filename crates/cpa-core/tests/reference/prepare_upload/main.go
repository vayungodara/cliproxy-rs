// Records Go v8.0.35's config.PrepareConfigFileForUpload (f9437700) for
// crates/cpa-core/tests/prepare_upload_go.rs. Run from a module nested under the v8
// module path (so internal/config is importable) with a replace to the reference
// clone:
//
//	go run . ../../fixtures/prepare_upload_go.json
//
// No network: every case is a file in a temporary directory.
package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"golang.org/x/crypto/bcrypt"
	"gopkg.in/yaml.v3"
)

type result struct {
	Name  string `json:"name"`
	YAML  string `json:"yaml"`
	Plain string `json:"plain,omitempty"`
	// PlainBytes is Plain as base64 when it is not UTF-8, which JSON cannot hold.
	PlainBytes []byte `json:"plain_bytes,omitempty"`
	// Error is Go's error text, empty on success.
	Error     string `json:"error"`
	Unchanged bool   `json:"unchanged"`
	// Result is the prepared file decoded as plain YAML, bcrypt hashes as "<bcrypt>";
	// null when it does not decode.
	Result any `json:"result"`
	// Verifies: the prepared file's effective secret is the bcrypt hash of Plain.
	Verifies bool `json:"verifies"`
	// EffectiveSame: the prepared file decodes to the same Config, secret aside,
	// including the json:"-" Host, Port and RemoteManagement; null when the input
	// does not decode.
	EffectiveSame *bool `json:"effective_same"`
}

const plain = "upload-plaintext-secret"

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func masked(v any) any {
	switch t := v.(type) {
	case map[string]any:
		for k, x := range t {
			t[k] = masked(x)
		}
	case map[any]any:
		// A non-string key: JSON object keys are its Go formatting.
		out := map[string]any{}
		for k, x := range t {
			out[fmt.Sprint(k)] = masked(x)
		}
		return out
	case []any:
		for i, x := range t {
			t[i] = masked(x)
		}
	case string:
		if len(t) == 60 && strings.HasPrefix(t, "$2") {
			return "<bcrypt>"
		}
	}
	return v
}

// effective is the decoded Config with its secret blanked, and the secret; ok is
// false when data does not decode.
func effective(data []byte) (snapshot, secret string, ok bool) {
	var cfg config.Config
	if yaml.Unmarshal(data, &cfg) != nil {
		return "", "", false
	}
	secret = cfg.RemoteManagement.SecretKey
	cfg.RemoteManagement.SecretKey = ""
	out, err := json.Marshal(struct {
		Config           config.Config
		Host             string
		Port             int
		RemoteManagement config.RemoteManagement
	}{cfg, cfg.Host, cfg.Port, cfg.RemoteManagement})
	must(err)
	return string(out), secret, true
}

func main() {
	hashed, err := bcrypt.GenerateFromPassword([]byte(plain), bcrypt.MinCost)
	must(err)
	hash := string(hashed)
	anchored := "defaults: &legacy\n  secret-key: " + plain + "\nremote-management: *legacy\nmanagement:\n  secret-key: "
	type tc struct{ name, yaml, plain string }
	cases := []tc{
		// TestPrepareConfigFileForUpload*.
		{"legacy layout", "# operator note\nport: 8317\nremote-management:\n  secret-key: " + plain + "\n", plain},
		{"v8 layout", "# operator note\nport: 8317\nmanagement:\n  secret-key: " + plain + "\n", plain},
		{"v8 secret spelled like its section", "management:\n  secret-key: management\n", "management"},
		{"overridden legacy secret spelled like the v8 section", "remote-management:\n  secret-key: management\nmanagement:\n  secret-key: '" + hash + "'\n", ""},
		{"yaml null beside string null secret", "proxy-url: null\nmanagement:\n  secret-key: 'null'\n", "null"},
		{"duplicated plaintext", "remote-management:\n  secret-key: " + plain + "\nmanagement:\n  secret-key: " + plain + "\n", plain},
		{"stale legacy beside hashed v8", "remote-management:\n  secret-key: " + plain + "\nmanagement:\n  secret-key: '" + hash + "'\n", plain},
		{"hashed legacy secret", "# operator note\nremote-management:\n  secret-key: '" + hash + "'\n", plain},
		{"hashed v8 secret", "# operator note\nmanagement:\n  secret-key: '" + hash + "'\n", plain},
		{"no secret", "# operator note\nport: 8317\n", ""},
		{"null v8 secret", "# operator note\nmanagement:\n  secret-key: null\n", ""},
		{"tilde legacy secret", "# operator note\nremote-management:\n  secret-key: ~\n", ""},
		{"empty file", "", ""},
		{"unparsable", "remote-management:\n  secret-key: " + plain + "\nport: [8317\n", plain},
		{"secret too long for bcrypt", "remote-management:\n  secret-key: " + strings.Repeat("s", 73) + "\n", strings.Repeat("s", 73)},
		{"mapping anchor", "defaults: &management\n  secret-key: " + plain + "\nmanagement: *management\n", plain},
		{"scalar anchor", "password: &password " + plain + "\nmanagement:\n  secret-key: *password\n", plain},
		{"merge key", "defaults: &management\n  secret-key: " + plain + "\nmanagement:\n  <<: *management\n  allow-remote: true\n", plain},
		{"shared anchor", "management: &management\n  secret-key: " + plain + "\nother: *management\n", plain},
		{"anchored legacy beside plaintext v8", anchored + "current-plaintext-secret\n", plain},
		{"anchored legacy beside hashed v8", anchored + "'" + hash + "'\n", plain},
		// Scalars compare by their text, not their parsed value.
		{"quoted hex secret beside an unquoted copy", "management:\n  secret-key: \"0x1F\"\ncopy: 0x1F\n", "0x1F"},
		{"decimal secret beside a hex copy", "management:\n  secret-key: '31'\ncopy: 0x1F\n", "31"},
		{"unquoted hex secret", "management:\n  secret-key: 0x1F\n", "0x1F"},
		{"unquoted boolean secret", "management:\n  secret-key: yes\n", "yes"},
		// Tagged scalars: a custom tag keeps the text, !!binary decodes base64.
		{"tagged copy of the secret", "management:\n  secret-key: secret\ncopy: !keep secret\n", "secret"},
		{"tagged mapping hiding a copy", "management:\n  secret-key: secret\ncopy: !keep {nested: [secret]}\n", "secret"},
		{"tagged secret", "management:\n  secret-key: !keep secret\n", "secret"},
		{"binary secret", "management:\n  secret-key: !!binary c2VjcmV0\n", "secret"},
		{"binary secret beside its base64 copy", "management:\n  secret-key: !!binary c2VjcmV0\ncopy: c2VjcmV0\n", "secret"},
		{"binary secret beside its decoded copy", "management:\n  secret-key: !!binary c2VjcmV0\ncopy: secret\n", "secret"},
		{"invalid binary secret", "management:\n  secret-key: !!binary 'c2VjcmV0!'\n", ""},
		{"binary secret with a space", "management:\n  secret-key: !!binary 'c2Vj cmV0'\n", ""},
		{"binary secret without padding", "management:\n  secret-key: !!binary YQ\n", ""},
		{"binary secret with unused bits", "management:\n  secret-key: !!binary YR==\n", "a"},
		{"binary secret that is not UTF-8", "management:\n  secret-key: !!binary /w==\n", "\xff"},
		{"verbatim binary secret beside its decoded copy", "management:\n  secret-key: !<tag:yaml.org,2002:binary> c2VjcmV0\ncopy: secret\n", "secret"},
		{"verbatim binary secret", "management:\n  secret-key: !<tag:yaml.org,2002:binary> c2VjcmV0\n", "secret"},
		{"escaped verbatim binary secret", "management:\n  secret-key: !<tag:yaml.org,2002:bin%61ry> c2VjcmV0\n", "secret"},
		{"escaped verbatim binary secret beside a copy", "management:\n  secret-key: !<tag:yaml.org,2002:bin%61ry> c2VjcmV0\ncopy: secret\n", "secret"},
		{"escaped shorthand binary secret", "management:\n  secret-key: !!bin%61ry c2VjcmV0\n", "secret"},
		{"escaped shorthand binary secret beside a copy", "management:\n  secret-key: !!bin%61ry c2VjcmV0\ncopy: secret\n", "secret"},
		{"named handle binary secret", "%TAG !e! tag:yaml.org,2002:\n---\nmanagement:\n  secret-key: !e!binary c2VjcmV0\n", "secret"},
		{"named handle binary secret beside a copy", "%TAG !e! tag:yaml.org,2002:\n---\nmanagement:\n  secret-key: !e!binary c2VjcmV0\ncopy: secret\n", "secret"},
		{"redefined secondary handle secret", "%TAG !! tag:example.com,2000:\n---\nmanagement:\n  secret-key: !!binary c2VjcmV0\n", "c2VjcmV0"},
		{"redefined secondary handle secret beside a copy", "%TAG !! tag:example.com,2000:\n---\nmanagement:\n  secret-key: !!binary c2VjcmV0\ncopy: secret\n", "c2VjcmV0"},
		{"malformed escape in the secret tag", "management:\n  secret-key: !<tag:yaml.org,2002:bin%6> c2VjcmV0\n", ""},
		{"binary anchor after a NEL-ended comment", "remote-management: # comment\u0085  secret-key: &a !!binary c2VjcmV0\nmanagement:\n  secret-key: *a\n", "secret"},
		{"binary anchor after a NEL-ended comment beside a copy", "remote-management: # comment\u0085  secret-key: &a !!binary c2VjcmV0\nmanagement:\n  secret-key: *a\ncopy: secret\n", "secret"},
		{"binary tag after a NEL-ended comment", "management:\n  secret-key: # c\u0085  !!binary c2VjcmV0\n", "secret"},
		{"binary tag after a NEL-ended comment beside a copy", "management:\n  secret-key: # c\u0085  !!binary c2VjcmV0\ncopy: secret\n", "secret"},
		{"line separator in a string", "management:\n  secret-key: " + plain + "\nnote: \"a\u2028b\"\n", plain},
		{"aliased legacy binary secret", "remote-management:\n  secret-key: &a !!binary c2VjcmV0\nmanagement:\n  secret-key: *a\n", "secret"},
		{"aliased legacy binary secret beside a copy", "remote-management:\n  secret-key: &a !!binary c2VjcmV0\nmanagement:\n  secret-key: *a\ncopy: secret\n", "secret"},
		{"aliased v8 binary secret", "management:\n  secret-key: &a !!binary c2VjcmV0\nremote-management:\n  secret-key: *a\n", "secret"},
		{"aliased v8 binary secret beside a copy", "management:\n  secret-key: &a !!binary c2VjcmV0\nremote-management:\n  secret-key: *a\ncopy: secret\n", "secret"},
		{"binary secret through an aliased section", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement: *d\n", "secret"},
		{"binary secret through an aliased section beside a copy", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement: *d\ncopy: secret\n", "secret"},
		{"binary secret through a merge", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement:\n  <<: *d\n", "secret"},
		{"binary secret through a merge beside a copy", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement:\n  <<: *d\ncopy: secret\n", "secret"},
		{"verbatim short binary secret", "management:\n  secret-key: !<!!binary> c2VjcmV0\n", "secret"},
		{"verbatim short binary secret beside a copy", "management:\n  secret-key: !<!!binary> c2VjcmV0\ncopy: secret\n", "secret"},
		{"escaped primary binary secret", "management:\n  secret-key: !%21binary c2VjcmV0\n", "secret"},
		{"escaped primary binary secret beside a copy", "management:\n  secret-key: !%21binary c2VjcmV0\ncopy: secret\n", "secret"},
		{"aliased plain secret beside an unrelated tag", "password: &p " + plain + "\nmanagement:\n  secret-key: *p\nother: !keep x\n", plain},
		// A custom-tagged null spelling is text; only !!null is null.
		{"tagged null copy", "management:\n  secret-key: 'null'\ncopy: !keep null\n", "null"},
		{"tagged tilde copy", "management:\n  secret-key: '~'\ncopy: !keep ~\n", "~"},
		{"tagged NULL copy", "management:\n  secret-key: 'NULL'\ncopy: [!keep NULL]\n", "NULL"},
		{"tagged null secret", "management:\n  secret-key: !keep null\n", "null"},
		{"core null tag copy", "management:\n  secret-key: 'null'\ncopy: !!null null\n", "null"},
		// The re-emitted tree must keep every other scalar's text.
		{"aliased hex secret", "management:\n  secret-key: &key 0x1F\ncopy: *key\n", "0x1F"},
		{"binary host beside a merged secret", "management:\n  <<: {secret-key: secret}\nserver:\n  host: !!binary bG9jYWxob3N0\n", "secret"},
		{"hex key beside a merged secret", "management:\n  <<: {secret-key: secret}\nextra: {0x1F: unchanged}\n", "secret"},
		{"merged secret", "management:\n  <<: {secret-key: secret}\nport: 8317\n", "secret"},
		{"merge key beside a hex value", "defaults: &management\n  secret-key: " + plain + "\nmanagement:\n  <<: *management\nport: 0x1F9D\n", plain},
		// bcrypt's 72-byte limit and Go's looksLikeBcrypt.
		{"71 bytes", "management:\n  secret-key: " + strings.Repeat("a", 71) + "\n", strings.Repeat("a", 71)},
		{"72 bytes", "management:\n  secret-key: " + strings.Repeat("a", 72) + "\n", strings.Repeat("a", 72)},
		{"73 bytes", "management:\n  secret-key: " + strings.Repeat("a", 73) + "\n", strings.Repeat("a", 73)},
		{"71 bytes multibyte", "management:\n  secret-key: " + strings.Repeat("é", 35) + "a\n", strings.Repeat("é", 35) + "a"},
		{"72 bytes multibyte", "management:\n  secret-key: " + strings.Repeat("é", 36) + "\n", strings.Repeat("é", 36)},
		{"73 bytes multibyte", "management:\n  secret-key: " + strings.Repeat("é", 36) + "a\n", strings.Repeat("é", 36) + "a"},
		{"bare $2a$", "management:\n  secret-key: '$2a$'\n", "$2a$"},
		{"bare $2b$", "management:\n  secret-key: '$2b$'\n", "$2b$"},
		{"bare $2y$", "management:\n  secret-key: '$2y$'\n", "$2y$"},
		// A secret in a value the startup validators would reject and quote.
		{"secret aliased into trusted-proxies", "management:\n  secret-key: &key synthetic-secret\ntrusted-proxies: [*key]\n", "synthetic-secret"},
		{"hash aliased into trusted-proxies", "management:\n  secret-key: &key '" + hash + "'\ntrusted-proxies: [*key]\n", plain},
		{"secret aliased into the relay public-ip", "management:\n  secret-key: &key synthetic-secret\ncodex:\n  live-media-relay:\n    public-ip: *key\n", "synthetic-secret"},
		// Layout cleanup: only declared overrides go (TestV8PresencePrecedenceAndCleanup).
		{"legacy only", "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\n", ""},
		{"mixed explicit zero", "# keep\nrequest-retry: 4\ndisable-cooling: true\napi-keys: [old]\nrouting:\n  retry: {request-retry: 0}\n  cooldown: {disable-cooling: false}\naccess: {api-keys: []}\n", ""},
		{"version does not force migration", "config-version: 8\nrequest-retry: 4\ndisable-cooling: true\napi-keys: [old]\n", ""},
		{"partial new block", "request-retry: 4\ndisable-cooling: true\napi-keys: [old]\nrouting: {retry: {max-retry-credentials: 2}}\n", ""},
		{"family beside its group, null legacy struct", "claude-api-key: [{api-key: a}]\napi-keys: {claude: [{keys: [{api-key: b}]}]}\nremote-management: null\nmanagement: {allow-remote: true}\n", ""},
		{"first prefix claims a leaf", "codex:\n  response-steering: true\noauth:\n  providers:\n    codex:\n      response-steering: false\n", ""},
		{"undeclared same-spelled oauth field", "codex:\n  live-media-relay:\n    enabled: true\n    allow-private-remote-ips: false\noauth:\n  providers:\n    codex:\n      live-media-relay:\n        allow-private-remote-ips: true\n", ""},
		{"private-IP alias beside the canonical field", "codex:\n  live-media-relay:\n    allow-private-remote-ips: false\noauth:\n  providers:\n    codex:\n      live-media-relay:\n        disable-private-remote-ips: false\n", ""},
		{"declared relay leaf overridden", "codex:\n  live-media-relay:\n    enabled: true\n    max-sessions: 3\noauth:\n  providers:\n    codex:\n      live-media-relay:\n        enabled: false\n", ""},
		{"empty legacy claude beside upstream", "claude: {}\nupstream:\n  claude:\n    model-level-cooling: true\n", ""},
		{"null legacy claude-code beside upstream", "claude-code: null\nupstream:\n  claude:\n    model-level-cooling: true\n", ""},
		{"empty historical oauth claude beside upstream", "oauth:\n  providers:\n    claude: {}\nupstream:\n  claude:\n    model-level-cooling: true\n", ""},
		{"null historical oauth xai beside upstream", "oauth:\n  providers:\n    xai: null\nupstream:\n  xai:\n    inject-x-search: true\n", ""},
		{"historical alias beside upstream", "oauth:\n  providers:\n    codex:\n      model-level-cooling: true\nupstream:\n  codex:\n    model-level-cooling: false\n", ""},
		{"scalar at a struct path", "tls: 5\nserver:\n  tls:\n    enable: false\n", ""},
		{"empty legacy struct, v8 struct present", "tls: {}\nserver:\n  tls:\n    enable: true\n", ""},
		{"declared claude header leaf overridden", "claude-header-defaults:\n  client-request-id: 'off'\nupstream:\n  claude:\n    header-defaults:\n      client-request-id: 'on'\n", ""},
		{"undeclared nested antigravity field", "antigravity:\n  signature-cache-enabled: false\noauth:\n  providers:\n    antigravity:\n      signature-cache-enabled: true\n", ""},
		{"empty legacy discovery interfaces", "discovery:\n  interfaces: {}\nserver:\n  discovery:\n    interfaces: {}\n", ""},
		// Dropping the overridden legacy field keeps a !!binary value elsewhere.
		{"binary value beside a dropped legacy field", "config-version: 8\nrequest-retry: 1\nrouting:\n  retry:\n    request-retry: 2\nrequests:\n  proxy-url: !!binary aHR0cDovL3guZXhhbXBsZTo4MA==\naccess:\n  api-keys: [&k !!binary ZmFrZS1iaW5hcnk=, *k]\nmanagement:\n  secret-key: " + plain + "\n", plain},
		// uint16 relay ports fail to decode even with the relay disabled.
		{"relay port above uint16", "management:\n  secret-key: " + plain + "\ncodex:\n  live-media-relay:\n    udp-port-min: 65536\n", plain},
		{"negative relay port", "management:\n  secret-key: " + plain + "\noauth:\n  providers:\n    codex:\n      live-media-relay:\n        udp-port-max: -1\n", plain},
		{"server port above uint16", "port: 70000\n", ""},
		// Go's upload decode checks v8 key groups (flattenV8) but not legacy weights
		// (validateCredentialWeightYAML runs only on a load).
		{"null legacy weight", "gemini-api-key: [{api-key: fake, weight: null}]\n", ""},
		{"legacy weight above the limit", "gemini-api-key: [{api-key: fake, weight: 1000001}]\n", ""},
		{"compatible weight above the limit", "openai-compatibility: [{name: x, base-url: https://x.invalid, api-key-entries: [{api-key: fake, weight: 1000001}]}]\n", ""},
		{"null group weight", "api-keys: {gemini: [{keys: [{api-key: fake, weight: null}]}]}\n", ""},
		{"non-integer legacy weight", "gemini-api-key: [{api-key: fake, weight: heavy}]\n", ""},
		{"group weight above the limit", "api-keys: {gemini: [{keys: [{api-key: fake, weight: 1000001}]}]}\n", ""},
	}
	dir, err := os.MkdirTemp("", "cpa-prepare-")
	must(err)
	defer os.RemoveAll(dir)
	var out []result
	for i, c := range cases {
		path := filepath.Join(dir, fmt.Sprintf("%d.yaml", i))
		must(os.WriteFile(path, []byte(c.yaml), 0o600))
		r := result{Name: c.name, YAML: c.yaml, Plain: c.plain}
		if !utf8.ValidString(c.plain) {
			r.PlainBytes = []byte(c.plain)
		}
		if errPrepare := config.PrepareConfigFileForUpload(path); errPrepare != nil {
			r.Error = errPrepare.Error()
		}
		data, errRead := os.ReadFile(path)
		must(errRead)
		r.Unchanged = string(data) == c.yaml
		var doc any
		if yaml.Unmarshal(data, &doc) == nil {
			r.Result = masked(doc)
		}
		after, secret, _ := effective(data)
		if before, _, ok := effective([]byte(c.yaml)); ok {
			same := before == after
			r.EffectiveSame = &same
		}
		r.Verifies = c.plain != "" && bcrypt.CompareHashAndPassword([]byte(secret), []byte(c.plain)) == nil
		out = append(out, r)
	}
	data, err := json.MarshalIndent(out, "", "  ")
	must(err)
	must(os.WriteFile(os.Args[1], append(data, '\n'), 0o644))
}
