// Generates management/config goldens by calling pinned CLIProxyAPI code in-process.
// It never opens network connections: gin engines run through httptest recorders.
package main

import (
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginhost"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/api"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/api/handlers/management"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	sdkAuth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"golang.org/x/crypto/bcrypt"
	"gopkg.in/yaml.v3"
)

type step struct {
	Remote  string              `json:"remote"`
	Headers map[string][]string `json:"headers,omitempty"`
	Status  int                 `json:"status"`
	Body    string              `json:"body"`
	// Response headers that Go's management middleware sets.
	Version string `json:"x_cpa_version"`
}

type accessScenario struct {
	Name          string   `json:"name"`
	Secret        string   `json:"secret,omitempty"`
	AllowRemote   bool     `json:"allow_remote"`
	Env           string   `json:"env,omitempty"`
	LocalPassword string   `json:"local_password,omitempty"`
	Trusted       []string `json:"trusted"`
	Steps         []step   `json:"steps"`
}

type ipCase struct {
	Trusted []string            `json:"trusted"`
	Remote  string              `json:"remote"`
	Headers map[string][]string `json:"headers,omitempty"`
	IP      string              `json:"ip"`
}

type authSummary struct {
	ID         string            `json:"id"`
	Provider   string            `json:"provider"`
	Label      string            `json:"label"`
	Prefix     string            `json:"prefix"`
	ProxyURL   string            `json:"proxy_url"`
	Disabled   bool              `json:"disabled"`
	Status     string            `json:"status"`
	Index      string            `json:"auth_index"`
	FileName   string            `json:"file_name,omitempty"`
	Attributes map[string]string `json:"attributes"`
	Metadata   map[string]any    `json:"metadata"`
}

type synthCase struct {
	Name   string            `json:"name"`
	YAML   string            `json:"yaml"`
	Files  map[string]string `json:"files,omitempty"`
	Error  string            `json:"error,omitempty"`
	Config []authSummary     `json:"config"`
	File   []authSummary     `json:"file"`
}

type loadCase struct {
	YAML  string `json:"yaml"`
	Error string `json:"error"`
}

// A request against a complete in-process Go server. Update, when set, is a config
// document written to disk and applied with UpdateClients before the request.
type routeStep struct {
	Update     string              `json:"update,omitempty"`
	Method     string              `json:"method"`
	Path       string              `json:"path"`
	Remote     string              `json:"remote"`
	Headers    map[string][]string `json:"headers,omitempty"`
	Body       string              `json:"body,omitempty"`
	Status     int                 `json:"status"`
	RespBody   string              `json:"resp_body"`
	RespHeader map[string]string   `json:"resp_headers"`
}

type routeScenario struct {
	Name          string      `json:"name"`
	YAML          string      `json:"yaml"`
	Env           string      `json:"env,omitempty"`
	LocalPassword string      `json:"local_password,omitempty"`
	Steps         []routeStep `json:"steps"`
}

var observedHeaders = []string{"Content-Type", "Cache-Control", "Access-Control-Allow-Origin", "Access-Control-Allow-Methods",
	"Access-Control-Allow-Headers", "Access-Control-Expose-Headers", "X-CPA-SUPPORT-PLUGIN", "X-CPA-COMMIT", "X-CPA-BUILD-DATE"}

// SecretHash is substituted for $HASH in scenario YAML (bcrypt of "fake-secret").
var secretHash string

func writeConfig(path, yamlText string) *config.Config {
	must(os.WriteFile(path, []byte(strings.ReplaceAll(yamlText, "$HASH", secretHash)), 0o600))
	cfg, err := config.LoadConfig(path)
	must(err)
	return cfg
}

func runRoutes(s routeScenario) routeScenario {
	if s.Env != "" {
		must(os.Setenv("MANAGEMENT_PASSWORD", s.Env))
	} else {
		must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	}
	dir, err := os.MkdirTemp("", "cpa-routes-")
	must(err)
	defer os.RemoveAll(dir)
	path := filepath.Join(dir, "config.yaml")
	cfg := writeConfig(path, s.YAML)
	var opts []api.ServerOption
	if s.LocalPassword != "" {
		opts = append(opts, api.WithLocalManagementPassword(s.LocalPassword))
	}
	server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path, append(opts, api.WithPluginHost(pluginhost.New()))...)
	for i := range s.Steps {
		st := &s.Steps[i]
		if st.Update != "" {
			server.UpdateClients(writeConfig(path, st.Update))
		}
		req := httptest.NewRequest(st.Method, st.Path, strings.NewReader(st.Body))
		req.RemoteAddr = st.Remote
		for k, vs := range st.Headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		rec := httptest.NewRecorder()
		server.Handler().ServeHTTP(rec, req)
		st.Status = rec.Code
		st.RespBody = rec.Body.String()
		st.RespHeader = map[string]string{}
		for _, name := range observedHeaders {
			if v := rec.Header().Get(name); v != "" {
				st.RespHeader[name] = v
			}
		}
		if rec.Header().Get("X-CPA-VERSION") != "" {
			st.RespHeader["X-CPA-VERSION"] = "present"
		}
	}
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	return s
}

// Header values as hex so arbitrary bytes (invalid UTF-8) survive JSON.
type ipBytesCase struct {
	Trusted []string `json:"trusted"`
	Remote  string   `json:"remote"`
	XFF     string   `json:"xff_hex"`
	RealIP  string   `json:"x_real_ip_hex,omitempty"`
	IP      string   `json:"ip"`
}

func runIPBytes(c ipBytesCase) ipBytesCase {
	engine := gin.New()
	must(engine.SetTrustedProxies(c.Trusted))
	engine.GET("/", func(ctx *gin.Context) { ctx.String(200, ctx.ClientIP()) })
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.RemoteAddr = c.Remote
	xff, err := hex.DecodeString(c.XFF)
	must(err)
	req.Header["X-Forwarded-For"] = []string{string(xff)}
	if c.RealIP != "" {
		real, errReal := hex.DecodeString(c.RealIP)
		must(errReal)
		req.Header["X-Real-Ip"] = []string{string(real)}
	}
	rec := httptest.NewRecorder()
	engine.ServeHTTP(rec, req)
	c.IP = rec.Body.String()
	return c
}

// A ConfigV8 call against a real Go server and the persisted result.
type configStep struct {
	Method   string `json:"method"`
	Path     string `json:"path"`
	Body     string `json:"body,omitempty"`
	Status   int    `json:"status"`
	Response any    `json:"response"`
	RawResp  string `json:"raw_response,omitempty"`
	// RawJSON is the exact JSON body gin wrote, for byte comparison.
	RawJSON  string `json:"raw_json,omitempty"`
	File     any    `json:"file"`
	Archived bool   `json:"file_has_archive"`
}

type configScenario struct {
	Name  string       `json:"name"`
	YAML  string       `json:"yaml"`
	Steps []configStep `json:"steps"`
}

func yamlToJSON(text string) any {
	var v any
	if err := yaml.Unmarshal([]byte(text), &v); err != nil {
		return "unparsable: " + err.Error()
	}
	raw, err := json.Marshal(v)
	must(err)
	var out any
	must(json.Unmarshal(raw, &out))
	return out
}

func runConfig(s configScenario) configScenario {
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	dir, err := os.MkdirTemp("", "cpa-config-")
	must(err)
	defer os.RemoveAll(dir)
	path := filepath.Join(dir, "config.yaml")
	cfg := writeConfig(path, s.YAML)
	server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path, api.WithPluginHost(pluginhost.New()))
	for i := range s.Steps {
		st := &s.Steps[i]
		req := httptest.NewRequest(st.Method, "/v8/management"+st.Path, strings.NewReader(st.Body))
		req.RemoteAddr = "127.0.0.1:1"
		req.Header.Set("Authorization", "Bearer fake-secret")
		rec := httptest.NewRecorder()
		server.Handler().ServeHTTP(rec, req)
		st.Status = rec.Code
		if strings.HasSuffix(st.Path, "config.yaml") && st.Method == http.MethodGet && rec.Code == 200 {
			st.Response = yamlToJSON(rec.Body.String())
			st.RawResp = rec.Body.String()
		} else if rec.Body.Len() > 0 {
			var v any
			if json.Unmarshal(rec.Body.Bytes(), &v) == nil {
				st.Response = v
				st.RawJSON = rec.Body.String()
			} else {
				st.RawResp = rec.Body.String()
			}
		}
		file, errRead := os.ReadFile(path)
		must(errRead)
		st.File = yamlToJSON(string(file))
		st.Archived = strings.Contains(string(file), "# mystery-section")
	}
	return s
}

// Credential management calls against a Go server whose auth manager persists
// through Go's file token store, with the resulting auth dir and config.
type credStep struct {
	Method      string            `json:"method"`
	Path        string            `json:"path"`
	Body        string            `json:"body,omitempty"`
	ContentType string            `json:"content_type,omitempty"`
	SleepMs     int               `json:"sleep_ms,omitempty"`
	Status      int               `json:"status"`
	Response    any               `json:"response"`
	Raw         string            `json:"raw_response,omitempty"`
	RawJSON     string            `json:"raw_json,omitempty"`
	Headers     map[string]string `json:"resp_headers,omitempty"`
	Files       map[string]any    `json:"files"`
	RawFiles    map[string]string `json:"raw_files,omitempty"`
	Config      any               `json:"config"`
	// AppendLog extends a log file (and sets its mtime) before the request.
	AppendLog *logFile `json:"append_log,omitempty"`
	// LogDir is the log directory after the step, for scenarios with log files.
	LogDir map[string]string `json:"log_dir,omitempty"`
}

// A file in the scenario's log directory, $WRITABLE_PATH/logs. Mtime is seconds
// after logEpoch, so cursors and listings are deterministic.
type logFile struct {
	Name  string `json:"name"`
	Text  string `json:"text"`
	Mtime int64  `json:"mtime"`
}

const logEpoch = 1700000000

func putLog(dir string, f logFile, appendText bool) {
	flags := os.O_CREATE | os.O_WRONLY | os.O_TRUNC
	if appendText {
		flags = os.O_CREATE | os.O_WRONLY | os.O_APPEND
	}
	file, err := os.OpenFile(filepath.Join(dir, f.Name), flags, 0o644)
	must(err)
	_, err = file.WriteString(f.Text)
	must(err)
	must(file.Close())
	at := time.Unix(logEpoch+f.Mtime, 0)
	must(os.Chtimes(filepath.Join(dir, f.Name), at, at))
}

func snapshotLogs(dir string) map[string]string {
	out := map[string]string{}
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
		data, err := os.ReadFile(filepath.Join(dir, e.Name()))
		must(err)
		out[e.Name()] = string(data)
	}
	return out
}

type credScenario struct {
	Name    string            `json:"name"`
	YAML    string            `json:"yaml"`
	Files   map[string]string `json:"auth_files"`
	Indexes map[string]string `json:"indexes"`
	Steps   []credStep        `json:"steps"`
	// Echo starts a local upstream for api-call; "$ECHO" in steps is its base URL.
	Echo bool `json:"echo,omitempty"`
	// RuntimeAuths registers runtime-only credentials (no file), as Go's AI Studio
	// websocket relay does in wsOnConnected.
	RuntimeAuths []string `json:"runtime_auths,omitempty"`
	// LogFiles, when set, become $WRITABLE_PATH/logs with WRITABLE_PATH at the root.
	LogFiles []logFile `json:"log_files,omitempty"`
}

