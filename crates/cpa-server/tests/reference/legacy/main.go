// Records Go's v0 management routes for the legacy.rs replay. Overlaid as
// cmd/zzlegacy/main.go in CLIProxyAPI 6fecc6e (see README.md). Every request goes
// through a complete api.NewServer via httptest: no sockets, no outbound calls.
package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/api"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"golang.org/x/crypto/bcrypt"
	"gopkg.in/yaml.v3"
)

type step struct {
	Method string `json:"method"`
	Path   string `json:"path"`
	Body   string `json:"body,omitempty"`
	Status int    `json:"status"`
	// Raw is the exact body: Go struct field order and spacing.
	Raw string `json:"raw_response"`
	// Config is the exact GET /v0/management/config body after a write.
	Config string `json:"config,omitempty"`
	// Groups names the v8 key groups in the saved file after a write (v8 files only).
	Groups map[string][]string `json:"groups,omitempty"`
}

// groupNames reads the saved file's api-keys groups by family.
func groupNames(path string) map[string][]string {
	data, err := os.ReadFile(path)
	must(err)
	var doc map[string]any
	must(yaml.Unmarshal(data, &doc))
	families, ok := doc["api-keys"].(map[string]any)
	if !ok {
		return nil
	}
	out := map[string][]string{}
	for family, groups := range families {
		names := []string{}
		list, _ := groups.([]any)
		for _, group := range list {
			entry, _ := group.(map[string]any)
			name, _ := entry["name"].(string)
			names = append(names, name)
		}
		out[family] = names
	}
	return out
}

type scenario struct {
	Name  string `json:"name"`
	YAML  string `json:"yaml"`
	Steps []step `json:"steps"`
}

var secretHash string

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func send(server *api.Server, method, path, body string) (int, string) {
	req := httptest.NewRequest(method, "/v0/management"+path, strings.NewReader(body))
	req.RemoteAddr = "127.0.0.1:1"
	req.Header.Set("Authorization", "Bearer fake-secret")
	if body != "" {
		req.Header.Set("Content-Type", "application/json")
	}
	rec := httptest.NewRecorder()
	server.Handler().ServeHTTP(rec, req)
	return rec.Code, rec.Body.String()
}

func run(s scenario) scenario {
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	dir, err := os.MkdirTemp("", "cpa-legacy-")
	must(err)
	defer os.RemoveAll(dir)
	path := filepath.Join(dir, "config.yaml")
	must(os.WriteFile(path, []byte(strings.ReplaceAll(s.YAML, "$HASH", secretHash)), 0o600))
	cfg, err := config.LoadConfig(path)
	must(err)
	server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path)
	for i := range s.Steps {
		st := &s.Steps[i]
		code, body := send(server, st.Method, st.Path, st.Body)
		st.Status = code
		st.Raw = body
		if st.Method != http.MethodGet {
			_, st.Config = send(server, http.MethodGet, "/config", "")
			st.Groups = groupNames(path)
		}
	}
	return s
}

func get(path string) step                { return step{Method: http.MethodGet, Path: path} }
func put(path, body string) step          { return step{Method: http.MethodPut, Path: path, Body: body} }
func patch(path, body string) step        { return step{Method: http.MethodPatch, Path: path, Body: body} }
func del(path string) step                { return step{Method: http.MethodDelete, Path: path} }
func post(path, body string) step         { return step{Method: http.MethodPost, Path: path, Body: body} }
func steps(s ...step) []step              { return s }
func cat(groups ...[]step) (out []step) {
	for _, g := range groups {
		out = append(out, g...)
	}
	return out
}

const minimalV8 = "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n"

