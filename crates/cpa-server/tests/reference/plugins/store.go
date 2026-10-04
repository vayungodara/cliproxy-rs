package main

import (
	"archive/zip"
	"bytes"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/pluginstore"
)

// The plugin store's HTTP side: fixed routes by exact URL, every request recorded.
// Bodies may name another route's zip as ZIPSHA(<url>); each side computes it from its
// own zip, so the archives need not be byte-identical between Go and Rust.

type storeZipEntry struct {
	Name string `json:"name"`
	// Plugin names a built c-shared plugin whose bytes become the entry (mode 0755).
	Plugin string `json:"plugin,omitempty"`
	Data   string `json:"data,omitempty"`
}

type storeRoute struct {
	Status  int               `json:"status"`
	Headers map[string]string `json:"headers,omitempty"`
	Body    string            `json:"body,omitempty"`
	Zip     []storeZipEntry   `json:"zip,omitempty"`
}

type storeDoer struct {
	mu       sync.Mutex
	built    string
	routes   map[string]storeRoute
	requests []map[string]any
}

var zipSHA = regexp.MustCompile(`ZIPSHA\(([^)]*)\)`)

func (d *storeDoer) zip(entries []storeZipEntry) []byte {
	var buf bytes.Buffer
	w := zip.NewWriter(&buf)
	for _, e := range entries {
		header := &zip.FileHeader{Name: e.Name, Method: zip.Store}
		data := []byte(e.Data)
		if e.Plugin != "" {
			var err error
			data, err = os.ReadFile(filepath.Join(d.built, e.Plugin+".so"))
			check(err)
			header.SetMode(0o755)
		}
		f, err := w.CreateHeader(header)
		check(err)
		_, err = f.Write(data)
		check(err)
	}
	check(w.Close())
	return buf.Bytes()
}

func (d *storeDoer) body(route storeRoute) []byte {
	if len(route.Zip) > 0 {
		return d.zip(route.Zip)
	}
	return []byte(zipSHA.ReplaceAllStringFunc(route.Body, func(m string) string {
		sum := sha256.Sum256(d.zip(d.routes[zipSHA.FindStringSubmatch(m)[1]].Zip))
		return hex.EncodeToString(sum[:])
	}))
}

func (d *storeDoer) Do(req *http.Request) (*http.Response, error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	d.requests = append(d.requests, map[string]any{"url": req.URL.String(), "headers": req.Header.Clone()})
	route, ok := d.routes[req.URL.String()]
	if !ok {
		return nil, fmt.Errorf("no route for %s", req.URL.String())
	}
	header := http.Header{}
	for k, v := range route.Headers {
		header.Set(k, v)
	}
	return &http.Response{StatusCode: route.Status, Header: header, Body: io.NopCloser(bytes.NewReader(d.body(route))), Request: req}, nil
}

// storeRoutes replaces the doer's routes with a copy of routes.
func (r *runner) storeRoutes(routes map[string]storeRoute) {
	copied := make(map[string]storeRoute, len(routes))
	for url, route := range routes {
		copied[url] = route
	}
	r.store.mu.Lock()
	r.store.routes = copied
	r.store.mu.Unlock()
	r.steps = append(r.steps, step{Op: "store_routes", Args: copied})
}

// zipDigests maps each zip route's SHA-256 to ZIPSHA(<url>), so outputs do not depend
// on the zip writer's exact bytes.
func (r *runner) zipDigests() []string {
	if r.store == nil {
		return nil
	}
	r.store.mu.Lock()
	defer r.store.mu.Unlock()
	var pairs []string
	for url, route := range r.store.routes {
		if len(route.Zip) > 0 {
			sum := sha256.Sum256(r.store.zip(route.Zip))
			pairs = append(pairs, hex.EncodeToString(sum[:]), "ZIPSHA("+url+")")
		}
	}
	return pairs
}

// storeRequests returns and clears the requests the store sent, sorted by URL then
// order (catalog release lookups run concurrently).
func (r *runner) storeRequests() {
	r.store.mu.Lock()
	out := r.store.requests
	r.store.requests = nil
	r.store.mu.Unlock()
	if out == nil {
		out = []map[string]any{}
	}
	sort.SliceStable(out, func(i, j int) bool { return out[i]["url"].(string) < out[j]["url"].(string) })
	r.steps = append(r.steps, step{Op: "store_requests", Result: out})
}

var retryAfter = regexp.MustCompile(`"retry_after":[0-9]+`)

// stableBody hides the seconds until a far-future rate-limit reset.
func stableBody(body string) string {
	return retryAfter.ReplaceAllString(body, `"retry_after":N`)
}