// echoHandler reports what an api-call upstream received. The Rust replay runs an
// equivalent server, so both sides compare the requests their clients sent.
func echoHandler(listener *string) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/redirect":
			w.Header().Set("Location", "/echo?from=redirect")
			w.WriteHeader(http.StatusFound)
			return
		case "/redirect307":
			w.Header().Set("Location", "/echo?from=307")
			w.WriteHeader(http.StatusTemporaryRedirect)
			return
		case "/chunked":
			w.Header().Set("Trailer", "X-Foo")
			w.Header().Set("Content-Type", "text/plain")
			w.WriteHeader(http.StatusOK)
			_, _ = w.Write([]byte("part"))
			w.(http.Flusher).Flush()
			_, _ = w.Write([]byte("two"))
			w.Header().Set("X-Foo", "bar")
			return
		case "/status":
			w.Header().Set("X-Multi", "a")
			w.Header().Add("X-Multi", "b")
			w.Header().Set("Content-Type", "text/plain")
			w.WriteHeader(http.StatusTeapot)
			_, _ = w.Write([]byte("teapot\xff"))
			return
		}
		body, _ := io.ReadAll(r.Body)
		headers := map[string][]string{}
		for _, k := range []string{"Authorization", "X-Custom", "Content-Type", "User-Agent", "Content-Length", "Accept-Encoding", "Referer"} {
			if v, ok := r.Header[k]; ok {
				headers[k] = v
			}
		}
		host := r.Host
		if host == *listener {
			host = "<listener>"
		}
		out := map[string]any{"method": r.Method, "path": r.URL.Path, "query": r.URL.RawQuery, "host": host, "headers": headers, "body": string(body), "content_length": r.ContentLength}
		w.Header().Set("Content-Type", "application/json")
		enc := json.NewEncoder(w)
		enc.SetEscapeHTML(false)
		_ = enc.Encode(out)
	})
}

func snapshotDir(dir string) map[string]any {
	out := map[string]any{}
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
		// Callback hand-off files are transient (cliproxy-rs keeps callbacks in memory).
		if strings.HasPrefix(e.Name(), ".oauth-") {
			continue
		}
		data, err := os.ReadFile(filepath.Join(dir, e.Name()))
		must(err)
		var v any
		if json.Unmarshal(data, &v) == nil {
			out[e.Name()] = v
		} else {
			out[e.Name()] = "raw:" + string(data)
		}
	}
	return out
}

func runCreds(s credScenario) credScenario {
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	dir, err := os.MkdirTemp("", "cpa-creds-")
	must(err)
	defer os.RemoveAll(dir)
	root := filepath.Join(dir, "fixture-root")
	authDir := filepath.Join(root, "auth")
	must(os.MkdirAll(authDir, 0o700))
	for name, body := range s.Files {
		must(os.WriteFile(filepath.Join(authDir, name), []byte(body), 0o600))
	}
	path := filepath.Join(root, "config.yaml")
	cfg := writeConfig(path, strings.ReplaceAll(s.YAML, "$AUTH", authDir))
	store := sdkAuth.GetTokenStore()
	if setter, ok := store.(interface{ SetBaseDir(string) }); ok {
		setter.SetBaseDir(authDir)
	}
	manager := coreauth.NewManager(store, nil, nil)
	ctx := &synthesizer.SynthesisContext{Config: cfg, AuthDir: authDir, Now: time.Now(), IDGenerator: synthesizer.NewStableIDGenerator()}
	files, err := synthesizer.NewFileSynthesizer().Synthesize(ctx)
	must(err)
	configAuths, err := synthesizer.NewConfigSynthesizer().Synthesize(ctx)
	must(err)
	cfgID := ""
	s.Indexes = map[string]string{}
	for _, a := range append(files, configAuths...) {
		_, errRegister := manager.Register(coreauth.WithSkipPersist(context.Background()), a)
		must(errRegister)
		key := a.FileName
		if key == "" {
			key = a.ID
			cfgID = a.ID
		}
		s.Indexes[key] = a.EnsureIndex()
	}
	logDir := filepath.Join(root, "logs")
	if s.LogFiles != nil {
		must(os.MkdirAll(logDir, 0o755))
		for _, f := range s.LogFiles {
			putLog(logDir, f, false)
		}
		must(os.Setenv("WRITABLE_PATH", root))
		defer os.Unsetenv("WRITABLE_PATH")
	}
	for _, id := range s.RuntimeAuths {
		now := time.Now().UTC()
		runtimeAuth := &coreauth.Auth{
			ID:         id,
			Provider:   "aistudio",
			Label:      id,
			Status:     coreauth.StatusActive,
			CreatedAt:  now,
			UpdatedAt:  now,
			Attributes: map[string]string{"runtime_only": "true"},
			Metadata:   map[string]any{"email": id},
		}
		_, errRegister := manager.Register(coreauth.WithSkipPersist(context.Background()), runtimeAuth)
		must(errRegister)
		s.Indexes[id] = runtimeAuth.EnsureIndex()
		// The service registers AI Studio's models for the auth (registerModelsForAuth).
		registry.GetGlobalRegistry().RegisterClient(id, "aistudio", registry.GetAIStudioModels())
		defer registry.GetGlobalRegistry().UnregisterClient(id)
	}
	server := api.NewServer(cfg, manager, sdkaccess.NewManager(), path, api.WithPluginHost(pluginhost.New()))
	echoURL := "http://echo.invalid"
	if s.Echo {
		listener := ""
		echo := httptest.NewServer(echoHandler(&listener))
		defer echo.Close()
		echoURL = echo.URL
		listener = strings.TrimPrefix(echo.URL, "http://")
	}
	lastState := "no-state"
	lastCursor := "no-cursor"
	resolve := func(text string) string {
		text = strings.ReplaceAll(text, "$STATE", lastState)
		text = strings.ReplaceAll(text, "$CURSOR", lastCursor)
		text = strings.ReplaceAll(text, "$ECHO", echoURL)
		text = strings.ReplaceAll(text, "$CFGID", cfgID)
		for name, index := range s.Indexes {
			text = strings.ReplaceAll(text, "$INDEX("+name+")", index)
		}
		return text
	}
	for i := range s.Steps {
		st := &s.Steps[i]
		if st.SleepMs > 0 {
			time.Sleep(time.Duration(st.SleepMs) * time.Millisecond)
		}
		if st.AppendLog != nil {
			putLog(logDir, *st.AppendLog, true)
		}
		req := httptest.NewRequest(st.Method, "/v8/management"+resolve(st.Path), strings.NewReader(resolve(st.Body)))
		req.RemoteAddr = "127.0.0.1:1"
		req.Header.Set("Authorization", "Bearer fake-secret")
		if st.ContentType != "" {
			req.Header.Set("Content-Type", st.ContentType)
		}
		rec := httptest.NewRecorder()
		server.Handler().ServeHTTP(rec, req)
		st.Status = rec.Code
		var v any
		if json.Unmarshal(rec.Body.Bytes(), &v) == nil && !strings.Contains(st.Path, "/download") {
			st.Response = v
			st.RawJSON = rec.Body.String()
			if m, ok := v.(map[string]any); ok && strings.HasPrefix(st.Path, "/oauth/auth-url") {
				if state, ok := m["state"].(string); ok {
					lastState = state
				}
			}
			if m, ok := v.(map[string]any); ok {
				if cursor, ok := m["next-cursor"].(string); ok && cursor != "" {
					lastCursor = cursor
				}
			}
		} else {
			st.Raw = rec.Body.String()
		}
		st.Headers = map[string]string{}
		for _, h := range []string{"Content-Type", "Content-Disposition"} {
			if value := rec.Header().Get(h); value != "" {
				st.Headers[h] = value
			}
		}
		st.Files = snapshotDir(authDir)
		// Exact bytes for raw-*.json uploads: Go keeps them when already canonical.
		st.RawFiles = map[string]string{}
		if matches, _ := filepath.Glob(filepath.Join(authDir, "raw-*.json")); len(matches) > 0 {
			for _, m := range matches {
				data, errRead := os.ReadFile(m)
				must(errRead)
				st.RawFiles[filepath.Base(m)] = string(data)
			}
		}
		// Credentials registered by this step (uploads) get indexes too.
		for _, a := range manager.List() {
			key := filepath.Base(a.FileName)
			if a.FileName == "" {
				key = a.ID
			}
			if _, seen := s.Indexes[key]; !seen {
				s.Indexes[key] = a.EnsureIndex()
			}
		}
		data, errRead := os.ReadFile(path)
		must(errRead)
		st.Config = yamlToJSON(string(data))
		if s.LogFiles != nil {
			st.LogDir = snapshotLogs(logDir)
		}
	}
	// Report paths relative to the placeholder root, as the Rust replay does.
	raw, err := json.Marshal(s.Steps)
	must(err)
	raw = []byte(strings.ReplaceAll(string(raw), root, fixtureRoot))
	must(json.Unmarshal([]byte(strings.ReplaceAll(string(raw), echoURL, "$ECHO")), &s.Steps))
	return s
}

// Go Config.OAuthOnlyFields (legacy names) for a config text.
type oauthOnlyCase struct {
	YAML   string   `json:"yaml"`
	Fields []string `json:"fields"`
}

// yaml.v3's YAML 1.1 bool compatibility: a spelling written raw into a typed bool,
// an optional (*bool) field and a string field, loaded with config.LoadConfig.
type yamlBoolCase struct {
	Spelling string `json:"spelling"`
	Field    string `json:"field"`
	Error    bool   `json:"error"`
	Value    any    `json:"value"`
}

// A config using anchors and merge keys, and the values Go loads from it.
type yamlMergeCase struct {
	Name   string         `json:"name"`
	YAML   string         `json:"yaml"`
	Error  bool           `json:"error"`
	Values map[string]any `json:"values,omitempty"`
}

func loadText(dir, text string) (*config.Config, error) {
	path := filepath.Join(dir, "config.yaml")
	must(os.WriteFile(path, []byte(text), 0o600))
	return config.LoadConfig(path)
}

func yamlBoolCases() []yamlBoolCase {
	dir, err := os.MkdirTemp("", "cpa-bools-")
	must(err)
	defer os.RemoveAll(dir)
	spellings := []string{"y", "Y", "yes", "Yes", "YES", "on", "On", "ON", "n", "N", "no", "No", "NO", "off", "Off", "OFF",
		"yEs", "oN", "nO", "true", "True", "TRUE", "tRue", "false", "False", "FALSE", `"yes"`, `'off'`, `"true"`, `'False'`,
		"!!str yes", "!!bool true", "1", "0", "t", "f", "~", "null", `""`}
	templates := map[string]string{
		"observability.logs.debug":                      "config-version: 8\nobservability:\n  logs:\n    debug: %s\n",
		"plugins.configs.x.enabled":                     "config-version: 8\nplugins:\n  configs:\n    x:\n      enabled: %s\n",
		"server.host":                                   "config-version: 8\nserver:\n  host: %s\n",
		"credentials.concurrency.cpa-heartbeat-timeout": "config-version: 8\ncredentials:\n  concurrency:\n    cpa-heartbeat-timeout: %s\n",
	}
	durations := []string{"3s", `"250ms"`, "1h2m3.5s", "0", "0s", "-1s", "5", "banana", "1d", `""`, "~", "yes"}
	var out []yamlBoolCase
	for _, field := range []string{"observability.logs.debug", "plugins.configs.x.enabled", "server.host", "credentials.concurrency.cpa-heartbeat-timeout"} {
		list := spellings
		if strings.HasPrefix(field, "credentials.") {
			list = durations
		}
		for _, spelling := range list {
			cfg, errLoad := loadText(dir, fmt.Sprintf(templates[field], spelling))
			c := yamlBoolCase{Spelling: spelling, Field: field, Error: errLoad != nil}
			if errLoad == nil {
				switch field {
				case "observability.logs.debug":
					c.Value = cfg.Debug
				case "plugins.configs.x.enabled":
					if item, ok := cfg.Plugins.Configs["x"]; ok && item.Enabled != nil {
						c.Value = *item.Enabled
					}
				case "server.host":
					c.Value = cfg.Host
				default:
					c.Value = cfg.CredentialConcurrency.CPAHeartbeatTimeout.String()
				}
			}
			out = append(out, c)
		}
	}
	return out
}