const legacyFull = `port: 8317
remote-management:
  secret-key: '$HASH'
debug: true
proxy-url: "socks5://127.0.0.1:1080"
request-retry: 5
max-retry-credentials: 2
max-retry-interval: 30
force-model-prefix: true
logging-to-file: false
logs-max-total-size-mb: 100
error-logs-max-files: 3
usage-statistics-enabled: true
request-log: true
ws-auth: true
quota-exceeded:
  switch-project: true
  switch-preview-model: false
routing:
  strategy: ff
api-keys:
  - fake-key-1
  - " fake-key-2 "
  - fake-key-1
gemini-api-key:
  - api-key: fake-gem-1
    base-url: https://gem.example.invalid
    headers: {X-A: b, " ": c}
    excluded-models: [" m1 ", M1, ""]
  - api-key: " fake-gem-2 "
    prefix: /team/
    priority: 2
    weight: 3
claude-api-key:
  - api-key: fake-cl-1
    models: [{name: claude-x, alias: cx}]
    cloak: {mode: always}
codex-api-key:
  - api-key: fake-cx-1
    base-url: https://codex.example.invalid
openai-compatibility:
  - name: local
    base-url: http://127.0.0.1:9/v1
    api-key-entries: [{api-key: fake-oa}]
    models: [{name: m, alias: a}]
vertex-api-key:
  - api-key: fake-vx-1
    base-url: https://vertex.example.invalid
xai-api-key:
  - api-key: fake-xai-1
meta-api-key:
  - api-key: fake-meta-1
interactions-api-key:
  - api-key: fake-int-1
oauth-excluded-models:
  claude: [m1, " M2 "]
  " Codex ": [x]
oauth-model-alias:
  codex: [{name: a, alias: b}]
oauth-request-scoped-errors:
  codex: [{status: 429, match: [quota], action: switch}]
discovery:
  enabled: true
  service-name: office
`

const v8Full = `config-version: 8
server:
  port: 9000
  discovery:
    enabled: false
management:
  secret-key: '$HASH'
access:
  api-keys: [fake-a, fake-b]
observability:
  logs:
    debug: true
    logging-to-file: true
    error-logs-max-files: 0
  usage:
    usage-statistics-enabled: false
routing:
  strategy: weighted-round-robin
  force-model-prefix: false
  retry:
    request-retry: 1
requests:
  proxy-url: http://proxy.example.invalid:8080
oauth:
  excluded-models:
    gemini-cli: [a]
  providers:
    aistudio:
      ws-auth: false
api-keys:
  gemini:
    - name: shared
      base-url: https://g8.example.invalid
      models: [{name: g, alias: h}]
      keys:
        - api-key: fake-g8-a
        - api-key: fake-g8-b
          prefix: other
  openai-compatibility:
    - name: compat
      base-url: http://127.0.0.1:9/v1
      keys: [{api-key: fake-oa8}]
`

const richV8 = `config-version: 8
server:
  host: 127.0.0.1
  tls:
    enable: true
    cert: " /c.pem "
  discovery:
    enabled: true
    service-type: ""
    subtypes: []
    interfaces:
      include: [en0]
    auth-required: false
management:
  secret-key: '$HASH'
credentials:
  concurrency:
    max-limit: 5
    busy-retry-min: 300ms
  in-flight:
    stale-after: 20s
routing:
  session-affinity: true
  session-affinity-ttl: 1h
  cooldown:
    disable-cooling: true
    transient-error-cooldown-seconds: 30
requests:
  passthrough-headers: true
  streaming:
    keepalive-seconds: 15
  payload:
    default:
      - models: [{name: "gpt-*", protocol: openai}]
        params: {"reasoning.effort": high}
observability:
  logs:
    logs-max-total-size-mb: -3
    error-logs-max-files: -1
  usage:
    redis-usage-queue-retention-seconds: 99999
  pprof:
    enable: true
    addr: "  "
multimedia:
  disable-image-generation: chat
  gpt-image-2-base-model: gpt-5
oauth:
  auth-auto-refresh-workers: 4
  model-alias:
    " Gemini-CLI ":
      - {name: a, alias: b, fork: true}
      - {name: c, alias: B}
      - {name: d, alias: d}
  settings:
    claude: [{name: m, max-context-length: 1000}, {name: m, max-context-length: 2000}]
  request-scoped-errors:
    codex: [{status: 0, match: [x], action: switch}]
  providers:
    claude:
      claude-code:
        disable-cloaking-model-list: true
      header-defaults:
        user-agent: "  ua  "
    codex:
      header-defaults:
        beta-features: " b "
      disable-codex-cloaking: true
plugins:
  enabled: false
  dir: ./p
api-keys:
  claude:
    - name: c
      base-url: https://claude.example.invalid
      headers: {" X ": " y ", Z: ""}
      excluded-models: [" A ", a, ""]
      keys:
        - api-key: fake-c1
          cloak: {mode: " auto ", sensitive-words: [" w ", ""]}
          fingerprint-profile: " OAUTH-CLI "
        - api-key: fake-c2
          fingerprint-profile: weird
  vertex:
    - name: v
      base-url: https://v.example.invalid
      models: [{name: " n ", alias: " a "}, {name: x, alias: ""}]
      keys: [{api-key: fake-v1}, {api-key: fake-v1}, {api-key: " "}]
  meta:
    - name: m
      keys: [{api-key: "dca:x"}, {api-key: fake-m}]
  xai:
    - name: x
      base-url: https://xai.example.invalid
      keys: [{api-key: fake-x, alpha-search: true}]
`