// storeScenarios covers plugin_store.go and plugin_store_release.go: the official
// registry plus a third-party source with bearer auth, direct and GitHub-release
// installs of the recorder plugin that the host then loads, and the error paths.
func storeScenarios(r *runner) {
	get := func(path string) { r.http(httpArgs{Method: "GET", Path: path}) }
	post := func(path, body string) { r.http(httpArgs{Method: "POST", Path: path, Body: body}) }
	v8 := "/v8/management/plugins/store"
	v0 := "/v0/management/plugin-store"
	official := pluginstore.DefaultRegistryURL
	third := "https://third.example/registry.json"
	thirdID := pluginstore.SourceID(third)
	recZip := "https://dl.example/store-rec.zip"
	api := "https://api.github.com/repos/o/"
	ghBase := "https://github.com/o/gh-rec/releases/download/v1.2.0/"
	ghZip := ghBase + "gh-rec_1.2.0_linux_amd64.zip"

	routes := map[string]storeRoute{
		official: {Status: 200, Body: `{"schema_version":2,"plugins":[` +
			`{"id":"store-rec","name":"Store <Rec>","description":"Records & replays","author":"a","version":"1.0.0","license":"MIT","homepage":"https://h.example","tags":["x","<y>"],` +
			`"install":{"type":"direct","artifacts":[{"goos":"linux","goarch":"amd64","url":"` + recZip + `","sha256":"ZIPSHA(` + recZip + `)"},{"goos":"darwin","goarch":"arm64","url":"https://dl.example/mac.zip","sha256":"` + strings.Repeat("a", 64) + `"}]}},` +
			`{"id":"gh-rec","name":"GH","description":"d","author":"a","repository":"https://github.com/o/gh-rec","logo":"https://l.example/x.png"},` +
			`{"id":"recorder-a","name":"A","description":"d","author":"a","version":"0.9","repository":"https://github.com/o/recorder-a"},` +
			`{"id":"gh-limited","name":"L","description":"d","author":"a","repository":"https://github.com/o/gh-limited"}]}`},
		third: {Status: 200, Body: `{"schema_version":1,"plugins":[` +
			`{"id":"store-rec","name":"Third","description":"d","author":"a","repository":"https://github.com/third/store-rec","auth_required":true},` +
			`{"id":"third-only","name":"T","description":"d","author":"a","repository":"https://github.com/third/only"}]}`},
		recZip:                             {Status: 200, Zip: []storeZipEntry{{Name: "store-rec-v1.0.0.so", Plugin: "recorder"}, {Name: "README.md", Data: "hi"}}},
		api + "recorder-a/releases/latest": {Status: 200, Body: `{"tag_name":"v1.1.0","assets":[]}`},
	}
	r.storeRoutes(routes)
	get(v8)
	r.storeRequests()
	// The release cache answers recorder-a's lookup this time.
	get(v0)
	r.storeRequests()

	post(v8+"/store-rec/install", ``)
	post(v8+"/store-rec/install?source=nope", ``)
	post(v8+"/nothere/install?source=official", ``)
	post(v8+"/nothere/install", ``)
	post(v8+"/bad!id/install", ``)
	post(v8+"/store-rec/install?source=official&version=1.0.0", `{"version":"2.0"}`)
	post(v8+"/store-rec/install?source=official", `{"version":1}`)
	post(v8+"/store-rec/install?source=official&version=v9", ` `)
	r.storeRequests()
	post(v8+"/store-rec/install?source=official", `{"version":"v1.0.0"}`)
	r.storeRequests()
	r.settle()
	r.plugins()
	get("/v8/management/plugins")
	post(v0+"/store-rec/install?source="+thirdID, ``)
	get(v8)
	r.storeRequests()

	// GitHub release by version: the tag as given, then with a v.
	routes[api+"gh-rec/releases/tags/1.2.0"] = storeRoute{Status: 404, Body: `{"message":"Not Found"}`}
	routes[api+"gh-rec/releases/tags/v1.2.0"] = storeRoute{Status: 200, Body: `{"tag_name":"v1.2.0","assets":[` +
		`{"name":"gh-rec_1.2.0_linux_amd64.zip","browser_download_url":"` + ghZip + `"},` +
		`{"name":"checksums.txt","browser_download_url":"` + ghBase + `checksums.txt"}]}`}
	routes[ghZip] = storeRoute{Status: 200, Zip: []storeZipEntry{{Name: "gh-rec.so", Plugin: "recorder"}}}
	routes[ghBase+"checksums.txt"] = storeRoute{Status: 200, Body: "ZIPSHA(" + ghZip + ")  gh-rec_1.2.0_linux_amd64.zip\n"}
	routes[api+"gh-rec/releases/latest"] = storeRoute{Status: 200, Body: `{"tag_name":"v1.3.0","assets":[]}`}
	r.storeRoutes(routes)
	post(v0+"/gh-rec/install?source=official&version=1.2.0", ``)
	r.storeRequests()
	r.settle()
	r.plugins()
	get(v8)
	r.storeRequests()

	// A GitHub rate limit answers 429, and the limiter then blocks without a request.
	routes[api+"gh-limited/releases/latest"] = storeRoute{Status: 403,
		Headers: map[string]string{"X-RateLimit-Remaining": "0", "X-RateLimit-Reset": "4102444800"},
		Body:    `{"message":"API rate limit exceeded"}`}
	r.storeRoutes(routes)
	post(v8+"/gh-limited/install?source=official", ``)
	post(v8+"/gh-limited/install?source=official", ``)
	r.storeRequests()

	// A failing source is listed beside the working one; all failing is a 502.
	routes[third] = storeRoute{Status: 500, Body: "down"}
	r.storeRoutes(routes)
	get(v8)
	routes[official] = storeRoute{Status: 404}
	r.storeRoutes(routes)
	get(v0)
	post(v8+"/store-rec/install", ``)
	r.storeRequests()

	// gin keeps a route tree per method.
	r.http(httpArgs{Method: "DELETE", Path: v8})
	get(v8 + "/quota")
	r.http(httpArgs{Method: "POST", Path: v8})
	get(v8 + "/store-rec/install")

	// json.Decoder reads only the first value of a plugin config body.
	r.http(httpArgs{Method: "PATCH", Path: "/v0/management/plugins/recorder-b/config", Body: `{"label":"b2"} trailing`})
	r.settle()
	r.plugins()
}
