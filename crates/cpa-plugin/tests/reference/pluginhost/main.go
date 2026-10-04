// Generates goldens for the Rust plugin host by driving the unmodified Go
// internal/pluginhost.Host (CLIProxyAPI 6fecc6e) with real c-shared plugins built from
// ../../goplugins. Every step records its inputs and the Go host's observable result;
// the Rust test replays the steps in order and compares. Nothing leaves the machine.
//
// Usage: go run . <dir with built plugins> <output.json>
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginhost"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
)

type step struct {
	Op     string          `json:"op"`
	Args   json.RawMessage `json:"args,omitempty"`
	Result any             `json:"result"`
}

type runner struct {
	built     string
	pluginDir string
	recordDir string
	authDir   string
	upstream  string // host:port of the raw upstream (upstream.go)
	host      *pluginhost.Host
	steps     []step
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: go run . <built plugins dir> <output.json>")
		os.Exit(2)
	}
	work, err := os.MkdirTemp("", "cpa-pluginhost-golden-")
	check(err)
	defer os.RemoveAll(work)
	r := &runner{
		built:     os.Args[1],
		pluginDir: filepath.Join(work, "plugins"),
		recordDir: filepath.Join(work, "records"),
		authDir:   filepath.Join(work, "auths"),
		upstream:  startUpstream(),
		host:      pluginhost.New(),
	}
	check(os.MkdirAll(r.pluginDir, 0o755))
	check(os.MkdirAll(r.recordDir, 0o755))
	check(os.MkdirAll(r.authDir, 0o755))
	scenarios(r)
	capabilityScenarios(r)
	proxyScenarios(r)
	authScenarios(r)
	out, err := json.MarshalIndent(map[string]any{"steps": r.steps}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[2], append(out, '\n'), 0o644))
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func (r *runner) add(op string, args any, result any) {
	raw, err := json.Marshal(args)
	check(err)
	r.steps = append(r.steps, step{Op: op, Args: raw, Result: result})
}

// files copies built plugins into the plugin directory: name -> built file stem.
func (r *runner) files(files map[string]string) {
	for name, from := range files {
		data, err := os.ReadFile(filepath.Join(r.built, from+".so"))
		check(err)
		check(os.WriteFile(filepath.Join(r.pluginDir, name), data, 0o755))
	}
	r.add("files", files, nil)
}

func (r *runner) apply(yaml string) {
	text := strings.NewReplacer("PLUGINDIR", r.pluginDir, "RECORDDIR", r.recordDir, "AUTHDIR", r.authDir).Replace(yaml)
	cfg, err := config.ParseConfigBytes([]byte(text))
	check(err)
	// cmd/server/main.go resolves auth-dir before anything sees the config.
	cfg.AuthDir, err = util.ResolveAuthDir(cfg.AuthDir)
	check(err)
	r.host.ApplyConfig(context.Background(), cfg)
	r.add("apply", yaml, r.registered())
}

type pluginInfo struct {
	ID            string         `json:"id"`
	Priority      int            `json:"priority"`
	Metadata      map[string]any `json:"metadata"`
	SupportsOAuth bool           `json:"supports_oauth"`
	OAuthProvider string         `json:"oauth_provider"`
	SupportsQuota bool           `json:"supports_quota"`
	QuotaProvider string         `json:"quota_provider"`
	Menus         []any          `json:"menus"`
}

func (r *runner) registered() []pluginInfo {
	out := []pluginInfo{}
	for _, info := range r.host.RegisteredPlugins() {
		raw, _ := json.Marshal(info.Metadata)
		var meta map[string]any
		_ = json.Unmarshal(raw, &meta)
		menus := []any{}
		for _, menu := range info.Menus {
			menus = append(menus, map[string]string{"path": menu.Path, "menu": menu.Menu, "description": menu.Description})
		}
		out = append(out, pluginInfo{
			ID: info.ID, Priority: info.Priority, Metadata: meta,
			SupportsOAuth: info.SupportsOAuth, OAuthProvider: info.OAuthProvider,
			SupportsQuota: info.SupportsQuota, QuotaProvider: info.QuotaProvider, Menus: menus,
		})
	}
	return out
}

func (r *runner) loaded(ids ...string) {
	out := map[string][2]bool{}
	for _, id := range ids {
		out[id] = [2]bool{r.host.PluginLoaded(id), r.host.PluginRegistered(id)}
	}
	r.add("loaded", ids, out)
}

func (r *runner) registerManagement(reserved ...string) {
	set := map[string]struct{}{}
	for _, key := range reserved {
		set[key] = struct{}{}
	}
	r.host.RegisterManagementRoutes(context.Background(), set)
	r.add("register_management", reserved, r.registered())
}