// readsEdge holds yaml.v3 null handling, omitempty pruning and sanitizer edge cases.
const readsEdge = `config-version: 8
server:
  discovery:
    enabled: true
    interfaces:
      include: [null, eth0]
management:
  secret-key: '$HASH'
observability:
  logs:
    error-logs-max-files: null
credentials:
  in-flight:
    snapshot-interval: null
  concurrency:
    busy-retry-min: null
    max-limit: null
oauth:
  providers:
    aistudio:
      ws-auth: null
    codex:
      live-media-relay:
        max-sessions: null
  model-alias:
    codex: [{name: a, alias: b, display-name: " "}, {name: c, alias: d, display-name: " D "}]
plugins:
  configs:
    example: {}
requests:
  payload:
    default-raw:
      - models: [{name: "gpt-*"}]
        params: {"a.b": "{not json", "c": "  ", "d": " {\"x\":1} ", "e": "[1,2]"}
      - models: [{name: "m"}]
        params: {}
    default:
      - models: [{name: "*"}]
        params: {n: 1.0, f: 0.5, big: 12345678901, obj: {z: 1, a: {y: 2.0, b: [3.0]}}}
api-keys:
  claude:
    - name: c
      keys:
        - api-key: fake-c1
          cloak: {}
        - api-key: fake-c2
          cloak: {mode: ""}
  vertex:
    - name: v
      models: [{name: n, alias: a, display-name: "  "}]
      keys: [{api-key: fake-v1}]
`

// writesV8 holds an entry pair in every key family (the Gemini pair shares one v8
// group, so a write regroups it) and one entry in every OAuth map.
const writesV8 = `config-version: 8
management:
  secret-key: '$HASH'
oauth:
  excluded-models:
    claude: [m1]
  model-alias:
    codex: [{name: a, alias: b}]
  request-scoped-errors:
    codex: [{status: 429, match: [quota], action: switch}]
api-keys:
  gemini:
    - name: shared
      base-url: https://g.example.invalid
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  interactions:
    - name: i
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  claude:
    - name: c
      base-url: https://c.example.invalid
      keys:
        - api-key: fake-r-1
          cloak: {mode: always, sensitive-words: [secret]}
        - api-key: fake-r-2
  codex:
    - name: x
      base-url: https://x.example.invalid
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  xai:
    - name: x
      base-url: https://xai.example.invalid
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  meta:
    - name: m
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  vertex:
    - name: v
      base-url: https://v.example.invalid
      models: [{name: n, alias: a}]
      keys: [{api-key: fake-r-1}, {api-key: fake-r-2}]
  openai-compatibility:
    - name: compat
      base-url: http://127.0.0.1:9/v1
      keys: [{api-key: fake-oa}]
`

