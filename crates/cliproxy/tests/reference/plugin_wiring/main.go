// Generates ../../fixtures/plugin_wiring_go.json: Go's server binary (CLIProxyAPI
// 6fecc6e, cmd/server) runs with recorder plugins and a local upstream, and every step
// sends identical raw HTTP requests. Each step records what the client got, what the
// upstream received and what the plugins were asked (exact request bytes). The Rust
// test replays the same steps against this repository's binary.
//
// Usage: go run . <server binary> <recorder.so> <output.json>
package main

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"sync"
	"syscall"
	"time"
)

type step struct {
	Op     string `json:"op"`
	Args   any    `json:"args,omitempty"`
	Result any    `json:"result,omitempty"`
}

type respond struct {
	Label    string `json:"label"`
	Method   string `json:"method"`
	Envelope string `json:"envelope"`
}

type route struct {
	Status  int               `json:"status"`
	Headers map[string]string `json:"headers,omitempty"`
	Body    string            `json:"body"`
	// Truncate declares a longer body than it sends, then drops the connection.
	Truncate bool `json:"truncate,omitempty"`
	// Then answers later requests to the same path, in order (the last repeats).
	Then []route `json:"then,omitempty"`
}

type scenario struct {
	Name     string            `json:"name"`
	Config   string            `json:"config"`
	Plugins  map[string]string `json:"plugins"`
	Auths    map[string]string `json:"auths,omitempty"`
	Upstream map[string]route  `json:"upstream,omitempty"`
	Responds []respond         `json:"responds,omitempty"`
	Steps    []step            `json:"steps"`
}

type httpArgs struct {
	Method  string      `json:"method"`
	Path    string      `json:"path"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body,omitempty"`
}

type httpResult struct {
	Status  int               `json:"status"`
	Headers map[string]string `json:"headers"`
	Body    string            `json:"body"`
}

type recordsArgs struct {
	Methods []string `json:"methods"`
}

func check(err error) {
	if err != nil {
		panic(err)
	}
}

type runner struct {
	server, recorder, work string
	dirs                   map[string]string
	port                   string
	upstream               *httptest.Server
	upstreamMu             sync.Mutex
	upstreamSeen           []map[string]any
	routes                 map[string]route
	served                 map[string]int
	cmd                    *exec.Cmd
}

func (r *runner) placeholders(s string) string {
	pairs := []string{"PORT", r.port, "UPSTREAM", r.upstream.URL}
	for key, dir := range r.dirs {
		pairs = append(pairs, key, dir)
	}
	return strings.NewReplacer(pairs...).Replace(s)
}

func (r *runner) normalize(s string) string {
	pairs := []string{r.upstream.URL, "UPSTREAM", strings.TrimPrefix(r.upstream.URL, "http://"), "UPSTREAMHOST"}
	for key, dir := range r.dirs {
		pairs = append(pairs, dir, key)
	}
	pairs = append(pairs, r.work, "WORK", "127.0.0.1:"+r.port, "127.0.0.1:PORT")
	return strings.NewReplacer(pairs...).Replace(s)
}

func freePort() string {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	check(err)
	defer l.Close()
	return fmt.Sprint(l.Addr().(*net.TCPAddr).Port)
}

func (r *runner) writeRespond(rs respond) {
	dir := filepath.Join(r.dirs["RECDIR"], "respond", rs.Label)
	check(os.MkdirAll(dir, 0o755))
	path := filepath.Join(dir, rs.Method+".json")
	if rs.Envelope == "" {
		_ = os.Remove(path)
		return
	}
	check(os.WriteFile(path, []byte(r.placeholders(rs.Envelope)), 0o644))
}