func yamlMergeCases() []yamlMergeCase {
	dir, err := os.MkdirTemp("", "cpa-merges-")
	must(err)
	defer os.RemoveAll(dir)
	cases := []yamlMergeCase{
		{Name: "anchors_and_merges", YAML: "config-version: 8\n" +
			"routing:\n  retry: &r\n    request-retry: 4\n    max-retry-interval: 9\n" +
			"api-keys:\n  claude:\n    - base-url: https://a.example.invalid\n      keys:\n" +
			"        - &k\n          api-key: fake-1\n          priority: 3\n" +
			"        - <<: *k\n          api-key: fake-2\n" +
			"        - <<: [{api-key: fake-3, priority: 7}, *k]\n" +
			"observability:\n  logs:\n    <<: [{debug: true, request-log: false}, {debug: false, request-log: true}]\n    logging-to-file: false\n"},
		{Name: "nested_merge_in_merged_mapping", YAML: "config-version: 8\n" +
			"routing:\n  retry:\n    <<: {<<: {request-retry: 6}, max-retry-interval: 2}\n"},
		{Name: "legacy_layout_merge", YAML: "request-retry: &n 5\nmax-retry-interval: *n\n" +
			"claude-api-key:\n  - &c {api-key: fake-l1, priority: 2}\n  - <<: *c\n    api-key: fake-l2\n"},
		{Name: "scalar_merge", YAML: "config-version: 8\nrouting:\n  retry:\n    <<: 5\n    request-retry: 1\n"},
		{Name: "scalar_in_merge_list", YAML: "config-version: 8\nrouting:\n  retry:\n    <<: [5]\n    request-retry: 1\n"},
	}
	for i := range cases {
		cfg, errLoad := loadText(dir, cases[i].YAML)
		cases[i].Error = errLoad != nil
		if errLoad != nil {
			continue
		}
		keys := []any{}
		for _, k := range cfg.ClaudeKey {
			keys = append(keys, map[string]any{"api-key": k.APIKey, "priority": k.Priority})
		}
		cases[i].Values = map[string]any{
			"request-retry":      cfg.RequestRetry,
			"max-retry-interval": cfg.MaxRetryInterval,
			"debug":              cfg.Debug,
			"request-log":        cfg.RequestLog,
			"claude":             keys,
		}
	}
	return cases
}

// A plugins.configs entry or int field as Go loads it: the typed values Go's config
// holds and the raw view GET /v0/management/plugins/x/config answers.
type typedScalarCase struct {
	Name     string `json:"name"`
	YAML     string `json:"yaml"`
	Error    bool   `json:"error"`
	Enabled  *bool  `json:"enabled,omitempty"`
	Priority *int   `json:"priority,omitempty"`
	Retry    *int   `json:"request_retry,omitempty"`
	RawView  any    `json:"raw_view,omitempty"`
}

func typedScalarCases() []typedScalarCase {
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	dir, err := os.MkdirTemp("", "cpa-typed-")
	must(err)
	defer os.RemoveAll(dir)
	head := "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n"
	plugin := func(entry string) string { return head + "plugins:\n  configs:\n    x:" + entry + "\n" }
	retry := func(v string) string { return head + "routing:\n  retry:\n    request-retry: " + v + "\n" }
	cases := []typedScalarCase{
		{Name: "null_entry", YAML: plugin("")},
		{Name: "float_priority", YAML: plugin(" {enabled: true, priority: 5.7}")},
		{Name: "negative_float_priority", YAML: plugin(" {priority: -1.5}")},
		{Name: "quoted_yes", YAML: plugin(" {enabled: \"yes\"}")},
		{Name: "plain_yes", YAML: plugin(" {enabled: yes, extra: on}")},
		{Name: "invalid_enabled", YAML: plugin(" {enabled: maybe}")},
		{Name: "string_priority", YAML: plugin(" {priority: \"5\"}")},
		{Name: "retry_float", YAML: retry("2.9")},
		{Name: "retry_negative_float", YAML: retry("-1.5")},
		{Name: "retry_exponent", YAML: retry("1e3")},
		{Name: "retry_hex", YAML: retry("0x10")},
		{Name: "retry_quoted", YAML: retry("\"5\"")},
	}
	for i := range cases {
		c := &cases[i]
		path := filepath.Join(dir, "config.yaml")
		must(os.WriteFile(path, []byte(strings.ReplaceAll(c.YAML, "$HASH", secretHash)), 0o600))
		cfg, errLoad := config.LoadConfig(path)
		c.Error = errLoad != nil
		if errLoad != nil {
			continue
		}
		retryValue := cfg.RequestRetry
		c.Retry = &retryValue
		item, ok := cfg.Plugins.Configs["x"]
		if !ok {
			continue
		}
		enabled := item.Enabled != nil && *item.Enabled
		priority := item.Priority
		c.Enabled, c.Priority = &enabled, &priority
		server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path)
		req := httptest.NewRequest(http.MethodGet, "/v0/management/plugins/x/config", nil)
		req.RemoteAddr = "127.0.0.1:1"
		req.Header.Set("Authorization", "Bearer fake-secret")
		rec := httptest.NewRecorder()
		server.Handler().ServeHTTP(rec, req)
		var v any
		if json.Unmarshal(rec.Body.Bytes(), &v) == nil {
			c.RawView = map[string]any{"status": rec.Code, "body": v}
		}
	}
	return cases
}

type output struct {
	TypedScalars []typedScalarCase `json:"typed_scalars"`
	YAMLBools    []yamlBoolCase    `json:"yaml_bools"`
	YAMLMerges   []yamlMergeCase   `json:"yaml_merges"`
	OAuthOnly    []oauthOnlyCase   `json:"oauth_only"`
	Credentials  []credScenario    `json:"credentials"`
	Materialized any               `json:"materialized_defaults"`
	Config       []configScenario  `json:"config_writes"`
	IPBytes      []ipBytesCase     `json:"client_ip_bytes"`
	Routes       []routeScenario   `json:"routes"`
	Access       []accessScenario  `json:"access"`
	IPs          []ipCase          `json:"client_ip"`
	Synth        []synthCase       `json:"synth"`
	Loads        []loadCase        `json:"load_errors"`
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}

func runAccess(s accessScenario) accessScenario {
	if s.Env != "" {
		must(os.Setenv("MANAGEMENT_PASSWORD", s.Env))
	} else {
		must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	}
	cfg := &config.Config{}
	cfg.RemoteManagement.AllowRemote = s.AllowRemote
	if s.Secret != "" {
		hash, err := bcrypt.GenerateFromPassword([]byte(s.Secret), 4)
		must(err)
		cfg.RemoteManagement.SecretKey = string(hash)
	}
	h := management.NewHandler(cfg, "", nil)
	if s.LocalPassword != "" {
		h.SetLocalPassword(s.LocalPassword)
	}
	engine := gin.New()
	must(engine.SetTrustedProxies(s.Trusted))
	engine.GET("/v8/management/probe", h.Middleware(), func(c *gin.Context) {
		c.JSON(http.StatusOK, gin.H{"ok": true})
	})
	for i := range s.Steps {
		req := httptest.NewRequest(http.MethodGet, "/v8/management/probe", nil)
		req.RemoteAddr = s.Steps[i].Remote
		for k, vs := range s.Steps[i].Headers {
			for _, v := range vs {
				req.Header.Add(k, v)
			}
		}
		rec := httptest.NewRecorder()
		engine.ServeHTTP(rec, req)
		s.Steps[i].Status = rec.Code
		s.Steps[i].Body = rec.Body.String()
		s.Steps[i].Version = rec.Header().Get("X-CPA-VERSION")
	}
	must(os.Unsetenv("MANAGEMENT_PASSWORD"))
	return s
}

