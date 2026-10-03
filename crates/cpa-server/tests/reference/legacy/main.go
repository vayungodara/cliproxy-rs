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
		put("/request-retry", `{"value":-2}`), put("/request-retry", `{"value":7}`),
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
		{Name: "scalars-v8", YAML: minimalV8, Steps: scalarWrites()},
		{Name: "scalars-legacy", YAML: legacyFull, Steps: scalarWrites()},
		{Name: "api-keys", YAML: v8Full, Steps: apiKeyWrites()},
	} {
		out = append(out, run(s))
	}
	data, err := json.Marshal(map[string]any{"scenarios": out})
	must(err)
	must(os.WriteFile(os.Args[1], append(data, '\n'), 0o644))
	fmt.Println("scenarios:", len(out))
}