// keyListWrites drives one provider key list route through Go's PATCH, PUT and
// DELETE branches; family-specific fields are ignored by families without them.
func keyListWrites(r string) []step {
	return steps(
		get(r),
		patch(r, `{"index":0,"value":{"priority":5,"prefix":" /p/ ","headers":{" X ":" y ","Z":""},"excluded-models":[" A ","a",""]}}`),
		patch(r, `{"match":" fake-r-2 ","value":{"weight":7,"disable-cooling":true,"request-retry":2}}`),
		patch(r, `{"match":"fake-r-2","value":{"weight":null,"disable-cooling":null,"request-retry":null}}`),
		patch(r, `{"index":0,"value":{"weight":1.5}}`), patch(r, `{"index":0,"value":{"weight":2000000}}`),
		patch(r, `{"index":0,"value":{"weight":"3"}}`), patch(r, `{"index":0,"value":{"disable-cooling":"yes"}}`),
		patch(r, `{"index":9,"value":{"priority":1}}`), patch(r, `{"match":"missing","value":{}}`),
		patch(r, `{"index":0}`), patch(r, `{"index":0,"value":null}`), patch(r, `not json`), patch(r, ``),
		patch(r, `{"index":0,"value":{"priority":"x"}}`), patch(r, `{"index":-1,"value":{"priority":1}}`),
		patch(r, `{"Index":0,"VALUE":{"Priority":3}} trailing`),
		patch(r, `{"index":0,"value":{"models":[{"name":" m ","alias":" a "},{"name":"","alias":""},{"name":"only"}]}}`),
		patch(r, `{"index":0,"value":{"request-scoped-errors":[{"status":429,"match":["quota"],"action":"switch"}]}}`),
		patch(r, `{"index":0,"value":{"request-scoped-errors":[],"models":[]}}`),
		patch(r, `{"index":0,"value":{"headers":{"A":"1"},"headers":{"B":"2","A":null}}}`),
		patch(r, `{"index":0,"value":{"prefix":"a&b","proxy-url":" socks5://h<1> "}}`),
		patch(r, `{"index":1,"value":{"alpha-search":true,"websockets":true,"disable-codex-cloaking":true,"rebuild-mid-system-message":true}}`),
		patch(r, `{"index":1,"value":{"disable-codex-cloaking":"x"}}`),
		patch(r, `{"index":0,"value":{"api-key":"  fake-r-new  ","base-url":" https://new.example.invalid "}}`),
		get(r),
		put(r, `[{"api-key":"fake-p1","base-url":"https://p.example.invalid","weight":3,"models":[{"name":" n ","alias":""}]},{"api-key":" fake-p2 ","prefix":"x/y","base-url":" https://p2.example.invalid "}]`),
		put(r, `{"items":[{"api-key":"fake-p3","base-url":"https://p3.example.invalid"}]}`),
		put(r, `{"ITEMS":[{"API-KEY":"fake-p4","base-url":"https://p4.example.invalid"}],"items":[{"base-url":"https://p5.example.invalid"}]}`),
		put(r, `{"items":[]}`), put(r, `[{"api-key":1}]`), put(r, `[{"api-key":"k","base-url":"https://k.example.invalid","weight":1000001}]`),
		put(r, `"x"`), put(r, `[] x`),
		put(r, `[{"api-key":"fake-q","base-url":""},{"api-key":"","base-url":"https://only-base.example.invalid"},{"api-key":"dca:x","base-url":"https://d.example.invalid"}]`),
		put(r, `[]`), get(r), put(r, `null`), get(r),
		del(r+"?api-key=nothing&base-url=https://a.example.invalid"),
		put(r, `[{"api-key":"d1","base-url":"https://a.example.invalid"},{"api-key":"d1","base-url":"https://b.example.invalid"},{"api-key":"d2","base-url":"https://a.example.invalid"},{"api-key":"d3","base-url":"https://a.example.invalid"}]`),
		patch(r, `{"match":"d1","value":{"priority":4}}`),
		patch(r+"?base-url=https://b.example.invalid", `{"match":"d1","value":{"priority":6}}`),
		del(r+"?api-key=d1"), del(r+"?api-key=d1&base-url=%20https://b.example.invalid%20"),
		del(r+"?api-key=missing"), del(r+"?api-key=missing&base-url=x"),
		del(r+"?index=1"), del(r+"?index=x"), del(r+"?index=99"), del(r), del(r+"?api-key=%20&index=0"),
		del(r+"?api-key=d2&base-url=https://a.example.invalid"),
		put(r, `[{"api-key":"e1","base-url":"https://e.example.invalid"},{"api-key":"e2","base-url":"https://e.example.invalid"}]`),
		patch(r, `{"index":0,"value":{"base-url":" "}}`), patch(r, `{"index":0,"value":{"api-key":"","base-url":""}}`),
		get(r),
	)
}

// decoderEdges exercises encoding/json corners: integer tokens, skipped members,
// string repair, EqualFold field names and Go's nesting limit.
func decoderEdges() []step {
	r := "/gemini-api-key"
	deep := strings.Repeat("[", 300) + strings.Repeat("]", 300)
	return steps(
		patch(r, `{"index":0,"value":{"weight":-0}}`), patch(r, `{"index":0,"value":{"weight":1e400}}`),
		patch(r, `{"index":0,"value":{"weight":1e2}}`), patch(r, `{"index":0,"value":{"weight": 4 }}`),
		patch(r, `{"index":0,"value":{"disable-cooling": true }}`),
		put(r, `[{"api-key":"k","unknown":1e400}]`),
		put(r, `[{"api-key":"k\ud800x","base-url":"https://\u00e9.example.invalid"}]`),
		put(r, `[{"api-key":"k","pr\u0131ority":7,"excluded-model\u017f":["X"]}]`),
		put(r, `[{"api-key":"k","x":`+deep+`}]`),
		put(r, `[{"api-key":"k","priority":01}]`), put(r, `[{"api-key":"k","prefix":"a\'b"}]`),
		patch(r, `{"index":0,"value":{"headers":{"A":"1"}},"index":"x"}`),
		patch(r, `{"match":"k","value":{"priority":2},"value":{"prefix":"p"}}`),
		put("/oauth-model-alias", `{"codex":[{"name":"\u017f","alias":"s"},{"name":"K","alias":"\u212a"},{"name":"i","alias":"\u0131"}]}`),
		get(r), get("/oauth-model-alias"),
	)
}