type httpArgs struct {
	Method  string              `json:"method"`
	Target  string              `json:"target"`
	Headers map[string][]string `json:"headers,omitempty"`
	Body    string              `json:"body,omitempty"`
}

type httpResult struct {
	Handled bool                `json:"handled"`
	Status  int                 `json:"status"`
	Headers map[string][]string `json:"headers"`
	Body    string              `json:"body"`
}

func (r *runner) serve(op string, args httpArgs) {
	req := httptest.NewRequest(args.Method, args.Target, strings.NewReader(strings.ReplaceAll(args.Body, "UPSTREAM", r.upstream)))
	for k, vs := range args.Headers {
		for _, v := range vs {
			req.Header.Add(k, v)
		}
	}
	rec := httptest.NewRecorder()
	var handled bool
	if op == "management" {
		handled = r.host.ServeManagementHTTP(rec, req)
	} else {
		handled = r.host.ServeResourceHTTP(rec, req)
	}
	res := httpResult{Handled: handled, Headers: map[string][]string{}}
	if handled {
		resp := rec.Result()
		body, _ := io.ReadAll(resp.Body)
		res.Status = resp.StatusCode
		res.Headers = resp.Header
		res.Body = normalizeTimes(strings.NewReplacer(r.upstream, "UPSTREAM", r.authDir, "AUTHDIR").Replace(string(body)))
	}
	r.add(op, args, res)
}

func (r *runner) unload(id string) {
	r.add("unload", id, r.host.UnloadPlugin(id))
}

func (r *runner) shutdown() {
	r.host.ShutdownAll()
	r.add("shutdown", nil, nil)
}

// records returns and clears what every recorder instance received since the last call.
func (r *runner) records() {
	out := map[string][]map[string]string{}
	entries, err := os.ReadDir(r.recordDir)
	check(err)
	names := []string{}
	for _, entry := range entries {
		if !entry.IsDir() {
			names = append(names, entry.Name())
		}
	}
	sort.Strings(names)
	for _, name := range names {
		path := filepath.Join(r.recordDir, name)
		data, err := os.ReadFile(path)
		check(err)
		scanner := bufio.NewScanner(bytes.NewReader(data))
		scanner.Buffer(make([]byte, 1<<20), 1<<24)
		for scanner.Scan() {
			var line map[string]string
			check(json.Unmarshal(scanner.Bytes(), &line))
			r.normalize(line)
			out[strings.TrimSuffix(name, ".jsonl")] = append(out[strings.TrimSuffix(name, ".jsonl")], line)
		}
		check(os.Remove(path))
	}
	r.add("records", nil, out)
}

// normalize writes the record directory as RECORDDIR and moves a lifecycle request's config_yaml into its own field as text with
// the run's directories replaced by placeholders; the Rust test compares it as YAML
// (yaml.v3 and the Rust emitter format differently) and the rest byte for byte.
func (r *runner) normalize(line map[string]string) {
	line["request"] = strings.ReplaceAll(strings.ReplaceAll(line["request"], r.recordDir, "RECORDDIR"), r.upstream, "UPSTREAM")
	// A management body carrying the upstream address is base64 in the record.
	var withBody struct{ Body []byte }
	if json.Unmarshal([]byte(line["request"]), &withBody) == nil && bytes.Contains(withBody.Body, []byte(r.upstream)) {
		old := base64.StdEncoding.EncodeToString(withBody.Body)
		normalized := base64.StdEncoding.EncodeToString(bytes.ReplaceAll(withBody.Body, []byte(r.upstream), []byte("UPSTREAM")))
		line["request"] = strings.Replace(line["request"], old, normalized, 1)
	}
	var req map[string]json.RawMessage
	if json.Unmarshal([]byte(line["request"]), &req) != nil {
		return
	}
	raw, ok := req["config_yaml"]
	if !ok {
		return
	}
	var yamlBytes []byte
	check(json.Unmarshal(raw, &yamlBytes))
	text := strings.NewReplacer(r.recordDir, "RECORDDIR", r.pluginDir, "PLUGINDIR", r.authDir, "AUTHDIR").Replace(string(yamlBytes))
	line["config_yaml"] = text
	line["request"] = strings.Replace(line["request"], string(raw), `"<config_yaml>"`, 1)
}

var _ = http.MethodGet

var (
	timestampPattern = regexp.MustCompile(`\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})`)
	bucketPattern    = regexp.MustCompile(`\d{2}:\d{2}-\d{2}:\d{2}`)
)

// normalizeTimes writes wall-clock timestamps as TIME and recent-request bucket labels
// as BUCKET, which differ between runs.
func normalizeTimes(s string) string {
	return bucketPattern.ReplaceAllString(timestampPattern.ReplaceAllString(s, "TIME"), "BUCKET")
}
