// Generates goldens for the Rust plugin management routes by driving a complete Go
// api.NewServer (CLIProxyAPI 6fecc6e) with a real plugin host and c-shared plugins built
// from crates/cpa-plugin/tests/goplugins. Saves reload the host through the server's
// config reload hook, as the Go service does. Nothing leaves the machine.
//
// Usage: go run . <dir with built plugins> <output.json>
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/api"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginhost"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginstore"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	sdkaccess "github.com/router-for-me/CLIProxyAPI/v8/sdk/access"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"golang.org/x/crypto/bcrypt"
	"gopkg.in/yaml.v3"
)

type step struct {
	Op     string `json:"op"`
	Args   any    `json:"args,omitempty"`
	Result any    `json:"result"`
}

type runner struct {
	built, pluginDir, recordDir, authDir, configPath string
	server                                           *api.Server
	store                                            *storeDoer
	steps                                            []step
	indexes                                          map[string]string // auth file name -> auth_index
	upstream                                         string            // the probe server's base URL
	upstreamMu                                       sync.Mutex
	upstreamSeen                                     []map[string]any
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func (r *runner) placeholders(s string) string {
	pairs := []string{"PLUGINDIR", r.pluginDir, "RECORDDIR", r.recordDir, "AUTHDIR", r.authDir, "UPSTREAM", r.upstream}
	for name, index := range r.indexes {
		pairs = append(pairs, "AUTHINDEX("+name+")", index)
	}
	return strings.NewReplacer(pairs...).Replace(s)
}

func (r *runner) normalize(s string) string {
	pairs := []string{r.pluginDir, "PLUGINDIR", r.recordDir, "RECORDDIR", r.authDir, "AUTHDIR"}
	if r.upstream != "" {
		pairs = append(pairs, r.upstream, "UPSTREAM", strings.TrimPrefix(r.upstream, "http://"), "UPSTREAMHOST")
	}
	pairs = append(pairs, r.zipDigests()...)
	for name, index := range r.indexes {
		pairs = append(pairs, index, "AUTHINDEX("+name+")")
	}
	return strings.NewReplacer(pairs...).Replace(s)
}

// respond stores the envelope a recorder answers method with ("" removes it).
func (r *runner) respond(label, method, envelope string) {
	dir := filepath.Join(r.recordDir, "respond", label)
	check(os.MkdirAll(dir, 0o755))
	path := filepath.Join(dir, method+".json")
	if envelope == "" {
		_ = os.Remove(path)
	} else {
		check(os.WriteFile(path, []byte(envelope), 0o644))
	}
	r.steps = append(r.steps, step{Op: "respond", Args: map[string]string{"label": label, "method": method, "envelope": envelope}})
}

var callbackID = regexp.MustCompile(`"host_callback_id":"[0-9]+"`)

// records returns and clears the quota.* calls every recorder received.
func (r *runner) records() {
	out := []map[string]string{}
	entries, err := os.ReadDir(r.recordDir)
	check(err)
	for _, entry := range entries {
		if entry.IsDir() {
			continue
		}
		path := filepath.Join(r.recordDir, entry.Name())
		data, err := os.ReadFile(path)
		check(err)
		check(os.Remove(path))
		for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n") {
			var rec map[string]string
			if json.Unmarshal([]byte(line), &rec) != nil || !(strings.HasPrefix(rec["method"], "quota.") || strings.HasPrefix(rec["method"], "auth.login.")) {
				continue
			}
			request := callbackID.ReplaceAllString(r.normalize(rec["request"]), `"host_callback_id":"#"`)
			out = append(out, map[string]string{"label": strings.TrimSuffix(entry.Name(), ".jsonl"), "method": rec["method"], "request": request})
		}
	}
	r.steps = append(r.steps, step{Op: "records", Result: out})
}

// upstreamRequests returns and clears what the probe server received.
func (r *runner) upstreamRequests() {
	r.upstreamMu.Lock()
	seen := r.upstreamSeen
	r.upstreamSeen = nil
	r.upstreamMu.Unlock()
	if seen == nil {
		seen = []map[string]any{}
	}
	raw, err := json.Marshal(seen)
	check(err)
	var out any
	check(json.Unmarshal([]byte(r.normalize(string(raw))), &out))
	r.steps = append(r.steps, step{Op: "upstream_requests", Result: out})
}

// authFiles records every file in the auth directory with its mode and content.
func (r *runner) authFiles() {
	entries, err := os.ReadDir(r.authDir)
	check(err)
	out := map[string]string{}
	for _, entry := range entries {
		path := filepath.Join(r.authDir, entry.Name())
		info, errInfo := os.Stat(path)
		check(errInfo)
		data, errRead := os.ReadFile(path)
		check(errRead)
		out[entry.Name()] = fmt.Sprintf("%o %s", info.Mode().Perm(), r.normalize(string(data)))
	}
	r.steps = append(r.steps, step{Op: "auth_files", Result: out})
}

func (r *runner) files(files map[string]string) {
	for name, from := range files {
		data, err := os.ReadFile(filepath.Join(r.built, from+".so"))
		check(err)
		check(os.WriteFile(filepath.Join(r.pluginDir, name), data, 0o755))
	}
	r.steps = append(r.steps, step{Op: "files", Args: files})
}

type httpArgs struct {
	Method string `json:"method"`
	Path   string `json:"path"`
	Body   string `json:"body,omitempty"`
	NoKey  bool   `json:"no_key,omitempty"`
}

type httpResult struct {
	Status      int    `json:"status"`
	ContentType string `json:"content_type"`
	Body        string `json:"body"`
	Version     bool   `json:"version_headers"`
}