// configYAMLWrites drives v0 PUT /config.yaml (Go PutConfigYAML): uploads that load,
// syntax and shape errors (400) and LoadConfig validation errors (422).
func configYAMLWrites() []step {
	r := "/config.yaml"
	v8Head := "config-version: 8\nmanagement:\n  secret-key: fake-secret\n"
	legacyHead := "remote-management:\n  secret-key: fake-secret\n"
	return steps(
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\n    # indented comment\nobservability:\n  logs:\n    debug: true\nrouting:\n  strategy: fill-first\n"),
		get("/debug"),
		put(r, "remote-management:\n  secret-key: fake-secret\ndebug: false\nrequest-retry: 4\napi-keys: [fake-k1]\n"),
		get("/request-retry"), get("/api-keys"),
		put(r, "a: [\n"), put(r, "- 1\n"), put(r, "plain scalar\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\napi-keys:\n  claude:\n    - name: c\n      keys: [{api-key: fake-k, weight: 2000000}]\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\nserver:\n  trusted-proxies: [not-an-ip]\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\napi-keys:\n  gemini: {name: x}\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\napi-keys:\n  codex:\n    - name: x\n      keys: [{api-key: fake-k, weight: 1.5}]\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: fake-secret\napi-keys:\n  xai: [{name: x}]\n"),
		put(r, "config-version: 7\nmanagement:\n  secret-key: fake-secret\n"),
		put(r, "remote-management:\n  secret-key: fake-secret\ngemini-api-key:\n  - api-key: fake-k\n    weight: 2000000\n"),
		put(r, "openai-compatibility:\n  - name: o\n    base-url: http://127.0.0.1:9\n    api-key-entries: [{api-key: a}, {api-key: b, weight: x}]\nremote-management:\n  secret-key: fake-secret\n"),
		put(r, v8Head+"api-keys:\n  gemini:\n    - name: g\n      keys:\n        - api-key: fake-k\n          <<: {weight: null}\n"),
		put(r, legacyHead+"gemini-api-key: [{api-key: fake-k, weight: 1.5}]\n"),
		put(r, v8Head+"api-keys:\n  gemini:\n    - unexpected: true\n      keys: []\n"),
		put(r, v8Head+"api-keys:\n  codex:\n    - name: c\n      keys: [plain]\n"),
		put(r, v8Head+"api-keys:\n  xai:\n    - name: x\n      keys: [{api-key: fake-k, base-url: http://x.invalid}]\n"),
		put(r, v8Head+"api-keys:\n  openai-compatibility:\n    - name: o\n      base-url: http://127.0.0.1:9\n      extra: 1\n      keys: [{api-key: fake-k}]\n"),
		put(r, "config-version: 8\nmanagement:\n  secret-key: "+strings.Repeat("a", 73)+"\n"),
		put(r, legacyHead+"gemini-api-key: [{api-key: fake-k, weight: 2000000}]\napi-keys: {gemini: []}\n"),
		get("/gemini-api-key"),
		get("/request-retry"),
		// Last: a 72-byte key is accepted and replaces the harness key (later reads 401).
		put(r, "config-version: 8\nmanagement:\n  secret-key: "+strings.Repeat("b", 72)+"\n"),
		// An empty upload is accepted (200) and leaves no management key; the harness
		// has no watcher to disable the routes as a real server's reload does, so the
		// Rust test checks that case on its own.
	)
}

