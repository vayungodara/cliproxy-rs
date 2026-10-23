// Generates goldens for crates/cpa-exec/src/proxy.rs with the Go standard library only:
// bufio.Scanner (ScanLines, Buffer(nil, max)) over scripted reads, http.Client
// redirect handling against a local scripted server, and the protocol the cloned
// default transport negotiates with local TLS and plain servers. Nothing leaves loopback.
//
// Run: go run main.go ../../fixtures/proxy_go.json
package main

import (
	"bufio"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
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
	// Script maps a request target, else a path, to its reply. In a Location, OTHER is
	// the second listener's URL (a different host name), OTHERHOST its host and port,
	// and MAINHOST the first listener's address.
	Script map[string]step `json:"script"`
	Hops   []hop           `json:"hops"`
	Status int             `json:"status"`
	Error  bool            `json:"error"`
	// Start is the first request's path (default /start), sent as written.
	Start string `json:"start"`
	// DisableCompression uses a transport with DisableCompression set.
	DisableCompression bool `json:"disable_compression"`
	// User is the first URL's user info, written before its host.
	User *string `json:"user,omitempty"`
	// Target replaces the first URL; MAINHOST stands for the first listener's address.
	Target string `json:"target,omitempty"`
	// DialMain dials the first listener whatever address a hop names, so a hop to
	// another port (an empty or default one) still reaches it.
	DialMain bool `json:"dial_main,omitempty"`
}

type recorder struct {
	mu     sync.Mutex
	hops   []hop
	script map[string]step
	other  *strings.Replacer
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
	// The exact request target first, so a bare `?` tells two hops apart.
	s, ok := r.script[req.RequestURI]
	if !ok {
		s, ok = r.script[req.URL.Path]
	}
	r.mu.Unlock()
	if !ok {
		w.WriteHeader(200)
		_, _ = w.Write([]byte("done"))
		return
	}
	if s.Location != "" {
		w.Header().Set("Location", r.other.Replace(s.Location))
	}
	w.WriteHeader(s.Status)
}