func runIP(c ipCase) ipCase {
	engine := gin.New()
	must(engine.SetTrustedProxies(c.Trusted))
	engine.GET("/", func(ctx *gin.Context) { ctx.String(200, ctx.ClientIP()) })
	req := httptest.NewRequest(http.MethodGet, "/", nil)
	req.RemoteAddr = c.Remote
	for k, vs := range c.Headers {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	rec := httptest.NewRecorder()
	engine.ServeHTTP(rec, req)
	c.IP = rec.Body.String()
	return c
}

func summarize(auths []*coreauth.Auth) []authSummary {
	out := make([]authSummary, 0, len(auths))
	for _, a := range auths {
		meta := map[string]any{}
		if a.Metadata != nil {
			raw, err := json.Marshal(a.Metadata)
			must(err)
			must(json.Unmarshal(raw, &meta))
		}
		attrs := map[string]string{}
		for k, v := range a.Attributes {
			attrs[k] = v
		}
		out = append(out, authSummary{
			ID: a.ID, Provider: a.Provider, Label: a.Label, Prefix: a.Prefix, ProxyURL: a.ProxyURL,
			Disabled: a.Disabled, Status: string(a.Status), Index: a.EnsureIndex(), FileName: a.FileName,
			Attributes: attrs, Metadata: meta,
		})
	}
	return out
}

// Auth paths are reported relative to this placeholder so fixtures are portable.
const fixtureRoot = "/fixture-root"

func runSynth(c synthCase) synthCase {
	dir, err := os.MkdirTemp("", "cpa-synth-")
	must(err)
	defer os.RemoveAll(dir)
	root := filepath.Join(dir, "fixture-root")
	must(os.MkdirAll(filepath.Join(root, "auth"), 0o700))
	path := filepath.Join(root, "config.yaml")
	must(os.WriteFile(path, []byte(c.YAML), 0o600))
	for name, body := range c.Files {
		must(os.WriteFile(filepath.Join(root, "auth", name), []byte(body), 0o600))
	}
	cfg, err := config.LoadConfig(path)
	if err != nil {
		c.Error = err.Error()
		return c
	}
	ctx := &synthesizer.SynthesisContext{Config: cfg, AuthDir: filepath.Join(root, "auth"), Now: time.Unix(0, 0).UTC(), IDGenerator: synthesizer.NewStableIDGenerator()}
	auths, err := synthesizer.NewConfigSynthesizer().Synthesize(ctx)
	if err != nil {
		c.Error = err.Error()
		return c
	}
	c.Config = summarize(auths)
	files, err := synthesizer.NewFileSynthesizer().Synthesize(ctx)
	must(err)
	// Absolute paths depend on the temp dir; rewrite them to the placeholder and
	// recompute the index the way Go would for that placeholder path.
	for _, a := range files {
		for k, v := range a.Attributes {
			if rel, errRel := filepath.Rel(root, v); errRel == nil && filepath.IsAbs(v) && rel != "" && rel[0] != '.' {
				a.Attributes[k] = filepath.Join(fixtureRoot, rel)
			}
		}
		a.Index = ""
	}
	sort.Slice(files, func(i, j int) bool { return files[i].ID < files[j].ID })
	c.File = summarize(files)
	return c
}

func h(pairs ...string) map[string][]string {
	out := map[string][]string{}
	for i := 0; i+1 < len(pairs); i += 2 {
		out[pairs[i]] = append(out[pairs[i]], pairs[i+1])
	}
	return out
}

func main() {
	gin.SetMode(gin.ReleaseMode)
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: manage <fixture.json> <model_definitions.json>")
		os.Exit(2)
	}
	// Go's static model definitions per management channel, embedded as data.
	definitions := map[string]any{}
	for _, channel := range []string{"claude", "gemini", "gemini-interactions", "vertex", "aistudio", "codex", "kimi", "antigravity", "xai", "devin", "meta"} {
		definitions[channel] = registry.GetStaticModelDefinitionsByChannel(channel)
	}
	defs, errDefs := json.Marshal(definitions)
	must(errDefs)
	must(os.WriteFile(os.Args[2], append(defs, '\n'), 0o644))
	good := h("Authorization", "Bearer fake-secret")
	wrong := h("Authorization", "Bearer fake-wrong")
	local := "127.0.0.1:40000"
	var out output
	hash, errHash := bcrypt.GenerateFromPassword([]byte("fake-secret"), 4)
	must(errHash)
	secretHash = string(hash)
	hx := func(s string) string { return hex.EncodeToString([]byte(s)) }
	for _, c := range []ipBytesCase{
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("\u00e9t\u00e9, 198.51.100.9")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("\xff\xfe, 198.51.100.9")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("\u00a0198.51.100.9\u2003")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("198.51.100.9 \u0085")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("198.51.100.9, \xff"), RealIP: hx("203.0.113.5")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("198.51.100.9\xff")},
		{Trusted: []string{"127.0.0.1"}, Remote: local, XFF: hx("\xff198.51.100.9")},
		{Trusted: []string{"::1"}, Remote: "[::1]:1", XFF: hx("x\x80y, ::1, 2001:db8::7")},
		{Trusted: []string{"fe80::/10"}, Remote: "[fe80::1%eth0]:1", XFF: hx("198.51.100.9")},
		{Trusted: nil, Remote: "[fe80::1%eth0]:1", XFF: hx("198.51.100.9")},
	} {
		out.IPBytes = append(out.IPBytes, runIPBytes(c))
	}
	// Keys Go's saver adds to an otherwise untouched v8 document on any PUT/PATCH.
	base := runConfig(configScenario{Name: "materialized", YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n",
		Steps: []configStep{{Method: http.MethodPatch, Path: "/config", Body: "{}"}}})
	out.Materialized = base.Steps[0].File
	out.YAMLBools = yamlBoolCases()
	out.YAMLMerges = yamlMergeCases()
	out.TypedScalars = typedScalarCases()
	for _, s := range credScenarios() {
		out.Credentials = append(out.Credentials, runCreds(s))
	}
	for _, s := range configScenarios() {
		out.Config = append(out.Config, runConfig(s))
	}
	for _, s := range routeScenarios() {
		out.Routes = append(out.Routes, runRoutes(s))
	}
	scenarios := []accessScenario{
		{Name: "local_header_forms", Secret: "fake-secret", Steps: []step{
			{Remote: local, Headers: good},
			{Remote: local, Headers: h("X-Management-Key", "fake-secret")},
			{Remote: local, Headers: h("Authorization", "fake-secret")},
			{Remote: local, Headers: h("Authorization", "bEaReR fake-secret")},
			{Remote: local, Headers: h("Authorization", "Bearer  fake-secret")},
			{Remote: local, Headers: h("Authorization", "Basic abc", "X-Management-Key", "fake-secret")},
			{Remote: local, Headers: h("Authorization", "Bearer ", "X-Management-Key", "fake-secret")},
			{Remote: local, Headers: h("Authorization", "Bearer fake-secret", "Authorization", "Bearer other")},
			{Remote: local},
		}},
		{Name: "remote_policy_is_exact_loopback_strings", Secret: "fake-secret", Steps: []step{
			{Remote: "203.0.113.5:1", Headers: good},
			{Remote: "127.0.0.2:1", Headers: good},
			{Remote: "[::1]:1", Headers: good},
			{Remote: "[::ffff:127.0.0.1]:1", Headers: good},
			{Remote: "[0:0:0:0:0:0:0:1]:1", Headers: good},
			{Remote: "203.0.113.5:1"},
		}},
		{Name: "ban_after_five_failures_including_missing", Secret: "fake-secret", Steps: []step{
			{Remote: local, Headers: wrong},
			{Remote: local, Headers: wrong},
			{Remote: local},
			{Remote: local, Headers: wrong},
			{Remote: local, Headers: wrong},
			{Remote: local, Headers: good},
			{Remote: "[::1]:2", Headers: good},
		}},
		{Name: "success_resets_count", Secret: "fake-secret", Steps: []step{
			{Remote: local, Headers: wrong}, {Remote: local, Headers: wrong}, {Remote: local, Headers: wrong}, {Remote: local, Headers: wrong},
			{Remote: local, Headers: good},
			{Remote: local, Headers: wrong}, {Remote: local, Headers: wrong}, {Remote: local, Headers: wrong}, {Remote: local, Headers: wrong},
			{Remote: local, Headers: good},
		}},
		{Name: "remote_failures_ban_before_policy", Secret: "fake-secret", Steps: []step{
			{Remote: "203.0.113.7:1", Headers: wrong},
			{Remote: "203.0.113.7:1", Headers: wrong},
		}},
		{Name: "allow_remote_bans_remote_ip", Secret: "fake-secret", AllowRemote: true, Steps: []step{
			{Remote: "203.0.113.7:1", Headers: good},
			{Remote: "203.0.113.7:1", Headers: wrong}, {Remote: "203.0.113.7:1", Headers: wrong}, {Remote: "203.0.113.7:1", Headers: wrong},
			{Remote: "203.0.113.7:1", Headers: wrong}, {Remote: "203.0.113.7:1", Headers: wrong},
			{Remote: "203.0.113.7:1", Headers: good},
			{Remote: local, Headers: good},
		}},
		{Name: "env_secret_allows_remote", Env: "fake-env", Steps: []step{
			{Remote: "203.0.113.8:1", Headers: h("Authorization", "Bearer fake-env")},
			{Remote: "203.0.113.8:1", Headers: wrong},
			{Remote: local, Headers: h("X-Management-Key", "fake-env")},
		}},
		{Name: "env_and_config_secret", Env: "fake-env", Secret: "fake-secret", Steps: []step{
			{Remote: "203.0.113.8:1", Headers: good},
			{Remote: "203.0.113.8:1", Headers: h("Authorization", "Bearer fake-env")},
		}},
		{Name: "local_password_is_loopback_only", Secret: "fake-secret", AllowRemote: true, LocalPassword: "fake-local", Steps: []step{
			{Remote: local, Headers: h("Authorization", "Bearer fake-local")},
			{Remote: "203.0.113.9:1", Headers: h("Authorization", "Bearer fake-local")},
			{Remote: "[::1]:3", Headers: h("X-Management-Key", "fake-local")},
		}},
		{Name: "local_password_without_secret", LocalPassword: "fake-local", Steps: []step{
			{Remote: local, Headers: h("Authorization", "Bearer fake-local")},
			{Remote: local},
		}},
		{Name: "trusted_loopback_proxy_uses_forwarded_ip", Secret: "fake-secret", Trusted: []string{"127.0.0.1", "::1"}, Steps: []step{
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.9", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "127.0.0.1", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.9, 127.0.0.1", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Real-IP", "::1", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "garbage", "X-Real-IP", "203.0.113.4", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "::ffff:127.0.0.1", "Authorization", "Bearer fake-secret")},
			{Remote: "203.0.113.5:1", Headers: h("X-Forwarded-For", "127.0.0.1", "Authorization", "Bearer fake-secret")},
		}},
		{Name: "untrusted_forwarded_headers_are_ignored", Secret: "fake-secret", Steps: []step{
			{Remote: "203.0.113.5:1", Headers: h("X-Forwarded-For", "127.0.0.1", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.9", "Authorization", "Bearer fake-secret")},
		}},
		{Name: "forwarded_junk_cannot_make_remote_local", Secret: "fake-secret", Trusted: []string{"127.0.0.1"}, Steps: []step{
			{Remote: local, Headers: h("X-Forwarded-For", "\u00e9t\u00e9, 198.51.100.9", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "\u00e9t\u00e9, 127.0.0.1", "Authorization", "Bearer fake-secret")},
		}},
		{Name: "forwarded_ip_is_the_ban_key", Secret: "fake-secret", Trusted: []string{"127.0.0.0/8"}, AllowRemote: true, Steps: []step{
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer x")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer x")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer x")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer x")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer x")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.20", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: h("X-Forwarded-For", "203.0.113.21", "Authorization", "Bearer fake-secret")},
			{Remote: local, Headers: good},
		}},
	}
	for _, s := range scenarios {
		out.Access = append(out.Access, runAccess(s))
	}

	trusted := [][]string{nil, {}, {"127.0.0.1"}, {"10.0.0.0/8", "::1"}, {"::ffff:127.0.0.1"}, {"0.0.0.0/0"}}
	remotes := []string{"127.0.0.1:1", "[::1]:1", "10.1.2.3:1", "[::ffff:10.1.2.3]:1", "198.51.100.1:1"}
	headerSets := []map[string][]string{
		nil,
		h("X-Forwarded-For", "203.0.113.1"),
		h("X-Forwarded-For", " 203.0.113.1 , 10.9.9.9 "),
		h("X-Forwarded-For", "10.9.9.9, 127.0.0.1"),
		h("X-Forwarded-For", "bogus, 203.0.113.1"),
		h("X-Forwarded-For", "203.0.113.1, bogus"),
		h("X-Forwarded-For", "", "X-Real-IP", "2001:db8::1"),
		h("X-Forwarded-For", "203.0.113.1", "X-Forwarded-For", "203.0.113.2"),
		h("X-Real-IP", "0:0:0:0:0:0:0:1"),
		h("X-Forwarded-For", "::FFFF:127.0.0.1"),
		h("X-Forwarded-For", "fe80::1%eth0", "X-Real-IP", "203.0.113.3"),
		h("X-Forwarded-For", "01.2.3.4"),
	}
	for _, t := range trusted {
		for _, r := range remotes {
			for _, hs := range headerSets {
				out.IPs = append(out.IPs, runIP(ipCase{Trusted: t, Remote: r, Headers: hs}))
			}
		}
	}

	for _, entries := range []string{
		"trusted-proxies: ['']", "trusted-proxies: [' 127.0.0.1']", "trusted-proxies: [nonsense]",
		"trusted-proxies: [10.0.0.0/33]", "trusted-proxies: ['::1/129']", "trusted-proxies: [fe80::1%eth0]",
		"server: {trusted-proxies: [127.0.0.1, '::1', 10.0.0.0/8]}", "trusted-proxies: [01.2.3.4]",
		"credentials: {in-flight: {snapshot-interval: 0s}}", "credentials: {in-flight: {stale-after: 5s}}",
		"credentials: {in-flight: {staging-retention: x}}", "credentials: {in-flight: {max-part-bytes: 1023}}",
		"credentials: {in-flight: {max-revision-bytes: 100}}", "credentials: {in-flight: {max-part-count: 63}}",
		"credentials: {in-flight: {max-aggregate-groups: 0}}", "credentials: {in-flight: {max-details: -1}}",
		"credentials: {in-flight: {max-string-bytes: 257}}", "credentials: {in-flight: {snapshot-interval: 1.5s, stale-after: 4.5s}}",
		"credentials: {in-flight: {snapshot-interval: 2, stale-after: 10s}}", "credential-in-flight: {max-details: -1}",
		"credentials: {in-flight: {snapshot-interval: null, stale-after: 1h1m0.5s}}", "credentials: {in-flight: {snapshot-interval: .5us}}",
		"credentials: {in-flight: {snapshot-interval: '-1s'}}", "credentials: {in-flight: {snapshot-interval: 1d}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, max-sessions: -1}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, public-ip: nonsense}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, public-ip: ' 2001:db8::1 '}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, udp-port-min: 10000}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, udp-port-min: 10001, udp-port-max: 10000}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, udp-port-min: 10000, udp-port-max: 10010}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, max-sessions: 2, udp-port-min: 10000, udp-port-max: 10003}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, ice-servers: [{urls: []}]}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, ice-servers: [{urls: ['turn:%zz', 'STUN:x']}, {urls: ['http://x']}]}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, ice-servers: [{urls: ['//x']}]}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: true, ice-servers: [{urls: ['turn://a b']}]}}}}",
		"oauth: {providers: {codex: {live-media-relay: {enabled: false, max-sessions: -1, ice-servers: [{urls: []}]}}}}",
		"oauth: {providers: {codex: {live-media-relay: {allow-private-remote-ips: true, disable-private-remote-ips: true}}}}",
		"codex: {live-media-relay: {enabled: true, max-sessions: -1}}",
	} {
		dir, err := os.MkdirTemp("", "cpa-load-")
		must(err)
		path := filepath.Join(dir, "config.yaml")
		must(os.WriteFile(path, []byte(entries+"\n"), 0o600))
		_, errLoad := config.LoadConfig(path)
		msg := ""
		if errLoad != nil {
			msg = errLoad.Error()
		}
		out.Loads = append(out.Loads, loadCase{YAML: entries, Error: msg})
		_ = os.RemoveAll(dir)
	}

	for _, c := range synthCases() {
		out.Synth = append(out.Synth, runSynth(c))
	}
	for _, text := range []string{
		"codex: {disable-codex-cloaking: true}\nws-auth: true\n",
		"config-version: 8\noauth: {providers: {codex: {disable-codex-cloaking: true, header-defaults: {user-agent: x}, live-media-relay: {enabled: false, ice-servers: []}}, aistudio: {ws-auth: null}, claude: {claude-code: {}}}}\n",
		"codex: {model-level-cooling: true}\noauth: {providers: {codex: {stream-bootstrap-buffering: false}}}\n",
		"flag: &f true\noauth: {providers: {codex: {response-steering: *f, orphan-delegation-compatibility: false}}}\n",
		"oauth: {providers: {antigravity: {antigravity-credits: true, signature-cache-enabled: false, signature-bypass-strict: true}, xai: {}, devin: {}}}\n",
		"oauth: {providers: {codex: {}, claude: {disable-claude-cloak-mode: true, header-defaults: {user-agent: y}}}}\n",
	} {
		cfg, err := config.ParseConfigBytes([]byte(text))
		must(err)
		fields := []string{}
		for k, v := range cfg.OAuthOnlyFields {
			if v {
				fields = append(fields, k)
			}
		}
		sort.Strings(fields)
		out.OAuthOnly = append(out.OAuthOnly, oauthOnlyCase{YAML: text, Fields: fields})
	}

	data, err := json.MarshalIndent(out, "", " ")
	must(err)
	must(os.WriteFile(os.Args[1], append(data, '\n'), 0o644))
}

