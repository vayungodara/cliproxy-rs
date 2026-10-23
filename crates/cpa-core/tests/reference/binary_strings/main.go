// Records how Go v8.0.35's config decoder reads !!binary and locally tagged
// scalars, for
// crates/cpa-core/tests/binary_strings_go.rs. yaml.v3 base64-decodes a scalar whose
// tag resolves to !!binary into any string field. Run from a module nested under the
// v8 module path (so internal/config is importable) with a replace to the reference
// clone:
//
//	go run . ../../fixtures/binary_strings_go.json
//
// No files and no network: each case is parsed from memory.
//
// A !!binary mapping key is left out: Go's layout code matches keys by their
// written text, and cliproxy-rs refuses such a key.
package main

import (
	"encoding/json"
	"os"
	"unicode/utf8"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"gopkg.in/yaml.v3"
)

type result struct {
	Name string `json:"name"`
	YAML string `json:"yaml"`
	// Error is Go's error text, empty on success.
	Error string `json:"error"`
	// SecretKey is the decoded management key; SecretBytes holds it as base64
	// instead when it is not UTF-8.
	SecretKey   string   `json:"secret_key"`
	SecretBytes []byte   `json:"secret_bytes,omitempty"`
	ProxyURL    string   `json:"proxy_url"`
	Port        int      `json:"port"`
	APIKeys     []string `json:"api_keys"`
}

