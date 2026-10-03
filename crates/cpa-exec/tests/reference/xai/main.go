// Generates byte-level goldens for the xAI executor by running the pinned Go
// XAIExecutor against a local raw-TCP capture server. A context round tripper sends
// every upstream request (api.x.ai, cli-chat-proxy.grok.com or a configured base URL)
// to the capture server and records the URL the executor chose. It never contacts a
// provider; keys are fake.
package main

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/modelconfig"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type upstream struct {
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body"`
}

type errOut struct {
	Status       int    `json:"status"`
	Message      string `json:"message"`
	RetryAfterMS int64  `json:"retry_after_ms"`
	// CredentialScoped is IsCredentialScoped(): a 429 that cools the whole credential.
	CredentialScoped bool `json:"credential_scoped"`
}

// bind is the configured xai-api-key model the conductor binds to the attempt.
type bind struct {
	Name     string                    `json:"name"`
	Thinking *registry.ThinkingSupport `json:"thinking,omitempty"`
	IsCompat bool                      `json:"is_compat,omitempty"`
}

type scenario struct {
	Name string `json:"name"`
	// Config is YAML; UPSTREAM is replaced with the capture server address.
	Config string `json:"config,omitempty"`
	// ConfigAuth selects the Nth synthesized xai credential; -1 uses Attributes/Metadata.
	ConfigAuth     int               `json:"config_auth"`
	Attributes     map[string]string `json:"attributes,omitempty"`
	Metadata       map[string]any    `json:"metadata,omitempty"`
	Model          string            `json:"model"`
	RequestedModel string            `json:"requested_model,omitempty"`
	Payload        string            `json:"payload"`
	Original       string            `json:"original,omitempty"`
	Source         string            `json:"source"`
	Response       string            `json:"response,omitempty"`
	// Op is execute, stream, count, images or videos.
	Op          string            `json:"op"`
	Alt         string            `json:"alt,omitempty"`
	Headers     map[string]string `json:"headers,omitempty"`
	RequestPath string            `json:"request_path,omitempty"`
	// ExecutionSession is opts.Metadata[execution_session_id].
	ExecutionSession string `json:"execution_session,omitempty"`
	// DerivedSession is req.Metadata[derived_session_id] (session.Enrich).
	DerivedSession string `json:"derived_session,omitempty"`
	// CallerKey is the downstream client API key (gin userApiKey).
	CallerKey string `json:"caller_key,omitempty"`
	// Websocket marks a downstream WebSocket turn.
	Websocket bool `json:"websocket,omitempty"`
	// Needs names shared helpers whose real port must land before Rust can match.
	Needs    []string  `json:"needs,omitempty"`
	Bind     *bind     `json:"bind,omitempty"`
	Upstream *upstream `json:"upstream,omitempty"`

	ResolvedModel map[string]any `json:"resolved_model,omitempty"`
	Usage         *usageOut      `json:"usage,omitempty"`
	URL           string         `json:"url,omitempty"`
	Request       string         `json:"request,omitempty"`
	RequestB64    string         `json:"request_b64,omitempty"`
	Output        string         `json:"output,omitempty"`
	Chunks        []string       `json:"chunks,omitempty"`
	Error         *errOut        `json:"error,omitempty"`
}

// usageOut is the record Go's usage reporter published for the attempt.
type usageOut struct {
	Input         int64  `json:"input"`
	Output        int64  `json:"output"`
	Reasoning     int64  `json:"reasoning"`
	Cached        int64  `json:"cached"`
	Total         int64  `json:"total"`
	Effort        string `json:"effort"`
	ResponseModel string `json:"response_model"`
	Failed        bool   `json:"failed"`
}

var records = make(chan usage.Record, 64)

type capturePlugin struct{}

func (capturePlugin) HandleUsage(_ context.Context, r usage.Record) { records <- r }