func synthCases() []synthCase {
	return []synthCase{
		{Name: "legacy_claude_and_overrides", YAML: `
claude-api-key:
  - api-key: " fake-claude-1 "
    base-url: "https://claude.example.invalid/ "
    priority: 3
    weight: 0
    prefix: "/team/"
    proxy-url: " socks5://proxy.example.invalid:1080 "
    headers: {X-Extra: " v ", X-Blank: " ", " ": x}
    excluded-models: [" Claude-Opus-*", claude-opus-*, "", Haiku]
    disable-cooling: false
    request-retry: -1
    request-scoped-errors:
      - {status: 400, match: ["overloaded"], action: stop}
    models:
      - {name: claude-sonnet-4-6, alias: sonnet}
    rebuild-mid-system-message: true
    fingerprint-profile: " CLI "
  - api-key: fake-claude-2
    weight: -4
    request-retry: 2
    prefix: a/b
  - api-key: ""
    base-url: ""
  - api-key: fake-claude-2
    weight: -4
    request-retry: 2
    prefix: a/b
`},
		{Name: "v8_groups_inherit_and_override", YAML: `
config-version: 8
api-keys:
  claude:
    - name: team
      base-url: https://claude.example.invalid
      priority: 5
      prefix: grp
      disable-cooling: true
      excluded-models: [group-model]
      headers: {X-Group: g}
      keys:
        - api-key: fake-k1
        - api-key: fake-k2
          priority: 9
          prefix: null
          disable-cooling: false
          weight: 7
          excluded-models: [key-model]
  codex:
    - name: c
      base-url: https://codex.example.invalid
      keys:
        - {api-key: fake-codex, websockets: true, alpha-search: true, disable-codex-cloaking: false}
    - name: no-base
      keys:
        - {api-key: fake-codex-dropped}
  xai:
    - base-url: https://xai.example.invalid
      keys: [{api-key: fake-xai, alpha-search: true}]
  meta:
    - keys: [{api-key: "dca:fake"}, {api-key: fake-meta}]
`},
		{Name: "gemini_vertex_compat", YAML: `
gemini-api-key:
  - {api-key: fake-g1, priority: 1}
  - {api-key: " fake-g1 "}
  - {api-key: "", base-url: ""}
  - {api-key: "", base-url: "https://gemini.example.invalid"}
interactions-api-key:
  - {api-key: fake-i1, excluded-models: [X]}
vertex-api-key:
  - {api-key: "", base-url: https://v.example.invalid}
  - api-key: fake-v1
    base-url: https://v.example.invalid
    weight: 3
    request-retry: 0
    models: [{name: m1, alias: a1}, {name: m2, alias: ""}]
  - {api-key: fake-v1, base-url: https://v.example.invalid}
openai-compatibility:
  - name: OpenRouter
    base-url: https://router.example.invalid
    priority: 2
    disable-cooling: true
    request-retry: 1
    headers: {X-H: v}
    api-key-entries:
      - {api-key: fake-or-1, weight: 4, proxy-url: http://p.example.invalid}
      - {api-key: fake-or-1}
  - name: nokeys
    base-url: https://nokeys.example.invalid
  - name: off
    disabled: true
    base-url: https://off.example.invalid
    api-key-entries: [{api-key: fake-off}]
  - name: nobase
    api-key-entries: [{api-key: fake-nobase}]
  - name: openai-compatible-pre
    base-url: https://pre.example.invalid
    api-key-entries: [{api-key: fake-pre}]
`},
		{Name: "oauth_files", YAML: `
auth-dir: ./auth
oauth-excluded-models:
  Claude: [" Opus-X ", opus-x, sonnet-y]
`, Files: map[string]string{
			"claude-a.json":  `{"type":"claude","email":"a@example.invalid","priority":"7","weight":2,"excluded_models":["Haiku-Z"],"prefix":"/p/","proxy_url":"http://p.example.invalid","note":" hi ","disabled":true}`,
			"codex-b.json":   `{"type":"codex","plan_type":" pro "}`,
			"gemini-c.json":  `{"type":"gemini"}`,
			"broken.json":    `{"type":`,
			"notype.json":    `{"email":"x@example.invalid"}`,
			"upper.JSON":     `{"type":"Claude","priority":1.9}`,
			"weightbad.json": `{"type":"claude","weight":"abc"}`,
		}},
		{Name: "kimi_files", YAML: "auth-dir: ./auth\n", Files: map[string]string{
			"k1-plain.json":          `{"type":"kimi"}`,
			"k2-type-ai.json":        `{"type":"Kimi-AI"}`,
			"k3-domain-ai.json":      `{"type":"kimi","domain":" AI "}`,
			"k4-unknown-domain.json": `{"type":"kimi.ai","domain":"example.org"}`,
			"k5-base-host.json":      `{"type":"kimi.com","base_url":" https://u:p@API.Kimi.AI:8443/coding "}`,
			"k6-mixed.json":          `{"type":"kimi","domain":"example.org","base-url":"//sub.kimi.ai/x"}`,
			"k7-no-scheme.json":      `{"type":"kimi-ai","base_url":"api.kimi.com/coding"}`,
			"k8-bad-port.json":       `{"type":"kimi-ai","base_url":"https://api.kimi.com:abc/v1"}`,
			"k9-blank.json":          `{"type":"kimi.ai","domain":"  ","base_url":7}`,
			"kimi.ai-name.json":      `{"type":"kimi"}`,
			"k11-bad-escape.json":    `{"type":"kimi","base_url":"https://api.kimi.ai/%zz"}`,
			"k12-colons.json":        `{"type":"kimi","base_url":"https://api.kimi.ai:1:2/x"}`,
			"k10-v6.json":            `{"type":"kimi-ai","base_url":"https://[::1]:8080/x","domain":"sub.kimi.com"}`,
		}},
		{Name: "invalid_weight", YAML: "claude-api-key:\n  - {api-key: fake, weight: 1000001}\n"},
	}
}

func routeScenarios() []routeScenario {
	local := "127.0.0.1:40000"
	key := h("Authorization", "Bearer fake-secret")
	withSecret := "remote-management:\n  secret-key: '$HASH'\nport: 0\n"
	noSecret := "remote-management:\n  allow-remote: false\nport: 0\n"
	get := func(path string, headers map[string][]string) routeStep {
		return routeStep{Method: http.MethodGet, Path: path, Remote: local, Headers: headers}
	}
	return []routeScenario{
		{Name: "unknown_paths_methods_and_preflight", YAML: withSecret, Steps: []routeStep{
			get("/v8/management/unknown", nil),
			get("/v8/management/unknown", key),
			{Method: http.MethodPost, Path: "/v8/management/config", Remote: local},
			{Method: http.MethodDelete, Path: "/v8/management/config", Remote: local, Headers: key},
			{Method: http.MethodHead, Path: "/v8/management/config", Remote: local, Headers: key},
			{Method: http.MethodOptions, Path: "/v8/management/config", Remote: "203.0.113.1:1"},
			{Method: http.MethodOptions, Path: "/anything/at/all", Remote: local},
			get("/v8/management/config", nil),
			get("/v8/management/config", h("Authorization", "Bearer wrong")),
			get("/v8/management", key),
			get("/v8/management/", key),
			get("/v0/management/unknown", key),
			get("/v8/management/credentials/status", key),
			{Method: http.MethodPatch, Path: "/v8/management/credentials/status", Remote: "203.0.113.1:1", Headers: key},
			get("/v8/management/config/server/port", key),
			get("/v8/management/config/server/missing", key),
			get("/v8/management/config/", key),
		}},
		{Name: "availability_follows_secret_across_reloads", YAML: noSecret, Steps: []routeStep{
			get("/v8/management/config", key),
			{Method: http.MethodOptions, Path: "/v8/management/config", Remote: local},
			{Update: withSecret, Method: http.MethodGet, Path: "/v8/management/config/port", Remote: local, Headers: key},
			{Update: noSecret, Method: http.MethodGet, Path: "/v8/management/config", Remote: local, Headers: key},
		}},
		{Name: "local_password_only_until_reload", YAML: noSecret, LocalPassword: "fake-local", Steps: []routeStep{
			get("/v8/management/config", h("Authorization", "Bearer fake-local")),
			{Update: noSecret, Method: http.MethodGet, Path: "/v8/management/config", Remote: local, Headers: h("Authorization", "Bearer fake-local")},
		}},
		{Name: "env_secret_survives_reload", YAML: noSecret, Env: "fake-env", Steps: []routeStep{
			get("/v8/management/config/port", h("Authorization", "Bearer fake-env")),
			{Update: noSecret, Method: http.MethodGet, Path: "/v8/management/config/port", Remote: "203.0.113.1:1", Headers: h("Authorization", "Bearer fake-env")},
		}},
	}
}

