package main

import (
	"bufio"
	"bytes"
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	internalcache "github.com/router-for-me/CLIProxyAPI/v8/internal/cache"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/util"
	// Production registers every translator through this package (cmd/server/main.go).
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	coreusage "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

// upstream is the scripted answer of the one-shot capture server.
type upstream struct {
	Status  int         `json:"status"`
	Headers [][2]string `json:"headers,omitempty"`
	Body    string      `json:"body"`
	// Missing is how many bytes the declared Content-Length promises beyond Body: the
	// connection closes early, a read error mid-stream.
	Missing int `json:"missing,omitempty"`
	// Stall keeps the connection open after Body until the client closes it: a stream
	// whose upstream stays silent.
	Stall bool `json:"stall,omitempty"`
}

type execError struct {
	Status  int    `json:"status"`
	Message string `json:"message"`
	// RetryAfterMs is the error's RetryAfter hint, when it carries one.
	RetryAfterMs *int64 `json:"retry_after_ms,omitempty"`
}

// usageOut is the part of a published usage record the executor decides.
type usageOut struct {
	InputTokens     int64  `json:"input_tokens"`
	OutputTokens    int64  `json:"output_tokens"`
	ReasoningTokens int64  `json:"reasoning_tokens"`
	CachedTokens    int64  `json:"cached_tokens"`
	TotalTokens     int64  `json:"total_tokens"`
	ResponseModel   string `json:"response_model"`
	ReasoningEffort string `json:"reasoning_effort"`
	Failed          bool   `json:"failed"`
	FailStatus      int    `json:"fail_status,omitempty"`
	FailBody        string `json:"fail_body,omitempty"`
}

type usageCapture chan coreusage.Record

func (c usageCapture) HandleUsage(_ context.Context, record coreusage.Record) { c <- record }

var capturedUsage = make(usageCapture, 16)

func awaitUsage(name string) *usageOut {
	select {
	case r := <-capturedUsage:
		if r.TraceID != name {
			panic(fmt.Sprintf("%s: usage record of %q", name, r.TraceID))
		}
		d := r.Detail
		return &usageOut{
			InputTokens: d.InputTokens, OutputTokens: d.OutputTokens, ReasoningTokens: d.ReasoningTokens,
			CachedTokens: d.CachedTokens, TotalTokens: d.TotalTokens, ResponseModel: r.ResponseModel,
			ReasoningEffort: r.ReasoningEffort, Failed: r.Failed,
			FailStatus: r.Fail.StatusCode, FailBody: r.Fail.Body,
		}
	case <-time.After(300 * time.Millisecond):
		return nil
	}
}

type execScenario struct {
	Name string `json:"name"`
	// GoTests are the Go executor tests whose inputs this scenario reproduces.
	GoTests []string `json:"go_tests,omitempty"`
	// Op is execute, stream or count.
	Op       string `json:"op"`
	Source   string `json:"source"`
	Response string `json:"response,omitempty"`
	Model    string `json:"model"`
	Payload  string `json:"payload"`
	Original string `json:"original,omitempty"`
	Alt      string `json:"alt,omitempty"`
	// Config is YAML for config.ParseConfigBytes.
	Config  string            `json:"config,omitempty"`
	Headers map[string]string `json:"headers,omitempty"`
	// AuthID is the credential's ID (process-wide cooldown state is keyed on it).
	AuthID string `json:"auth_id"`
	// Metadata and Attributes describe the credential; UPSTREAM is the capture server.
	Metadata   map[string]any    `json:"metadata"`
	Attributes map[string]string `json:"attributes"`
	// DerivedSession is the conductor's derived_session_id metadata.
	DerivedSession string `json:"derived_session,omitempty"`
	// WebSearchModels are registered for this scenario only, as Antigravity models with
	// native web search (GetAvailableModelsByProvider).
	WebSearchModels []string `json:"web_search_models,omitempty"`
	// Credits runs the attempt the way the conductor's credits fallback does
	// (WithAntigravityCredits); KnownCredits first records a known, available hint so
	// no background credits lookup races the capture server.
	Credits      bool `json:"credits,omitempty"`
	KnownCredits bool `json:"known_credits,omitempty"`
	// CreditsHint is the credential's credits hint after the attempt.
	CreditsHint *creditsHint `json:"credits_hint,omitempty"`
	// Scripted answers every request in process (Go's context round tripper) instead
	// of the capture server; Requests then records what the client handed it.
	Scripted []reply    `json:"scripted,omitempty"`
	Upstream *upstream  `json:"upstream,omitempty"`
	Request  string     `json:"request,omitempty"`
	Requests []recorded `json:"requests,omitempty"`
	Output   string     `json:"output,omitempty"`
	Chunks   []string   `json:"chunks,omitempty"`
	Error    *execError `json:"error,omitempty"`
	Usage    *usageOut  `json:"usage,omitempty"`
	// CancelWhenIdle cancels the request context once the stream has delivered nothing
	// for 200 ms: a client that goes away mid-stream.
	CancelWhenIdle bool `json:"cancel_when_idle,omitempty"`
	// ReplayKey is a reasoning replay session key; Replay is what the ledger holds under
	// it for the scenario's model after the attempt (absent when nothing is stored).
	ReplayKey string   `json:"replay_key,omitempty"`
	Replay    []string `json:"replay,omitempty"`
}

type creditsHint struct {
	Known           bool    `json:"known"`
	Available       bool    `json:"available"`
	CreditAmount    float64 `json:"credit_amount"`
	MinCreditAmount float64 `json:"min_credit_amount"`
}

func captureOnce(ln net.Listener, up *upstream, done chan<- string) {
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
	fmt.Fprintf(&out, "Content-Length: %d\r\n", len(up.Body)+up.Missing)
	out.WriteString("Connection: close\r\n\r\n")
	out.WriteString(up.Body)
	_, _ = conn.Write(out.Bytes())
	if up.Stall {
		_, _ = io.Copy(io.Discard, conn)
	}
	done <- raw.String()
}

func execErrorOf(err error) *execError {
	out := &execError{Message: err.Error()}
	if s, ok := err.(interface{ StatusCode() int }); ok {
		out.Status = s.StatusCode()
	}
	if ra, ok := err.(interface{ RetryAfter() *time.Duration }); ok && ra.RetryAfter() != nil {
		ms := ra.RetryAfter().Milliseconds()
		out.RetryAfterMs = &ms
	}
	return out
}

func runExec(s *execScenario) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		panic(err)
	}
	defer ln.Close()
	addr := ln.Addr().String()
	done := make(chan string, 1)
	if s.Upstream == nil && s.Scripted == nil {
		// A scenario Go is expected to reject locally still gets an answer, so a request
		// it does send is recorded instead of hanging.
		s.Upstream = jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"unexpected"}]},"finishReason":"STOP"}]},"traceId":"t-unexpected"}`)
	}
	if s.Upstream != nil {
		go captureOnce(ln, s.Upstream, done)
	}
	cfg := &config.Config{}
	if s.Config != "" {
		cfg, err = config.ParseConfigBytes([]byte(s.Config))
		if err != nil {
			panic(fmt.Sprintf("%s: %v", s.Name, err))
		}
	}
	attrs := map[string]string{}
	for k, v := range s.Attributes {
		attrs[k] = strings.ReplaceAll(v, "UPSTREAM", addr)
	}
	metadata := map[string]any{}
	for k, v := range s.Metadata {
		if text, isText := v.(string); isText {
			v = strings.ReplaceAll(text, "UPSTREAM", addr)
		}
		metadata[k] = v
	}
	if len(s.WebSearchModels) > 0 {
		var models []*registry.ModelInfo
		for _, id := range s.WebSearchModels {
			models = append(models, &registry.ModelInfo{ID: id, SupportsWebSearch: true})
		}
		registry.GetGlobalRegistry().RegisterClient("web-search-"+s.Name, "antigravity", models)
		defer registry.GetGlobalRegistry().UnregisterClient("web-search-" + s.Name)
	}
	auth := &cliproxyauth.Auth{ID: s.AuthID, Provider: "antigravity", Attributes: attrs, Metadata: metadata}
	exec := executor.NewAntigravityExecutor(cfg)

	req := cliproxyexecutor.Request{Model: s.Model, Payload: []byte(s.Payload), Metadata: map[string]any{}}
	headers := http.Header{}
	for k, v := range s.Headers {
		headers.Set(k, v)
	}
	opts := cliproxyexecutor.Options{
		Stream:       s.Op == "stream",
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
	if s.DerivedSession != "" {
		opts.Metadata[cliproxyexecutor.DerivedSessionIDMetadataKey] = s.DerivedSession
	}
	ctx := coreusage.WithTraceID(context.Background(), s.Name)
	if s.KnownCredits {
		cliproxyauth.SetAntigravityCreditsHint(s.AuthID, cliproxyauth.AntigravityCreditsHint{Known: true, Available: true})
	}
	if s.Credits {
		ctx = cliproxyauth.WithAntigravityCredits(ctx)
	}
	var rt *scripted
	if s.Scripted != nil {
		rt = &scripted{replies: append([]reply(nil), s.Scripted...)}
		ctx = context.WithValue(ctx, "cliproxy.roundtripper", http.RoundTripper(rt))
	}

	switch s.Op {
	case "execute":
		resp, errExec := exec.Execute(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = execErrorOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	case "stream":
		streamCtx, cancel := context.WithCancel(ctx)
		defer cancel()
		result, errExec := exec.ExecuteStream(streamCtx, auth, req, opts)
		if errExec != nil {
			s.Error = execErrorOf(errExec)
			break
		}
		chunks := result.Chunks
		for chunks != nil {
			var idle <-chan time.Time
			if s.CancelWhenIdle && streamCtx.Err() == nil {
				idle = time.After(200 * time.Millisecond)
			}
			select {
			case chunk, ok := <-chunks:
				if !ok {
					chunks = nil
					continue
				}
				if streamCtx.Err() != nil {
					continue // the client is gone; nothing more reaches it
				}
				if chunk.Err != nil {
					s.Error = execErrorOf(chunk.Err)
					continue
				}
				s.Chunks = append(s.Chunks, string(chunk.Payload))
			case <-idle:
				cancel()
			}
		}
	case "count":
		resp, errExec := exec.CountTokens(ctx, auth, req, opts)
		if errExec != nil {
			s.Error = execErrorOf(errExec)
		} else {
			s.Output = string(resp.Payload)
		}
	default:
		panic(s.Op)
	}
	s.Usage = awaitUsage(s.Name)
	if s.ReplayKey != "" {
		items, _ := internalcache.GetAntigravityReasoningReplayItems(s.Model, s.ReplayKey)
		for _, item := range items {
			s.Replay = append(s.Replay, string(item))
		}
	}
	if s.Credits {
		if hint, ok := cliproxyauth.GetAntigravityCreditsHint(s.AuthID); ok {
			s.CreditsHint = &creditsHint{Known: hint.Known, Available: hint.Available, CreditAmount: hint.CreditAmount, MinCreditAmount: hint.MinCreditAmount}
		}
	}
	if rt != nil {
		for _, r := range rt.requests {
			r.URL = strings.ReplaceAll(r.URL, addr, "UPSTREAM")
			s.Requests = append(s.Requests, r)
		}
	}
	if s.Upstream != nil {
		select {
		case raw := <-done:
			normalized := strings.ReplaceAll(raw, addr, "UPSTREAM")
			if !utf8.ValidString(normalized) {
				panic(s.Name + ": capture is not UTF-8")
			}
			s.Request = normalized
		case <-time.After(200 * time.Millisecond):
		}
	}
}

// sse joins Antigravity stream events the way the upstream frames them.
func sse(events ...string) string {
	var b strings.Builder
	for _, e := range events {
		b.WriteString("data: ")
		b.WriteString(e)
		b.WriteString("\r\n\r\n")
	}
	return b.String()
}

func jsonOK(body string) *upstream {
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
}

func sseOK(events ...string) *upstream {
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: sse(events...)}
}

func failed(status int, body string) *upstream {
	return &upstream{Status: status, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
}

// claudeSig is an opaque thinking signature the upstream returns for Claude models.
const claudeSig = "RXFzQ2t3SUJHQUlpUUhmYWtlLWNsYXVkZS1zaWduYXR1cmUtZm9yLWFudGlncmF2aXR5LXRlc3Rz"

func execScenarios() []execScenario {
	creds := func(extra map[string]any) map[string]any {
		m := map[string]any{
			"type":         "antigravity",
			"access_token": "ya29.fake-ag",
			"expired":      "2099-01-01T00:00:00Z",
			"project_id":   "proj-ag",
			"email":        "ag@example.com",
		}
		for k, v := range extra {
			if v == nil {
				delete(m, k)
			} else {
				m[k] = v
			}
		}
		return m
	}
	attrs := func(extra map[string]string) map[string]string {
		m := map[string]string{"base_url": "http://UPSTREAM"}
		for k, v := range extra {
			m[k] = v
		}
		return m
	}
	geminiAnswer := `{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hi <there> & bye"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":7,"totalTokenCount":18,"thoughtsTokenCount":2},"modelVersion":"gemini-3-flash","responseId":"resp-1"},"traceId":"trace-ag-1"}`
	streamEvents := []string{
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Thinking "}]}}],"usageMetadata":{"promptTokenCount":5,"totalTokenCount":5},"modelVersion":"gemini-3-flash","responseId":"resp-s"},"traceId":"trace-s-NAME"}`,
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"more","thoughtSignature":"c2lnbmF0dXJlLW9uZS10aGF0LWlzLWxvbmc="}]}}],"modelVersion":"gemini-3-flash","responseId":"resp-s"},"traceId":"trace-s-NAME"}`,
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Hello"}]}}],"modelVersion":"gemini-3-flash","responseId":"resp-s"},"traceId":"trace-s-NAME"}`,
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":" world"},{"functionCall":{"name":"lookup","args":{"q":"x","n":1.50}}}]}}],"modelVersion":"gemini-3-flash","responseId":"resp-s"},"traceId":"trace-s-NAME"}`,
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"inline_data":{"mime_type":"image/png","data":"AAAA"}},{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":9,"totalTokenCount":20,"thoughtsTokenCount":6},"modelVersion":"gemini-3-flash","responseId":"resp-s"},"traceId":"trace-s-NAME"}`,
	}
	stream := func(name string) *upstream {
		events := make([]string, len(streamEvents))
		for i, e := range streamEvents {
			events[i] = strings.ReplaceAll(e, "NAME", name)
		}
		return sseOK(events...)
	}
	claudeEvents := func(name string) *upstream {
		return sseOK(
			`{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"Let me think","thoughtSignature":"`+claudeSig+`"}]}}],"modelVersion":"claude-sonnet-4-6","responseId":"msg-c"},"traceId":"trace-c-`+name+`"}`,
			`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Answer: "}]}}],"modelVersion":"claude-sonnet-4-6","responseId":"msg-c"},"traceId":"trace-c-`+name+`"}`,
			`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"42"},{"functionCall":{"id":"toolu_01","name":"calc","args":{"expr":"6*7"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":12,"totalTokenCount":42},"modelVersion":"claude-sonnet-4-6","responseId":"msg-c"},"traceId":"trace-c-`+name+`"}`,
		)
	}
	rateLimited := `{"error":{"code":429,"message":"Resource has been exhausted (e.g. check quota).","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"RATE_LIMIT_EXCEEDED","domain":"cloudcode-pa.googleapis.com"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"30s"}]}}`
	quota := `{"error":{"code":429,"message":"You have exhausted your capacity on this model.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"},{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"3600.5s"}]}}`
	reserved := "cpa_gemini_0123456789abcdef0123456789abcdef"
	capsule, errSeal := helps.SealAntigravityCompaction("Earlier: the user asked for a plan & got one <ok>.", "gemini-3-flash")
	if errSeal != nil {
		panic(errSeal)
	}
	tools := `"tools":[{"type":"function","function":{"name":"get_weather","description":"Weather","parameters":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","additionalProperties":false,"properties":{"city":{"type":"string","format":"city-name","default":"Paris"},"days":{"type":["integer","null"],"minimum":1}},"required":["city"]}}}]`
	scenarios := []execScenario{
		{Name: "gemini_execute_basic", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"hello ag basic"}]}],"generationConfig":{"maxOutputTokens":100000,"temperature":0.5},"safetySettings":[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"BLOCK_NONE"}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_execute_leading_model_turn", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"model","parts":[{"text":"I start"}]},{"role":"user","parts":[{"text":"then you"}]},{"role":"model","parts":[{"text":"and I end"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "openai_execute_tools_schema", Op: "execute", Source: "openai", Model: "gemini-3.1-pro-low",
			Payload:  `{"model":"gemini-3.1-pro-low","messages":[{"role":"system","content":"Be terse."},{"role":"user","content":"weather in paris ag"}],` + tools + `,"max_tokens":200000,"response_format":{"type":"json_schema","json_schema":{"name":"w","schema":{"type":"object","additionalProperties":false,"properties":{"t":{"type":"number","exclusiveMinimum":0}}}}}}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "responses_execute_reasoning", Op: "execute", Source: "openai-response", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"responses ag hello"}]}],"reasoning":{"effort":"high"},"max_output_tokens":64}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "claude_model_execute", Op: "execute", Source: "claude", Model: "claude-sonnet-4-6",
			Payload:  `{"model":"claude-sonnet-4-6","max_tokens":1024,"messages":[{"role":"user","content":"compute ag"}],"tools":[{"name":"calc","description":"calc","input_schema":{"type":"object","properties":{"expr":{"type":"string"}}}}],"thinking":{"type":"enabled","budget_tokens":2048}}`,
			Upstream: claudeEvents("claude_model_execute")},
		{Name: "gemini_pro_execute_merges_stream", Op: "execute", Source: "gemini", Model: "gemini-3-pro-preview",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"pro merge ag"}]}]}`,
			Upstream: stream("gemini_pro_execute_merges_stream")},
		{Name: "openai_pro_execute_merges_stream", Op: "execute", Source: "openai", Model: "gemini-3-pro-preview",
			Payload:  `{"model":"gemini-3-pro-preview","messages":[{"role":"user","content":"pro merge openai"}]}`,
			Upstream: sseOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]}}]},"traceId":"t-pm"}`, `{"candidates":[{"content":{"role":"model","parts":[{"text":" bare"}]},"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7}}`)},
		{Name: "gemini_pro_execute_empty_stream", Op: "execute", Source: "gemini", Model: "gemini-3-pro-preview",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"pro empty ag"}]}]}`,
			Upstream: &upstream{Status: 200, Body: "data: [DONE]\n\n"}},
		{Name: "execute_429_rate_limited", Op: "execute", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-cool",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"cooldown first"}]}]}`,
			Upstream: failed(429, rateLimited)},
		{Name: "execute_in_short_cooldown", Op: "execute", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-cool",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"cooldown second"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "execute_cooldown_other_model", Op: "execute", Source: "gemini", Model: "gemini-3.1-flash-lite", AuthID: "ag-cool",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"cooldown other model"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "execute_429_cooling_disabled", Op: "execute", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-nocool",
			Metadata: creds(map[string]any{"disable_cooling": "true"}),
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"no cooldown first"}]}]}`,
			Upstream: failed(429, rateLimited)},
		{Name: "execute_after_cooling_disabled_429", Op: "execute", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-nocool",
			Metadata: creds(map[string]any{"disable_cooling": "true"}),
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"no cooldown second"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "execute_429_quota_exhausted", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"quota ag"}]}]}`,
			Upstream: failed(429, quota)},
		{Name: "execute_429_message_delay", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"message delay ag"}]}]}`,
			Upstream: failed(429, `{"error":{"code":429,"message":"Please retry after 4s.","status":"UNAVAILABLE"}}`)},
		{Name: "execute_400", Op: "execute", Source: "openai", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","messages":[{"role":"user","content":"bad request ag"}]}`,
			Upstream: failed(400, `{"error":{"code":400,"message":"Invalid JSON payload","status":"INVALID_ARGUMENT"}}`)},
		{Name: "execute_user_agent_and_custom_headers", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Attributes: attrs(map[string]string{"user_agent": "antigravity/1.23.2 windows/amd64 google-api-nodejs-client/9.0.0", "header:X-Extra": " extra-value ", "header:Authorization": "Bearer override"}),
			Payload:    `{"contents":[{"role":"user","parts":[{"text":"headers ag"}]}]}`,
			Upstream:   jsonOK(geminiAnswer)},
		{Name: "execute_metadata_base_url_and_user_agent", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Metadata:   creds(map[string]any{"base_url": "http://UPSTREAM/", "user_agent": " curl/8 "}),
			Attributes: map[string]string{},
			Payload:    `{"contents":[{"role":"user","parts":[{"text":"metadata base ag"}]}]}`,
			Upstream:   jsonOK(geminiAnswer)},
		{Name: "gemini_function_response_roles", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"tools ag roles"}]},{"role":"model","parts":[{"functionCall":{"id":"a","name":"first","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"id":"b","name":"second","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"id":"b","name":"unknown","response":{"r":2}}},{"functionResponse":{"id":"a","response":{"r":1}}}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_function_response_name_match", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"tools ag names"}]},{"role":"model","parts":[{"functionCall":{"name":"alpha","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"functionCall":{"name":"beta","args":{}}}]},{"role":"function","parts":[{"functionResponse":{"name":"beta","response":{}}},{"functionResponse":{"name":"alpha","response":{}}}]},{"role":"user","parts":[{"text":"next"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_degrade_provenance_ids", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"degrade ag"}]},{"role":"model","parts":[{"functionCall":{"id":"` + reserved + `","name":"f","args":{"a":1}},"thoughtSignature":"c3RhbGUtc2lnbmF0dXJlLWZyb20tYW5vdGhlci1hY2NvdW50"}]},{"role":"user","parts":[{"functionResponse":{"id":"` + reserved + `","name":"f","response":{"ok":true}}}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_invalid_pairing", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Payload: `{"contents":[{"role":"user","parts":[{"text":"pairing ag"}]},{"role":"model","parts":[{"functionCall":{"id":"x","name":"f","args":{}}}]},{"role":"user","parts":[{"text":"no response"}]}]}`},
		{Name: "gpt_oss_execute", Op: "execute", Source: "openai", Model: "gpt-oss-120b-medium",
			Payload:  `{"model":"gpt-oss-120b-medium","messages":[{"role":"user","content":"oss ag"}],"max_tokens":99999}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "openai_derived_session", Op: "execute", Source: "openai", Model: "gemini-3-flash", DerivedSession: "ctx:v1:0123abcd",
			Payload:  `{"model":"gemini-3-flash","messages":[{"role":"assistant","content":"no user text"}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_explicit_session", Op: "execute", Source: "gemini", Model: "gemini-3-flash", DerivedSession: "ctx:v1:ignored",
			Payload:  `{"sessionId":"ignored-top","request":{"sessionId":"  "},"contents":[{"role":"user","parts":[{"text":"explicit session ag"}]}],"toolConfig":{"functionCallingConfig":{"mode":"AUTO"}}}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "gemini_execute_alt", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Alt: "json pretty",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"alt ag"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "sensitive_words", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Config:   "antigravity:\n  sensitive-words: [\"Claude Code\", \"secret\"]\n",
			Payload:  `{"systemInstruction":{"parts":[{"text":"You are Claude Code with a SECRET plan."}]},"contents":[{"role":"user","parts":[{"text":"sensitive ag Claude Code"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "missing_project", Op: "execute", Source: "gemini", Model: "gemini-3-flash",
			Metadata: creds(map[string]any{"project_id": nil}),
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"no project ag"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "claude_client_gemini_model_signatures", Op: "execute", Source: "claude", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","max_tokens":512,"messages":[{"role":"user","content":"sig ag"},{"role":"assistant","content":[{"type":"thinking","thinking":"bad","signature":"not-a-gemini-signature"},{"type":"thinking","thinking":"","signature":"cpa-gemini-carrier-v1:next:text:bm90LWEtc2ln"},{"type":"text","text":"ok"}]},{"role":"user","content":"again"}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "claude_client_claude_model_empty_signature", Op: "execute", Source: "claude", Model: "claude-sonnet-4-6",
			Payload:  `{"model":"claude-sonnet-4-6","max_tokens":512,"messages":[{"role":"user","content":"empty sig ag"},{"role":"assistant","content":[{"type":"thinking","thinking":"unsigned","signature":""},{"type":"text","text":"ok"}]},{"role":"user","content":"again"}]}`,
			Upstream: claudeEvents("claude_client_claude_model_empty_signature")},
		{Name: "image_model_execute", Op: "execute", Source: "gemini", Model: "gemini-3.1-flash-image",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"draw ag"}]}],"generationConfig":{"imageConfig":{"aspectRatio":"16:9"}}}`,
			Upstream: sseOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"inlineData":{"mimeType":"image/png","data":"iVBOR"}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1290,"totalTokenCount":1294}},"traceId":"t-img"}`)},
		{Name: "compaction_item_expands", Op: "execute", Source: "openai-response", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","input":[{"type":"compaction","encrypted_content":"` + capsule + `"},{"type":"message","role":"user","content":[{"type":"input_text","text":"continue ag"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "compaction_item_invalid", Op: "execute", Source: "openai-response", Model: "gemini-3-flash",
			Payload: `{"model":"gemini-3-flash","input":[{"type":"compaction","encrypted_content":"cpa-ag-compact-v1:AAAA"}]}`},
		{Name: "compaction_alt_compact", Op: "execute", Source: "openai-response", Model: "gemini-3-flash", Alt: "responses/compact",
			Payload:  `{"model":"gemini-3-flash","input":"summarize me ag","tools":[{"type":"function","name":"x","parameters":{"type":"object"}}],"metadata":{"k":"v"},"stream":true}`,
			Upstream: jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"hidden"},{"text":"The summary."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":3,"totalTokenCount":43}},"traceId":"t-cmp"}`)},
		{Name: "compaction_trigger_stream", Op: "stream", Source: "openai-response", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"trigger ag"}]},{"type":"compaction_trigger"}],"stream":true}`,
			Upstream: jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Stream summary."}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"candidatesTokenCount":2,"totalTokenCount":10}},"traceId":"t-cmps"}`)},
		{Name: "compaction_alt_stream_rejected", Op: "stream", Source: "openai-response", Model: "gemini-3-flash", Alt: "responses/compact",
			Payload: `{"model":"gemini-3-flash","input":"x"}`},

		{Name: "gemini_stream", Op: "stream", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"stream ag gemini"}]}]}`,
			Upstream: stream("gemini_stream")},
		{Name: "gemini_stream_alt", Op: "stream", Source: "gemini", Model: "gemini-3-flash", Alt: "json",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"stream ag alt"}]}]}`,
			Upstream: stream("gemini_stream_alt")},
		{Name: "openai_stream", Op: "stream", Source: "openai", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","messages":[{"role":"user","content":"stream ag openai"}],"stream":true}`,
			Upstream: stream("openai_stream")},
		{Name: "responses_stream", Op: "stream", Source: "openai-response", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","input":"stream ag responses","stream":true}`,
			Upstream: stream("responses_stream")},
		{Name: "claude_stream_claude_model", Op: "stream", Source: "claude", Model: "claude-sonnet-4-6",
			Payload:  `{"model":"claude-sonnet-4-6","max_tokens":1024,"stream":true,"messages":[{"role":"user","content":"stream ag claude"}]}`,
			Upstream: claudeEvents("claude_stream_claude_model")},
		{Name: "claude_stream_gemini_model", Op: "stream", Source: "claude", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","max_tokens":1024,"stream":true,"messages":[{"role":"user","content":"stream ag claude gemini"}]}`,
			Upstream: stream("claude_stream_gemini_model")},
		{Name: "stream_429", Op: "stream", Source: "openai", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","messages":[{"role":"user","content":"stream 429 ag"}],"stream":true}`,
			Upstream: failed(429, quota)},
		{Name: "stream_request_stream_field", Op: "stream", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"request":{"stream":true},"contents":[{"role":"user","parts":[{"text":"stream field ag"}]}]}`,
			Upstream: stream("stream_request_stream_field")},

		{Name: "gemini_count", Op: "count", Source: "gemini", Model: "gemini-3-flash",
			Payload:  `{"contents":[{"role":"model","parts":[{"text":"leading model"}]},{"role":"user","parts":[{"text":"count ag"}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object"}}]}],"toolConfig":{"functionCallingConfig":{"mode":"ANY"}},"safetySettings":[],"generationConfig":{"maxOutputTokens":10}}`,
			Upstream: jsonOK(`{"totalTokens":42,"promptTokensDetails":[{"modality":"TEXT","tokenCount":42}]}`)},
		{Name: "claude_count", Op: "count", Source: "claude", Model: "claude-sonnet-4-6", Alt: "x y",
			Payload:  `{"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"count ag claude"}]}`,
			Upstream: jsonOK(`{"totalTokens":17}`)},
		{Name: "count_429", Op: "count", Source: "openai", Model: "gemini-3-flash",
			Payload:  `{"model":"gemini-3-flash","messages":[{"role":"user","content":"count 429 ag"}]}`,
			Upstream: failed(429, rateLimited)},

		{Name: "credits_claude_execute", Op: "execute", Source: "claude", Model: "claude-sonnet-4-6", Credits: true, KnownCredits: true,
			Config:   "quota-exceeded:\n  antigravity-credits: true\n",
			Payload:  `{"model":"claude-sonnet-4-6","max_tokens":256,"messages":[{"role":"user","content":"credits ag"}]}`,
			Upstream: claudeEvents("credits_claude_execute")},
		{Name: "credits_bypass_short_cooldown", Op: "stream", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-cool", Credits: true, KnownCredits: true,
			Config:   "quota-exceeded:\n  antigravity-credits: true\n",
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"credits cooldown bypass"}]}]}`,
			Upstream: stream("credits_bypass_short_cooldown")},
		{Name: "credits_flag_without_config", Op: "execute", Source: "gemini", Model: "gemini-3-flash", AuthID: "ag-cool", Credits: true, KnownCredits: true,
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"credits flag only"}]}]}`,
			Upstream: jsonOK(geminiAnswer)},
		{Name: "credits_balance_exhausted", Op: "execute", Source: "claude", Model: "claude-sonnet-4-6", Credits: true, KnownCredits: true,
			Config:   "quota-exceeded:\n  antigravity-credits: true\n",
			Payload:  `{"model":"claude-sonnet-4-6","max_tokens":256,"messages":[{"role":"user","content":"credits empty"}]}`,
			Upstream: failed(429, `{"error":{"code":429,"message":"Not enough credits.","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"QUOTA_EXHAUSTED"},{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"INSUFFICIENT_G1_CREDITS_BALANCE"}]}}`)},
		{Name: "claude_web_search_grounding", Op: "execute", Source: "claude", Model: "gemini-3-flash", WebSearchModels: []string{"gemini-3-flash"},
			Payload: `{"model":"gemini-3-flash","max_tokens":256,"messages":[{"role":"user","content":"search ag"}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":3}]}`,
			Scripted: []reply{
				ok(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Found it."}]},"finishReason":"STOP","groundingMetadata":{"webSearchQueries":["search ag"],"groundingChunks":[{"web":{"uri":"https://vertexaisearch.cloud.google.com/grounding-api-redirect/AAA","title":"a.example"}},{"web":{"uri":"https://example.com/direct","title":"direct"}},{"web":{"uri":"https://vertexaisearch.cloud.google.com/grounding-api-redirect/AAA","title":"again"}},{"web":{"uri":"https://vertexaisearch.cloud.google.com/grounding-api-redirect/BBB","title":"b"}}]}}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":3,"totalTokenCount":8}},"traceId":"t-ws"}`),
				{Status: 302, Body: "", Location: "https://a.example/article"},
				{Status: 200, Body: ""},
			}},
	}
	scenarios = append(scenarios, replayChains()...)
	return append(scenarios, goSuiteScenarios()...)
}