func (r *runner) start(sc *scenario) {
	r.work = must(os.MkdirTemp("", "cpa-plugin-wiring-"))
	r.dirs = map[string]string{
		"PLUGINDIR": filepath.Join(r.work, "plugins"),
		"RECDIR":    filepath.Join(r.work, "rec"),
		"AUTHDIR":   filepath.Join(r.work, "auths"),
	}
	for _, dir := range r.dirs {
		check(os.MkdirAll(dir, 0o700))
	}
	r.port = freePort()
	r.upstreamMu.Lock()
	r.routes = sc.Upstream
	r.served = map[string]int{}
	r.upstreamSeen = nil
	r.upstreamMu.Unlock()
	for file := range sc.Plugins {
		data := must(os.ReadFile(r.recorder))
		check(os.WriteFile(filepath.Join(r.dirs["PLUGINDIR"], file), data, 0o755))
	}
	for name, body := range sc.Auths {
		check(os.WriteFile(filepath.Join(r.dirs["AUTHDIR"], name), []byte(r.placeholders(body)), 0o600))
	}
	for _, rs := range sc.Responds {
		r.writeRespond(rs)
	}
	configPath := filepath.Join(r.work, "config.yaml")
	check(os.WriteFile(configPath, []byte(r.placeholders(sc.Config)), 0o600))
	logFile := must(os.Create(filepath.Join(r.work, "server.log")))
	r.cmd = exec.Command(r.server, "-config", configPath, "-local-model")
	r.cmd.Dir = r.work
	r.cmd.Env = []string{"HOME=" + r.work, "PATH=/usr/bin:/bin", "TZ=UTC"}
	r.cmd.Stdout, r.cmd.Stderr = logFile, logFile
	check(r.cmd.Start())
	deadline := time.Now().Add(30 * time.Second)
	for {
		res, err := r.raw(httpArgs{Method: "GET", Path: "/healthz"})
		if err == nil && res.Status == 200 {
			return
		}
		if time.Now().After(deadline) {
			log, _ := os.ReadFile(filepath.Join(r.work, "server.log"))
			panic(fmt.Sprintf("%s: server not ready: %v\n%s", sc.Name, err, log))
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func (r *runner) stop() {
	_ = r.cmd.Process.Signal(syscall.SIGTERM)
	done := make(chan struct{})
	go func() { _ = r.cmd.Wait(); close(done) }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		_ = r.cmd.Process.Kill()
		<-done
	}
	check(os.RemoveAll(r.work))
}

// raw sends one HTTP/1.1 request with exactly these header lines and reads the reply.
func (r *runner) raw(args httpArgs) (httpResult, error) {
	conn, err := net.DialTimeout("tcp", "127.0.0.1:"+r.port, 5*time.Second)
	if err != nil {
		return httpResult{}, err
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(30 * time.Second))
	body := r.placeholders(args.Body)
	var b strings.Builder
	fmt.Fprintf(&b, "%s %s HTTP/1.1\r\nHost: cpa.test\r\n", args.Method, r.placeholders(args.Path))
	for _, h := range args.Headers {
		fmt.Fprintf(&b, "%s: %s\r\n", h[0], r.placeholders(h[1]))
	}
	if body != "" || args.Method == "POST" {
		fmt.Fprintf(&b, "Content-Length: %d\r\n", len(body))
	}
	b.WriteString("Connection: close\r\n\r\n")
	b.WriteString(body)
	if _, err := io.WriteString(conn, b.String()); err != nil {
		return httpResult{}, err
	}
	resp, err := http.ReadResponse(bufio.NewReader(conn), nil)
	if err != nil {
		return httpResult{}, err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(resp.Body)
	if err != nil {
		return httpResult{}, err
	}
	headers := map[string]string{}
	for name, values := range resp.Header {
		switch name {
		case "Date", "Content-Length", "Connection", "Keep-Alive":
			continue
		}
		if name == "X-Cpa-Trace-Id" {
			values = []string{"SET"}
		}
		headers[name] = r.normalize(strings.Join(values, "\n"))
	}
	return httpResult{Status: resp.StatusCode, Headers: headers, Body: r.normalize(string(data))}, nil
}

var callbackID = regexp.MustCompile(`"host_callback_id":"[0-9]+"`)

// Per-request values: lifecycle request IDs, trace (request) IDs and timestamps.
// Compat credential IDs and indexes hash the upstream URL, whose port varies.
var varying = regexp.MustCompile(`"(RequestID|TraceID|StartedAt|CompletedAt|RequestedAt|selected_auth_id|selected_auth_index|AuthID|AuthIndex)":"[^"]+"`)

// Durations vary per run.
var durations = regexp.MustCompile(`"(Latency|TTFT)":[0-9]+`)

// Base64 bodies; model lists in them carry registration times.
var (
	bodyField = regexp.MustCompile(`"Body":"([A-Za-z0-9+/=]+)"`)
	created   = regexp.MustCompile(`"created":[0-9]+`)
)

func maskCreated(request string) string {
	return bodyField.ReplaceAllStringFunc(request, func(field string) string {
		raw, err := base64.StdEncoding.DecodeString(bodyField.FindStringSubmatch(field)[1])
		if err != nil {
			return field
		}
		return `"Body":"` + base64.StdEncoding.EncodeToString(created.ReplaceAll(raw, []byte(`"created":0`))) + `"`
	})
}

// records returns and clears the calls every recorder received whose method is listed
// (an entry ending in "." matches a prefix).
func (r *runner) records(args recordsArgs) []map[string]string {
	out := []map[string]string{}
	entries := must(os.ReadDir(r.dirs["RECDIR"]))
	names := []string{}
	for _, entry := range entries {
		if !entry.IsDir() {
			names = append(names, entry.Name())
		}
	}
	sort.Strings(names)
	for _, name := range names {
		path := filepath.Join(r.dirs["RECDIR"], name)
		data := must(os.ReadFile(path))
		check(os.Remove(path))
		for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n") {
			var rec map[string]string
			if json.Unmarshal([]byte(line), &rec) != nil || !matches(args.Methods, rec["method"]) {
				continue
			}
			request := callbackID.ReplaceAllString(r.normalize(rec["request"]), `"host_callback_id":"#"`)
			request = maskCreated(varying.ReplaceAllString(request, `"$1":"#"`))
			request = durations.ReplaceAllString(request, `"$1":0`)
			out = append(out, map[string]string{"label": strings.TrimSuffix(name, ".jsonl"), "method": rec["method"], "request": request})
		}
	}
	return out
}

func matches(methods []string, method string) bool {
	for _, m := range methods {
		if m == method || (strings.HasSuffix(m, ".") && strings.HasPrefix(method, m)) {
			return true
		}
	}
	return false
}

func (r *runner) upstreamRequests() []map[string]any {
	r.upstreamMu.Lock()
	defer r.upstreamMu.Unlock()
	seen := r.upstreamSeen
	r.upstreamSeen = nil
	if seen == nil {
		seen = []map[string]any{}
	}
	var out []map[string]any
	raw := must(json.Marshal(seen))
	check(json.Unmarshal([]byte(r.normalize(string(raw))), &out))
	return out
}

func (r *runner) run(sc *scenario) {
	r.start(sc)
	defer r.stop()
	for i := range sc.Steps {
		s := &sc.Steps[i]
		switch s.Op {
		case "respond":
			var rs respond
			remarshal(s.Args, &rs)
			r.writeRespond(rs)
		case "http":
			var args httpArgs
			remarshal(s.Args, &args)
			res, err := r.raw(args)
			check(err)
			s.Result = res
		case "records":
			var args recordsArgs
			remarshal(s.Args, &args)
			s.Result = r.records(args)
		case "upstream":
			s.Result = r.upstreamRequests()
		case "sleep":
			var ms int
			remarshal(s.Args, &ms)
			time.Sleep(time.Duration(ms) * time.Millisecond)
		default:
			panic("unknown op " + s.Op)
		}
	}
}

func remarshal(in, out any) {
	check(json.Unmarshal(must(json.Marshal(in)), out))
}

func must[T any](v T, err error) T {
	check(err)
	return v
}

func main() {
	if len(os.Args) != 4 {
		fmt.Fprintln(os.Stderr, "usage: go run . <server binary> <recorder.so> <output.json>")
		os.Exit(2)
	}
	r := &runner{server: os.Args[1], recorder: os.Args[2]}
	r.upstream = httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, req *http.Request) {
		body, _ := io.ReadAll(req.Body)
		header := req.Header.Clone()
		r.upstreamMu.Lock()
		r.upstreamSeen = append(r.upstreamSeen, map[string]any{"method": req.Method, "uri": req.RequestURI, "header": header, "body": string(body)})
		rt, ok := r.routes[req.URL.Path]
		if ok {
			n := r.served[req.URL.Path]
			r.served[req.URL.Path] = n + 1
			if n > 0 && len(rt.Then) > 0 {
				rt = rt.Then[min(n, len(rt.Then))-1]
			}
		}
		r.upstreamMu.Unlock()
		w.Header()["Date"] = nil
		if !ok {
			w.WriteHeader(http.StatusNotFound)
			return
		}
		for name, value := range rt.Headers {
			w.Header().Set(name, value)
		}
		if rt.Truncate {
			w.Header().Set("Content-Length", fmt.Sprint(len(rt.Body)+100))
		}
		w.WriteHeader(rt.Status)
		_, _ = io.WriteString(w, rt.Body)
	}))
	defer r.upstream.Close()
	all := scenarios()
	for i := range all {
		r.run(&all[i])
	}
	out := must(json.MarshalIndent(map[string]any{"scenarios": all}, "", "  "))
	check(os.WriteFile(os.Args[3], append(out, '\n'), 0o644))
}
