package executor

// Recorder for cliproxy-rs. record.py copies Go's own Claude test files into a scratch
// copy of CLIProxyAPI 6fecc6e and rewrites selected call sites to the rsfix* wrappers
// below. Each wrapper calls the real function and records its inputs and outputs,
// tagged with the Go test that made the call, so the Rust port can replay every case
// against Go's results. Go's own assertions still run unchanged.
//
// TestZZZRSFixWrite (last, by file order) writes the records to $RSFIX_OUT.

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"gopkg.in/yaml.v3"
)

var (
	rsfixMu      sync.Mutex
	rsfixRecords []map[string]any
)

// rsfixTest names the Go test (or fuzz seed run) on the calling goroutine's stack.
func rsfixTest() string {
	pcs := make([]uintptr, 128)
	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	for {
		frame, more := frames.Next()
		name := frame.Function[strings.LastIndex(frame.Function, "/")+1:]
		if parts := strings.Split(name, "."); len(parts) > 1 && parts[0] == "executor" &&
			(strings.HasPrefix(parts[1], "Test") || strings.HasPrefix(parts[1], "Fuzz")) {
			return parts[1]
		}
		if !more {
			return ""
		}
	}
}

func rsfixRecord(fn string, fields map[string]any) {
	fields["fn"] = fn
	fields["test"] = rsfixTest()
	rsfixMu.Lock()
	rsfixRecords = append(rsfixRecords, fields)
	rsfixMu.Unlock()
}

// rsfixBytes keeps invalid UTF-8 exactly (encoding/json would replace it).
func rsfixBytes(b []byte) any {
	if b == nil {
		return nil
	}
	if utf8.Valid(b) {
		return string(b)
	}
	return map[string]string{"b64": base64.StdEncoding.EncodeToString(b)}
}

func rsfixError(fields map[string]any, err error) map[string]any {
	if err != nil {
		fields["error"] = err.Error()
		var scoped cliproxyexecutor.RequestScopedError
		fields["request_scoped"] = errors.As(err, &scoped) && scoped.IsRequestScoped()
	}
	return fields
}

func rsfixClone(b []byte) []byte { return append([]byte(nil), b...) }

// --- MCP tool aliasing (claude_executor_request.go) ---

func rsfixRemapWithOptions(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string) {
	in := rsfixClone(body)
	out, reverse := remapOAuthToolNamesWithOptions(body, options)
	rsfixRecord("remap", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse})
	return out, reverse
}

func rsfixRemap(body []byte) ([]byte, map[string]string) {
	return rsfixRemapWithOptions(body, claudeMCPAliasOptions{secret: "cpa-claude-mcp-default-caller"})
}

func rsfixRemapLegacy(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string) {
	in := rsfixClone(body)
	out, reverse := remapOAuthToolNamesWithOptionsLegacy(body, options)
	rsfixRecord("remap", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse, "legacy": true})
	return out, reverse
}

func rsfixRemapBatched(body []byte, options claudeMCPAliasOptions) ([]byte, map[string]string, bool) {
	in := rsfixClone(body)
	out, reverse, ok := remapOAuthToolNamesWithBatchedEdits(body, options)
	rsfixRecord("remap_batched", map[string]any{"body": rsfixBytes(in), "secret": options.secret, "out": rsfixBytes(out), "reverse": reverse, "ok": ok})
	return out, reverse, ok
}

func rsfixRestore(body []byte, reverse map[string]string) ([]byte, error) {
	in := rsfixClone(body)
	out, err := reverseRemapOAuthToolNames(body, reverse)
	rsfixRecord("restore", rsfixError(map[string]any{"body": rsfixBytes(in), "reverse": reverse, "out": rsfixBytes(out)}, err))
	return out, err
}