func claudeWrites() []step {
	r := "/claude-api-key"
	return steps(
		patch(r, `{"index":0,"value":{"fingerprint-profile":" OAUTH-CLI "}}`),
		patch(r, `{"index":0,"value":{"fingerprint-profile":"weird<x>"}}`),
		patch(r, `{"index":0,"value":{"cloak":{"mode":" never ","sensitive-words":[" w ",""],"cache-user-id":true}}}`),
		patch(r, `{"index":0,"value":{"cloak":{"mode":"","strict-mode":true}}}`),
		patch(r, `{"index":0,"value":{"cloak":"x"}}`), patch(r, `{"index":0,"value":{"cloak":{"mode":1}}}`),
		patch(r, `{"index":0,"value":{"base-url":"https://moved.example.invalid"}}`),
		patch(r, `{"index":0,"value":{"cloak":{"mode":"auto"}}}`),
		patch(r, `{"index":0,"value":{"api-key":"fake-c-new","cloak":{"strict-mode":true}}}`),
		patch(r, `{"index":0,"value":{"cloak":{"mode":" "}}}`),
		patch(r, `{"index":0,"value":{"cloak":null}}`),
		patch(r, `{"index":1,"value":{"cloak":{}}}`),
		put(r, `[{"api-key":"fake-c-new","base-url":"https://moved.example.invalid"},{"api-key":"fake-r-2","base-url":"https://c.example.invalid","cloak":{"mode":" "}}]`),
		put(r, `[{"api-key":"fake-c-new","base-url":"https://moved.example.invalid","fingerprint-profile":"bogus"}]`),
		put(r, `[{"api-key":" fake-c-x ","base-url":" https://c.example.invalid ","fingerprint-profile":" Claude-Code-CLI ","headers":{"A":" b "},"cloak":{"sensitive-words":[""]}}]`),
		get(r),
	)
}

func openAIWrites() []step {
	r := "/openai-compatibility"
	return steps(
		patch(r, `{"name":" compat ","value":{"priority":2,"prefix":" p ","disabled":true,"support-prompt-cache-key":true,"headers":{"A":"1"}}}`),
		patch(r, `{"name":"compat","value":{"api-key-entries":[{"api-key":" k1 ","weight":2},{"api-key":"k2"}],"models":[{"name":"m","alias":"a"}]}}`),
		patch(r, `{"index":0,"value":{"api-key-entries":[{"api-key":"k","weight":1000001}]}}`),
		patch(r, `{"index":0,"value":{"disable-cooling":5,"base-url":""}}`),
		patch(r, `{"name":"missing","value":{}}`), patch(r, `{"match":"compat","value":{"priority":9}}`),
		patch(r, `{"index":0,"value":{"name":" renamed ","request-retry":3}}`),
		put(r, `[{"name":"a","base-url":" http://a.example.invalid ","api-key-entries":[{"api-key":" x "}]},{"name":"b","base-url":" "},{"name":"c","base-url":"http://c.example.invalid","headers":{" ":"x"}}]`),
		put(r, `[{"name":"a","base-url":"http://a.example.invalid","api-key-entries":[{"api-key":"x"},{"api-key":"y","weight":2000000}]}]`),
		patch(r, `{"index":1,"value":{"base-url":" "}}`),
		del(r+"?name=a"), del(r+"?name=%20c"), del(r+"?index=0"), del(r+"?index=0"), del(r),
		put(r, `null`), put(r, `{"items":[{"name":"z","base-url":"http://z.example.invalid"}]}`),
		get(r),
	)
}