func runRedirect(c redirectCase) redirectCase {
	main, _ := net.Listen("tcp", "127.0.0.1:0")
	other, _ := net.Listen("tcp", "127.0.0.1:0")
	defer main.Close()
	defer other.Close()
	_, otherPort, _ := net.SplitHostPort(other.Addr().String())
	rec := &recorder{script: c.Script, other: strings.NewReplacer(
		"OTHERHOST", "localhost:"+otherPort, "MAINHOST", main.Addr().String(), "OTHER", "http://localhost:"+otherPort)}
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
	target := base + c.Start
	if c.User != nil {
		target = "http://" + *c.User + "@" + main.Addr().String() + c.Start
	}
	if c.Target != "" {
		target = strings.Replace(c.Target, "MAINHOST", main.Addr().String(), 1)
	}
	req, _ := http.NewRequest(c.Method, target, body)
	for k, v := range c.Headers {
		if k == "Host" {
			// Go reads a custom Host from req.Host only (GoHeaders' "Host").
			req.Host = v
			continue
		}
		req.Header.Set(k, v)
	}
	client := &http.Client{}
	if c.DisableCompression || c.DialMain {
		transport := &http.Transport{DisableCompression: c.DisableCompression}
		if c.DialMain {
			transport.DialContext = func(ctx context.Context, network, _ string) (net.Conn, error) {
				return (&net.Dialer{}).DialContext(ctx, network, main.Addr().String())
			}
		}
		client.Transport = transport
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

// protocolCase is one request through a clone of http.DefaultTransport (proxyutil's
// cloneDefaultTransport, ForceAttemptHTTP2 kept) to a local server.
type protocolCase struct {
	Name string `json:"name"`
	// Server: "h2" (TLS offering h2 then http/1.1), "h1" (TLS offering http/1.1 only)
	// or "plain" (no TLS).
	Server string `json:"server"`
	// Proxy: "" (direct) or "http" (an HTTP CONNECT proxy, proxyutil's ModeProxy).
	Proxy string `json:"proxy"`
	// What the server saw: the request protocol, the client's ALPN offer and the
	// transport's default User-Agent.
	Proto      string   `json:"proto"`
	ClientALPN []string `json:"client_alpn"`
	UserAgent  string   `json:"user_agent"`
}

// connectProxy tunnels every CONNECT to target.
func connectProxy(target string) net.Listener {
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	go func() {
		for {
			conn, err := ln.Accept()
			if err != nil {
				return
			}
			go func(conn net.Conn) {
				defer conn.Close()
				reader := bufio.NewReader(conn)
				req, err := http.ReadRequest(reader)
				if err != nil || req.Method != http.MethodConnect {
					return
				}
				upstream, err := net.Dial("tcp", target)
				if err != nil {
					return
				}
				defer upstream.Close()
				_, _ = conn.Write([]byte("HTTP/1.1 200 Connection established\r\n\r\n"))
				go func() { _, _ = io.Copy(upstream, reader) }()
				_, _ = io.Copy(conn, upstream)
			}(conn)
		}
	}()
	return ln
}

func runProtocol(c protocolCase) protocolCase {
	var mu sync.Mutex
	server := httptest.NewUnstartedServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		c.Proto, c.UserAgent = r.Proto, r.Header.Get("User-Agent")
		mu.Unlock()
		_, _ = w.Write([]byte("ok"))
	}))
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.Proxy = nil
	if c.Server == "plain" {
		server.Start()
	} else {
		protos := []string{"http/1.1"}
		if c.Server == "h2" {
			protos = []string{"h2", "http/1.1"}
		}
		server.TLS = &tls.Config{NextProtos: protos, GetConfigForClient: func(hello *tls.ClientHelloInfo) (*tls.Config, error) {
			mu.Lock()
			c.ClientALPN = append([]string{}, hello.SupportedProtos...)
			mu.Unlock()
			return nil, nil
		}}
		server.StartTLS()
		// Only the trust store differs from production (system roots there).
		pool := x509.NewCertPool()
		pool.AddCert(server.Certificate())
		transport.TLSClientConfig = &tls.Config{RootCAs: pool}
	}
	defer server.Close()
	if c.Proxy == "http" {
		proxy := connectProxy(server.Listener.Addr().String())
		defer proxy.Close()
		proxyURL, _ := url.Parse("http://" + proxy.Addr().String())
		transport.Proxy = http.ProxyURL(proxyURL)
	}
	resp, err := (&http.Client{Transport: transport}).Get(server.URL + "/")
	if err != nil {
		panic(err)
	}
	_, _ = io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	transport.CloseIdleConnections()
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
	accept := map[string]string{"Accept": "application/json"}
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
		// The merged path stays a path, never a network-path reference to host "api".
		{Name: "get-double-slash-path-exact", Method: "GET", Headers: auth, Start: "//api/item", Script: map[string]step{
			"//api/item": {307, "next"}}},
		// The authority ends at `?`: the base path is empty, and a slash in the query is
		// not a directory.
		{Name: "get-query-only-start-exact", Method: "GET", Headers: auth, Start: "?q=/x/item", Script: map[string]step{
			"/": {307, "next"}}},
		// A query-only reference keeps the base path, dot segments removed; a
		// fragment-only one keeps the base query too.
		{Name: "get-query-reference-exact", Method: "GET", Headers: auth, Start: "/a/./b?k=v", Script: map[string]step{
			"/a/./b": {302, "?z=1"}}},
		{Name: "get-fragment-reference-exact", Method: "GET", Headers: auth, Start: "/a/./b?k=v", Script: map[string]step{
			"/a/./b": {302, "#frag"}}},
		// Every hop resolves and escapes its target as Go does (the model catalog reads).
		{Name: "get-invalid-escape-location-exact", Method: "GET", Headers: auth, Start: "/r1", Script: map[string]step{
			"/r1": {302, "/x/%zz/c.json"}}},
		{Name: "get-invalid-host-escape-location-exact", Method: "GET", Headers: auth, Start: "/r2", Script: map[string]step{
			"/r2": {302, "//h%41/c.json"}}},
		{Name: "get-opaque-location-exact", Method: "GET", Headers: auth, Start: "/r3", Script: map[string]step{
			"/r3": {302, "http:c.json"}}},
		{Name: "get-scheme-relative-path-location-exact", Method: "GET", Headers: auth, Start: "/r4", Script: map[string]step{
			"/r4": {302, "http:/c.json"}}},
		{Name: "get-scheme-without-host-location-exact", Method: "GET", Headers: auth, Start: "/r5", Script: map[string]step{
			"/r5": {302, "https:/evil.example/c.json"}}},
		{Name: "get-encoded-dots-relative-exact", Method: "GET", Headers: auth, Start: "/a/%2e%2e/r", Script: map[string]step{
			"/a/../r": {302, "d.json"}}},
		{Name: "get-encoded-dots-second-hop-exact", Method: "GET", Headers: auth, Start: "/m1", Script: map[string]step{
			"/m1": {302, "/m2/%2E%2e/x"}, "/m2/../x": {302, "y?a='1'#f"}}},
		{Name: "get-apostrophe-query-exact", Method: "GET", Headers: auth, Start: "/q", Script: map[string]step{
			"/q": {302, "/c.json?v='1'&w=a"}}},
		{Name: "get-fragment-keeps-quoted-query-exact", Method: "GET", Headers: auth, Start: "/f?k='v'#src", Script: map[string]step{
			"/f": {302, "#frag"}}},
		{Name: "get-triple-slash-location-exact", Method: "GET", Headers: auth, Start: "/t", Script: map[string]step{
			"/t": {302, "///x/c.json"}}},
		{Name: "get-backslash-space-location-exact", Method: "GET", Headers: auth, Start: "/b", Script: map[string]step{
			"/b": {302, "/a\\..\\my dir/m|1.json"}}},
		{Name: "get-utf8-location-exact", Method: "GET", Headers: auth, Start: "/u", Script: map[string]step{
			"/u": {302, "/mod\u00e8les/c.json"}}},
		// Only the base's RawQuery is inherited: a bare `?` (ForceQuery) is not.
		{Name: "get-force-query-fragment-exact", Method: "GET", Headers: auth, Start: "/a/b?", Script: map[string]step{
			"/a/b?": {302, "#f"}}},
		{Name: "get-force-query-bare-fragment-exact", Method: "GET", Headers: auth, Start: "/a/b?", Script: map[string]step{
			"/a/b?": {302, "#"}}},
		// User info is never sent in the URL: each hop whose URL carries it (a relative
		// Location inherits it) sends it unescaped as Basic auth, unless the request
		// already has an Authorization header.
		{Name: "get-user-info-hops-exact", Method: "GET", Headers: accept, User: str("u:p"), Start: "/u1", Script: map[string]step{
			"/u1": {302, "/u2"}, "/u2": {302, "OTHER/u3"}}},
		{Name: "get-user-info-empty-name-exact", Method: "GET", Headers: accept, User: str(":tok"), Start: "/u1"},
		{Name: "get-user-info-empty-exact", Method: "GET", Headers: accept, User: str(""), Start: "/u1"},
		{Name: "get-user-info-not-utf8-exact", Method: "GET", Headers: accept, User: str("u%FF:p"), Start: "/u1"},
		{Name: "get-user-info-escaped-exact", Method: "GET", Headers: accept, User: str("a%40b:p%3Aq+r"), Start: "/u1"},
		{Name: "get-user-info-with-authorization-exact", Method: "GET", Headers: auth, User: str("u:p"), Start: "/u1"},
		// A bare `?` reference takes ForceQuery from itself and drops the base query.
		{Name: "get-bare-query-reference-exact", Method: "GET", Headers: auth, Start: "/q1?x=1", Script: map[string]step{
			"/q1?x=1": {302, "?"}}},
		// A Location's own user info is Basic auth on another host too; on the same host
		// an Authorization header the request carries wins over it.
		{Name: "get-user-info-cross-host-location-exact", Method: "GET", Headers: accept, User: str("u:p"), Start: "/x1", Script: map[string]step{
			"/x1": {302, "http://w:z@OTHERHOST/x2"}}},
		{Name: "get-user-info-same-host-location-bearer-exact", Method: "GET", Headers: auth, User: str("u:p"), Start: "/y1", Script: map[string]step{
			"/y1": {302, "http://v:q@MAINHOST/y2"}}},
		// Go writes the Host header with the port as written: an empty one on a redirect
		// (the first hop drops it), or the scheme's default. cliproxy-rs refuses these.
		{Name: "get-empty-then-default-port-location-exact", Method: "GET", Headers: auth, Start: "/p1", DialMain: true, Script: map[string]step{
			"/p1": {302, "http://127.0.0.1:/p2"}, "/p2": {302, "http://127.0.0.1:80/p3"}}},
		{Name: "get-default-port-source-exact", Method: "GET", Headers: auth, Start: "/p3", Target: "http://127.0.0.1:80/p3", DialMain: true},
		{Name: "get-empty-port-source-exact", Method: "GET", Headers: auth, Start: "/p4", Target: "http://127.0.0.1:/p4", DialMain: true, Script: map[string]step{
			"/p4": {302, "/p5"}}},
		{Name: "put-empty-body", Method: "PUT", Body: str(""), Headers: auth},
		{Name: "delete-nil-body", Method: "DELETE", Headers: auth},
		{Name: "patch-empty-body-307", Method: "PATCH", Body: str(""), Headers: form, Script: map[string]step{
			"/start": {307, "/b"}}},
	}
	for i := range redirects {
		redirects[i] = runRedirect(redirects[i])
	}
	protocols := []protocolCase{
		{Name: "tls-offering-h2", Server: "h2"},
		{Name: "tls-http1-only", Server: "h1"},
		{Name: "plain-http", Server: "plain"},
		{Name: "tls-offering-h2-via-connect-proxy", Server: "h2", Proxy: "http"},
		{Name: "tls-http1-only-via-connect-proxy", Server: "h1", Proxy: "http"},
	}
	for i := range protocols {
		protocols[i] = runProtocol(protocols[i])
	}
	out, _ := json.MarshalIndent(map[string]any{
		"source":    runtime.Version() + " standard library",
		"lines":     lines,
		"redirects": redirects,
		"protocols": protocols,
	}, "", " ")
	if err := os.WriteFile(os.Args[1], append(out, '\n'), 0o644); err != nil {
		panic(err)
	}
}