// replayChains are multi-turn conversations through the reasoning replay ledger: each
// turn's answer is committed under the chain's session, and the next turn's history,
// with signatures dropped or IDs rewritten by the client, is restored from it. Turns
// run in order and share process-wide replay state, as in production.
func replayChains() []execScenario {
	session := func(id string) map[string]string { return map[string]string{"Session-Id": id} }
	callSig := "c2lnLWNhbGwtcmVwbGF5LW9uZS0xMjM="
	thoughtSig := "c2lnLXRob3VnaHQtcmVwbGF5LW9uZQ=="
	textSig := "c2lnLXRleHQtdHVybi10d28tcmVwbGF5"
	turn1 := `{"response":{"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"plan","thoughtSignature":"` + thoughtSig + `"},{"text":"Calling."},{"functionCall":{"id":"fc-native-1","name":"lookup","args":{"q":"replay"}},"thoughtSignature":"` + callSig + `"},{"functionCall":{"id":"fc-native-2","name":"lookup","args":{"q":"two"}}}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":9,"candidatesTokenCount":4,"totalTokenCount":13}},"traceId":"t-r1"}`
	done := `{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Done.","thoughtSignature":"` + textSig + `"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2,"totalTokenCount":22}},"traceId":"t-r2"}`
	history := func(args string) string {
		return `{"role":"user","parts":[{"text":"replay gemini one"}]},{"role":"model","parts":[{"text":"Calling."},{"functionCall":{"id":"fc-native-1","name":"lookup","args":` + args + `}},{"functionCall":{"id":"fc-native-2","name":"lookup","args":{"q":"two"}}}]},{"role":"user","parts":[{"functionResponse":{"id":"fc-native-1","name":"lookup","response":{"r":1}}},{"functionResponse":{"id":"fc-native-2","name":"lookup","response":{"r":2}}}]}`
	}

	weatherArgs := `{"city":"Paris"}`
	provenance := util.GeminiClaudeToolUseID("native-w1", "get_weather", weatherArgs)
	claudeTools := `"tools":[{"name":"get_weather","description":"Weather","input_schema":{"type":"object","properties":{"city":{"type":"string"},"days":{"type":"integer","default":3}},"required":["city"]}}]`
	claudeHeaders := map[string]string{"X-Claude-Code-Session-Id": "replay-claude-session"}

	proSig := "c2lnLXByby1jYWxsLXJlcGxheS0xMjM0"
	finalSig := "c2lnLXN0cmVhbS1maW5hbC10ZXh0LXNpZw=="
	proHistory := `{"role":"user","parts":[{"text":"replay pro one"}]},{"role":"model","parts":[{"functionCall":{"id":"pro-1","name":"lookup","args":{"q":"pro"}}}]},{"role":"user","parts":[{"functionResponse":{"id":"pro-1","name":"lookup","response":{"r":"ok"}}}]}`

	clearSig := "c2lnLWNsZWFyLWNhbGwtcmVwbGF5LTk5"
	clearHistory := `{"contents":[{"role":"user","parts":[{"text":"replay clear one"}]},{"role":"model","parts":[{"functionCall":{"id":"clr-1","name":"lookup","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"id":"clr-1","name":"lookup","response":{}}}]}]}`

	return []execScenario{
		{Name: "replay_gemini_turn1", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-gemini"),
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"replay gemini one"}]}]}`,
			Upstream: jsonOK(turn1)},
		{Name: "replay_gemini_turn2_restores", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-gemini"),
			Payload:  `{"contents":[` + history(`{"q":"replay"}`) + `]}`,
			Upstream: jsonOK(done)},
		{Name: "replay_gemini_turn3_changed_call", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-gemini"),
			Payload:  `{"contents":[` + history(`{"q":"changed"}`) + `,{"role":"model","parts":[{"text":"Done."}]},{"role":"user","parts":[{"text":"and now?"}]}]}`,
			Upstream: geminiDone("t-r3")},

		{Name: "replay_claude_turn1", Op: "stream", Source: "claude", Model: "gemini-3-flash", Headers: claudeHeaders,
			Payload: `{"model":"gemini-3-flash","max_tokens":512,"stream":true,` + claudeTools + `,"messages":[{"role":"user","content":"weather replay?"}]}`,
			Upstream: sseOK(
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Let me check."}]}}]},"traceId":"t-c1"}`,
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"native-w1","name":"get_weather","args":`+weatherArgs+`},"thoughtSignature":"`+callSig+`"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"totalTokenCount":17}},"traceId":"t-c1"}`,
			)},
		{Name: "replay_claude_turn2_provenance", Op: "execute", Source: "claude", Model: "gemini-3-flash", Headers: claudeHeaders,
			Payload:  `{"model":"gemini-3-flash","max_tokens":512,` + claudeTools + `,"messages":[{"role":"user","content":"weather replay?"},{"role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"` + provenance + `","name":"get_weather","input":{"city":"Paris","days":3}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"` + provenance + `","content":"sunny"}]}]}`,
			Upstream: geminiDone("t-c2")},

		{Name: "replay_pro_turn1_merged", Op: "execute", Source: "gemini", Model: "gemini-3-pro-preview", Headers: session("replay-pro"),
			Payload: `{"contents":[{"role":"user","parts":[{"text":"replay pro one"}]}]}`,
			Upstream: sseOK(
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"pro-1","name":"lookup","args":{"q":"pro"}}}]}}]},"traceId":"t-p1"}`,
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"thoughtSignature":"`+proSig+`"}]},"finishReason":"STOP"}]},"traceId":"t-p1"}`,
			)},
		{Name: "replay_pro_turn2_stream", Op: "stream", Source: "gemini", Model: "gemini-3-pro-preview", Headers: session("replay-pro"),
			Payload: `{"contents":[` + proHistory + `]}`,
			Upstream: sseOK(
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Fin"}]}}]},"traceId":"t-p2"}`,
				`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"al.","thoughtSignature":"`+finalSig+`"}]},"finishReason":"STOP"}]},"traceId":"t-p2"}`,
			)},
		{Name: "replay_pro_turn3_after_stream", Op: "execute", Source: "gemini", Model: "gemini-3-pro-preview", Headers: session("replay-pro"),
			Payload:  `{"contents":[` + proHistory + `,{"role":"model","parts":[{"text":"Final."}]},{"role":"user","parts":[{"text":"thanks"}]}]}`,
			Upstream: sseOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Bye."}]},"finishReason":"STOP"}]},"traceId":"t-p3"}`)},

		{Name: "replay_clear_turn1", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-clear"),
			Payload:  `{"contents":[{"role":"user","parts":[{"text":"replay clear one"}]}]}`,
			Upstream: jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"id":"clr-1","name":"lookup","args":{}},"thoughtSignature":"` + clearSig + `"}]},"finishReason":"STOP"}]},"traceId":"t-x1"}`)},
		{Name: "replay_clear_turn2_rejected", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-clear"),
			Payload:  clearHistory,
			Upstream: failed(400, `{"error":{"code":400,"message":"Corrupted thought signature.","status":"INVALID_ARGUMENT"}}`)},
		{Name: "replay_clear_turn3_cleared", Op: "execute", Source: "gemini", Model: "gemini-3-flash", Headers: session("replay-clear"),
			Payload:  clearHistory,
			Upstream: geminiDone("t-x3")},
	}
}

// goSuiteScenarios reproduce the upstream answers and requests of Go's executor
// tests (antigravity_executor_*_test.go) named in GoTests.
func goSuiteScenarios() []execScenario {
	noFinishReason := `data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"CqcECqQEARFNMg8iZ3xVniKaulgBlzhJJDlc","functionCall":{"name":"bash","args":{"command":"ls -la .."},"id":"call_524881"}}],"role":"model"}}],"modelVersion":"gemini-3.7-flash","responseId":"resp-nofr","usageMetadata":{"promptTokenCount":153609,"candidatesTokenCount":17,"totalTokenCount":153720,"thoughtsTokenCount":94}}}