func mapWrites() []step {
	return steps(
		put("/oauth-excluded-models", `{" Codex ":[" A ","a",""],"claude":[]}`),
		put("/oauth-excluded-models", `{"items":{"x":["m"]}}`), put("/oauth-excluded-models", `{"items":["a"]}`),
		put("/oauth-excluded-models", `[]`), put("/oauth-excluded-models", `{"a":"b"}`),
		patch("/oauth-excluded-models", `{"provider":" GEMINI ","models":["x"," X "]}`),
		patch("/oauth-excluded-models", `{"provider":"gemini","models":[]}`),
		patch("/oauth-excluded-models", `{"provider":"gemini","models":[" "]}`),
		patch("/oauth-excluded-models", `{"models":["x"]}`), patch("/oauth-excluded-models", `{"provider":" "}`),
		patch("/oauth-excluded-models", `{"provider":"a","models":"x"}`),
		del("/oauth-excluded-models?provider=%20ITEMS"), del("/oauth-excluded-models?provider=none"), del("/oauth-excluded-models"),
		put("/oauth-excluded-models", `null`), del("/oauth-excluded-models?provider=x"),
		get("/oauth-excluded-models"),
		put("/oauth-model-alias", `{"codex":[{"name":"a","alias":"b"},{"name":"c","alias":"B"},{"name":"d","alias":"d"}]," Gemini-CLI ":[]}`),
		patch("/oauth-model-alias", `{"channel":"Gemini-CLI","aliases":[{"name":" x ","alias":" y ","fork":true,"display-name":" Y "}]}`),
		patch("/oauth-model-alias", `{"provider":"claude","channel":null,"aliases":[{"name":"p","alias":"q"}]}`),
		patch("/oauth-model-alias", `{"provider":"gemini-cli","aliases":[]}`), patch("/oauth-model-alias", `{"channel":"gemini-cli"}`),
		patch("/oauth-model-alias", `{}`), patch("/oauth-model-alias", `{"aliases":5}`),
		del("/oauth-model-alias?channel=CODEX"), del("/oauth-model-alias?provider=claude"), del("/oauth-model-alias?provider=x"),
		del("/oauth-model-alias"), put("/oauth-model-alias", `{}`), get("/oauth-model-alias"),
		put("/oauth-request-scoped-errors", `{"codex":[{"status":429,"match":[" q "],"action":" SWITCH "},{"status":0,"match":["x"],"action":"a"}]}`),
		patch("/oauth-request-scoped-errors", `{"channel":"claude","rules":[{"status":500,"match-regexr":["^x"],"action":"retry"}]}`),
		patch("/oauth-request-scoped-errors", `{"channel":"claude","rules":[{"status":500,"action":"retry"}]}`),
		patch("/oauth-request-scoped-errors", `{"channel":"none","rules":[]}`),
		del("/oauth-request-scoped-errors?channel=codex"), del("/oauth-request-scoped-errors?channel=codex"),
		put("/oauth-request-scoped-errors", `{"items":{"codex":[{"status":1,"match":["m"],"action":"x"}]}}`),
		get("/oauth-request-scoped-errors"),
	)
}

func fieldRoutes() []step {
	var out []step
	for _, p := range []string{"/debug", "/logging-to-file", "/logs-max-total-size-mb", "/error-logs-max-files",
		"/usage-statistics-enabled", "/proxy-url", "/quota-exceeded/switch-project", "/quota-exceeded/switch-preview-model",
		"/request-log", "/ws-auth", "/request-retry", "/max-retry-credentials", "/max-retry-interval",
		"/force-model-prefix", "/routing/strategy", "/api-keys", "/gemini-api-key", "/interactions-api-key",
		"/claude-api-key", "/codex-api-key", "/xai-api-key", "/meta-api-key", "/openai-compatibility",
		"/vertex-api-key", "/oauth-excluded-models", "/oauth-model-alias", "/oauth-request-scoped-errors"} {
		out = append(out, get(p))
	}
	return out
}

func scalarWrites() []step {
	return steps(
		put("/debug", `{"value":true}`), patch("/debug", `{"value":false}`), put("/debug", `{"value":"true"}`),
		put("/debug", `{}`), put("/debug", `not json`), put("/debug", ``), put("/debug", `{"Value":true}`),
		put("/logging-to-file", `{"value":true}`), put("/usage-statistics-enabled", `{"value":true}`),
		put("/request-log", `{"value":true}`), put("/ws-auth", `{"value":true}`),
		put("/quota-exceeded/switch-project", `{"value":true}`), patch("/quota-exceeded/switch-preview-model", `{"value":true}`),
		put("/force-model-prefix", `{"value":true}`), put("/force-model-prefix", `{"value":"x"}`),
		put("/logs-max-total-size-mb", `{"value":-5}`), put("/logs-max-total-size-mb", `{"value":250}`),
		put("/logs-max-total-size-mb", `{"value":1.5}`), put("/logs-max-total-size-mb", `{"value":"7"}`),
		put("/error-logs-max-files", `{"value":-1}`), put("/error-logs-max-files", `{"value":0}`),
		put("/request-retry", `{"value":-2}`), put("/request-retry", `{"value":7}`), put("/request-retry", `{"value":-0}`),
		put("/request-retry", `{"VALU\u0117":3}`), put("/debug", `{"valu\u0435":true}`),
		put("/max-retry-credentials", `{"value":4}`), patch("/max-retry-interval", `{"value":12}`),
		put("/proxy-url", `{"value":"  http://p.example.invalid:1  "}`), put("/proxy-url", `{"value":5}`),
		del("/proxy-url"),
		put("/routing/strategy", `{"value":"RR"}`), put("/routing/strategy", `{"value":"wrr"}`),
		put("/routing/strategy", `{"value":"bogus"}`), put("/routing/strategy", `{"value":""}`),
		patch("/routing/strategy", `{"value":"Fill-First"}`),
		get("/debug"), get("/routing/strategy"), get("/error-logs-max-files"),
	)
}