func rsfixRestoreLine(line []byte, reverse map[string]string) ([]byte, error) {
	in := rsfixClone(line)
	out, err := reverseRemapOAuthToolNamesFromStreamLine(line, reverse)
	rsfixRecord("restore_line", rsfixError(map[string]any{"line": rsfixBytes(in), "reverse": reverse, "out": rsfixBytes(out)}, err))
	return out, err
}

func rsfixParseAlias(name string) (claudeMCPAliasParts, bool) {
	parts, ok := parseClaudeMCPAlias(name)
	rsfixRecord("parse_alias", map[string]any{"name": name, "ok": ok, "server": parts.server, "tool_id": parts.toolID, "semantic": parts.semantic})
	return parts, ok
}

// --- Executor entry points (Execute, ExecuteStream, CountTokens) ---

// rsfixBuffer collects a body while the executor reads it.
type rsfixBuffer struct {
	mu        sync.Mutex
	buf       bytes.Buffer
	readError string
}

func (b *rsfixBuffer) MarshalJSON() ([]byte, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return json.Marshal(rsfixBytes(append([]byte{}, b.buf.Bytes()...)))
}

type rsfixTee struct {
	body io.ReadCloser
	buf  *rsfixBuffer
}

func (t *rsfixTee) Read(p []byte) (int, error) {
	n, err := t.body.Read(p)
	t.buf.mu.Lock()
	t.buf.buf.Write(p[:n])
	// The first body read error other than EOF: the replay ends its body the same way
	// (a short Content-Length body or an unterminated chunked one).
	if err != nil && err != io.EOF && t.buf.readError == "" {
		t.buf.readError = err.Error()
	}
	t.buf.mu.Unlock()
	return n, err
}

func (t *rsfixTee) Close() error { return t.body.Close() }

type rsfixExchange struct {
	Method          string              `json:"method"`
	URL             string              `json:"url"`
	Host            string              `json:"host"`
	Headers         map[string][]string `json:"headers"`
	Body            any                 `json:"body"`
	Status          int                 `json:"status"`
	ResponseHeaders map[string][]string `json:"response_headers"`
	ResponseBody    *rsfixBuffer        `json:"response_body"`
	Error           string              `json:"error,omitempty"`
}

// MarshalJSON adds the body read error (read_error) to the exchange's fields.
func (e *rsfixExchange) MarshalJSON() ([]byte, error) {
	type plain rsfixExchange
	e.ResponseBody.mu.Lock()
	readError := e.ResponseBody.readError
	e.ResponseBody.mu.Unlock()
	return json.Marshal(struct {
		*plain
		ReadError string `json:"read_error,omitempty"`
	}{(*plain)(e), readError})
}

type rsfixCall struct {
	mu        sync.Mutex
	exchanges []*rsfixExchange
	chunks    []map[string]any
}

func (c *rsfixCall) MarshalJSON() ([]byte, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	return json.Marshal(map[string]any{"exchanges": c.exchanges, "chunks": c.chunks})
}

// rsfixRT records every upstream exchange, then forwards to the round tripper the test
// installed (or Go's default transport, which the executor uses for local servers).
type rsfixRT struct {
	inner http.RoundTripper
	call  *rsfixCall
}

func (r *rsfixRT) RoundTrip(req *http.Request) (*http.Response, error) {
	var body []byte
	if req.Body != nil {
		body, _ = io.ReadAll(req.Body)
		_ = req.Body.Close()
		req.Body = io.NopCloser(bytes.NewReader(body))
	}
	exchange := &rsfixExchange{Method: req.Method, URL: req.URL.String(), Host: req.Host, Headers: req.Header.Clone(), Body: rsfixBytes(body), ResponseBody: &rsfixBuffer{}}
	r.call.mu.Lock()
	r.call.exchanges = append(r.call.exchanges, exchange)
	r.call.mu.Unlock()
	resp, err := r.inner.RoundTrip(req)
	if err != nil {
		exchange.Error = err.Error()
		return resp, err
	}
	exchange.Status = resp.StatusCode
	exchange.ResponseHeaders = resp.Header.Clone()
	resp.Body = &rsfixTee{body: resp.Body, buf: exchange.ResponseBody}
	return resp, nil
}

