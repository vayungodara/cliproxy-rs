package executor

// Fixture generator for cliproxy-rs device providers (Kimi, Meta, Devin).
// Runs the real Go executors against a raw HTTP/1.1 capture server and records the
// exact upstream request (ordered header lines, body) and the downstream result.
// Enable with RSFIX_OUT=<dir>; otherwise every generator test is skipped.

import (
	"bufio"
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
)

type rsfixResponse struct {
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers"`
	Body    string      `json:"body"`
	// BodyB64 carries binary bodies (Connect frames).
	BodyB64 string `json:"body_b64,omitempty"`
}

type rsfixCaptured struct {
	Method  string      `json:"method"`
	Target  string      `json:"target"`
	Headers [][2]string `json:"headers"`
	Body    string      `json:"body"`
	BodyB64 string      `json:"body_b64,omitempty"`
}

type rsfixServer struct {
	t         *testing.T
	ln        net.Listener
	mu        sync.Mutex
	responses []rsfixResponse
	captured  []rsfixCaptured
	binary    bool
}

func newRSFixServer(t *testing.T, binary bool, responses ...rsfixResponse) *rsfixServer {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	s := &rsfixServer{t: t, ln: ln, responses: responses, binary: binary}
	go s.serve()
	t.Cleanup(func() { _ = ln.Close() })
	return s
}

func (s *rsfixServer) URL() string { return "http://" + s.ln.Addr().String() }

func (s *rsfixServer) serve() {
	for {
		conn, err := s.ln.Accept()
		if err != nil {
			return
		}
		go s.handle(conn)
	}
}

func (s *rsfixServer) handle(conn net.Conn) {
	defer func() { _ = conn.Close() }()
	reader := bufio.NewReader(conn)
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return
		}
		parts := strings.SplitN(strings.TrimRight(line, "\r\n"), " ", 3)
		if len(parts) < 2 {
			return
		}
		capture := rsfixCaptured{Method: parts[0], Target: parts[1]}
		contentLength := -1
		chunked := false
		for {
			headerLine, errHeader := reader.ReadString('\n')
			if errHeader != nil {
				return
			}
			headerLine = strings.TrimRight(headerLine, "\r\n")
			if headerLine == "" {
				break
			}
			name, value, _ := strings.Cut(headerLine, ":")
			value = strings.TrimSpace(value)
			capture.Headers = append(capture.Headers, [2]string{name, value})
			switch strings.ToLower(name) {
			case "content-length":
				contentLength, _ = strconv.Atoi(value)
			case "transfer-encoding":
				chunked = strings.EqualFold(value, "chunked")
			}
		}
		var body []byte
		if chunked {
			for {
				sizeLine, errSize := reader.ReadString('\n')
				if errSize != nil {
					return
				}
				size, _ := strconv.ParseInt(strings.TrimSpace(strings.Split(sizeLine, ";")[0]), 16, 64)
				if size == 0 {
					_, _ = reader.ReadString('\n')
					break
				}
				chunk := make([]byte, size)
				if _, errRead := io.ReadFull(reader, chunk); errRead != nil {
					return
				}
				body = append(body, chunk...)
				_, _ = reader.ReadString('\n')
			}
		} else if contentLength > 0 {
			body = make([]byte, contentLength)
			if _, errRead := io.ReadFull(reader, body); errRead != nil {
				return
			}
		}
		if s.binary {
			capture.BodyB64 = b64(body)
		} else {
			capture.Body = string(body)
		}
		s.mu.Lock()
		s.captured = append(s.captured, capture)
		var resp rsfixResponse
		if len(s.responses) > 0 {
			resp = s.responses[0]
			s.responses = s.responses[1:]
		} else {
			resp = rsfixResponse{Status: 500, Body: "no scripted response"}
		}
		s.mu.Unlock()
		respBody := []byte(resp.Body)
		if resp.BodyB64 != "" {
			respBody = unb64(resp.BodyB64)
		}
		var out bytes.Buffer
		fmt.Fprintf(&out, "HTTP/1.1 %d %s\r\n", resp.Status, http.StatusText(resp.Status))
		for _, h := range resp.Headers {
			fmt.Fprintf(&out, "%s: %s\r\n", h[0], h[1])
		}
		fmt.Fprintf(&out, "Content-Length: %d\r\n\r\n", len(respBody))
		out.Write(respBody)
		if _, errWrite := conn.Write(out.Bytes()); errWrite != nil {
			return
		}
	}
}

func (s *rsfixServer) Captured() []rsfixCaptured {
	time.Sleep(20 * time.Millisecond)
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]rsfixCaptured(nil), s.captured...)
}

type rsfixDownstream struct {
	Body      string      `json:"body,omitempty"`
	Chunks    []string    `json:"chunks,omitempty"`
	Headers   [][2]string `json:"headers,omitempty"`
	ErrStatus int         `json:"err_status,omitempty"`
	ErrBody   string      `json:"err_body,omitempty"`
	StreamErr string      `json:"stream_err,omitempty"`
}

