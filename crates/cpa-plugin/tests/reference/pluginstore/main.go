// Generates the plugin store golden for the Rust port by running the unmodified Go
// internal/pluginstore (CLIProxyAPI 6fecc6e) through its exported API: registry,
// manifest, version, checksum and auth rules, the client against a fake HTTP doer
// (every request it makes is recorded), and installs of real zip archives into a
// scratch directory. Nothing leaves the machine.
//
// Usage: go run . <output.json>
package main

import (
	"archive/zip"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginstore"
	"gopkg.in/yaml.v3"
)

type testCase struct {
	Op     string          `json:"op"`
	Args   json.RawMessage `json:"args"`
	Result any             `json:"result"`
}

var cases []testCase

func check(err error) {
	if err != nil {
		panic(err)
	}
}

func add(op string, args any, result any) {
	raw, err := json.Marshal(args)
	check(err)
	cases = append(cases, testCase{Op: op, Args: raw, Result: result})
}

func outcome(value any, err error) map[string]any {
	if err != nil {
		var rate *pluginstore.RateLimitError
		limited := errors.As(err, &rate)
		text := err.Error()
		// Cooldowns relative to now (Retry-After, secondary limits) become
		// SOON(<seconds>) so the fixture is stable; the replay does the same.
		if limited && time.Until(rate.RetryAt) < time.Hour {
			stamp := rate.RetryAt.UTC().Format(time.RFC3339)
			text = strings.ReplaceAll(text, stamp, fmt.Sprintf("SOON(%d)", rate.RetryAfterSeconds(time.Now())))
		}
		return map[string]any{"error": text, "rate_limited": limited}
	}
	return map[string]any{"ok": value}
}

// ---- the fake doer -----------------------------------------------------------------

type route struct {
	Status  int                 `json:"status"`
	Headers map[string][]string `json:"headers,omitempty"`
	Body    string              `json:"body,omitempty"`
	BodyB64 string              `json:"body_b64,omitempty"`
	Error   string              `json:"error,omitempty"`
}

type recorded struct {
	URL     string              `json:"url"`
	Headers map[string][]string `json:"headers"`
}

type fakeDoer struct {
	routes   map[string]route
	requests []recorded
}

func (d *fakeDoer) Do(req *http.Request) (*http.Response, error) {
	d.requests = append(d.requests, recorded{URL: req.URL.String(), Headers: req.Header.Clone()})
	r, ok := d.routes[req.URL.String()]
	if !ok {
		return nil, fmt.Errorf("no route for %s", req.URL.String())
	}
	if r.Error != "" {
		return nil, errors.New(r.Error)
	}
	body := []byte(r.Body)
	if r.BodyB64 != "" {
		var err error
		body, err = base64.StdEncoding.DecodeString(r.BodyB64)
		check(err)
	}
	return &http.Response{StatusCode: r.Status, Header: http.Header(r.Headers), Body: io.NopCloser(bytes.NewReader(body)), Request: req}, nil
}

type clientArgs struct {
	RegistryURL  string                   `json:"registry_url,omitempty"`
	NetworkScope string                   `json:"network_scope,omitempty"`
	UserAgent    string                   `json:"user_agent,omitempty"`
	Auth         []pluginstore.AuthConfig `json:"auth,omitempty"`
	Routes       map[string]route         `json:"routes,omitempty"`
	Env          map[string]string        `json:"env,omitempty"`
}

func (a clientArgs) client() (pluginstore.Client, *fakeDoer) {
	doer := &fakeDoer{routes: a.Routes, requests: []recorded{}}
	return pluginstore.Client{
		HTTPClient:   doer,
		NetworkScope: a.NetworkScope,
		RateLimiter:  &pluginstore.GitHubRateLimiter{},
		RegistryURL:  a.RegistryURL,
		UserAgent:    a.UserAgent,
		Auth:         a.Auth,
	}, doer
}

func withEnv(env map[string]string, f func()) {
	for k, v := range env {
		check(os.Setenv(k, v))
	}
	defer func() {
		for k := range env {
			check(os.Unsetenv(k))
		}
	}()
	f()
}

// ---- zips ----------------------------------------------------------------------------

type zipEntry struct {
	Name    string
	Data    string
	Mode    fs.FileMode
	SetMode bool
	Deflate bool
}

func makeZip(entries []zipEntry) string {
	var buf bytes.Buffer
	w := zip.NewWriter(&buf)
	for _, e := range entries {
		header := &zip.FileHeader{Name: e.Name, Method: zip.Store}
		if e.Deflate {
			header.Method = zip.Deflate
		}
		if e.SetMode {
			header.SetMode(e.Mode)
		}
		f, err := w.CreateHeader(header)
		check(err)
		_, err = f.Write([]byte(e.Data))
		check(err)
	}
	check(w.Close())
	return base64.StdEncoding.EncodeToString(buf.Bytes())
}

func sha(b64 string) string {
	data, err := base64.StdEncoding.DecodeString(b64)
	check(err)
	sum := sha256.Sum256(data)
	return hex.EncodeToString(sum[:])
}

type fileInfo struct {
	Path   string `json:"path"`
	SHA256 string `json:"sha256"`
	Mode   string `json:"mode"`
}

func listFiles(root string) []fileInfo {
	out := []fileInfo{}
	check(filepath.WalkDir(root, func(path string, d fs.DirEntry, err error) error {
		check(err)
		if d.IsDir() {
			return nil
		}
		data, errRead := os.ReadFile(path)
		check(errRead)
		info, errInfo := d.Info()
		check(errInfo)
		rel, _ := filepath.Rel(root, path)
		sum := sha256.Sum256(data)
		out = append(out, fileInfo{Path: filepath.ToSlash(rel), SHA256: hex.EncodeToString(sum[:]), Mode: fmt.Sprintf("%o", info.Mode().Perm())})
		return nil
	}))
	sort.Slice(out, func(i, j int) bool { return out[i].Path < out[j].Path })
	return out
}

