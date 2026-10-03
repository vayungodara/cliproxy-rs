// Generates route-level goldens for the image and video endpoints by running the
// unmodified Go server binary (cmd/server at 6fecc6e) against a scripted local capture
// server. Run it inside a loopback-only network namespace; it never contacts a provider
// and every key is fake.
//
// Usage: media <go-server-binary> <output.json>
package main

import (
	"bufio"
	"bytes"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

type upstream struct {
	DelayMs int         `json:"delay_ms,omitempty"`
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body"`
}

type scenario struct {
	Name        string            `json:"name"`
	Method      string            `json:"method"`
	Path        string            `json:"path"`
	ContentType string            `json:"content_type,omitempty"`
	Body        string            `json:"body,omitempty"`
	BodyB64     string            `json:"body_b64,omitempty"`
	Headers     map[string]string `json:"headers,omitempty"`
	Upstreams   []upstream        `json:"upstreams,omitempty"`

	Status         int               `json:"status"`
	ResponseHeader map[string]string `json:"response_headers"`
	Response       string            `json:"response,omitempty"`
	ResponseB64    string            `json:"response_b64,omitempty"`
	Requests       []string          `json:"requests"`
}

// Config is shared by every scenario. UPSTREAM is the capture server, PORT the Go
// server's port and AUTHDIR its auth directory; the Rust test fills them in the same way.
const Config = `config-version: 8
server:
  host: 127.0.0.1
  port: PORT
management:
  disable-control-panel: true
requests:
  nonstream-keepalive-interval: 1
  streaming:
    keepalive-seconds: 1
access:
  api-keys: [client-key-1]
oauth:
  auth-dir: AUTHDIR
api-keys:
  xai:
    - name: xai-1
      base-url: http://UPSTREAM/v1
      keys:
        - api-key: sk-fake-xai-a
        - api-key: sk-fake-xai-b
  openai-compatibility:
    - name: Acme
      base-url: http://UPSTREAM/v1
      models:
        - name: acme-image
          alias: img
          image: true
        - name: acme-chat
          alias: chat
      keys:
        - api-key: sk-fake-acme
`

// capture answers each connection with the next scripted reply and records the raw
// request. Requests arrive one at a time.
type capture struct {
	mu       sync.Mutex
	replies  []upstream
	requests []string
	addr     string
}

func (c *capture) serve(ln net.Listener) {
	for {
		conn, err := ln.Accept()
		if err != nil {
			return
		}
		c.handle(conn)
	}
}

func (c *capture) handle(conn net.Conn) {
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	reader := bufio.NewReader(conn)
	var raw bytes.Buffer
	length := 0
	for {
		line, err := reader.ReadString('\n')
		raw.WriteString(line)
		if err != nil {
			return
		}
		if strings.HasPrefix(strings.ToLower(line), "content-length:") {
			length, _ = strconv.Atoi(strings.TrimSpace(line[len("content-length:"):]))
		}
		if line == "\r\n" {
			break
		}
	}
	body := make([]byte, length)
	_, _ = io.ReadFull(reader, body)
	raw.Write(body)
	c.mu.Lock()
	c.requests = append(c.requests, raw.String())
	reply := upstream{Status: 599, Body: "no scripted reply"}
	if len(c.replies) > 0 {
		reply, c.replies = c.replies[0], c.replies[1:]
	}
	c.mu.Unlock()
	time.Sleep(time.Duration(reply.DelayMs) * time.Millisecond)
	payload := strings.ReplaceAll(reply.Body, "UPSTREAM", c.addr)
	var out bytes.Buffer
	fmt.Fprintf(&out, "HTTP/1.1 %d %s\r\n", reply.Status, http.StatusText(reply.Status))
	for _, h := range reply.Headers {
		fmt.Fprintf(&out, "%s: %s\r\n", h[0], h[1])
	}
	fmt.Fprintf(&out, "Content-Length: %d\r\nConnection: close\r\n\r\n%s", len(payload), payload)
	_, _ = conn.Write(out.Bytes())
}

var (
	boundaryRe  = regexp.MustCompile(`boundary=([0-9a-f]{60})`)
	createdRe   = regexp.MustCompile(`"(created_at|created)":(1[7-9]\d{8})`)
	videoIDRe   = regexp.MustCompile(`"video_[0-9a-f]{32}"`)
	xaiKeyRe    = regexp.MustCompile(`sk-fake-xai-[a-z]`)
	keyAliases  = map[string]string{}
	keepHeaders = []string{"Content-Type", "Cache-Control", "Content-Disposition", "Content-Length", "Etag", "Last-Modified"}
)

// normalize masks what Go varies per run: the capture address, the multipart writer's
// random boundary, clock-derived timestamps and generated video IDs. The xAI keys become
// XAI-KEY-<n> in order of first use: credential IDs hash the base URL, which holds the
// capture server's random port, so which key sorts first changes between runs.
func normalize(s, addr string) string {
	s = strings.ReplaceAll(s, addr, "UPSTREAM")
	s = xaiKeyRe.ReplaceAllStringFunc(s, func(key string) string {
		if _, ok := keyAliases[key]; !ok {
			keyAliases[key] = fmt.Sprintf("XAI-KEY-%d", len(keyAliases)+1)
		}
		return keyAliases[key]
	})
	if m := boundaryRe.FindStringSubmatch(s); m != nil {
		s = canonicalForm(strings.ReplaceAll(s, m[1], "BOUNDARY"))
	}
	s = createdRe.ReplaceAllStringFunc(s, func(m string) string {
		parts := createdRe.FindStringSubmatch(m)
		if n, _ := strconv.ParseInt(parts[2], 10, 64); n >= 1750000000 {
			return `"` + parts[1] + `":"<now>"`
		}
		return m
	})
	return videoIDRe.ReplaceAllString(s, `"video_<id>"`)
}

// canonicalForm sorts the parts of a rebuilt multipart body within each group Go writes
// in map order (buildOpenAICompatImagesMultipartRequest ranges over form.Value, then
// form.File): model and stream lead, then the values, then the files.
func canonicalForm(s string) string {
	const sep = "--BOUNDARY"
	i := strings.Index(s, "\r\n\r\n")
	if i < 0 || !strings.HasPrefix(s[i+4:], sep+"\r\n") {
		return s
	}
	head, pieces := s[:i+4], strings.Split(s[i+4:], sep)
	if len(pieces) < 3 {
		return s
	}
	var lead, values, files []string
	for _, p := range pieces[1 : len(pieces)-1] {
		switch {
		case strings.Contains(p, `name="model"`+"\r\n"), strings.Contains(p, `name="stream"`+"\r\n"):
			lead = append(lead, p)
		case strings.Contains(p, "filename="):
			files = append(files, p)
		default:
			values = append(values, p)
		}
	}
	sort.Strings(values)
	sort.Strings(files)
	parts := append(append(lead, values...), files...)
	return head + pieces[0] + sep + strings.Join(parts, sep) + sep + pieces[len(pieces)-1]
}

func freePort() int {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer ln.Close()
	return ln.Addr().(*net.TCPAddr).Port
}

func main() {
	if len(os.Args) != 3 {
		fmt.Fprintln(os.Stderr, "usage: media <go-server-binary> <output.json>")
		os.Exit(2)
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer ln.Close()
	cap := &capture{addr: ln.Addr().String()}
	go cap.serve(ln)

	dir, err := os.MkdirTemp("", "cpa-media-")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(dir)
	authDir := filepath.Join(dir, "auths")
	if err := os.MkdirAll(authDir, 0o700); err != nil {
		panic(err)
	}
	port := freePort()
	cfg := strings.NewReplacer("UPSTREAM", cap.addr, "PORT", strconv.Itoa(port), "AUTHDIR", authDir).Replace(Config)
	cfgPath := filepath.Join(dir, "config.yaml")
	if err := os.WriteFile(cfgPath, []byte(cfg), 0o600); err != nil {
		panic(err)
	}
	server := exec.Command(os.Args[1], "-config", cfgPath, "-local-model")
	server.Dir = dir
	var logs bytes.Buffer
	server.Stdout, server.Stderr = &logs, &logs
	if err := server.Start(); err != nil {
		panic(err)
	}
	defer func() { _ = server.Process.Kill(); _, _ = server.Process.Wait() }()
	base := fmt.Sprintf("http://127.0.0.1:%d", port)
	client := &http.Client{Timeout: 20 * time.Second, Transport: &http.Transport{DisableCompression: true}}
	ready := false
	for i := 0; i < 200 && !ready; i++ {
		if resp, errGet := client.Get(base + "/healthz"); errGet == nil {
			resp.Body.Close()
			ready = resp.StatusCode == 200
		}
		if !ready {
			time.Sleep(50 * time.Millisecond)
		}
	}
	if !ready {
		panic("go server did not start:\n" + logs.String())
	}

	all := scenarios()
	for i := range all {
		s := &all[i]
		cap.mu.Lock()
		cap.replies = append([]upstream(nil), s.Upstreams...)
		cap.requests = nil
		cap.mu.Unlock()
		req, errReq := http.NewRequest(s.Method, base+s.Path, strings.NewReader(s.Body))
		if !utf8.ValidString(s.Body) {
			s.BodyB64, s.Body = base64.StdEncoding.EncodeToString([]byte(s.Body)), ""
		}
		if errReq != nil {
			panic(errReq)
		}
		req.Header.Set("Authorization", "Bearer client-key-1")
		req.Header.Set("User-Agent", "media-golden/1")
		if s.ContentType != "" {
			req.Header.Set("Content-Type", s.ContentType)
		}
		for k, v := range s.Headers {
			req.Header.Set(k, v)
		}
		resp, errDo := client.Do(req)
		if errDo != nil {
			panic(fmt.Sprintf("%s: %v", s.Name, errDo))
		}
		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		s.Status = resp.StatusCode
		s.ResponseHeader = map[string]string{}
		for _, h := range keepHeaders {
			if v := resp.Header.Get(h); v != "" {
				s.ResponseHeader[h] = v
			}
		}
		if text := normalize(string(body), cap.addr); utf8.ValidString(text) {
			s.Response = text
		} else {
			s.ResponseB64 = base64.StdEncoding.EncodeToString(body)
		}
		time.Sleep(20 * time.Millisecond)
		cap.mu.Lock()
		for _, r := range cap.requests {
			s.Requests = append(s.Requests, normalize(r, cap.addr))
		}
		cap.mu.Unlock()
		if s.Requests == nil {
			s.Requests = []string{}
		}
	}
	data, err := json.MarshalIndent(map[string]any{"config": Config, "scenarios": all}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[2], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