func rsfixJSON(v any) any {
	raw, err := json.Marshal(v)
	if err != nil {
		return map[string]string{"unmarshalable": err.Error()}
	}
	var out any
	_ = json.Unmarshal(raw, &out)
	return out
}

// rsfixModelInfo adds the json:"-" fields executors read under cpa_core's raw keys.
func rsfixModelInfo(info *registry.ModelInfo) any {
	if info == nil {
		return nil
	}
	out, _ := rsfixJSON(info).(map[string]any)
	out["is_compat"] = info.IsCompat
	out["user_defined"] = info.UserDefined
	out["support_configuration_update"] = info.SupportConfigurationUpdate
	return out
}

func rsfixMetadata(metadata map[string]any) any {
	out := map[string]any{}
	for key, value := range metadata {
		if info, ok := value.(*registry.ModelInfo); ok {
			out[key] = rsfixModelInfo(info)
			continue
		}
		out[key] = rsfixJSON(value)
	}
	return out
}

func rsfixErrorInfo(err error) any {
	if err == nil {
		return nil
	}
	info := map[string]any{"message": err.Error()}
	var status interface{ StatusCode() int }
	if errors.As(err, &status) {
		info["status"] = status.StatusCode()
	}
	var request interface{ IsRequestScoped() bool }
	if errors.As(err, &request) {
		info["request_scoped"] = request.IsRequestScoped()
	}
	var credential interface{ IsCredentialScoped() bool }
	if errors.As(err, &credential) {
		info["credential_scoped"] = credential.IsCredentialScoped()
	}
	var retry interface{ RetryAfter() *time.Duration }
	if errors.As(err, &retry) && retry.RetryAfter() != nil {
		info["retry_after_ms"] = retry.RetryAfter().Milliseconds()
	}
	var terminated *cliproxyexecutor.RequestTerminatedError
	if errors.As(err, &terminated) && terminated != nil {
		info["direct_status"] = terminated.HTTPStatus
		info["direct_body"] = rsfixBytes(terminated.Body)
	}
	return info
}