func relResult(result pluginstore.InstallResult, root string) pluginstore.InstallResult {
	result.Path = strings.ReplaceAll(result.Path, root, "PLUGINS")
	return result
}

// ---- main ------------------------------------------------------------------------------

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: go run . <output.json>")
		os.Exit(2)
	}
	registryCases()
	sourceCases()
	manifestCases()
	manifestYAMLCases()
	versionCases()
	checksumCases()
	authCases()
	clientCases()
	installCases()
	homeSyncCases()
	out, err := json.MarshalIndent(map[string]any{"cases": cases}, "", "  ")
	check(err)
	check(os.WriteFile(os.Args[1], append(out, '\n'), 0o644))
}

func parseRegistry(data string) {
	registry, err := pluginstore.ParseRegistry([]byte(data))
	add("parse_registry", map[string]string{"data": data}, outcome(registry, err))
}

const sha64 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

func registryCases() {
	parseRegistry(`{"schema_version":1,"plugins":[{"id":" demo ","name":" Demo ","description":"d","author":"a","version":"1.0.0","repository":"https://github.com/own%20er/repo","tags":[" x ","y"],"logo":" l ","auth_required":true,"extra":1}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"direct","name":"D","description":"d","author":"a","version":"v2.0.0","install":{"type":" Direct ","artifacts":[{"goos":"macos","goarch":"x86_64","url":" https://dl.example/d.zip ","sha256":" ` + strings.ToUpper(sha64) + ` ","size":10},{"goos":"linux","goarch":"aarch64","url":"http://dl.example/d2.zip","sha256":"` + sha64 + `"}]},"versions":[{"version":"v1.9.0","install":{"artifacts":[{"goos":"linux","goarch":"amd64","url":"https://dl.example/old.zip","sha256":"` + sha64 + `"}]}}]}]}`)
	parseRegistry(`{"schema_version":3,"plugins":[]}`)
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0","install":{"type":"direct"}}]}`)
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"d","description":"d","author":"a","repository":"https://github.com/o/r"}]}`)
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"-bad","name":"n","description":"d","author":"a","repository":"https://github.com/o/r"}]}`)
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"ok","name":"n","description":"d","author":"a","version":"v1.0","repository":"https://github.com/o/r"}]}`)
	for _, repo := range []string{"https://github.com/o", "http://github.com/o/r", "https://github.com/o/r.git", "https://github.com/o/r?x=1", "https://gitlab.com/o/r", "https://github.com/o/r/extra", "https://github.com/o%zz/r"} {
		parseRegistry(`{"schema_version":1,"plugins":[{"id":"ok","name":"n","description":"d","author":"a","repository":"` + repo + `"}]}`)
	}
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"a","name":"n","description":"d","author":"a","repository":"https://github.com/o/r"},{"id":"a","name":"n","description":"d","author":"a","repository":"https://github.com/o/r"}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","install":{"type":"direct","artifacts":[]}}]}`)
	for _, artifact := range []string{
		`{"goarch":"amd64","url":"https://x/a.zip","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","url":"https://x/a.zip","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"/a.zip","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"ftp://x/a.zip","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip?Token=1","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip?ok=1&api%5Fkey=2","sha256":"` + sha64 + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip","sha256":"abc"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip","sha256":"` + strings.Repeat("g", 64) + `"}`,
		`{"goos":"linux","goarch":"amd64","url":"https://x/a.zip","sha256":"` + sha64 + `","size":-1}`,
	} {
		parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0","install":{"type":"direct","artifacts":[` + artifact + `]}}]}`)
	}
	direct := `"install":{"type":"direct","artifacts":[{"goos":"linux","goarch":"amd64","url":"https://x/a.zip","sha256":"` + sha64 + `"}]}`
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a",` + direct + `}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0",` + direct + `,"versions":[{"version":"bad!"}]}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0",` + direct + `,"versions":[{"version":"0.9",` + direct + `},{"version":"v0.9",` + direct + `}]}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0",` + direct + `,"versions":[{"version":"0.9","install":{"type":"github-release"}}]}]}`)
	parseRegistry(`{"schema_version":2,"plugins":[{"id":"d","name":"D","description":"d","author":"a","version":"1.0",` + direct + `,"versions":[{"version":"0.9"}]}]}`)
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"z","name":"n","description":"d","author":"a","install":{"type":"zip"}}]}`)
	parseRegistry(`{"schema_version":1,"plugins":[`)
	parseRegistry(`{"schema_version":"1"}`)
	// json.NewDecoder: trailing bytes after the first value are never read.
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"t","name":"n","description":"d","author":"a","repository":"https://github.com/o/r"}]} trailing {`)
	parseRegistry(``)
	parseRegistry(" \n ")
	parseRegistry(`{"schema_version":1,"plugins":[{"id":"x","tags":"t"}]}`)
}

func sourceCases() {
	for _, urls := range [][]string{
		nil,
		{"", " https://example.com/r.json ", pluginstore.DefaultRegistryURL, "https://example.com/r.json", "not a url", "https://user@other.example:8443/x"},
	} {
		sources, err := pluginstore.NormalizeSources(urls)
		add("normalize_sources", map[string]any{"urls": urls}, outcome(sources, err))
	}
}

var directPlugin = pluginstore.Plugin{
	ID: "direct", Name: "Direct", Description: "d", Author: "a", Version: "1.0.0", Tags: []string{"t"},
	Install: pluginstore.InstallPlan{Type: "direct", Artifacts: []pluginstore.Artifact{{GOOS: "linux", GOARCH: "amd64", URL: "https://dl.example/direct.zip", SHA256: sha64, Size: 5}}},
}
var githubPlugin = pluginstore.Plugin{ID: "gh", Name: "GH", Description: "d", Author: "a", Repository: "https://github.com/o/r", Logo: " logo "}
var source = pluginstore.Source{ID: "official", Name: "Official", URL: "https://store.example/registry.json"}

func manifestCases() {
	manifest, err := pluginstore.ManifestFromPlugin(source, directPlugin)
	add("manifest_from_plugin", map[string]any{"source": source, "plugin": directPlugin}, outcome(manifest, err))
	manifest, err = pluginstore.ManifestFromPlugin(source, githubPlugin)
	add("manifest_from_plugin", map[string]any{"source": source, "plugin": githubPlugin}, outcome(manifest, err))
	for _, tag := range []string{"v1.2.3", "bad tag"} {
		manifest, err = pluginstore.ManifestFromRelease(source, githubPlugin, pluginstore.Release{TagName: tag})
		add("manifest_from_release", map[string]any{"source": source, "plugin": githubPlugin, "tag": tag}, outcome(manifest, err))
	}
	pinned := func(url string) pluginstore.InstallPlan {
		return pluginstore.InstallPlan{Type: "direct", Artifacts: []pluginstore.Artifact{{GOOS: "linux", GOARCH: "amd64", URL: url, SHA256: sha64}}}
	}
	for _, m := range []pluginstore.Manifest{
		{},
		{Version: "v1"},
		{Version: "x1"},
		{Version: "1.0", SchemaVersion: 3, Install: pinned("https://x/a.zip")},
		{Version: "1.0", Install: pluginstore.InstallPlan{Type: "direct"}},
		{Version: "1.0", ID: "-x", Install: pluginstore.InstallPlan{Type: "direct"}},
		{Version: "1.0", ID: "d", Install: pinned("https://x/a.zip")},
		{Version: "1.0", ID: "d", Install: pinned("https://x/a.zip?q=1")},
		{Version: "1.0", ID: "d", Install: pinned("https://u:p@x/a.zip")},
		{Version: "1.0", ID: "d", Install: pinned("https://x/a.zip#frag")},
		{Version: "1.0", ID: "d", Install: pluginstore.InstallPlan{Type: "direct"}},
		{Version: "1.0", ID: "d", Install: pluginstore.InstallPlan{Type: "direct"}, SourceURL: "mailto:x"},
		{Version: "1.0", ID: "d", Install: pluginstore.InstallPlan{Type: "direct"}, SourceURL: "ftp://x/r.json"},
		{Version: "1.0", ID: "d", Install: pluginstore.InstallPlan{Type: "direct"}, SourceURL: "https://x/r.json?secret=1"},
		{Version: "1.0", ID: "d", Install: pluginstore.InstallPlan{Type: "direct"}, SourceURL: "https://x/r.json"},
		{Version: "1.0", ID: "gh", Name: "n", Description: "d", Author: "a", Repository: "https://github.com/o/r"},
		{Version: "1.0", ID: "gh", Name: "n", Description: "d", Author: "a", Repository: "https://github.com/o/r", ReleaseTag: "v1.1"},
		{Version: "v1.0", ID: "gh", Name: "n", Description: "d", Author: "a", Repository: "https://github.com/o/r", ReleaseTag: "V1.0"},
		{Version: "1.0", ID: "gh", Name: "n", Description: "d", Author: "a", ReleaseTag: "1.0"},
		{Version: "1.0", Install: pluginstore.InstallPlan{Type: "zip"}},
	} {
		add("manifest_validate", map[string]any{"manifest": m}, outcome(nil, m.Validate()))
	}
}

func manifestYAMLCases() {
	for _, text := range []string{
		"source-id: official\nsource-url: ' https://x/r.json '\n",
		"schema-version: []\nsource-id: official\n",
		"schema-version: \"2\"\nsource-id: a\n",
		"schema-version: 2.7\nsource-id: a\n",
		"schema-version: true\nsource-id: a\n",
		"schema-version: 0x10\nsource-id: a\n",
		"id: 5\nsource-id: 7\nsource-url: true\n",
		"tags: x\nsource-id: a\n",
		"tags: [a, [b]]\nsource-id: a\n",
		"tags: [a, 1, ~]\nsource-id: a\n",
		"install: {type: direct, artifacts: [{size: big}]}\nsource-id: a\n",
		"install: {type: direct, artifacts: [~, {size: 3, url: u}]}\nsource-id: a\n",
		"install: []\nsource-id: a\n",
		"install: ~\nsource-id: s\n",
		"install: {type: [x]}\nsource-id: s\n",
		"x",
		"[1]",
		"~",
		"",
		"source-id: !custom tagged\n",
		"source-id: [a]\n",
		"unknown: [1]\nsource-id: s\n",
		"Source-ID: s\n",
		"source-id: {a: 1}\n",
		"release-tag: 1.0\nsource-id: s\n",
	} {
		// pluginStoreConfiguredSource reads the source only when the decode succeeds.
		var manifest pluginstore.Manifest
		err := yaml.Unmarshal([]byte(text), &manifest)
		if err != nil {
			manifest = pluginstore.Manifest{}
		}
		add("manifest_yaml", map[string]string{"text": text}, map[string]any{"ok": err == nil, "source_id": strings.TrimSpace(manifest.SourceID), "source_url": strings.TrimSpace(manifest.SourceURL)})
	}
}

func versionCases() {
	for _, pair := range [][2]string{{"1.2.0", "v1.10"}, {"1.10", "1.9"}, {"1.0", "1.0.0"}, {"1.0-beta", "1.0"}, {"", "1.0"}, {"v", "V"}, {"1.-1", "1.0"}, {"2", "10"}} {
		add("update_available", map[string]any{"installed": pair[0], "latest": pair[1]}, pluginstore.UpdateAvailable(pair[0], pair[1]))
	}
}

func checksumCases() {
	for _, data := range []string{
		"# comment\n\n" + sha64 + "  *a.zip\n" + strings.ToUpper(sha64[:63]) + "B b.zip extra\n",
		sha64 + "\n",
		"abc a.zip\n",
		strings.Repeat("z", 64) + " a.zip\n",
	} {
		sums, err := pluginstore.ParseChecksums([]byte(data))
		add("parse_checksums", map[string]any{"data": data}, outcome(sums, err))
	}
	content := "hello"
	sum := sha256.Sum256([]byte(content))
	sums := map[string]string{"a.zip": hex.EncodeToString(sum[:]), "b.zip": sha64}
	for _, name := range []string{"a.zip", "b.zip", "c.zip"} {
		add("verify_checksum", map[string]any{"name": name, "data": content, "checksums": sums}, outcome(nil, pluginstore.VerifyChecksum(name, []byte(content), sums)))
	}
}

func authCases() {
	rules := []pluginstore.AuthConfig{
		{Match: "https://store.example/private/", Type: "Bearer", TokenEnv: " CPA_STORE_GOLDEN_TOKEN ", ApplyTo: []string{"Registry", " registry ", ""}},
		{Match: "https://api.github.com/repos/o/r/releases", Type: "github-token", TokenEnv: "CPA_STORE_GOLDEN_GH"},
		{Match: "https://dl.example/", Type: "basic", UsernameEnv: "CPA_STORE_GOLDEN_USER", PasswordEnv: "CPA_STORE_GOLDEN_PASS", ApplyTo: []string{"artifact"}},
		{Match: "", Type: "bearer", TokenEnv: "CPA_STORE_GOLDEN_TOKEN"},
	}
	env := map[string]string{"CPA_STORE_GOLDEN_TOKEN": "t1", "CPA_STORE_GOLDEN_GH": "g1", "CPA_STORE_GOLDEN_USER": "u", "CPA_STORE_GOLDEN_PASS": " p "}
	private := pluginstore.Source{ID: "p", Name: "P", URL: "https://store.example/private/registry.json"}
	for _, item := range []struct {
		source pluginstore.Source
		plugin pluginstore.Plugin
		env    map[string]string
	}{
		{private, githubPlugin, env},
		{source, githubPlugin, env},
		{source, githubPlugin, nil},
		{source, directPlugin, env},
		{source, pluginstore.Plugin{ID: "x", Repository: "https://github.com/other/r"}, env},
	} {
		var configured bool
		withEnv(item.env, func() { configured = pluginstore.PluginAuthConfigured(item.source, item.plugin, rules) })
		add("plugin_auth_configured", map[string]any{"source": item.source, "plugin": item.plugin, "auth": rules, "env": item.env}, configured)
	}
}

type clientCall struct {
	Call   string                   `json:"call"`
	Plugin pluginstore.Plugin       `json:"plugin,omitempty"`
	Tag    string                   `json:"tag,omitempty"`
	Asset  pluginstore.ReleaseAsset `json:"asset,omitempty"`
	Art    pluginstore.Artifact     `json:"artifact,omitempty"`
}

func runClient(args clientArgs, calls []clientCall) {
	client, doer := args.client()
	results := []any{}
	withEnv(args.Env, func() {
		for _, call := range calls {
			ctx := context.Background()
			switch call.Call {
			case "fetch_registry":
				results = append(results, outcome(client.FetchRegistry(ctx)))
			case "fetch_latest_release":
				results = append(results, outcome(client.FetchLatestRelease(ctx, call.Plugin)))
			case "fetch_release_by_tag":
				results = append(results, outcome(client.FetchReleaseByTag(ctx, call.Plugin, call.Tag)))
			case "download_asset":
				data, err := client.DownloadAsset(ctx, call.Asset)
				results = append(results, outcome(string(data), err))
			case "download_artifact":
				data, err := client.DownloadArtifact(ctx, call.Art)
				results = append(results, outcome(string(data), err))
			case "latest_release_cache_key":
				results = append(results, outcome(client.LatestReleaseCacheKey(call.Plugin)))
			default:
				panic(call.Call)
			}
		}
	})
	add("client", map[string]any{"client": args, "calls": calls}, map[string]any{"results": results, "requests": doer.requests})
}

func ok(body string) route { return route{Status: 200, Body: body} }

func clientCases() {
	registry := `{"schema_version":1,"plugins":[{"id":"gh","name":"n","description":"d","author":"a","repository":"https://github.com/o/r"}]}`
	fetch := []clientCall{{Call: "fetch_registry"}}
	runClient(clientArgs{Routes: map[string]route{pluginstore.DefaultRegistryURL: ok(registry)}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/a/r.json", UserAgent: " ua ", Routes: map[string]route{
		"https://store.example/a/r.json":     {Status: 302, Headers: map[string][]string{"Location": {"../b/r.json?x=1"}}},
		"https://store.example/b/r.json?x=1": {Status: 307, Headers: map[string][]string{"Location": {"https://cdn.example/r.json"}}},
		"https://cdn.example/r.json":         ok(registry),
	}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/r.json", Routes: map[string]route{"https://store.example/r.json": {Status: 301}}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/r.json", Routes: map[string]route{"https://store.example/r.json": {Status: 302, Headers: map[string][]string{"Location": {"mailto:x"}}}}}, fetch)
	loop := map[string]route{}
	for i := 0; i < 12; i++ {
		loop[fmt.Sprintf("https://store.example/%d", i)] = route{Status: 302, Headers: map[string][]string{"Location": {fmt.Sprintf("/%d", i+1)}}}
	}
	runClient(clientArgs{RegistryURL: "https://store.example/0", Routes: loop}, fetch)
	private := map[string]route{"https://store.example/private/r.json": ok(registry)}
	for _, rule := range []pluginstore.AuthConfig{
		{Match: "https://store.example/private", Type: "bearer", TokenEnv: "CPA_STORE_GOLDEN_TOKEN"},
		{Match: "https://STORE.example/private/", Type: "basic", UsernameEnv: "CPA_STORE_GOLDEN_USER", PasswordEnv: "CPA_STORE_GOLDEN_PASS"},
		{Match: "https://store.example/", Type: "header", HeaderName: "x-store-key", HeaderValueEnv: "CPA_STORE_GOLDEN_TOKEN"},
		{Match: "https://store.example/", Type: "header", HeaderValueEnv: "CPA_STORE_GOLDEN_TOKEN"},
		{Match: "https://store.example/", Type: "github-token", TokenEnv: "CPA_STORE_GOLDEN_MISSING"},
		{Match: "https://store.example/", Type: "bearer"},
		{Match: "https://store.example/", Type: "magic"},
		{Match: "https://store.example/", Type: "bearer", TokenEnv: "CPA_STORE_GOLDEN_TOKEN", ApplyTo: []string{"artifact"}},
		{Match: "https://store.example/", Type: "none"},
	} {
		runClient(clientArgs{RegistryURL: "https://store.example/private/r.json", Auth: []pluginstore.AuthConfig{rule}, Routes: private,
			Env: map[string]string{"CPA_STORE_GOLDEN_TOKEN": " t1 ", "CPA_STORE_GOLDEN_USER": "u", "CPA_STORE_GOLDEN_PASS": "p"}}, fetch)
	}
	// Redirects resolve as url.URL.Parse does: dot segments of absolute references are
	// removed before auth rules match, user info survives until validation, and a
	// relative reference is rooted against an empty base path.
	privateRule := []pluginstore.AuthConfig{{Match: "https://store.example/private", Type: "bearer", TokenEnv: "CPA_STORE_GOLDEN_TOKEN"}}
	tokenEnv := map[string]string{"CPA_STORE_GOLDEN_TOKEN": "t1"}
	runClient(clientArgs{RegistryURL: "https://store.example/private/r.json", Auth: privateRule, Env: tokenEnv, Routes: map[string]route{
		"https://store.example/private/r.json": {Status: 302, Headers: map[string][]string{"Location": {"https://store.example/private/../public/r.json"}}},
		"https://store.example/public/r.json":  ok(registry),
	}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/private/r.json", Auth: privateRule, Env: tokenEnv, Routes: map[string]route{
		"https://store.example/private/r.json":    {Status: 302, Headers: map[string][]string{"Location": {"/private/./a/../r2.json?x"}}},
		"https://store.example/private/r2.json?x": ok(registry),
	}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/r.json", Routes: map[string]route{
		"https://store.example/r.json": {Status: 301, Headers: map[string][]string{"Location": {"https://u:p@store.example/x"}}},
	}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example", Routes: map[string]route{
		"https://store.example":              {Status: 302, Headers: map[string][]string{"Location": {"r.json"}}},
		"https://store.example/r.json":       {Status: 302, Headers: map[string][]string{"Location": {"?"}}},
		"https://store.example/r.json?":      {Status: 302, Headers: map[string][]string{"Location": {"#frag"}}},
		"https://store.example/r.json?#frag": ok(registry),
	}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/a/b", Routes: map[string]route{
		"https://store.example/a/b": {Status: 302, Headers: map[string][]string{"Location": {"//cdn.example/../c/./d/.."}}},
		"https://cdn.example/c/":    ok(registry),
	}}, fetch)
	plain := map[string]route{"http://store.example/r.json": ok(registry)}
	runClient(clientArgs{RegistryURL: "http://store.example/r.json", Routes: plain}, fetch)
	runClient(clientArgs{RegistryURL: "http://store.example/r.json", Routes: plain, Auth: []pluginstore.AuthConfig{{Match: "http://store.example/", AllowInsecure: true}}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/r.json?access_token=1", Routes: plain}, fetch)
	runClient(clientArgs{RegistryURL: "https://u:p@store.example/r.json", Routes: plain}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/missing.json", Routes: map[string]route{"https://store.example/missing.json": {Status: 404, Body: "  not found  "}}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/private/r.json", Auth: []pluginstore.AuthConfig{{Match: "https://store.example/", Type: "bearer", TokenEnv: "CPA_STORE_GOLDEN_TOKEN"}},
		Env: map[string]string{"CPA_STORE_GOLDEN_TOKEN": "t"}, Routes: map[string]route{"https://store.example/private/r.json": {Status: 500, Body: "secret detail"}}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/r.json?keep=1#frag", Routes: map[string]route{"https://store.example/r.json?keep=1#frag": {Error: "connection refused"}}}, fetch)
	runClient(clientArgs{RegistryURL: "https://store.example/bad.json", Routes: map[string]route{"https://store.example/bad.json": ok(`{"schema_version":9}`)}}, fetch)

	release := `{"tag_name":"v1.2.0","assets":[{"name":"gh_1.2.0_linux_amd64.zip","url":"https://api.github.com/repos/o/r/releases/assets/1","browser_download_url":"https://github.com/o/r/releases/download/v1.2.0/gh_1.2.0_linux_amd64.zip"}]}`
	reset := map[string][]string{"X-Ratelimit-Remaining": {"0"}, "X-Ratelimit-Reset": {"4102444800"}}
	runClient(clientArgs{NetworkScope: "proxy-a", Routes: map[string]route{
		"https://api.github.com/repos/o/r/releases/latest":      ok(release),
		"https://api.github.com/repos/o/r/releases/tags/v1.0+1": {Status: 403, Headers: reset, Body: `{"message":"API rate limit exceeded"}`},
	}}, []clientCall{
		{Call: "fetch_latest_release", Plugin: githubPlugin},
		{Call: "fetch_release_by_tag", Plugin: githubPlugin, Tag: " v1.0+1 "},
		{Call: "fetch_latest_release", Plugin: githubPlugin},
		{Call: "fetch_release_by_tag", Plugin: githubPlugin, Tag: " "},
		{Call: "fetch_latest_release", Plugin: pluginstore.Plugin{Repository: "https://github.com/o"}},
	})
	twice := []clientCall{{Call: "fetch_latest_release", Plugin: githubPlugin}, {Call: "fetch_latest_release", Plugin: githubPlugin}}
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 403, Body: `{"message":"You have exceeded a secondary rate limit"}`}}}, twice)
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 429, Headers: map[string][]string{"Retry-After": {"120"}}}}}, twice)
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 403, Body: `{"message":"Resource not accessible by integration"}`}}}, twice)
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 200, Body: `{"tag_name":`}}}, twice[:1])
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 403, Body: `{"MESSAGE":"API rate limit exceeded"}`}}}, twice)
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 403, Body: `{"message":5,"Message":"API rate limit exceeded"}`}}}, twice)
	runClient(clientArgs{Routes: map[string]route{"https://api.github.com/repos/o/r/releases/latest": {Status: 403, Body: `{"message":"API rate limit exceeded"} x`}}}, twice[:1])
	ghRule := []pluginstore.AuthConfig{{Match: "https://api.github.com/", Type: "github-token", TokenEnv: "CPA_STORE_GOLDEN_GH"}}
	runClient(clientArgs{NetworkScope: "scope", Auth: ghRule, Env: map[string]string{"CPA_STORE_GOLDEN_GH": "secret"}, Routes: map[string]route{
		"https://api.github.com/repos/o/r/releases/latest":   ok(release),
		"https://api.github.com/repos/o/r/releases/assets/1": ok("from-api"),
	}}, []clientCall{
		{Call: "latest_release_cache_key", Plugin: githubPlugin},
		{Call: "fetch_latest_release", Plugin: githubPlugin},
		{Call: "download_asset", Asset: pluginstore.ReleaseAsset{Name: "a", APIURL: "https://api.github.com/repos/o/r/releases/assets/1", BrowserDownloadURL: "https://github.com/o/r/releases/download/x/a"}},
		{Call: "download_asset", Asset: pluginstore.ReleaseAsset{Name: "none"}},
	})
	runClient(clientArgs{Routes: map[string]route{
		"https://github.com/o/r/releases/download/x/a": ok("from-browser"),
		"https://dl.example/a.zip":                     ok("0123456789"),
	}}, []clientCall{
		{Call: "download_asset", Asset: pluginstore.ReleaseAsset{Name: "a", APIURL: "https://api.github.com/repos/o/r/releases/assets/1", BrowserDownloadURL: "https://github.com/o/r/releases/download/x/a"}},
		{Call: "download_artifact", Art: pluginstore.Artifact{GOOS: "linux", GOARCH: "amd64", URL: "https://dl.example/a.zip", SHA256: sha64, Size: 4}},
		{Call: "download_artifact", Art: pluginstore.Artifact{GOOS: "linux", GOARCH: "amd64", URL: "https://dl.example/a.zip", SHA256: sha64}},
		{Call: "download_artifact", Art: pluginstore.Artifact{GOOS: "linux", URL: "https://dl.example/a.zip", SHA256: sha64}},
	})
}

type installArgs struct {
	Client   clientArgs           `json:"client"`
	Call     string               `json:"call"`
	Plugin   pluginstore.Plugin   `json:"plugin,omitempty"`
	Tag      string               `json:"tag,omitempty"`
	Version  string               `json:"version,omitempty"`
	Zip      string               `json:"zip,omitempty"`
	Manifest pluginstore.Manifest `json:"manifest,omitempty"`
	GOOS     string               `json:"goos"`
	GOARCH   string               `json:"goarch"`
	// Runs share a plugins directory until a run sets Fresh.
	Fresh bool `json:"fresh,omitempty"`
}

var pluginsDir string

func runInstall(args installArgs) {
	if pluginsDir == "" || args.Fresh {
		dir, err := os.MkdirTemp("", "cpa-pluginstore-golden-")
		check(err)
		pluginsDir = dir
	}
	client, doer := args.Client.client()
	options := pluginstore.InstallOptions{PluginsDir: pluginsDir, GOOS: args.GOOS, GOARCH: args.GOARCH}
	var result pluginstore.InstallResult
	var err error
	ctx := context.Background()
	withEnv(args.Client.Env, func() {
		switch args.Call {
		case "install":
			result, err = client.Install(ctx, args.Plugin, options)
		case "install_version":
			result, err = client.InstallVersion(ctx, args.Plugin, args.Tag, args.Version, options)
		case "install_manifest":
			result, err = client.InstallManifest(ctx, args.Manifest, options)
		case "install_archive":
			data, errDecode := base64.StdEncoding.DecodeString(args.Zip)
			check(errDecode)
			result, err = pluginstore.InstallArchive(data, args.Plugin, options)
		default:
			panic(args.Call)
		}
	})
	if err != nil {
		err = errors.New(strings.ReplaceAll(err.Error(), pluginsDir, "PLUGINS"))
	}
	add("install", args, map[string]any{"result": outcome(relResult(result, pluginsDir), err), "requests": doer.requests, "files": listFiles(pluginsDir)})
}

func installCases() {
	lib := "\x7fELF-plugin-v1.2.0"
	archive := makeZip([]zipEntry{{Name: "README.md", Data: "readme"}, {Name: "docs/", Data: ""}, {Name: "gh.so", Data: lib, SetMode: true, Mode: 0o750, Deflate: true}})
	checksums := sha(archive) + "  gh_1.2.0_linux_amd64.zip\n"
	release := func(tag, archiveName string) string {
		return `{"tag_name":"` + tag + `","assets":[{"name":"` + archiveName + `","url":"https://api.github.com/repos/o/r/releases/assets/1","browser_download_url":"https://github.com/o/r/releases/download/` + tag + `/` + archiveName + `"},{"name":"checksums.txt","url":"https://api.github.com/repos/o/r/releases/assets/2","browser_download_url":"https://github.com/o/r/releases/download/` + tag + `/checksums.txt"}]}`
	}
	routes := map[string]route{
		"https://api.github.com/repos/o/r/releases/latest":                         ok(release("v1.2.0", "gh_1.2.0_linux_amd64.zip")),
		"https://github.com/o/r/releases/download/v1.2.0/gh_1.2.0_linux_amd64.zip": {Status: 200, BodyB64: archive},
		"https://github.com/o/r/releases/download/v1.2.0/checksums.txt":            ok(checksums),
	}
	runInstall(installArgs{Client: clientArgs{Routes: routes}, Call: "install", Plugin: githubPlugin, GOOS: "linux", GOARCH: "x86_64", Fresh: true})
	runInstall(installArgs{Client: clientArgs{Routes: routes}, Call: "install", Plugin: githubPlugin, GOOS: "linux", GOARCH: "amd64"})
	tagged := map[string]route{
		"https://api.github.com/repos/o/r/releases/tags/v1.3.0":                    ok(release("v1.3.0", "gh_1.3.0_linux_amd64.zip")),
		"https://api.github.com/repos/o/r/releases/tags/1.3.0":                     ok(release("1.4.0", "gh_1.3.0_linux_amd64.zip")),
		"https://github.com/o/r/releases/download/v1.3.0/gh_1.3.0_linux_amd64.zip": {Status: 200, BodyB64: makeZip([]zipEntry{{Name: "gh-v1.3.0.so", Data: "v1.3.0"}})},
		"https://github.com/o/r/releases/download/v1.3.0/checksums.txt":            ok(strings.Repeat("0", 64) + " gh_1.3.0_linux_amd64.zip\n"),
	}
	runInstall(installArgs{Client: clientArgs{Routes: tagged}, Call: "install_version", Plugin: githubPlugin, Tag: "v1.3.0", Version: "1.3.0", GOOS: "linux", GOARCH: "amd64"})
	runInstall(installArgs{Client: clientArgs{Routes: tagged}, Call: "install_version", Plugin: githubPlugin, Version: "v1.3.0", GOOS: "linux", GOARCH: "amd64"})
	runInstall(installArgs{Client: clientArgs{Routes: tagged}, Call: "install_version", Plugin: githubPlugin, Version: "bad!", GOOS: "linux", GOARCH: "amd64"})
	runInstall(installArgs{Client: clientArgs{Routes: routes}, Call: "install", Plugin: githubPlugin, GOOS: "darwin", GOARCH: "arm64"})
	noChecksums := map[string]route{"https://api.github.com/repos/o/r/releases/latest": ok(`{"tag_name":"1.2.0","assets":[{"name":"gh_1.2.0_linux_amd64.zip","browser_download_url":"https://x/a.zip"}]}`)}
	runInstall(installArgs{Client: clientArgs{Routes: noChecksums}, Call: "install", Plugin: githubPlugin, GOOS: "linux", GOARCH: "amd64"})

	directZip := makeZip([]zipEntry{{Name: "direct-v1.0.0.so", Data: "direct-lib"}})
	direct := directPlugin
	direct.Install = pluginstore.InstallPlan{Type: "direct", Artifacts: []pluginstore.Artifact{{GOOS: "linux", GOARCH: "amd64", URL: "https://dl.example/direct.zip", SHA256: sha(directZip)}}}
	directRoutes := map[string]route{"https://dl.example/direct.zip": {Status: 200, BodyB64: directZip}}
	runInstall(installArgs{Client: clientArgs{Routes: directRoutes}, Call: "install", Plugin: direct, GOOS: "linux", GOARCH: "amd64", Fresh: true})
	runInstall(installArgs{Client: clientArgs{Routes: directRoutes}, Call: "install", Plugin: direct, GOOS: "windows", GOARCH: "amd64"})
	bad := direct
	bad.Install = pluginstore.InstallPlan{Type: "direct", Artifacts: []pluginstore.Artifact{{GOOS: "linux", GOARCH: "amd64", URL: "https://dl.example/direct.zip", SHA256: sha64}}}
	runInstall(installArgs{Client: clientArgs{Routes: directRoutes}, Call: "install", Plugin: bad, GOOS: "linux", GOARCH: "amd64"})

	manifest, err := pluginstore.ManifestFromPlugin(source, direct)
	check(err)
	runInstall(installArgs{Client: clientArgs{Routes: directRoutes}, Call: "install_manifest", Manifest: manifest, GOOS: "linux", GOARCH: "amd64", Fresh: true})
	sourced := pluginstore.Manifest{ID: "direct", Version: "0.9.0", SourceURL: "https://store.example/v2.json", Install: pluginstore.InstallPlan{Type: "direct"}}
	oldZip := makeZip([]zipEntry{{Name: "direct.so", Data: "old"}})
	v2 := `{"schema_version":2,"plugins":[{"id":"direct","name":"D","description":"d","author":"a","version":"1.0.0","install":{"type":"direct","artifacts":[{"goos":"linux","goarch":"amd64","url":"https://dl.example/direct.zip","sha256":"` + sha(directZip) + `"}]},"versions":[{"version":"v0.9.0","install":{"artifacts":[{"goos":"linux","goarch":"amd64","url":"https://dl.example/old.zip","sha256":"` + sha(oldZip) + `"}]}}]}]}`
	sourceRoutes := map[string]route{"https://store.example/v2.json": ok(v2), "https://dl.example/old.zip": {Status: 200, BodyB64: oldZip}, "https://dl.example/direct.zip": {Status: 200, BodyB64: directZip}}
	runInstall(installArgs{Client: clientArgs{Routes: sourceRoutes}, Call: "install_manifest", Manifest: sourced, GOOS: "linux", GOARCH: "amd64"})
	sourced.Version = "2.0.0"
	runInstall(installArgs{Client: clientArgs{Routes: sourceRoutes}, Call: "install_manifest", Manifest: sourced, GOOS: "linux", GOARCH: "amd64"})
	sourced.Version = "1.0.0"
	sourced.ID = "other"
	runInstall(installArgs{Client: clientArgs{Routes: sourceRoutes}, Call: "install_manifest", Manifest: sourced, GOOS: "linux", GOARCH: "amd64"})
	ghManifest := pluginstore.Manifest{ID: "gh", Name: "GH", Description: "d", Author: "a", Repository: "https://github.com/o/r", Version: "1.3.0", ReleaseTag: "v1.3.0"}
	runInstall(installArgs{Client: clientArgs{Routes: tagged}, Call: "install_manifest", Manifest: ghManifest, GOOS: "linux", GOARCH: "amd64"})

	plugin := pluginstore.Plugin{ID: "p", Version: "v1.0"}
	for _, entries := range [][]zipEntry{
		{{Name: "p.so", Data: "fat"}},
		{{Name: "p.so", Data: "zero", SetMode: true, Mode: 0}},
		{{Name: "p-v1.0.so", Data: "versioned", SetMode: true, Mode: 0o640}},
		{{Name: "lib/p.so", Data: "nested"}},
		{{Name: "q.so", Data: "wrong"}},
		{{Name: "p.so", Data: "a"}, {Name: "p-v1.0.so", Data: "b"}},
		{{Name: "p.so", Data: "link", SetMode: true, Mode: fs.ModeSymlink | 0o777}},
		{{Name: `dir\p.so`, Data: "x"}},
		{{Name: "/p.so", Data: "x"}},
		{{Name: "../p.so", Data: "x"}},
		{{Name: "a/../p.so", Data: "cleaned"}},
		{{Name: "readme.txt", Data: "x"}},
		{{Name: "P.SO.txt", Data: "x"}, {Name: "x.DLL", Data: "y"}},
		{{Name: "sub/", Data: ""}, {Name: "p.so", Data: "dir-first", SetMode: true, Mode: 0o700}},
	} {
		runInstall(installArgs{Call: "install_archive", Plugin: plugin, Zip: makeZip(entries), GOOS: "linux", GOARCH: "amd64", Fresh: true})
	}
	big := strings.Repeat("A", 4096)
	for _, patch := range []func([]byte){
		func(central []byte) { binary.LittleEndian.PutUint32(central[24:], 1) },
		func(central []byte) { binary.LittleEndian.PutUint32(central[24:], 1<<20) },
		func(central []byte) { binary.LittleEndian.PutUint16(central[10:], 99) },
		func(central []byte) { binary.LittleEndian.PutUint32(central[16:], 7) },
	} {
		zipped, err := base64.StdEncoding.DecodeString(makeZip([]zipEntry{{Name: "p.so", Data: big, Deflate: true}}))
		check(err)
		at := bytes.Index(zipped, []byte{'P', 'K', 1, 2})
		patch(zipped[at:])
		runInstall(installArgs{Call: "install_archive", Plugin: plugin, Zip: base64.StdEncoding.EncodeToString(zipped), GOOS: "linux", GOARCH: "amd64", Fresh: true})
	}
	runInstall(installArgs{Call: "install_archive", Plugin: plugin, Zip: base64.StdEncoding.EncodeToString([]byte("not a zip")), GOOS: "linux", GOARCH: "amd64"})
	runInstall(installArgs{Call: "install_archive", Plugin: pluginstore.Plugin{ID: "-p", Version: "1"}, Zip: makeZip(nil), GOOS: "linux", GOARCH: "amd64"})
	runInstall(installArgs{Call: "install_archive", Plugin: pluginstore.Plugin{ID: "p", Version: "x"}, Zip: makeZip(nil), GOOS: "linux", GOARCH: "amd64"})
}

func homeSyncCases() {
	now := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	ghManifest := `{"id":"gh","name":"GH","description":"d","author":"a","repository":"https://github.com/o/r","version":"1.0","release_tag":"v1.0"}`
	pinned := `{"id":"d","version":"1.0","install":{"type":"direct","artifacts":[{"goos":"linux","goarch":"amd64","url":"URL","sha256":"` + sha64 + `"}]}}`
	for _, body := range []string{
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + ghManifest + `,"auth":[{"match":"https://api.github.com/","type":"github-token","token":"dG9r"}]}]}`,
		`{"schema_version":2,"expires_at":"2026-01-03T00:00:00Z","items":[]}`,
		`{"schema_version":1,"items":[]}`,
		`{"schema_version":1,"expires_at":"2026-01-02T03:04:05Z","items":[]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":{"id":"d","version":"1.0","install":{"type":"direct"},"source_url":"https://x/r.json"}}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + strings.ReplaceAll(pinned, "URL", "http://x/a.zip") + `}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + strings.ReplaceAll(pinned, "URL", "https://x/a.zip") + `},{"manifest":` + strings.ReplaceAll(pinned, "URL", "https://x/b.zip") + `}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + ghManifest + `,"auth":[{"match":"http://x/"}]}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + ghManifest + `,"auth":[{"match":"https://x/","apply_to":["everything"]}]}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + ghManifest + `,"auth":[{"match":"https://x/","type":"header","header_name":"X-A:"}]}]}`,
		`{"schema_version":1,"expires_at":"2026-01-03T00:00:00Z","items":[{"manifest":` + ghManifest + `,"auth":[{"match":"https://x/","type":"basic","username":"dQ=="}]}]}`,
	} {
		var response pluginstore.PluginSyncResponse
		check(json.Unmarshal([]byte(body), &response))
		add("home_sync_validate", map[string]any{"body": body, "now": now}, outcome(nil, response.Validate(now)))
	}
}
