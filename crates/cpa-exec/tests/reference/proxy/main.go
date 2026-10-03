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
	IOError bool `json:"io_error"`
	// Tokens and Error: the reader returns EOF or the error together with the last
	// bytes, as net/http does for a Content-Length body and gzip.Reader does at its end.
	Tokens []string `json:"tokens"`
	Error  string   `json:"error"`
	// Separate*: the reader returns EOF or the error on a read of its own.
	SeparateTokens []string `json:"separate_tokens"`
	SeparateError  string   `json:"separate_error"`
}

// scripted serves each chunk across as many Reads as the caller's buffer needs, then
// ends with EOF or an I/O error: together with the last bytes when attach is set,
// otherwise on a Read of its own.
type scripted struct {
	chunks []string
	fail   bool
	attach bool
}

func (s *scripted) terminal() error {
	if s.fail {
		return errors.New("read failed")
	}
	return io.EOF
}

func (s *scripted) Read(p []byte) (int, error) {
	for len(s.chunks) > 0 && s.chunks[0] == "" {
		s.chunks = s.chunks[1:]
	}
	if len(s.chunks) == 0 {
		return 0, s.terminal()
	}
	n := copy(p, s.chunks[0])
	s.chunks[0] = s.chunks[0][n:]
	if s.attach && strings.Join(s.chunks, "") == "" {
		s.chunks = nil
		return n, s.terminal()
	}
	return n, nil
}

func scan(c lineCase, attach bool) ([]string, string) {
	reader := &scripted{chunks: append([]string(nil), c.Chunks...), fail: c.IOError, attach: attach}
	scanner := bufio.NewScanner(reader)
	scanner.Buffer(nil, c.Max)
	tokens := []string{}
	for scanner.Scan() {
		tokens = append(tokens, scanner.Text())
	}
	switch err := scanner.Err(); {
	case err == nil:
		return tokens, ""
	case errors.Is(err, bufio.ErrTooLong):
		return tokens, "too_long"
	default:
		return tokens, "io"
	}
}

func runLines(c lineCase) lineCase {
	c.Tokens, c.Error = scan(c, true)
	c.SeparateTokens, c.SeparateError = scan(c, false)
	return c
}

type step struct {
	Status   int    `json:"status"`
	Location string `json:"location"`
}

type hop struct {
	UserAgent      string `json:"user_agent"`
	HasUserAgent   bool   `json:"has_user_agent"`
	AcceptEncoding string `json:"accept_encoding"`
	Method         string `json:"method"`
	Path           string `json:"path"`
	Host           string `json:"host"`
	Referer        string `json:"referer"`
	Authorization  string `json:"authorization"`
	ContentType    string `json:"content_type"`
	ContentLength  string `json:"content_length"`
	Body           string `json:"body"`
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
	// Start is the first request's path (default /start), sent as written.
	Start string `json:"start"`
	// DisableCompression uses a transport with DisableCompression set.
	DisableCompression bool `json:"disable_compression"`
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
	_, hasUserAgent := req.Header["User-Agent"]
	r.hops = append(r.hops, hop{
		UserAgent:      req.Header.Get("User-Agent"),
		HasUserAgent:   hasUserAgent,
		AcceptEncoding: req.Header.Get("Accept-Encoding"),
		Method:         req.Method,
		Path:           req.RequestURI,
		Host:           req.Host,
		Referer:        req.Header.Get("Referer"),
		Authorization:  req.Header.Get("Authorization"),
		ContentType:    req.Header.Get("Content-Type"),
		ContentLength:  strings.Join(req.Header.Values("Content-Length"), ","),
		Body:           string(body),
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
	if c.Start == "" {
		c.Start = "/start"
	}
	req, _ := http.NewRequest(c.Method, base+c.Start, body)
	for k, v := range c.Headers {
		if k == "Host" {
			// Go reads a custom Host from req.Host only (GoHeaders' "Host").
			req.Host = v
			continue
		}
		req.Header.Set(k, v)
	}
	client := &http.Client{}
	if c.DisableCompression {
		client.Transport = &http.Transport{DisableCompression: true}
	}
	resp, err := client.Do(req)
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
		{Name: "exact-max-then-io-error", Max: 8, Chunks: []string{x(8)}, IOError: true},
		{Name: "exact-max-then-more", Max: 8, Chunks: []string{x(8), "y\n"}},
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
		{Name: "get-empty-user-agent", Method: "GET", Headers: map[string]string{"User-Agent": "", "Accept": "application/json"}},
		{Name: "get-custom-user-agent", Method: "GET", Headers: map[string]string{"User-Agent": "cli/1.0"}},
		{Name: "get-disable-compression", Method: "GET", Headers: auth, DisableCompression: true},
		{Name: "get-accept-encoding-explicit", Method: "GET", Headers: map[string]string{"Accept-Encoding": "br"}},
		{Name: "get-dot-segments-exact", Method: "GET", Headers: auth, Start: "/videos/..", Script: map[string]step{
			"/videos/..": {302, "./b/../c"}}},
		{Name: "get-dot-escaped-exact", Method: "GET", Headers: auth, Start: "/videos/%2E%2E/x?q=a%20b"},
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