func (r *runner) http(args httpArgs) {
	req := httptest.NewRequest(args.Method, r.placeholders(args.Path), strings.NewReader(r.placeholders(args.Body)))
	req.RemoteAddr = "127.0.0.1:40000"
	if !args.NoKey {
		req.Header.Set("Authorization", "Bearer fake-secret")
	}
	rec := httptest.NewRecorder()
	r.server.Handler().ServeHTTP(rec, req)
	r.steps = append(r.steps, step{Op: "http", Args: args, Result: httpResult{
		Status:      rec.Code,
		ContentType: rec.Header().Get("Content-Type"),
		Body:        stableBody(r.normalize(rec.Body.String())),
		Version:     rec.Header().Get("X-CPA-VERSION") != "",
	}})
}

// settle lets the asynchronous reload after a save finish.
func (r *runner) settle() {
	time.Sleep(700 * time.Millisecond)
	r.steps = append(r.steps, step{Op: "settle"})
}

// plugins records the saved plugins section as JSON.
func (r *runner) plugins() {
	data, err := os.ReadFile(r.configPath)
	check(err)
	var doc map[string]any
	check(yaml.Unmarshal(data, &doc))
	raw, err := json.Marshal(doc["plugins"])
	check(err)
	var out any
	check(json.Unmarshal([]byte(r.normalize(string(raw))), &out))
	r.steps = append(r.steps, step{Op: "plugins", Result: out})
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: go run . <built plugins dir> <output.json>")
		os.Exit(2)
	}
	work, err := os.MkdirTemp("", "cpa-plugin-routes-golden-")
	check(err)
	defer os.RemoveAll(work)
	r := &runner{
		built:      os.Args[1],
		pluginDir:  filepath.Join(work, "plugins"),
		recordDir:  filepath.Join(work, "records"),
		authDir:    filepath.Join(work, "auths"),
		configPath: filepath.Join(work, "config.yaml"),
		indexes:    map[string]string{},
	}
	check(os.MkdirAll(r.pluginDir, 0o755))
	check(os.MkdirAll(r.recordDir, 0o755))
	check(os.MkdirAll(r.authDir, 0o700))
	// The quota probe's upstream: fixed answers by path, every request recorded.
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		body, _ := io.ReadAll(req.Body)
		header := req.Header.Clone()
		header.Del("Content-Length")
		r.upstreamMu.Lock()
		r.upstreamSeen = append(r.upstreamSeen, map[string]any{"method": req.Method, "uri": req.RequestURI, "host": req.Host, "header": header, "body": string(body)})
		r.upstreamMu.Unlock()
		route, ok := upstreamRoutes[req.URL.Path]
		w.Header()["Date"] = nil
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		if route.Location != "" {
			w.Header().Set("Location", route.Location)
		}
		w.WriteHeader(route.Status)
		_, _ = io.WriteString(w, route.Body)
	}))
	defer upstream.Close()
	r.upstream = upstream.URL
	r.steps = append(r.steps, step{Op: "upstream", Args: upstreamRoutes})
	for name, body := range authFiles {
		check(os.WriteFile(filepath.Join(r.authDir, name), []byte(r.placeholders(body)), 0o600))
	}
	r.steps = append(r.steps, step{Op: "auths", Args: authFiles})
	r.files(map[string]string{
		"recorder-a.so":        "recorder",
		"recorder-b.so":        "recorder",
		"recorder-c.so":        "recorder",
		"recorder-d-v1.0.0.so": "recorder",
		"recorder-d-v1.2.0.so": "recorder",
	})
	hash, err := bcrypt.GenerateFromPassword([]byte("fake-secret"), 4)
	check(err)
	text := strings.ReplaceAll(r.placeholders(initialConfig), "$HASH", string(hash))
	r.steps = append(r.steps, step{Op: "config", Args: initialConfig})
	check(os.WriteFile(r.configPath, []byte(text), 0o600))
	cfg, err := config.LoadConfig(r.configPath)
	check(err)
	// Credentials as Go's watcher synthesizes them from the auth dir.
	manager := coreauth.NewManager(nil, nil, nil)
	synth := &synthesizer.SynthesisContext{Config: cfg, AuthDir: r.authDir, Now: time.Now(), IDGenerator: synthesizer.NewStableIDGenerator()}
	auths, err := synthesizer.NewFileSynthesizer().Synthesize(synth)
	check(err)
	for _, a := range auths {
		_, errRegister := manager.Register(coreauth.WithSkipPersist(context.Background()), a)
		check(errRegister)
		r.indexes[a.FileName] = a.EnsureIndex()
	}
	host := pluginhost.New()
	host.ApplyConfig(context.Background(), cfg)
	var server *api.Server
	server = api.NewServer(cfg, manager, sdkaccess.NewManager(), r.configPath,
		api.WithPluginHost(host),
		api.WithConfigReloadHook(func(ctx context.Context, cfg *config.Config) {
			host.ApplyConfig(ctx, cfg)
			server.RefreshPluginManagementRoutes()
		}))
	r.server = server
	// The plugin store's HTTP client and rate limiter, as Go's own store tests set them.
	r.store = &storeDoer{built: r.built, routes: map[string]storeRoute{}}
	server.ManagementHandler().SetPluginStoreTestHooks(r.store, &pluginstore.GitHubRateLimiter{})
	check(os.Setenv("CPA_STORE_ROUTES_TOKEN", "store-token"))
	server.RefreshPluginManagementRoutes()
	scenarios(r)
	host.ShutdownAll()
	out, err := json.MarshalIndent(map[string]any{"steps": r.steps}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[2], append(out, '\n'), 0o644))
}