// capture serves one connection: records the raw request and replies with the script.
func capture(ln net.Listener, up *upstream, done chan<- string) {
	conn, err := ln.Accept()
	if err != nil {
		done <- ""
		return
	}
	defer conn.Close()
	_ = conn.SetDeadline(time.Now().Add(10 * time.Second))
	reader := bufio.NewReader(conn)
	var raw bytes.Buffer
	length := 0
	for {
		line, errLine := reader.ReadString('\n')
		raw.WriteString(line)
		if errLine != nil {
			done <- raw.String()
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
	var out bytes.Buffer
	fmt.Fprintf(&out, "HTTP/1.1 %d %s\r\n", up.Status, http.StatusText(up.Status))
	for _, h := range up.Headers {
		fmt.Fprintf(&out, "%s: %s\r\n", h[0], h[1])
	}
	fmt.Fprintf(&out, "Content-Length: %d\r\n", len(up.Body))
	out.WriteString("Connection: close\r\n\r\n")
	out.WriteString(up.Body)
	_, _ = conn.Write(out.Bytes())
	done <- raw.String()
}

// redirect sends every request to the capture server, recording the URL Go chose.
type redirect struct {
	addr string
	url  *string
}

func (r redirect) RoundTrip(req *http.Request) (*http.Response, error) {
	*r.url = req.URL.String()
	out := req.Clone(req.Context())
	out.URL.Scheme = "http"
	out.URL.Host = r.addr
	out.Host = r.addr
	return http.DefaultTransport.RoundTrip(out)
}

func statusOf(err error) *errOut {
	out := &errOut{Message: err.Error(), RetryAfterMS: -1}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	if r, ok := err.(interface{ RetryAfter() *time.Duration }); ok && r.RetryAfter() != nil {
		out.RetryAfterMS = r.RetryAfter().Milliseconds()
	}
	if c, ok := err.(interface{ IsCredentialScoped() bool }); ok {
		out.CredentialScoped = c.IsCredentialScoped()
	}
	return out
}

// resolvedJSON is the bound ModelInfo as the Rust registry stores it: Go's JSON with
// the internal flags thinking reads added when set.
func resolvedJSON(info *registry.ModelInfo) map[string]any {
	raw, err := json.Marshal(info)
	if err != nil {
		panic(err)
	}
	out := map[string]any{}
	if err := json.Unmarshal(raw, &out); err != nil {
		panic(err)
	}
	if info.IsCompat {
		out["is_compat"] = true
	}
	if info.UserDefined {
		out["user_defined"] = true
	}
	if info.SupportConfigurationUpdate {
		out["support_configuration_update"] = true
	}
	return out
}

func run(s *scenario) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer ln.Close()
	addr := ln.Addr().String()
	done := make(chan string, 1)
	if s.Upstream != nil {
		go capture(ln, s.Upstream, done)
	}

	cfg := &config.Config{}
	if s.Config != "" {
		cfg, err = config.ParseConfigBytes([]byte(strings.ReplaceAll(s.Config, "UPSTREAM", addr)))
		if err != nil {
			panic(fmt.Sprintf("%s: %v", s.Name, err))
		}
	}
	var auth *cliproxyauth.Auth
	if s.ConfigAuth >= 0 {
		auths, errSynth := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
			Config:      cfg,
			Now:         time.Unix(0, 0),
			IDGenerator: synthesizer.NewStableIDGenerator(),
		})
		if errSynth != nil {
			panic(errSynth)
		}
		var xai []*cliproxyauth.Auth
		for _, a := range auths {
			if a.Provider == "xai" {
				xai = append(xai, a)
			}
		}
		auth = xai[s.ConfigAuth]
	} else {
		attrs := map[string]string{}
		for k, v := range s.Attributes {
			attrs[k] = strings.ReplaceAll(v, "UPSTREAM", addr)
		}
		metadata := map[string]any{}
		for k, v := range s.Metadata {
			if text, ok := v.(string); ok {
				v = strings.ReplaceAll(text, "UPSTREAM", addr)
			}
			metadata[k] = v
		}
		auth = &cliproxyauth.Auth{Provider: "xai", Attributes: attrs, Metadata: metadata}
	}

	var exec cliproxyauth.ProviderExecutor = executor.NewXAIExecutor(cfg)
	if auth.AuthKind() == cliproxyauth.AuthKindAPIKey {
		exec = exec.(cliproxyauth.APIKeyConfigExecutor).ForAPIKey()
	}
	req := cliproxyexecutor.Request{Model: s.Model, Payload: []byte(s.Payload), Metadata: map[string]any{}}
	headers := http.Header{}
	for k, v := range s.Headers {
		headers.Set(k, v)
	}
	opts := cliproxyexecutor.Options{
		Alt:          s.Alt,
		Headers:      headers,
		SourceFormat: sdktranslator.FromString(s.Source),
		Metadata:     map[string]any{},
	}
	if s.Response != "" {
		opts.ResponseFormat = sdktranslator.FromString(s.Response)
	}
	if s.Original != "" {
		opts.OriginalRequest = []byte(s.Original)
	}
	if s.RequestedModel != "" {
		opts.Metadata[cliproxyexecutor.RequestedModelMetadataKey] = s.RequestedModel
	}
	if s.RequestPath != "" {
		opts.Metadata[cliproxyexecutor.RequestPathMetadataKey] = s.RequestPath
	}
	if s.ExecutionSession != "" {
		opts.Metadata[cliproxyexecutor.ExecutionSessionMetadataKey] = s.ExecutionSession
	}
	if s.DerivedSession != "" {
		req.Metadata[cliproxyexecutor.DerivedSessionIDMetadataKey] = s.DerivedSession
	}
	if key := strings.TrimSpace(headers.Get("Idempotency-Key")); key != "" {
		opts.Metadata["idempotency_key"] = key
	}
	switch s.Op {
	case "images":
		opts.SourceFormat = sdktranslator.FromString("openai-image")
	case "videos":
		opts.SourceFormat = sdktranslator.FromString("openai-video")
	}
	if s.Bind != nil {
		info := modelconfig.ResolveModelInfo(s.Bind.Name, "xai", s.Bind.Thinking)
		info.IsCompat = s.Bind.IsCompat
		req.Metadata["cliproxy.resolved_api_key_model_info"] = info
		s.ResolvedModel = resolvedJSON(info)
	}

	for len(records) > 0 {
		<-records
	}
	var chosen string
	ctx := context.WithValue(context.Background(), "cliproxy.roundtripper", http.RoundTripper(redirect{addr: addr, url: &chosen}))
	ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
	ginCtx.Request = httptest.NewRequest(http.MethodPost, "/v1/responses", nil)
	ginCtx.Request.Header = headers.Clone()
	if s.CallerKey != "" {
		ginCtx.Set("userApiKey", s.CallerKey)
	}
	ctx = context.WithValue(ctx, "gin", ginCtx)
	if s.Websocket {
		ctx = cliproxyexecutor.WithDownstreamWebsocket(ctx)
	}

	switch s.Op {
	case "execute", "images", "videos":
		resp, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	case "stream":
		opts.Stream = true
		result, errExec := exec.ExecuteStream(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
			break
		}
		for chunk := range result.Chunks {
			if chunk.Err != nil {
				s.Error = statusOf(chunk.Err)
				continue
			}
			s.Chunks = append(s.Chunks, string(chunk.Payload))
		}
	case "count":
		resp, errExec := exec.CountTokens(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = statusOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	default:
		panic(s.Op)
	}
	if s.Op != "count" {
		select {
		case r := <-records:
			s.Usage = &usageOut{Input: r.Detail.InputTokens, Output: r.Detail.OutputTokens, Reasoning: r.Detail.ReasoningTokens,
				Cached: r.Detail.CachedTokens, Total: r.Detail.TotalTokens, Effort: r.ReasoningEffort, ResponseModel: r.ResponseModel, Failed: r.Failed}
		case <-time.After(500 * time.Millisecond):
		}
	}
	if s.Upstream != nil {
		select {
		case raw := <-done:
			s.URL = chosen
			normalized := strings.ReplaceAll(raw, addr, "UPSTREAM")
			if utf8.ValidString(normalized) {
				s.Request = normalized
			} else {
				s.RequestB64 = base64.StdEncoding.EncodeToString([]byte(normalized))
			}
		case <-time.After(300 * time.Millisecond):
			// No upstream call was made; the scripted reply is unused.
		}
	}
}

// grokBlob is a structurally valid Grok encrypted-content value (the shape Go's cache
// tests use), distinct per seed.
func grokBlob(seed byte) string {
	buf := make([]byte, 0, 256)
	for i := 0; len(buf) < 256; i++ {
		sum := sha256.Sum256([]byte{byte(i), byte(i >> 8), seed, 99})
		buf = append(buf, sum[:]...)
	}
	return base64.RawStdEncoding.EncodeToString(buf[:256])
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintln(os.Stderr, "usage: generator OUTPUT.json")
		os.Exit(2)
	}
	gin.SetMode(gin.ReleaseMode)
	usage.RegisterPlugin(capturePlugin{})
	all := scenarios()
	blobs := strings.NewReplacer("GROKENC1", grokBlob(1), "GROKENC2", grokBlob(2), "GROKENC3", grokBlob(3))
	for i := range all {
		all[i].Payload = blobs.Replace(all[i].Payload)
		all[i].Original = blobs.Replace(all[i].Original)
		if all[i].Upstream != nil {
			all[i].Upstream.Body = blobs.Replace(all[i].Upstream.Body)
		}
		run(&all[i])
	}
	data, err := json.MarshalIndent(map[string]any{"scenarios": all}, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