func configScenarios() []configScenario {
	get := func(path string) configStep { return configStep{Method: http.MethodGet, Path: path} }
	put := func(path, body string) configStep { return configStep{Method: http.MethodPut, Path: path, Body: body} }
	patch := func(path, body string) configStep {
		return configStep{Method: http.MethodPatch, Path: path, Body: body}
	}
	del := func(path string) configStep { return configStep{Method: http.MethodDelete, Path: path} }
	owner := "# Owner config\nhost: \"\"\nport: 8317 # listen\nremote-management:\n  # allow the dashboard\n  allow-remote: true\n  secret-key: \"$HASH\" # hashed\nauth-dir: \"~/.cli-proxy-api\"\napi-keys:\n  - \"sk-fake-owner\"\n# disabled for now\nrequest-retry: 3\nmystery-section:\n  nested: [1, 2]\n"
	v8 := "config-version: 8\nmanagement:\n  secret-key: '$HASH'\nrouting:\n  strategy: round-robin\n  retry:\n    request-retry: 2\n  cooldown:\n    disable-cooling: true\nserver:\n  mystery-field: 1\n  port: 0\nmystery-section: {a: 1}\n"
	turn := "config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  providers:\n    codex:\n      live-media-relay:\n        ice-servers:\n          - {urls: ['turn:a.example.invalid'], username: fake-user-a, credential: fake-cred-a}\n          - {urls: ['turn:b.example.invalid'], username: fake-user-b, credential: fake-cred-b}\n"
	nonDefaults := "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n  panel-github-repository: fake/panel\n" +
		"server:\n  host: 127.0.0.1\n  port: 9999\n  discovery:\n    service-type: _x._tcp\n" +
		"credentials:\n  in-flight:\n    snapshot-interval: 3s\n    stale-after: 20s\n    staging-retention: 2m\n    max-part-bytes: 524288\n    max-part-count: 63\n    max-revision-bytes: 16777215\n    max-aggregate-groups: 99\n    max-details: 9\n    max-string-bytes: 255\n  concurrency:\n    max-limit: 5\n    busy-retry-min: 1s\n" +
		"oauth:\n  providers:\n    aistudio:\n      ws-auth: false\n  excluded-models:\n    claude: [opus-x]\n    gemini: [b]\n  model-alias:\n    claude:\n      - {name: a, alias: b}\n" +
		"observability:\n  logs:\n    error-logs-max-files: 3\n  usage:\n    redis-usage-queue-retention-seconds: 30\n  pprof:\n    addr: 127.0.0.1:1\n" +
		"multimedia:\n  disable-image-generation: chat\nrouting:\n  retry:\n    request-retry: 4\n"
	return []configScenario{
		{Name: "null_writes_take_go_defaults", YAML: nonDefaults, Steps: []configStep{
			put("/config/oauth/providers/aistudio/ws-auth", `null`),
			put("/config/observability/logs/error-logs-max-files", `null`),
			put("/config/observability/usage/redis-usage-queue-retention-seconds", `null`),
			put("/config/observability/pprof/addr", `null`),
			put("/config/management/panel-github-repository", `null`),
			put("/config/server/discovery/service-type", `null`),
			put("/config/credentials/in-flight/snapshot-interval", `null`),
			put("/config/credentials/in-flight/max-part-bytes", `null`),
			put("/config/credentials/in-flight/stale-after", `null`),
			put("/config/credentials/in-flight/staging-retention", `null`),
			put("/config/credentials/in-flight/max-part-count", `null`),
			put("/config/credentials/in-flight/max-revision-bytes", `null`),
			put("/config/credentials/in-flight/max-aggregate-groups", `null`),
			put("/config/credentials/in-flight/max-details", `null`),
			put("/config/credentials/in-flight/max-string-bytes", `null`),
			put("/config/credentials/concurrency/max-limit", `null`),
			put("/config/credentials/concurrency/busy-retry-min", `null`),
			put("/config/multimedia/disable-image-generation", `null`),
			put("/config/routing/retry/request-retry", `null`),
			put("/config/server/host", `null`),
			put("/config/server/port", `null`),
			patch("/config", `{"server":{"discovery":{"service-type":null}},"oauth":{"providers":{"aistudio":{"ws-auth":null}}}}`),
		}},
		{Name: "malformed_oauth_maps_are_rejected_before_sanitizing", YAML: nonDefaults, Steps: []configStep{
			put("/config/oauth/excluded-models", `{"claude":"opus-*"}`),
			put("/config/oauth/model-alias", `{"claude":[{"name":["x"],"alias":"y"}]}`),
			put("/config/oauth/settings", `{"claude":"x"}`),
			put("/config/oauth/request-scoped-errors/claude", `[{"status":"x"}]`),
			put("/config/oauth/excluded-models/gemini", `[" B ", "b", "c"]`),
			patch("/config", `{"oauth":{"model-alias":{"Gemini":[{"name":"m","alias":"n"},{"name":"m2","alias":"N"}]}}}`),
		}},
		{Name: "owner_legacy_migrates_on_first_write", YAML: owner, Steps: []configStep{
			get("/config"), get("/config.yaml"), get("/config/server"), get("/config/nope"),
			put("/config/routing/strategy", `"fill-first"`), get("/config"),
		}},
		{Name: "patch_null_and_delete_prunes_empty_ancestors", YAML: v8, Steps: []configStep{
			patch("/config", `{"routing":{"cooldown":{"disable-cooling":null}}}`),
			del("/config/routing/retry/request-retry"),
			del("/config/routing/cooldown/disable-cooling"),
			del("/config/routing/strategy"),
			del("/config/routing/strategy"),
			del("/config/server/port"),
		}},
		{Name: "rejected_writes_leave_the_file", YAML: v8, Steps: []configStep{
			put("/config/server/port", `"bad"`),
			put("/config/access", `null`),
			put("/config/oauth/providers/codex/unknown", `true`),
			put("/config/credentials/concurrency/lifecycle-config-revision", `5`),
			put("/config/plugins/auth-revision", `1`),
			put("/config/access/api-keys/0", `"x"`),
			put("/config/routing/strategy/x", `"x"`),
			put("/config", `[]`),
			put("/config", `{bad`),
			put("/config", ``),
			put("/config.yaml", ``),
			put("/config.yaml", `# only a comment`),
			put("/config", `{"port": 1}`),
			put("/config/api-keys/unknown-provider", `[]`),
			put("/config/config-version", `7`),
			put("/config/api-keys/claude", `[{"keys":[{"api-key":"fake","base-url":"https://x.invalid"}]}]`),
			put("/config/api-keys/claude", `[{"keys":[{"api-key":"fake","weight":1000001}]}]`),
			patch("/config/routing", `{"retry":{"request-retry":"three"}}`),
			put("/config/routing/session-affinity-ttl", `5`),
		}},
		{Name: "accepted_value_writes", YAML: v8, Steps: []configStep{
			put("/config/api-keys/claude", `[{"name":"team","base-url":"https://claude.example.invalid","priority":3,"keys":[{"api-key":"fake-a"},{"api-key":"fake-b","weight":0,"prefix":null}]}]`),
			patch("/config/oauth", `{"excluded-models":{"claude":["Opus-*"]},"model-alias":{"claude":[{"name":"claude-sonnet-4-6","alias":"sonnet"}]}}`),
			put("/config/access/api-keys", `["fake-client-1","fake-client-2"]`),
			put("/config/requests/payload/default", `[{"models":[{"name":"*","protocol":"openai"}],"params":{"temperature":0.5}}]`),
			get("/config"),
			put("/config/management/secret-key", `"fake-rotated"`),
			get("/config"),
		}},
		// YAML 1.1 bool spellings and merge keys in the loaded file, and bool spellings
		// written as JSON strings (Go's typed saver persists booleans).
		{Name: "yaml_bools_and_merges", YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n" +
			"routing:\n  session-affinity: yes\n  retry: &r\n    request-retry: 2\n" +
			"observability:\n  logs:\n    <<: {debug: on, request-log: Off}\n    logging-to-file: false\n", Steps: []configStep{
			get("/config"),
			put("/config/routing/retry/request-retry", `3`),
			put("/config/routing/session-affinity-subagents", `"off"`),
			put("/config/observability/logs/request-log", `"YES"`),
			put("/config/routing/force-model-prefix", `"yEs"`),
			put("/config/routing/force-model-prefix", `"true"`),
			get("/config"),
		}},
		{Name: "turn_secrets_redacted_and_preserved_by_urls", YAML: turn, Steps: []configStep{
			get("/config/oauth/providers/codex/live-media-relay"),
			put("/config/oauth/providers/codex/live-media-relay/ice-servers", `[{"urls":["turn:b.example.invalid"]},{"urls":["turn:c.example.invalid"]},{"urls":["turn:a.example.invalid"],"username":""}]`),
			get("/config.yaml"),
		}},
		{Name: "root_put_and_yaml_put", YAML: v8, Steps: []configStep{
			put("/config", `{"server":{"port":1},"management":{"secret-key":"fake-secret"}}`),
			put("/config.yaml", "config-version: 8\nmanagement:\n  secret-key: fake-secret\nserver:\n  port: 2 # yaml\n"),
			patch("/config.yaml", "{}"),
		}},
		// Plugin entries keep their raw scalars through writes; floats in Go int fields
		// truncate (the typed saver writes ints back outside plugin entries).
		{Name: "plugin_entries_and_float_ints", YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n" +
			"routing:\n  retry:\n    request-retry: 2.9\nplugins:\n  configs:\n    x:\n      enabled: \"yes\"\n      priority: 5.7\n    y:\n", Steps: []configStep{
			get("/config/plugins"),
			patch("/config/routing", `{"strategy":"fill-first"}`),
			patch("/config/plugins/configs/x", `{"priority":6.5}`),
			patch("/config/plugins/configs/x", `{"enabled":"on"}`),
			get("/config/plugins"),
		}},
		// gin's c.JSON escapes <, > and & (and U+2028/U+2029) in every string.
		{Name: "html_escaping_in_json", YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\n" +
			"server:\n  host: \"a<b>&c\\u2028d\\u2029\"\n", Steps: []configStep{
			get("/config/server/host"),
			get("/config/server"),
			patch("/config/server", `{"host":"<x&y>"}`),
			get("/config/server/host"),
		}},
		// Settings inherited through merge keys survive edits next to them (Go expands
		// aliases before editing); /config/config.yaml is a key lookup, not the YAML
		// download; merges in uploads are flattened, scalar merges are rejected.
		{Name: "inherited_merges_survive_writes", YAML: "config-version: 8\n<<:\n  management:\n    secret-key: '$HASH'\n" +
			"  access:\n    api-keys: [fake-client-1]\n  routing:\n    retry:\n      request-retry: 2\n", Steps: []configStep{
			patch("/config", `{"access":{}}`),
			patch("/config/routing/retry", `{"max-retry-interval":5}`),
			patch("/config/management", `{"allow-remote":false}`),
			get("/config"),
			get("/config/config.yaml"),
			get("/config/management/config.yaml"),
			put("/config/config.yaml", `1`),
			put("/config.yaml", "config-version: 8\nmanagement: &m\n  secret-key: fake-secret\nrouting:\n  retry: &r {request-retry: 3}\n  <<: {strategy: fill-first}\n"),
			put("/config.yaml", "config-version: 8\nmanagement:\n  secret-key: fake-secret\nrouting:\n  <<: 5\n"),
			get("/config"),
		}},
	}
}

// logsYAML is a v8 config with the two log switches the log routes read.
func logsYAML(toFile, requestLog bool) string {
	return fmt.Sprintf("config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  auth-dir: $AUTH\nobservability:\n  logs:\n    logging-to-file: %t\n    request-log: %t\n", toFile, requestLog)
}

