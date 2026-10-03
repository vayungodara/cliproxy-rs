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
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/api"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginhost"
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
	built, pluginDir, recordDir, configPath string
	server                                  *api.Server
	steps                                   []step
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func (r *runner) placeholders(s string) string {
	return strings.NewReplacer("PLUGINDIR", r.pluginDir, "RECORDDIR", r.recordDir).Replace(s)
}

func (r *runner) normalize(s string) string {
	return strings.NewReplacer(r.pluginDir, "PLUGINDIR", r.recordDir, "RECORDDIR").Replace(s)
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
	req := httptest.NewRequest(args.Method, args.Path, strings.NewReader(r.placeholders(args.Body)))
	req.RemoteAddr = "127.0.0.1:40000"
	if !args.NoKey {
		req.Header.Set("Authorization", "Bearer fake-secret")
	}
	rec := httptest.NewRecorder()
	r.server.Handler().ServeHTTP(rec, req)
	r.steps = append(r.steps, step{Op: "http", Args: args, Result: httpResult{
		Status:      rec.Code,
		ContentType: rec.Header().Get("Content-Type"),
		Body:        r.normalize(rec.Body.String()),
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
		configPath: filepath.Join(work, "config.yaml"),
	}
	check(os.MkdirAll(r.pluginDir, 0o755))
	check(os.MkdirAll(r.recordDir, 0o755))
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
	host := pluginhost.New()
	host.ApplyConfig(context.Background(), cfg)
	var server *api.Server
	server = api.NewServer(cfg, coreauth.NewManager(nil, nil, nil), sdkaccess.NewManager(), r.configPath,
		api.WithPluginHost(host),
		api.WithConfigReloadHook(func(ctx context.Context, cfg *config.Config) {
			host.ApplyConfig(ctx, cfg)
			server.RefreshPluginManagementRoutes()
		}))
	r.server = server
	server.RefreshPluginManagementRoutes()
	scenarios(r)
	host.ShutdownAll()
	out, err := json.MarshalIndent(map[string]any{"steps": r.steps}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[2], append(out, '\n'), 0o644))
}