func main() {
	type tc struct{ name, yaml string }
	cases := []tc{
		{"plain secret", "management:\n  secret-key: secret\n"},
		{"binary v8 secret", "management:\n  secret-key: !!binary c2VjcmV0\n"},
		{"binary legacy secret", "remote-management:\n  secret-key: !!binary c2VjcmV0\n"},
		{"quoted binary secret", "management:\n  secret-key: !!binary 'c2VjcmV0'\n"},
		{"block binary secret", "management:\n  secret-key: !!binary |\n    c2Vj\n    cmV0\n"},
		{"verbatim binary secret", "management:\n  secret-key: !<tag:yaml.org,2002:binary> c2VjcmV0\n"},
		{"escaped binary secret", "management:\n  secret-key: !!bin%61ry c2VjcmV0\n"},
		{"short-form binary secret", "management:\n  secret-key: !<!!binary> c2VjcmV0\n"},
		{"escaped primary binary secret", "management:\n  secret-key: !%21binary c2VjcmV0\n"},
		{"named handle binary secret", "%TAG !e! tag:yaml.org,2002:\n---\nmanagement:\n  secret-key: !e!binary c2VjcmV0\n"},
		{"redefined secondary handle secret", "%TAG !! tag:example.com,2000:\n---\nmanagement:\n  secret-key: !!binary c2VjcmV0\n"},
		{"custom-tagged secret", "management:\n  secret-key: !keep c2VjcmV0\n"},
		{"non-specific tagged secret", "management:\n  secret-key: ! secret\n"},
		{"custom-tagged number secret", "management:\n  secret-key: !keep 0x1F\n"},
		{"custom-tagged port", "port: !keep 8317\n"},
		{"aliased custom-tagged secret", "proxy-url: &a !keep secret\nmanagement:\n  secret-key: *a\n"},
		// Anchor redefinition: libyaml binds each alias to the latest definition.
		{"redefined anchor around a binary secret", "a: &x unused\nb: &x {secret-key: !!binary c2VjcmV0}\nmanagement: *x\nc: &y {panel-github-repository: original, secret-key: other}\n"},
		{"redefined anchor", "a: &x 1\nb: &x 2\nport: *x\n"},
		{"alternating redefinitions over a binary scalar", "l0: &a !!binary c2VjcmV0\nl1: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nl2: &a [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nl3: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nl4: &a [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nl5: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nl6: &a [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nl7: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nl8: &a [*b, *b, *b, *b, *b, *b, *b, *b, *b, *b]\nl9: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a, *a]\nend: &a []\n"},
		// Tagged and aliased mapping keys.
		{"locally tagged api-keys key", "access: {!keep api-keys: [!!binary a2V5]}\n"},
		{"locally tagged port key", "!keep port: 1\n"},
		{"aliased port key", "name: &k port\n*k : 1\n"},
		// The non-specific tag and literal standard tags.
		{"non-specific null secret", "management:\n  secret-key: ! null\n"},
		{"non-specific quoted null secret", "management:\n  secret-key: ! 'null'\n"},
		{"non-specific number secret", "management:\n  secret-key: ! 0x1F\n"},
		{"non-specific port", "port: ! 8317\n"},
		{"literal null tag", "management:\n  secret-key: !<!!null> null\n"},
		{"literal null tag on text", "management:\n  secret-key: !<!!null> secret\n"},
		{"literal str tag", "management:\n  secret-key: !<!!str> 0x1F\n"},
		{"literal int tag", "port: !<!!int> 8317\n"},
		// A NUL escape: libyaml cuts the tag there, Go keeps it.
		{"NUL in a binary tag", "management:\n  secret-key: !!binary%00x c2VjcmV0\n"},
		{"NUL in a tag directive", "%TAG !e! tag:yaml.org,2002:bin%00\n---\nmanagement:\n  secret-key: !e!ary c2VjcmV0\n"},
		{"NUL in a tag after a block scalar holding a quote", "note: |\n  \"\nmanagement:\n  secret-key: !%21binary%00x c2VjcmV0\n"},
		{"NUL escape in an untagged value", "management:\n  secret-key: secret%00\n"},
		{"NUL in a tag after a BOM", "\ufeffmanagement:\n  secret-key: !%00 ~\n"},
		{"secret after a BOM", "\ufeffmanagement:\n  secret-key: secret\n"},
		// Tagged keys, merge keys among them.
		{"merge key", "<<:\n  management:\n    secret-key: secret\n"},
		{"canonically tagged merge key", "!!str <<:\n  management:\n    secret-key: secret\n"},
		{"canonically tagged port key", "!!str port: 1\n"},
		// The non-specific tag keeps the scalar's own text.
		{"non-specific document marker secret", "management:\n  secret-key: ! ---\n"},
		{"non-specific multi-line secret", "management:\n  secret-key: ! first\n\n    second\n"},
		{"non-specific boolean word secret", "management:\n  secret-key: ! yes\n"},
		{"non-specific tilde secret", "management:\n  secret-key: ! ~\n"},
		{"literal null tag on a tilde", "management:\n  secret-key: !<!!null> ~\n"},
		// A collection as a mapping key.
		{"binary inside a mapping key", "x-review:\n  first: keep\n  ? {x: !!binary c2VjcmV0}\n  : ignored\n"},
		{"binary inside a mapping key under management", "management:\n  secret-key: plain\n  ? {x: !!binary c2VjcmV0}\n  : ignored\n"},
		{"binary proxy-url", "proxy-url: !!binary aHR0cDovL3Byb3h5Lmxhbjo4MA==\n"},
		{"binary api key", "api-keys: [plain, !!binary a2V5]\n"},
		{"aliased binary secret", "proxy-url: &a !!binary c2VjcmV0\nmanagement:\n  secret-key: *a\n"},
		{"binary secret through a merge", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement:\n  <<: *d\n"},
		{"binary secret through an aliased section", "defaults: &d\n  secret-key: !!binary c2VjcmV0\nmanagement: *d\n"},
		{"unused bits", "management:\n  secret-key: !!binary YR==\n"},
		{"invalid base64", "management:\n  secret-key: !!binary 'c2VjcmV0!'\n"},
		{"missing padding", "management:\n  secret-key: !!binary YQ\n"},
		{"space in base64", "management:\n  secret-key: !!binary 'c2Vj cmV0'\n"},
		{"invalid base64 in an unknown setting", "extra: !!binary '!!!'\n"},
		{"binary unknown setting", "extra: !!binary c2VjcmV0\n"},
		{"not UTF-8", "management:\n  secret-key: !!binary /w==\n"},
		{"binary port", "port: !!binary ODMxNw==\n"},
		{"binary boolean", "debug: !!binary dHJ1ZQ==\n"},
		// Go reads a plugin's pin from the raw node, so these stay unpinned there.
		{"binary plugin pin", "plugins:\n  configs:\n    x:\n      store:\n        version: !!binary MS4yLjM=\n"},
		{"aliased binary plugin pin", "pin: &v !!binary MS4yLjM=\nplugins:\n  configs:\n    x:\n      store:\n        version: *v\n"},
		{"binary plugin pin through a merge", "base: &b\n  release-tag: !!binary djEuMi4z\nplugins:\n  configs:\n    x:\n      store:\n        <<: *b\n"},
		{"shadowed binary plugin pin", "defaults: &d {version: !!binary MS4yLjM=}\nplugins:\n  configs:\n    x:\n      store: {<<: *d, version: !keep 4.5.6}\n"},
		{"binary plugin setting", "plugins:\n  configs:\n    x:\n      store:\n        channel: !!binary c3RhYmxl\n"},
	}
	var out []result
	for _, c := range cases {
		r := result{Name: c.name, YAML: c.yaml}
		// ParseConfigBytes also validates and hashes the secret; the plain decode
		// shows the values as read.
		var cfg config.Config
		if _, err := config.ParseConfigBytes([]byte(c.yaml)); err != nil {
			r.Error = err.Error()
		} else if err := yaml.Unmarshal([]byte(c.yaml), &cfg); err != nil {
			panic(err)
		} else {
			r.SecretKey = cfg.RemoteManagement.SecretKey
			if !utf8.ValidString(r.SecretKey) {
				r.SecretBytes, r.SecretKey = []byte(r.SecretKey), ""
			}
			r.ProxyURL, r.Port, r.APIKeys = cfg.ProxyURL, cfg.Port, cfg.APIKeys
		}
		out = append(out, r)
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