func credScenarios() []credScenario {
	jwt := func(claims string) string {
		enc := func(s string) string { return strings.TrimRight(base64.URLEncoding.EncodeToString([]byte(s)), "=") }
		return enc(`{"alg":"none"}`) + "." + enc(claims) + ".sig"
	}
	idToken := jwt(`{"email":"b@example.invalid","https://api.openai.com/auth":{"chatgpt_plan_type":"pro","chatgpt_account_id":"acc-fake"}}`)
	files := map[string]string{
		"claude-a.json":   `{"type":"claude","email":"a@example.invalid","priority":"7","note":" hi ","access_token":"fake-at","refresh_token":"fake-rt","expired":"2099-01-01T00:00:00Z","quota_probe":{"kind":"fake"},"zz":{"keep":[1,2]}}`,
		"codex-b.json":    `{"type":"codex","id_token":"` + idToken + `","access_token":"fake","websockets":"true","request-retry":3,"project_id":" proj "}`,
		"expired-c.json":  `{"type":"claude","access_token":"x","expired":"2000-01-01T00:00:00Z","weight":2}`,
		"disabled-d.json": `{"type":"claude","disabled":true,"email":"d@example.invalid"}`,
		"notype.json":     `{"email":"x@example.invalid"}`,
		"readme.txt":      "not json",
	}
	get := func(path string) credStep { return credStep{Method: http.MethodGet, Path: path} }
	call := func(method, path, body string) credStep { return credStep{Method: method, Path: path, Body: body} }
	boundary := "fixtureboundary"
	multipartBody := "--" + boundary + "\r\nContent-Disposition: form-data; name=\"b\"; filename=\"up2.txt\"\r\nContent-Type: text/plain\r\n\r\nnope\r\n" +
		"--" + boundary + "\r\nContent-Disposition: form-data; name=\"a\"; filename=\"dir/up1.json\"\r\nContent-Type: application/json\r\n\r\n{\"type\":\"claude\",\"email\":\"u@example.invalid\"}\r\n" +
		"--" + boundary + "--\r\n"
	mp := func(ct, body string) credStep {
		if ct == "" {
			ct = "multipart/form-data; boundary=b"
		}
		return credStep{Method: http.MethodPost, Path: "/credentials", Body: body, ContentType: ct}
	}
	return []credScenario{{
		Name:  "credential_inventory_and_edits",
		YAML:  "remote-management:\n  secret-key: '$HASH'\nauth-dir: $AUTH\nclaude-api-key:\n  - api-key: fake-cfg-key\n    base-url: https://claude.example.invalid\n",
		Files: files,
		Steps: []credStep{
			get("/credentials"),
			get("/credentials?name=claude-a.json"),
			get("/credentials?auth_index=$INDEX(codex-b.json)"),
			get("/credentials?name=claude-a.json&auth_index=$INDEX(codex-b.json)"),
			get("/credentials?page=2&page_size=2"),
			get("/credentials?page=0"),
			get("/credentials?page_size=x"),
			get("/credentials/download?name=notype.json"),
			get("/credentials/download?name=../x.json"),
			get("/credentials/download?name=readme.txt"),
			get("/credentials/download?name=missing.json"),
			get("/credentials/download"),
			call(http.MethodPatch, "/credentials/status", `{"name":"claude-a.json","disabled":true}`),
			call(http.MethodPatch, "/credentials/status", `{"name":"claude-a.json"}`),
			call(http.MethodPatch, "/credentials/status", `{}`),
			call(http.MethodPatch, "/credentials/status", `{"name":"nope.json","disabled":true}`),
			call(http.MethodPatch, "/credentials/status", `{"name":"claude-a.json","auth_index":"$INDEX(codex-b.json)","disabled":false}`),
			call(http.MethodPatch, "/credentials/status", `not json`),
			call(http.MethodPatch, "/credentials/status", `{"name":"$CFGID","disabled":true}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"codex-b.json","priority":5,"note":"n","headers":{"X-A":"1"},"request_retry":-1,"weight":3,"meta.inner":"v"}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"codex-b.json","weight":"x"}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"codex-b.json"}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"codex-b.json","request_retry.x":1}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"codex-b.json","excluded-models":["a"],"excluded_models":["b"],"headers":{"X-A":""}}`),
			call(http.MethodPatch, "/credentials/fields", `{"priority":1}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"missing.json","priority":1}`),
			get("/credentials?name=codex-b.json"),
			call(http.MethodPost, "/credentials?name=new.json", `{"type":"claude","email":"n@example.invalid"}`),
			call(http.MethodPost, "/credentials?name=bad.json", `{`),
			call(http.MethodPost, "/credentials?name=x.txt", `{}`),
			call(http.MethodPost, "/credentials?name=../evil.json", `{}`),
			{Method: http.MethodPost, Path: "/credentials", Body: multipartBody, ContentType: "multipart/form-data; boundary=" + boundary},
			{Method: http.MethodPost, Path: "/credentials", Body: "--" + boundary + "--\r\n", ContentType: "multipart/form-data; boundary=" + boundary},
			// Hostile and unusual multipart input, parsed by Go's mime/multipart.
			mp("", "--b\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp;semi.json\"\r\n\r\n{\"type\":\"claude\"}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"C:\\dir\\mp-win.json\"\r\n\r\n{\"type\":\"claude\"}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename*=UTF-8''mp%2Dstar.json\r\n\r\n{\"type\":\"claude\"}\r\n--b--\r\n"),
			mp("", "preamble\n--b\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-lf.json\"\n\n{\"type\":\"claude\"}\n--b--\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-cut.json\"\r\n\r\n{\"type\":\"claude\"}"),
			mp("", "--b\r\nbogus\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\n continued: x\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nBad Key: x\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-hdr.json\""),
			mp("", "hello"),
			mp("", ""),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"\"\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; filename=\"mp-noname.json\"\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: attachment; name=\"f\"; filename=\"mp-att.json\"\r\n\r\n{}\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-x.json\"\r\n\r\n{}\r\n--b\r\nX: y\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-y.json\"\r\n\r\n{}\r\n--bX\r\n--b--\r\n"),
			mp("", "--b\r\nContent-Disposition: form-data; name=\"z\"; filename=\"mp-z.json\"\r\n\r\n{\"type\":\"claude\"}\r\n--b \t\r\nContent-Disposition: form-data; name=\"a\"; filename=\"mp-a.txt\"\r\n\r\nx\r\n--b--"),
			mp("multipart/form-data", "--b--\r\n"),
			mp("multipart/form-data; boundary", "--b--\r\n"),
			mp("multipart/form-data; boundary=\"\"", "----\r\n"),
			mp("multipart/form-data; boundary=b; boundary=c", "--b--\r\n"),
			mp("multipart/form-data;boundary=\"b\";", "--b--\r\n"),
			mp("Multipart/Form-Data; boundary=b", "--b--\r\n"),
			mp("multipart/form-data ; boundary=b", "--b\r\nContent-Disposition: form-data; name=\"f\"; filename=\"mp-sp.json\"\r\n\r\n{\"type\":\"claude\"}\r\n--b--\r\n"),
			get("/credentials?name=new.json"),
			call(http.MethodPost, "/credentials?name=raw-pretty.json", "{\n  \"type\": \"claude\",\n  \"disabled\": false,\n  \"priority\": 1.0\n}\n"),
			call(http.MethodPost, "/credentials?name=raw-alias.json", "{\n  \"type\": \"claude\",\n  \"proxy-url\": \"http://p.example.invalid\",\n  \"disabled\": true\n}\n"),
			call(http.MethodPost, "/credentials?name=raw-flag.json", `{"type":"claude","disabled":"yes"}`),
			// Files no synthesizer claims are registered as fallback auths until restart.
			call(http.MethodPost, "/credentials?name=fallback.json", `{"email":"f@example.invalid","disabled":true,"headers":{"X-A":" 1 "},"note":"n"}`),
			get("/credentials?name=fallback.json"),
			call(http.MethodPatch, "/credentials/status", `{"name":"fallback.json","disabled":true}`),
			get("/credentials?name=fallback.json"),
			call(http.MethodPost, "/credentials?name=gem.json", `{"type":"gemini","email":"g@example.invalid"}`),
			get("/credentials?name=gem.json"),
			call(http.MethodDelete, "/credentials?name=fallback.json", ""),
			get("/credentials?name=fallback.json"),
			// Config API keys accept field edits in memory; `type` may change on files.
			call(http.MethodPatch, "/credentials/fields", `{"name":"$CFGID","note":"updated","priority":3,"headers":{"X-B":"2"}}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"$CFGID","disabled":"true"}`),
			call(http.MethodPost, "/credentials?name=retype.json", `{"type":"claude","email":"r@example.invalid"}`),
			call(http.MethodPatch, "/credentials/fields", `{"name":"retype.json","type":"codex"}`),
			call(http.MethodDelete, "/credentials?name=retype.json", ""),
			call(http.MethodDelete, "/credentials?name=new.json", ""),
			call(http.MethodDelete, "/credentials", `{"names":["up1.json","missing.json"]}`),
			call(http.MethodDelete, "/credentials", `["../x.json"]`),
			call(http.MethodDelete, "/credentials", ``),
			call(http.MethodPost, "/routing/cooldown/reset", `{"auth_index":"$INDEX(claude-a.json)"}`),
			call(http.MethodPost, "/routing/cooldown/reset", `{}`),
			call(http.MethodPost, "/routing/cooldown/reset", `{"auth_index":"nope"}`),
			get("/routing/model-definitions/claude"),
			get("/routing/model-definitions/CODEX"),
			get("/routing/model-definitions/nope"),
			call(http.MethodPost, "/credentials/refresh", `{}`),
			call(http.MethodPost, "/credentials/refresh", `{"name":"missing.json"}`),
			call(http.MethodDelete, "/credentials?all=true", ""),
			get("/credentials"),
		},
	}, {
		Name: "api_call_and_usage", Echo: true,
		Files: map[string]string{
			"claude-a.json": files["claude-a.json"],
			"quote.json":    `{"type":"claude","access_token":"a\"b<c"}`,
			"empty.json":    `{"type":"claude","email":"e@example.invalid"}`,
		},
		YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  auth-dir: $AUTH\napi-keys:\n  claude:\n    - base-url: https://claude.example.invalid\n      keys:\n        - api-key: fake-usage\n  openai-compatibility:\n    - name: Compat-One\n      base-url: https://compat.example.invalid/v1\n      keys:\n        - api-key: fake-compat\n",
		Steps: []credStep{
			call(http.MethodPost, "/requests/api-call", ``),
			call(http.MethodPost, "/requests/api-call", `[]`),
			call(http.MethodPost, "/requests/api-call", `{"method":1}`),
			call(http.MethodPost, "/requests/api-call", `{"header":{"a":1},"method":"GET"}`),
			call(http.MethodPost, "/requests/api-call", `null`),
			call(http.MethodPost, "/requests/api-call", `{"method":"get"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"/relative"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"http://"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"mailto:x@example.invalid"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/echo","proxy_url":"ftp://proxy.example.invalid"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/echo","header":{"Authorization":"Bearer $TOKEN$"}}`),
			call(http.MethodPost, "/requests/api-call", `{"auth_index":"nope","method":"GET","url":"$ECHO/echo","data":"$TOKEN$"}`),
			call(http.MethodPost, "/requests/api-call", `{"auth_index":"$INDEX(empty.json)","method":"GET","url":"$ECHO/echo","data":"$TOKEN$"}`),
			call(http.MethodPost, "/requests/api-call", `{"auth_index":"$INDEX(claude-a.json)","method":"post","url":"$ECHO/echo?q=1","header":{"Authorization":"Bearer $TOKEN$","X-Custom":"c","Host":"override.example.invalid"},"data":"{\"t\":\"$TOKEN$\"}"}`),
			call(http.MethodPost, "/requests/api-call", `{"AuthIndex":"$INDEX($CFGID)","method":"PUT","url":"$ECHO/echo","header":{"X-Custom":"$TOKEN$"},"data":"plain $TOKEN$"}`),
			call(http.MethodPost, "/requests/api-call", `{"authIndex":"$INDEX(quote.json)","method":"PATCH","url":"$ECHO/echo","data":"{\"t\":\"$TOKEN$\"}"}`),
			call(http.MethodPost, "/requests/api-call", `{"authIndex":"$INDEX(quote.json)","method":"PATCH","url":"$ECHO/echo","data":"not json $TOKEN$"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"POST","url":"$ECHO/redirect","header":{"Content-Type":"text/plain","Authorization":"keep"},"data":"x"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/status"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GE T","url":"$ECHO/echo"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"http://127.0.0.1:1/unreachable"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/echo","proxy_url":"direct"} trailing`),
			call(http.MethodPost, "/requests/api-call", `{"method":1,"method":"GET","url":"$ECHO/echo"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","method":null,"url":"$ECHO/echo"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/echo","header":{"X-Custom":"a","Authorization":"x"},"header":{"X-Custom":"b"}}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/redirect307","header":{"Content-Type":"text/plain"}}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"POST","url":"$ECHO/redirect307","header":{"Content-Type":"text/plain"},"data":"kept"}`),
			call(http.MethodPost, "/requests/api-call", `{"method":"GET","url":"$ECHO/chunked"}`),
			get("/observability/usage/api-keys"),
			get("/observability/usage/queue"),
			get("/observability/usage/queue?count=abc"),
			get("/observability/usage/queue?count=-2"),
			get("/observability/usage/queue?count=%2B3"),
		},
	}, {
		Name: "oauth_sessions", Files: map[string]string{},
		YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  auth-dir: $AUTH\n",
		Steps: []credStep{
			get("/oauth/auth-url"),
			get("/oauth/auth-url?provider=nope"),
			call(http.MethodPost, "/oauth/import", ``),
			call(http.MethodPost, "/oauth/import?provider=nope", ``),
			get("/oauth/status"),
			get("/oauth/status?state=a/b"),
			get("/oauth/status?state=unknown-1"),
			call(http.MethodDelete, "/oauth/session", ``),
			call(http.MethodDelete, "/oauth/session?state=a..b", ``),
			call(http.MethodDelete, "/oauth/session?state=unknown-1", ``),
			call(http.MethodPost, "/oauth/callback", ``),
			call(http.MethodPost, "/oauth/callback", `[]`),
			call(http.MethodPost, "/oauth/callback", `{}`),
			call(http.MethodPost, "/oauth/callback", `{"state":1}`),
			call(http.MethodPost, "/oauth/callback", `{"state":"x y"}`),
			call(http.MethodPost, "/oauth/callback", `{"state":"s1"}`),
			call(http.MethodPost, "/oauth/callback", `{"STATE":"s1","Code":"c"}`),
			call(http.MethodPost, "/oauth/callback", `{"state":123,"state":"s1","code":"c"}`),
			call(http.MethodPost, "/oauth/callback", `{"state":"s1","state":null,"code":"c"}`),
			call(http.MethodPost, "/oauth/callback", `{"redirect_url":"http://h.example.invalid/cb?state=s2&code=c"}`),
			call(http.MethodPost, "/oauth/callback", `{"redirect_url":":bad"}`),
			get("/oauth/callback?state=s1&error_description=denied"),
			get("/oauth/callback?code=c"),
			get("/oauth/auth-url?provider=CLAUDE"),
			get("/oauth/status?state=$STATE"),
			call(http.MethodPost, "/oauth/callback", `{"state":"$STATE","provider":"codex","code":"c"}`),
			call(http.MethodPost, "/oauth/callback", `{"state":"$STATE","provider":"bad_name","code":"c"}`),
			call(http.MethodPost, "/oauth/callback", `{"redirect_url":"http://localhost:54545/callback?state=$STATE&error=access_denied"}`),
			{Method: http.MethodGet, Path: "/oauth/status?state=$STATE", SleepMs: 1500},
			call(http.MethodPost, "/oauth/callback", `{"state":"$STATE","code":"c"}`),
			call(http.MethodDelete, "/oauth/session?state=$STATE", ``),
			get("/oauth/auth-url?provider=codex"),
			get("/oauth/status?state=$STATE"),
			call(http.MethodDelete, "/oauth/session?state=$STATE", ``),
			get("/oauth/status?state=$STATE"),
			call(http.MethodPost, "/oauth/callback", `{"state":"$STATE","code":"c"}`),
		},
	}, {
		// Log routes with logging to file and request logging off: the application log
		// is unavailable, error request logs are listed and downloadable.
		Name: "logs_disabled", Files: map[string]string{}, YAML: logsYAML(false, false),
		LogFiles: []logFile{
			{Name: "main.log", Text: "[2023-11-14 22:14:00] [--------] [info ] [main.go:1] hidden\n", Mtime: 5},
			{Name: "error-v1-chat-2026-01-01T000000-aaaa1111.log", Text: "first error\n", Mtime: 10},
			{Name: "error-v1-chat-2026-01-02T000000-bbbb2222.log", Text: "second error, longer\n", Mtime: 20},
			{Name: "error-notes.txt", Text: "not a log\n", Mtime: 30},
			{Name: "v1-chat-error-2026.log", Text: "not an error log\n", Mtime: 40},
			{Name: "v1-chat-completions-2026-01-01T000000-abcd1234.log", Text: "request one\n", Mtime: 50},
		},
		Steps: []credStep{
			get("/observability/logs"),
			call(http.MethodDelete, "/observability/logs", ``),
			get("/observability/logs/errors"),
			get("/observability/logs/errors/error-v1-chat-2026-01-01T000000-aaaa1111.log"),
			get("/observability/logs/errors/main.log"),
			get("/observability/logs/errors/error-missing.log"),
			get("/observability/logs/errors/error-notes.txt"),
			get("/observability/logs/requests/abcd1234"),
		},
	}, {
		// The application log: rotated files oldest first, tail limits, the legacy
		// after cutoff, Go's incremental cursor across appends and partial lines,
		// request logs by ID and DELETE.
		Name: "logs_enabled", Files: map[string]string{}, YAML: logsYAML(true, true),
		LogFiles: []logFile{
			{Name: "main-2023-11-14T22-12-00.000.log", Text: "[2023-11-14 22:11:00] [--------] [info ] [main.go:3] gamma\n" +
				"[2023-11-14 22:11:30] [--------] [warn ] [main.go:4] delta\n", Mtime: 3},
			{Name: "main.log.1", Text: "[2023-11-14 22:10:00] [--------] [info ] [main.go:1] alpha\n" +
				"[2023-11-14 22:10:30] [--------] [info ] [main.go:2] beta\n", Mtime: 2},
			{Name: "main.log", Text: "[2023-11-14 22:14:00] [--------] [info ] [main.go:5] epsilon\n" +
				"continuation without a timestamp\n" +
				"[2023-11-14 22:15:00] [abcd1234] [error] [main.go:6] zeta\n", Mtime: 20},
			{Name: "error-v1-chat-2026-01-01T000000-aaaa1111.log", Text: "hidden while request-log is on\n", Mtime: 10},
			{Name: "v1-chat-completions-2026-01-01T000000-abcd1234.log", Text: "older by mtime\n", Mtime: 40},
			{Name: "v1-chat-completions-2026-01-02T000000-abcd1234.log", Text: "same mtime, later name time\n", Mtime: 50},
			{Name: "v1-chat-completions-2026-01-01T000000-abcd1234.log.gz", Text: "not a .log\n", Mtime: 90},
			{Name: "v1-responses-2025-12-31T000000_2-abcd1234.log", Text: "same mtime, earlier name time\n", Mtime: 50},
			{Name: "v1-x-2026-01-03T000000-eeee0000.log", Text: "other id\n", Mtime: 60},
		},
		Steps: []credStep{
			get("/observability/logs"),
			get("/observability/logs?limit=2"),
			get("/observability/logs?limit=abc"),
			get("/observability/logs?limit=0"),
			get("/observability/logs?after=1700000460"),
			get("/observability/logs?after=1700000460&limit=1"),
			get("/observability/logs"),
			{Method: http.MethodGet, Path: "/observability/logs?cursor=$CURSOR",
				AppendLog: &logFile{Name: "main.log", Text: "[2023-11-14 22:16:00] [--------] [info ] [main.go:7] eta\n", Mtime: 30}},
			{Method: http.MethodGet, Path: "/observability/logs?cursor=$CURSOR",
				AppendLog: &logFile{Name: "main.log", Text: "[2023-11-14 22:17:00] [--------] [info ] [main.go:8] partial", Mtime: 31}},
			{Method: http.MethodGet, Path: "/observability/logs?cursor=$CURSOR&limit=5",
				AppendLog: &logFile{Name: "main.log", Text: " theta\n[2023-11-14 22:18:00] [--------] [info ] [main.go:9] iota\n", Mtime: 32}},
			get("/observability/logs?cursor=not-base64!"),
			get("/observability/logs?cursor=e30"),
			get("/observability/logs/errors"),
			get("/observability/logs/requests/0198ffff-abcd1234"),
			get("/observability/logs/requests/eeee0000"),
			get("/observability/logs/requests/zzzz9999"),
			get("/observability/logs/requests/a%5Cb"),
			call(http.MethodDelete, "/observability/logs", ``),
		},
	}, {
		// Runtime-only credentials (AI Studio websocket relay): listed from memory with
		// no path, hidden while disabled; file operations find no file.
		Name: "runtime_credentials", RuntimeAuths: []string{"aistudio-0123456789abcdef"},
		Files: map[string]string{"claude-a.json": `{"type":"claude","email":"a@example.invalid"}`},
		YAML:  "config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  auth-dir: $AUTH\n",
		Steps: []credStep{
			get("/credentials"),
			get("/credentials?page=1&page_size=10"),
			get("/credentials?type=aistudio"),
			get("/credentials/download?name=aistudio-0123456789abcdef"),
			get("/credentials/models?name=aistudio-0123456789abcdef"),
			call(http.MethodPatch, "/credentials/fields", `{"name":"aistudio-0123456789abcdef","priority":3}`),
			call(http.MethodPost, "/routing/cooldown/reset", `{"auth_index":"$INDEX(aistudio-0123456789abcdef)"}`),
			call(http.MethodPatch, "/credentials/status", `{"name":"aistudio-0123456789abcdef","disabled":true}`),
			get("/credentials"),
			get("/credentials?page=1&page_size=10"),
			call(http.MethodPatch, "/credentials/status", `{"name":"aistudio-0123456789abcdef","disabled":false}`),
			get("/credentials"),
			call(http.MethodDelete, "/credentials?name=aistudio-0123456789abcdef", ``),
			call(http.MethodDelete, "/credentials", `{"names":["aistudio-0123456789abcdef"]}`),
			get("/credentials"),
		},
	}, {
		// The dashboard's capability probes: Go rejects each with 400 before any I/O.
		Name: "dashboard_probes", Files: files, YAML: "config-version: 8\nmanagement:\n  secret-key: '$HASH'\noauth:\n  auth-dir: $AUTH\napi-keys:\n  claude:\n    - keys:\n        - api-key: fake-probe\n",
		Steps: []credStep{
			{Method: http.MethodPost, Path: "/credentials", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodPost, Path: "/credentials/refresh", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodPatch, Path: "/credentials/fields", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodDelete, Path: "/credentials", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodPost, Path: "/routing/cooldown/reset", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodPost, Path: "/requests/api-call", Body: "{}", ContentType: "application/json"},
			{Method: http.MethodPost, Path: "/oauth/import", Body: "{}", ContentType: "application/json"},
			get("/oauth/auth-url"),
			get("/observability/usage/queue?count=0"),
		},
	}}
}