// rsfixBegin records the inputs of one executor call and installs the exchange recorder.
func rsfixBegin(kind string, e *ClaudeExecutor, ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (context.Context, map[string]any, *rsfixCall) {
	call := &rsfixCall{}
	inner, _ := ctx.Value("cliproxy.roundtripper").(http.RoundTripper)
	if inner == nil {
		inner = http.DefaultTransport
	}
	ctx = context.WithValue(ctx, "cliproxy.roundtripper", http.RoundTripper(&rsfixRT{inner: inner, call: call}))
	var configYAML string
	if e.cfg != nil {
		configYAML = rsfixConfigYAML(e.cfg)
	}
	var authRecord any
	if auth != nil {
		authRecord = map[string]any{
			"id": auth.ID, "provider": auth.Provider, "label": auth.Label, "prefix": auth.Prefix,
			"proxy_url": auth.ProxyURL, "disabled": auth.Disabled,
			// Copied now: tests mutate the credential between calls.
			"attributes": rsfixJSON(auth.Attributes), "metadata": rsfixJSON(auth.Metadata),
		}
	}
	record := map[string]any{
		"kind":     kind,
		"config":   configYAML,
		"executor": map[string]any{"has_model_normalizer": e.upstreamModelNormalizer != nil, "has_profile_fetcher": e.oauthProfileFetcher != nil},
		"auth":     authRecord,
		"request": map[string]any{
			"model": req.Model, "payload": rsfixBytes(req.Payload), "format": req.Format.String(),
			"metadata": rsfixMetadata(req.Metadata),
		},
		"options": map[string]any{
			"stream": opts.Stream, "alt": opts.Alt, "source_format": opts.SourceFormat.String(),
			"response_format": opts.ResponseFormat.String(), "original_request": rsfixBytes(opts.OriginalRequest),
			"metadata": rsfixMetadata(opts.Metadata),
		},
		// What the executor reads from the context: gin headers merged with
		// opts.Headers, and the caller key that seeds MCP aliases.
		"headers":     resolveIncomingClaudeHeaders(ctx, opts.Headers),
		"caller":      strings.TrimSpace(helps.APIKeyFromContext(ctx)),
		"date":        claudeCodeCurrentTime(e.cfg, auth).Format("2006-01-02"),
		"proxy":       (auth != nil && strings.TrimSpace(auth.ProxyURL) != "") || (e.cfg != nil && strings.TrimSpace(e.cfg.ProxyURL) != ""),
		"exchanges":   call,
		"test_helper": ctx.Value("rsfix.helper"),
	}
	rsfixRecord(kind, record)
	return ctx, record, call
}

// rsfixConfigYAML is the test's config struct as YAML without zero-valued leaves.
// Go's tests build config.Config literals whose zero fields the executor reads as
// unset; written out, zeros such as snapshot-interval: 0 would be rejected by a loader.
func rsfixConfigYAML(cfg *config.Config) string {
	copied := *cfg
	copied.Payload = config.PayloadConfig{
		Default:     rsfixPayloadRules(cfg.Payload.Default, false),
		DefaultRaw:  rsfixPayloadRules(cfg.Payload.DefaultRaw, true),
		Override:    rsfixPayloadRules(cfg.Payload.Override, false),
		OverrideRaw: rsfixPayloadRules(cfg.Payload.OverrideRaw, true),
		Filter:      cfg.Payload.Filter,
	}
	raw, err := yaml.Marshal(&copied)
	if err != nil {
		return "unmarshalable: " + err.Error()
	}
	var tree any
	if err := yaml.Unmarshal(raw, &tree); err != nil {
		return "unmarshalable: " + err.Error()
	}
	pruned, _ := rsfixPrune(tree)
	if pruned == nil {
		return ""
	}
	out, _ := yaml.Marshal(pruned)
	return string(out)
}

// rsfixPayloadRules copies payload rules with JSON byte values written the way a config
// file holds them: text for raw rules, and for the others the JSON value that sjson
// writes for a json.RawMessage.
func rsfixPayloadRules(rules []config.PayloadRule, raw bool) []config.PayloadRule {
	out := make([]config.PayloadRule, len(rules))
	for i, rule := range rules {
		out[i] = rule
		out[i].Params = map[string]any{}
		for key, value := range rule.Params {
			var data []byte
			switch v := value.(type) {
			case json.RawMessage:
				data = v
			case []byte:
				data = v
			default:
				out[i].Params[key] = value
				continue
			}
			if raw {
				out[i].Params[key] = string(data)
				continue
			}
			var decoded any
			if json.Unmarshal(data, &decoded) == nil {
				out[i].Params[key] = decoded
			} else {
				out[i].Params[key] = string(data)
			}
		}
	}
	return out
}

func rsfixPrune(v any) (any, bool) {
	switch value := v.(type) {
	case map[string]any:
		out := map[string]any{}
		for key, child := range value {
			if pruned, keep := rsfixPrune(child); keep {
				out[key] = pruned
			}
		}
		return out, len(out) > 0
	case []any:
		if len(value) == 0 {
			return nil, false
		}
		out := make([]any, len(value))
		for i, child := range value {
			pruned, keep := rsfixPrune(child)
			if !keep {
				pruned = map[string]any{}
				if _, isMap := child.(map[string]any); !isMap {
					pruned = child
				}
			}
			out[i] = pruned
		}
		return out, true
	case nil:
		return nil, false
	case bool:
		return value, value
	case int:
		return value, value != 0
	case float64:
		return value, value != 0
	case string:
		return value, value != ""
	default:
		return value, true
	}
}

type rsfixExecutor interface {
	Execute(context.Context, *cliproxyauth.Auth, cliproxyexecutor.Request, cliproxyexecutor.Options) (cliproxyexecutor.Response, error)
}

type rsfixStreamExecutor interface {
	ExecuteStream(context.Context, *cliproxyauth.Auth, cliproxyexecutor.Request, cliproxyexecutor.Options) (*cliproxyexecutor.StreamResult, error)
}

type rsfixCountExecutor interface {
	CountTokens(context.Context, *cliproxyauth.Auth, cliproxyexecutor.Request, cliproxyexecutor.Options) (cliproxyexecutor.Response, error)
}

func rsfixResponse(record map[string]any, resp cliproxyexecutor.Response, err error) {
	rsfixMu.Lock()
	defer rsfixMu.Unlock()
	record["result"] = map[string]any{"payload": rsfixBytes(resp.Payload), "headers": resp.Headers, "error": rsfixErrorInfo(err)}
}

func rsfixExecute(e rsfixExecutor, ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	claude, ok := e.(*ClaudeExecutor)
	if !ok || claude == nil {
		return e.Execute(ctx, auth, req, opts)
	}
	ctx, record, _ := rsfixBegin("execute", claude, ctx, auth, req, opts)
	resp, err := claude.Execute(ctx, auth, req, opts)
	rsfixResponse(record, resp, err)
	return resp, err
}

func rsfixCountTokens(e rsfixCountExecutor, ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (cliproxyexecutor.Response, error) {
	claude, ok := e.(*ClaudeExecutor)
	if !ok || claude == nil {
		return e.CountTokens(ctx, auth, req, opts)
	}
	ctx, record, _ := rsfixBegin("count_tokens", claude, ctx, auth, req, opts)
	resp, err := claude.CountTokens(ctx, auth, req, opts)
	rsfixResponse(record, resp, err)
	return resp, err
}

func rsfixExecuteStream(e rsfixStreamExecutor, ctx context.Context, auth *cliproxyauth.Auth, req cliproxyexecutor.Request, opts cliproxyexecutor.Options) (*cliproxyexecutor.StreamResult, error) {
	claude, ok := e.(*ClaudeExecutor)
	if !ok || claude == nil {
		return e.ExecuteStream(ctx, auth, req, opts)
	}
	ctx, record, call := rsfixBegin("execute_stream", claude, ctx, auth, req, opts)
	result, err := claude.ExecuteStream(ctx, auth, req, opts)
	rsfixMu.Lock()
	info := map[string]any{"error": rsfixErrorInfo(err)}
	if result != nil {
		info["headers"] = result.Headers
	}
	record["result"] = info
	rsfixMu.Unlock()
	if err != nil || result == nil {
		return result, err
	}
	out := make(chan cliproxyexecutor.StreamChunk)
	// The executor's cancellation chunk is a non-blocking send: it lands only while
	// someone receives, so the forwarder must be running before the caller proceeds.
	ready := make(chan struct{})
	go func() {
		defer close(out)
		close(ready)
		for chunk := range result.Chunks {
			call.mu.Lock()
			call.chunks = append(call.chunks, map[string]any{"payload": rsfixBytes(chunk.Payload), "error": rsfixErrorInfo(chunk.Err)})
			call.mu.Unlock()
			out <- chunk
		}
	}()
	<-ready
	return &cliproxyexecutor.StreamResult{Headers: result.Headers, Chunks: out}, nil
}

func TestZZZRSFixWrite(t *testing.T) {
	out := os.Getenv("RSFIX_OUT")
	if out == "" {
		t.Skip("RSFIX_OUT not set")
	}
	rsfixMu.Lock()
	defer rsfixMu.Unlock()
	data, err := json.MarshalIndent(rsfixRecords, "", " ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