func apiKeyWrites() []step {
	return steps(
		put("/api-keys", `["k1"," k2 ","k1"]`), get("/api-keys"),
		put("/api-keys", `{"items":["k3"]}`), put("/api-keys", `{"items":[]}`), put("/api-keys", `[]`),
		put("/api-keys", `"x"`),
		put("/api-keys", `{"items":7,"items":["replacement"]}`), put("/api-keys", `{"ITEMS":["replacement"]}`),
		put("/api-keys", `{"items":["replacement"],"ITEMS":null}`), put("/api-keys", `["a",null]`),
		put("/api-keys", `{"items":["a",1]}`), put("/api-keys", `[1]`),
		put("/api-keys", `{"items":["a","b"],"items":[null]}`), put("/api-keys", `{"item\u017f":["s1","s2"]}`),
		del("/api-keys?index=%C2%A00"), del("/api-keys?index=%0A0"), del("/api-keys?index=%0B1"),
		patch("/api-keys", `{"index":0,"value":"k0"}`), patch("/api-keys", `{"index":9,"value":"k9"}`),
		patch("/api-keys", `{"old":"k0","new":"k00"}`), patch("/api-keys", `{"old":"missing","new":"k-new"}`),
		patch("/api-keys", `{"old":"k00"}`), patch("/api-keys", `{"old":null,"new":"k-null"}`),
		get("/api-keys"),
		del("/api-keys?index=1"), del("/api-keys?index=x&value=k-new"), del("/api-keys?value=%20k-null%20"),
		del("/api-keys?index=99"), del("/api-keys"),
		get("/api-keys"),
	)
}

func main() {
	gin.SetMode(gin.ReleaseMode)
	hash, errHash := bcrypt.GenerateFromPassword([]byte("fake-secret"), 4)
	must(errHash)
	secretHash = string(hash)
	var out []scenario
	for _, s := range []scenario{
		{Name: "reads-minimal", YAML: minimalV8, Steps: cat(steps(get("/config")), fieldRoutes())},
		{Name: "reads-legacy", YAML: legacyFull, Steps: cat(steps(get("/config")), fieldRoutes())},
		{Name: "reads-v8", YAML: v8Full, Steps: cat(steps(get("/config")), fieldRoutes())},
		{Name: "reads-rich", YAML: richV8, Steps: cat(steps(get("/config")), fieldRoutes())},
		{Name: "reads-edge", YAML: readsEdge, Steps: cat(steps(get("/config")), fieldRoutes())},
		{Name: "scalars-v8", YAML: minimalV8, Steps: scalarWrites()},
		{Name: "scalars-legacy", YAML: legacyFull, Steps: scalarWrites()},
		{Name: "api-keys", YAML: v8Full, Steps: apiKeyWrites()},
		{Name: "gemini-writes", YAML: writesV8, Steps: keyListWrites("/gemini-api-key")},
		{Name: "decoder-edges", YAML: writesV8, Steps: decoderEdges()},
		{Name: "interactions-writes", YAML: writesV8, Steps: keyListWrites("/interactions-api-key")},
		{Name: "claude-writes", YAML: writesV8, Steps: cat(keyListWrites("/claude-api-key"))},
		{Name: "claude-cloak", YAML: writesV8, Steps: claudeWrites()},
		{Name: "codex-writes", YAML: writesV8, Steps: keyListWrites("/codex-api-key")},
		{Name: "xai-writes", YAML: writesV8, Steps: keyListWrites("/xai-api-key")},
		{Name: "meta-writes", YAML: writesV8, Steps: keyListWrites("/meta-api-key")},
		{Name: "vertex-writes", YAML: writesV8, Steps: keyListWrites("/vertex-api-key")},
		{Name: "openai-writes", YAML: writesV8, Steps: openAIWrites()},
		{Name: "map-writes", YAML: writesV8, Steps: mapWrites()},
		{Name: "config-yaml-put", YAML: writesV8, Steps: configYAMLWrites()},
		{Name: "legacy-writes", YAML: legacyFull, Steps: cat(claudeWrites(), mapWrites())},
	} {
		out = append(out, run(s))
	}
	data, err := json.Marshal(map[string]any{"scenarios": out})
	must(err)
	must(os.WriteFile(os.Args[1], append(data, '\n'), 0o644))
	fmt.Println("scenarios:", len(out))
}