type rsfixFixture struct {
	Name       string            `json:"name"`
	Credential map[string]any    `json:"credential"`
	Attributes map[string]string `json:"attributes,omitempty"`
	Request    map[string]any    `json:"request"`
	Responses  []rsfixResponse   `json:"responses"`
	Upstream   []rsfixCaptured   `json:"upstream"`
	Downstream rsfixDownstream   `json:"downstream"`
	Extra      map[string]any    `json:"extra,omitempty"`
}

func rsfixOut(t *testing.T) string {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	return dir
}

func rsfixWrite(t *testing.T, provider string, fixture rsfixFixture) {
	dir := filepath.Join(rsfixOut(t), provider)
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
	raw, err := json.MarshalIndent(fixture, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(dir, fixture.Name+".json"), append(raw, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func rsfixStatus(err error) (int, string) {
	if err == nil {
		return 0, ""
	}
	type statusCoder interface{ StatusCode() int }
	if sc, ok := err.(statusCoder); ok {
		return sc.StatusCode(), err.Error()
	}
	return -1, err.Error()
}

func b64(raw []byte) string { return base64.StdEncoding.EncodeToString(raw) }

func unb64(raw string) []byte {
	out, _ := base64.StdEncoding.DecodeString(raw)
	return out
}

func rsfixHeaders(h http.Header, names ...string) [][2]string {
	var out [][2]string
	for _, name := range names {
		if v := h.Get(name); v != "" {
			out = append(out, [2]string{name, v})
		}
	}
	return out
}

// rsfixUsagePlugin captures the usage records Go's executors publish, so fixtures can
// carry the reporter's view (tokens, response model, translated reasoning effort).
type rsfixUsagePlugin struct {
	mu      sync.Mutex
	records []usage.Record
}

func (p *rsfixUsagePlugin) HandleUsage(_ context.Context, record usage.Record) {
	p.mu.Lock()
	p.records = append(p.records, record)
	p.mu.Unlock()
}

var rsfixUsage = func() *rsfixUsagePlugin {
	p := &rsfixUsagePlugin{}
	usage.RegisterNamedPlugin("rsfix-capture", p)
	return p
}()

// rsfixResetUsage drops records from earlier cases.
func rsfixResetUsage() {
	time.Sleep(30 * time.Millisecond)
	rsfixUsage.mu.Lock()
	rsfixUsage.records = nil
	rsfixUsage.mu.Unlock()
}

// rsfixTakeUsage returns the records published since the last reset.
func rsfixTakeUsage() []map[string]any {
	time.Sleep(80 * time.Millisecond)
	rsfixUsage.mu.Lock()
	defer rsfixUsage.mu.Unlock()
	var out []map[string]any
	for _, r := range rsfixUsage.records {
		out = append(out, map[string]any{
			"input_tokens":     r.Detail.InputTokens,
			"output_tokens":    r.Detail.OutputTokens,
			"cached_tokens":    r.Detail.CachedTokens,
			"total_tokens":     r.Detail.TotalTokens,
			"reasoning_tokens": r.Detail.ReasoningTokens,
			"response_model":   r.ResponseModel,
			"reasoning_effort": r.ReasoningEffort,
			"model":            r.Model,
			"failed":           r.Failed,
			"service_tier":     r.ResponseServiceTier,
			"fail_status":      r.Fail.StatusCode,
			"fail_body":        r.Fail.Body,
			"ttft_set":         r.TTFT > 0,
		})
	}
	rsfixUsage.records = nil
	return out
}

// rsfixCapture returns ctx with a gin context (its request carrying headers) and a
// function returning Go's request-log text for the calls made with it: API_REQUEST and
// API_RESPONSE as the helps logging functions build them in memory (the executor's
// config must have RequestLog on), timestamps masked. origin is the capture server.
func rsfixCapture(ctx context.Context, headers http.Header, origin string) (context.Context, func() map[string]any) {
	gin.SetMode(gin.TestMode)
	ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
	ginCtx.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
	for k, vs := range headers {
		for _, v := range vs {
			ginCtx.Request.Header.Add(k, v)
		}
	}
	return context.WithValue(ctx, "gin", ginCtx), func() map[string]any { return rsfixCaptureOf(ginCtx, origin) }
}

// rsfixCaptureOf reads the request-log text recorded on ginCtx.
func rsfixCaptureOf(ginCtx *gin.Context, origin string) map[string]any {
	out := map[string]any{"origin": origin}
	for key, name := range map[string]string{"API_REQUEST": "request", "API_RESPONSE": "response"} {
		if v, ok := ginCtx.Get(key); ok {
			if b, ok := v.([]byte); ok {
				lines := strings.Split(string(b), "\n")
				for i, line := range lines {
					if strings.HasPrefix(line, "Timestamp: ") {
						lines[i] = "Timestamp: <time>"
					}
				}
				out[name] = strings.Join(lines, "\n")
			}
		}
	}
	return out
}

// rsfixLabel is the Auth.Label the watcher's synthesizers give a credential: the email
// or provider type for auth files, <provider>-apikey for config API keys.
func rsfixLabel(provider string, meta map[string]any, attrs map[string]string) string {
	if strings.HasPrefix(attrs["source"], "config:") {
		return provider + "-apikey"
	}
	if email, _ := meta["email"].(string); email != "" {
		return email
	}
	if kind, _ := meta["type"].(string); kind != "" {
		return kind
	}
	return provider
}
