// Generates management/config goldens by calling pinned CLIProxyAPI code in-process.
// It never opens network connections: gin engines run through httptest recorders.
package main

import (
	"context"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
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
	server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path, opts...)
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
	server := api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), path)
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
	Status      int               `json:"status"`
	Response    any               `json:"response"`
	Raw         string            `json:"raw_response,omitempty"`
	Headers     map[string]string `json:"resp_headers,omitempty"`
	Files       map[string]any    `json:"files"`
	RawFiles    map[string]string `json:"raw_files,omitempty"`
	Config      any               `json:"config"`
}

type credScenario struct {
	Name    string            `json:"name"`
	YAML    string            `json:"yaml"`
	Files   map[string]string `json:"auth_files"`
	Indexes map[string]string `json:"indexes"`
	Steps   []credStep        `json:"steps"`
}

func snapshotDir(dir string) map[string]any {
	out := map[string]any{}
	entries, _ := os.ReadDir(dir)
	for _, e := range entries {
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
	server := api.NewServer(cfg, manager, sdkaccess.NewManager(), path)
	resolve := func(text string) string {
		text = strings.ReplaceAll(text, "$CFGID", cfgID)
		for name, index := range s.Indexes {
			text = strings.ReplaceAll(text, "$INDEX("+name+")", index)
		}
		return text
	}
	for i := range s.Steps {
		st := &s.Steps[i]
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
	}
	// Report paths relative to the placeholder root, as the Rust replay does.
	raw, err := json.Marshal(s.Steps)
	must(err)
	must(json.Unmarshal([]byte(strings.ReplaceAll(string(raw), root, fixtureRoot)), &s.Steps))
	return s
}

type output struct {
	Credentials  []credScenario   `json:"credentials"`
	Materialized any              `json:"materialized_defaults"`
	Config       []configScenario `json:"config_writes"`
	IPBytes      []ipBytesCase    `json:"client_ip_bytes"`
	Routes       []routeScenario  `json:"routes"`
	Access       []accessScenario `json:"access"`
	IPs          []ipCase         `json:"client_ip"`
	Synth        []synthCase      `json:"synth"`
	Loads        []loadCase       `json:"load_errors"`
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
	return []configScenario{
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
	}
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
			get("/credentials?name=new.json"),
			call(http.MethodPost, "/credentials?name=raw-pretty.json", "{\n  \"type\": \"claude\",\n  \"disabled\": false,\n  \"priority\": 1.0\n}\n"),
			call(http.MethodPost, "/credentials?name=raw-alias.json", "{\n  \"type\": \"claude\",\n  \"proxy-url\": \"http://p.example.invalid\",\n  \"disabled\": true\n}\n"),
			call(http.MethodPost, "/credentials?name=raw-flag.json", `{"type":"claude","disabled":"yes"}`),
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
	}}
}