`
	splitTerminal := `data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"first"}]},"finishReason":"STOP"}],"modelVersion":"gemini-3.7-flash","responseId":"resp-split"},"traceId":"trace-split"}

data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":""}]}}],"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":22,"totalTokenCount":33},"modelVersion":"gemini-3.7-flash","responseId":"resp-split"},"traceId":"trace-split"}

`
	stream := func(body string) *upstream {
		return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body}
	}
	geminiHello := `{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}`
	openaiHello := `{"model":"gemini-3.7-flash","messages":[{"role":"user","content":"hello"}],"stream":true}`
	interactions := `{"model":"gemini-3.1-pro-low","input":[{"type":"user_input","content":[{"type":"text","text":"hi"}]}],"tools":[{"name":"get_weather","description":"weather","type":"function","parameters":{"type":"object","properties":{"location":{"type":"string"}},"required":["location"]}}],"generation_config":{"tool_choice":"auto","thinking_level":"high","thinking_summaries":"auto"},"stream":true,"store":false}`
	return []execScenario{
		{Name: "go_stream_no_finish_reason_gemini", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamSynthesizesGeminiTerminalChunkWithUsage"},
			Payload: geminiHello, Upstream: stream(noFinishReason)},
		{Name: "go_stream_no_finish_reason_openai", Op: "stream", Source: "openai", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamSynthesizesOpenAIToolCallsFinishReason"},
			Payload: openaiHello, Upstream: stream(noFinishReason)},
		{Name: "go_stream_empty_gemini", Op: "stream", Source: "gemini", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamDoesNotFinalizeEmptyUpstreamStream"},
			Payload: geminiHello, Upstream: stream("")},
		{Name: "go_stream_empty_openai", Op: "stream", Source: "openai", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamDoesNotFinalizeEmptyUpstreamStream"},
			Payload: openaiHello, Upstream: stream("")},
		{Name: "go_stream_claude_read_error", Op: "stream", Source: "claude", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamDoesNotEmitClaudeMessageStopOnReadError"},
			Payload: `{"model":"gemini-3.7-flash","max_tokens":256,"stream":true,"messages":[{"role":"user","content":"hello"}]}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Missing: 64,
				Body: `data: {"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"partial"}]}}],"modelVersion":"gemini-3.7-flash","responseId":"resp-cut"}}` + "\n\n"}},
		{Name: "go_stream_split_terminal_usage", Op: "stream", Source: "openai", Model: "gemini-3.7-flash",
			GoTests: []string{"TestAntigravityStreamFinalizesSplitTerminalUsageOnce", "TestAntigravityStreamReportsSplitTerminalUsage"},
			Payload: openaiHello, Upstream: stream(splitTerminal)},
		{Name: "go_stream_interactions", Op: "stream", Source: "interactions", Model: "gemini-3.1-pro-low", Original: interactions,
			GoTests:  []string{"TestAntigravityExecutorExecuteStreamTranslatesInteractionsRequest"},
			Payload:  interactions,
			Upstream: stream("data: {\"response\":{\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":1,\"candidatesTokenCount\":1,\"totalTokenCount\":2}}}\n\n")},
	}
}

func geminiDone(trace string) *upstream {
	return jsonOK(`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}]},"traceId":"` + trace + `"}`)
}

// Every scenario not listing its own credential uses the default one.
func defaultCredentials(all []execScenario) {
	for i := range all {
		s := &all[i]
		if s.AuthID == "" {
			s.AuthID = s.Name
		}
		if s.Metadata == nil {
			s.Metadata = map[string]any{
				"type":         "antigravity",
				"access_token": "ya29.fake-ag",
				"expired":      "2099-01-01T00:00:00Z",
				"project_id":   "proj-ag",
				"email":        "ag@example.com",
			}
		}
		if s.Attributes == nil {
			s.Attributes = map[string]string{"base_url": "http://UPSTREAM"}
		}
	}
}
