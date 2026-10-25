package config

import (
	"encoding/json"
	"os"
	"reflect"
	"strconv"
	"strings"
	"testing"
)

// Records the string settings Go v8.0.35 loads from plain YAML scalars that are not
// strings (numbers, booleans): yaml.v3 decodes each into the Go string as its text.
// Each check names a field by its Go YAML path and by its v8 path in cliproxy-rs.
func TestZZScalarSettings(t *testing.T) {
	type check struct {
		Go    string `json:"go"`
		Rust  string `json:"rust"`
		Value string `json:"value"`
	}
	type record struct {
		Name   string  `json:"name"`
		YAML   string  `json:"yaml"`
		Checks []check `json:"checks"`
	}
	cases := []record{
		{Name: "v8", YAML: `server:
  host: 1.50
  github-token: 0x1F
  discovery: {service-name: 1.0, subtypes: [007], interfaces: {include: [0x1F], exclude: [true]}}
management: {panel-github-repository: 2024}
access: {api-keys: [0x1F, 1.50]}
requests:
  payload:
    filter: [{models: [{name: m, protocol: 1.0, from-protocol: 2.0, exist: [1.50], not-exist: [007], headers: {x-v: 1.10}}], params: [1.50, 0x1F]}]
oauth:
  auth-dir: 1.50
  model-alias: {claude: [{name: m1, alias: a1, display-name: 2024}]}
  request-scoped-errors: {claude: [{status: 400, match: [1.50], action: stop}]}
  settings: {claude: [{name: 1.50, alias: 2.50}]}
  providers: {antigravity: {sensitive-words: [007]}}
plugins: {store-sources: [1.50]}
api-keys:
  claude:
    - name: 1.50
      base-url: http://127.0.0.1:1
      prefix: 0x1F
      headers: {X-V: 1.10}
      models: [{name: m, alias: a, display-name: 1.50}]
      keys: [{api-key: 0x1F, fingerprint-profile: 1.0}]
  openai-compatibility:
    - name: 2024
      base-url: http://127.0.0.1:2
      keys: [{api-key: 1e3}]
      models: [{name: om, alias: oa}]
`, Checks: []check{
			{Go: "host", Rust: "server.host"},
			{Go: "github-token", Rust: "server.github-token"},
			{Go: "discovery.service-name", Rust: "server.discovery.service-name"},
			{Go: "discovery.subtypes.0", Rust: "server.discovery.subtypes.0"},
			{Go: "discovery.interfaces.include.0", Rust: "server.discovery.interfaces.include.0"},
			{Go: "discovery.interfaces.exclude.0", Rust: "server.discovery.interfaces.exclude.0"},
			{Go: "remote-management.panel-github-repository", Rust: "management.panel-github-repository"},
			{Go: "api-keys.0", Rust: "access.api-keys.0"},
			{Go: "api-keys.1", Rust: "access.api-keys.1"},
			{Go: "payload.filter.0.models.0.protocol", Rust: "requests.payload.filter.0.models.0.protocol"},
			{Go: "payload.filter.0.models.0.from-protocol", Rust: "requests.payload.filter.0.models.0.from-protocol"},
			{Go: "payload.filter.0.models.0.exist.0", Rust: "requests.payload.filter.0.models.0.exist.0"},
			{Go: "payload.filter.0.models.0.not-exist.0", Rust: "requests.payload.filter.0.models.0.not-exist.0"},
			{Go: "payload.filter.0.models.0.headers.x-v", Rust: "requests.payload.filter.0.models.0.headers.x-v"},
			{Go: "payload.filter.0.params.0", Rust: "requests.payload.filter.0.params.0"},
			{Go: "payload.filter.0.params.1", Rust: "requests.payload.filter.0.params.1"},
			{Go: "auth-dir", Rust: "oauth.auth-dir"},
			{Go: "oauth-model-alias.claude.0.display-name", Rust: "oauth.model-alias.claude.0.display-name"},
			{Go: "oauth-request-scoped-errors.claude.0.match.0", Rust: "oauth.request-scoped-errors.claude.0.match.0"},
			{Go: "oauth-settings.claude.0.name", Rust: "oauth.settings.claude.0.name"},
			{Go: "oauth-settings.claude.0.alias", Rust: "oauth.settings.claude.0.alias"},
			{Go: "antigravity.sensitive-words.0", Rust: "oauth.providers.antigravity.sensitive-words.0"},
			{Go: "plugins.store-sources.0", Rust: "plugins.store-sources.0"},
			{Go: "claude-api-key.0.prefix", Rust: "api-keys.claude.0.prefix"},
			{Go: "claude-api-key.0.headers.X-V", Rust: "api-keys.claude.0.headers.X-V"},
			{Go: "claude-api-key.0.models.0.display-name", Rust: "api-keys.claude.0.models.0.display-name"},
			{Go: "claude-api-key.0.api-key", Rust: "api-keys.claude.0.keys.0.api-key"},
			{Go: "claude-api-key.0.fingerprint-profile", Rust: "api-keys.claude.0.keys.0.fingerprint-profile"},
			{Go: "openai-compatibility.0.name", Rust: "api-keys.openai-compatibility.0.name"},
			{Go: "openai-compatibility.0.api-key-entries.0.api-key", Rust: "api-keys.openai-compatibility.0.keys.0.api-key"},
		}},
		{Name: "legacy", YAML: `host: 1.50
auth-dir: 007
api-keys: [0x1F]
github-token: 1.50
claude-api-key: [{api-key: 0x1F, prefix: 1.50, base-url: http://127.0.0.1:1, headers: {X-V: 1.10}}]
openai-compatibility: [{name: 2024, base-url: http://127.0.0.1:2, api-key-entries: [{api-key: 1e3}]}]
oauth-settings: {claude: [{name: 1.50, alias: 2.50}]}
proxy-url: 1.50
`, Checks: []check{
			{Go: "proxy-url", Rust: "requests.proxy-url"},
			{Go: "host", Rust: "server.host"},
			{Go: "auth-dir", Rust: "oauth.auth-dir"},
			{Go: "api-keys.0", Rust: "access.api-keys.0"},
			{Go: "github-token", Rust: "server.github-token"},
			{Go: "claude-api-key.0.api-key", Rust: "api-keys.claude.0.keys.0.api-key"},
			{Go: "claude-api-key.0.prefix", Rust: "api-keys.claude.0.prefix"},
			{Go: "claude-api-key.0.headers.X-V", Rust: "api-keys.claude.0.headers.X-V"},
			{Go: "openai-compatibility.0.name", Rust: "api-keys.openai-compatibility.0.name"},
			{Go: "openai-compatibility.0.api-key-entries.0.api-key", Rust: "api-keys.openai-compatibility.0.keys.0.api-key"},
			{Go: "oauth-settings.claude.0.name", Rust: "oauth.settings.claude.0.name"},
			{Go: "oauth-settings.claude.0.alias", Rust: "oauth.settings.claude.0.alias"},
		}},
		{Name: "anchored-key", YAML: `x: {&k 1.50: v}
oauth:
  model-alias: {codex: [{name: *k, alias: z}]}
`, Checks: []check{
			{Go: "oauth-model-alias.codex.0.name", Rust: "oauth.model-alias.codex.0.name"},
		}},
		{Name: "v8.0.35-fields", YAML: `oauth: {providers: {codex: {signin-with-chatgpt-identifier: 1.50}}}
client: {codex: {web-search-result-cache: {ttl: 007}}}
routing: {session-affinity-rebind-on: 0x1F}
upstream: {claude: {header-defaults: {anthropic-beta-append: [1.50], anthropic-beta-strip: [true]}}}
api-keys:
  claude:
    - base-url: http://127.0.0.1:1
      models: [{name: m, alias: a, thinking: {disable-type: 1.0}}]
      keys: [{api-key: k}]
`, Checks: []check{
			{Go: "codex.signin-with-chatgpt-identifier", Rust: "oauth.providers.codex.signin-with-chatgpt-identifier"},
			{Go: "client.codex.web-search-result-cache.ttl", Rust: "client.codex.web-search-result-cache.ttl"},
			{Go: "routing.session-affinity-rebind-on", Rust: "routing.session-affinity-rebind-on"},
			{Go: "claude-header-defaults.anthropic-beta-append.0", Rust: "upstream.claude.header-defaults.anthropic-beta-append.0"},
			{Go: "claude-header-defaults.anthropic-beta-strip.0", Rust: "upstream.claude.header-defaults.anthropic-beta-strip.0"},
			{Go: "claude-api-key.0.models.0.thinking.disable-type", Rust: "api-keys.claude.0.models.0.thinking.disable-type"},
		}},
	}
	for i := range cases {
		cfg, err := ParseConfigBytes([]byte(cases[i].YAML))
		if err != nil {
			t.Fatalf("%s: %v", cases[i].Name, err)
		}
		for j := range cases[i].Checks {
			c := &cases[i].Checks[j]
			v := reflect.ValueOf(cfg)
			for _, part := range strings.Split(c.Go, ".") {
				v = step(v, part)
				if !v.IsValid() {
					t.Fatalf("%s: no %s", cases[i].Name, c.Go)
				}
			}
			c.Value = v.String()
		}
	}
	data, _ := json.MarshalIndent(map[string]any{"cases": cases}, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// step follows one YAML path part: a struct field by its yaml tag (inline structs
// included), a slice index, or a map key.
func step(v reflect.Value, part string) reflect.Value {
	for v.Kind() == reflect.Pointer || v.Kind() == reflect.Interface {
		v = v.Elem()
	}
	switch v.Kind() {
	case reflect.Struct:
		for i := 0; i < v.NumField(); i++ {
			tag := strings.Split(v.Type().Field(i).Tag.Get("yaml"), ",")
			if tag[0] == part {
				return v.Field(i)
			}
			if len(tag) > 1 && tag[0] == "" && tag[1] == "inline" {
				if f := step(v.Field(i), part); f.IsValid() {
					return f
				}
			}
		}
	case reflect.Slice:
		if n, err := strconv.Atoi(part); err == nil && n < v.Len() {
			return v.Index(n)
		}
	case reflect.Map:
		return v.MapIndex(reflect.ValueOf(part))
	}
	return reflect.Value{}
}
