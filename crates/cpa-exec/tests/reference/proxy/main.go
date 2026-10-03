// Generates goldens for crates/cpa-exec/src/proxy.rs with the Go standard library only:
// bufio.Scanner (ScanLines, Buffer(nil, max)) over scripted reads, and http.Client
// redirect handling against a local scripted server. Nothing leaves loopback.
//
// Run: go run main.go ../../fixtures/proxy_go.json
package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"os"
	"runtime"
	"strings"
	"sync"
)

type lineCase struct {
	Name   string   `json:"name"`
	Max    int      `json:"max"`
	Chunks []string `json:"chunks"`
	// IOError ends the reads with an I/O error instead of io.EOF.
	IOError bool     `json:"io_error"`
	Tokens  []string `json:"tokens"`
	Error   string   `json:"error"`
}

// scripted serves each chunk across as many Reads as the caller's buffer needs, then
// returns (0, EOF) or (0, error) on its own Read, as a network body does.
type scripted struct {
	chunks []string
	fail   bool
}

func (s *scripted) Read(p []byte) (int, error) {
	for len(s.chunks) > 0 && s.chunks[0] == "" {
		s.chunks = s.chunks[1:]
	}
	if len(s.chunks) == 0 {
		if s.fail {
			return 0, errors.New("read failed")
		}
		return 0, io.EOF
	}
	n := copy(p, s.chunks[0])
	s.chunks[0] = s.chunks[0][n:]
	return n, nil
}

func runLines(c lineCase) lineCase {
	scanner := bufio.NewScanner(&scripted{chunks: append([]string(nil), c.Chunks...), fail: c.IOError})
	scanner.Buffer(nil, c.Max)
	c.Tokens = []string{}
	for scanner.Scan() {
		c.Tokens = append(c.Tokens, scanner.Text())
	}
	switch err := scanner.Err(); {
	case err == nil:
	case errors.Is(err, bufio.ErrTooLong):
		c.Error = "too_long"
	default:
		c.Error = "io"
	}
	return c
}

type step struct {
	Status   int    `json:"status"`
	Location string `json:"location"`
}

type hop struct {
	Method        string `json:"method"`
	Path          string `json:"path"`
	Host          string `json:"host"`
	Referer       string `json:"referer"`
	Authorization string `json:"authorization"`
	ContentType   string `json:"content_type"`
	ContentLength string `json:"content_length"`
	Body          string `json:"body"`
}

type redirectCase struct {
	Name    string            `json:"name"`
	Method  string            `json:"method"`
	Body    *string           `json:"body"`
	Headers map[string]string `json:"headers"`
	// Script maps a path to its reply; a Location starting with OTHER points at the
	// second listener (a different host name).
	Script map[string]step `json:"script"`
	Hops   []hop           `json:"hops"`
	Status int             `json:"status"`
	Error  bool            `json:"error"`
}

type recorder struct {
	mu     sync.Mutex
	hops   []hop
	script map[string]step
	other  string
}

func (r *recorder) ServeHTTP(w http.ResponseWriter, req *http.Request) {
	body, _ := io.ReadAll(req.Body)
	r.mu.Lock()
	r.hops = append(r.hops, hop{
		Method:        req.Method,
		Path:          req.URL.Path,
		Host:          req.Host,
		Referer:       req.Header.Get("Referer"),
		Authorization: req.Header.Get("Authorization"),
		ContentType:   req.Header.Get("Content-Type"),
		ContentLength: strings.Join(req.Header.Values("Content-Length"), ","),
		Body:          string(body),
	})
	s, ok := r.script[req.URL.Path]
	r.mu.Unlock()
	if !ok {
		w.WriteHeader(200)
		_, _ = w.Write([]byte("done"))
		return
	}
	if s.Location != "" {
		w.Header().Set("Location", strings.Replace(s.Location, "OTHER", r.other, 1))
	}
	w.WriteHeader(s.Status)
}

func runRedirect(c redirectCase) redirectCase {
	main, _ := net.Listen("tcp", "127.0.0.1:0")
	other, _ := net.Listen("tcp", "127.0.0.1:0")
	defer main.Close()
	defer other.Close()
	_, otherPort, _ := net.SplitHostPort(other.Addr().String())
	rec := &recorder{script: c.Script, other: "http://localhost:" + otherPort}
	go http.Serve(main, rec)
	go http.Serve(other, rec)
	base := "http://" + main.Addr().String()
	var body io.Reader
	if c.Body != nil {
		body = strings.NewReader(*c.Body)
	}
	req, _ := http.NewRequest(c.Method, base+"/start", body)
	for k, v := range c.Headers {
		if k == "Host" {
			// Go reads a custom Host from req.Host only (GoHeaders' "Host").
			req.Host = v
			continue
		}
		req.Header.Set(k, v)
	}
	resp, err := (&http.Client{}).Do(req)
	if err != nil {
		c.Error = true
	} else {
		c.Status = resp.StatusCode
		_, _ = io.Copy(io.Discard, resp.Body)
		resp.Body.Close()
	}
	c.Hops = rec.hops
	for i := range c.Hops {
		h := &c.Hops[i]
		h.Referer = strings.Replace(h.Referer, base, "BASE", 1)
		h.Host = strings.NewReplacer(main.Addr().String(), "BASE", "localhost:"+otherPort, "OTHER").Replace(h.Host)
	}
	return c
}

func main() {
	x := func(n int) string { return strings.Repeat("x", n) }
	lines := []lineCase{
		{Name: "cr-lf-mix", Max: 64, Chunks: []string{"data: a\r\rdata: b", "\r\n\nfinal", ""}},
		{Name: "partial-then-io-error", Max: 64, Chunks: []string{"data: a\ndata: part"}, IOError: true},
		{Name: "io-error-without-data", Max: 64, IOError: true},
		{Name: "io-error-after-newline", Max: 64, Chunks: []string{"a\n"}, IOError: true},
		{Name: "max-plus-newline-one-chunk", Max: 64, Chunks: []string{x(64) + "\n"}},
		{Name: "max-minus-one-plus-newline", Max: 64, Chunks: []string{x(63) + "\n"}},
		{Name: "short-then-long-one-chunk", Max: 64, Chunks: []string{"a\n" + x(64) + "\nb\n"}},
		{Name: "exact-max-unterminated", Max: 64, Chunks: []string{x(64)}},
		{Name: "max-minus-one-unterminated", Max: 64, Chunks: []string{x(63)}},
		{Name: "split-long-line", Max: 64, Chunks: []string{x(40), x(24), "\n"}},
		{Name: "split-fitting-line", Max: 64, Chunks: []string{x(40), x(23), "\n"}},
		{Name: "many-lines-small-window", Max: 5, Chunks: []string{"aaa\nbbb\nccc\nd", "d\n"}},
		{Name: "empty-lines", Max: 8, Chunks: []string{"\n\n\r\n"}},
		{Name: "too-long-before-io-error", Max: 8, Chunks: []string{x(9)}, IOError: true},
	}
	for i := range lines {
		lines[i] = runLines(lines[i])
	}
	str := func(s string) *string { return &s }
	auth := map[string]string{"Authorization": "Bearer fake-token", "Accept": "application/json"}
	form := map[string]string{"Authorization": "Bearer fake-token", "Content-Type": "application/x-www-form-urlencoded"}
	redirects := []redirectCase{
		{Name: "get-301-302-303", Method: "GET", Headers: auth, Script: map[string]step{
			"/start": {301, "/b"}, "/b": {302, "/c"}, "/c": {303, "/d"}}},
		{Name: "get-307-308", Method: "GET", Headers: auth, Script: map[string]step{
			"/start": {307, "/b"}, "/b": {308, "/c"}}},
		{Name: "get-cross-host-strips-credentials", Method: "GET", Headers: auth, Script: map[string]step{
			"/start": {302, "OTHER/b"}}},
		{Name: "get-custom-host-relative-then-absolute", Method: "GET",
			Headers: map[string]string{"Host": "custom.example", "Accept": "application/json"},
			Script:  map[string]step{"/start": {302, "/b"}, "/b": {302, "OTHER/c"}}},
		{Name: "get-no-location", Method: "GET", Headers: auth, Script: map[string]step{"/start": {302, ""}}},
		{Name: "get-ten-redirects", Method: "GET", Headers: auth, Script: map[string]step{
			"/start": {302, "/1"}, "/1": {302, "/2"}, "/2": {302, "/3"}, "/3": {302, "/4"}, "/4": {302, "/5"},
			"/5": {302, "/6"}, "/6": {302, "/7"}, "/7": {302, "/8"}, "/8": {302, "/9"}, "/9": {302, "/10"}}},
		{Name: "post-303-becomes-get", Method: "POST", Body: str("a=1"), Headers: form, Script: map[string]step{
			"/start": {303, "/b"}}},
		{Name: "post-307-resends-body", Method: "POST", Body: str("a=1"), Headers: form, Script: map[string]step{
			"/start": {307, "/b"}}},
		{Name: "post-empty-body-302", Method: "POST", Body: str(""), Headers: form, Script: map[string]step{
			"/start": {302, "/b"}}},
		{Name: "post-nil-body", Method: "POST", Headers: auth},
		{Name: "put-empty-body", Method: "PUT", Body: str(""), Headers: auth},
		{Name: "delete-nil-body", Method: "DELETE", Headers: auth},
		{Name: "patch-empty-body-307", Method: "PATCH", Body: str(""), Headers: form, Script: map[string]step{
			"/start": {307, "/b"}}},
	}
	for i := range redirects {
		redirects[i] = runRedirect(redirects[i])
	}
	out, _ := json.MarshalIndent(map[string]any{
		"source":    runtime.Version() + " standard library",
		"lines":     lines,
		"redirects": redirects,
	}, "", " ")
	if err := os.WriteFile(os.Args[1], append(out, '\n'), 0o644); err != nil {
		panic(err)
	}
}
