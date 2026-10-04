package executor

import (
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"

	"github.com/gin-gonic/gin"
	"net/url"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/buildinfo"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	sdkAuth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type rsfixRedirect struct{ host string }

func (r rsfixRedirect) RoundTrip(req *http.Request) (*http.Response, error) {
	clone := req.Clone(req.Context())
	clone.URL.Scheme = "http"
	clone.URL.Host = r.host
	if clone.Host == "" {
		clone.Host = req.URL.Host
	}
	return rsfixBaseTransport.RoundTrip(clone)
}

var rsfixBaseTransport = http.DefaultTransport.(*http.Transport).Clone()

type rsfixExecCase struct {
	name      string
	source    sdktranslator.Format
	model     string
	stream    bool
	alt       string
	count     bool
	body      string
	meta      map[string]any
	headers   http.Header
	responses []rsfixResponse
	// cfg is config.yaml text for the executor (payload rules); empty is a zero config.
	cfg string
}

func rsfixRunExecutor(t *testing.T, provider string, exec cliproxyauth.ProviderExecutor, tc rsfixExecCase, baseKey string, basePath string) {
	srv := newRSFixServer(t, false, tc.responses...)
	meta := map[string]any{}
	for k, v := range tc.meta {
		meta[k] = v
	}
	if baseKey != "" {
		meta[baseKey] = srv.URL() + basePath
	}
	recorded := map[string]any{}
	for k, v := range meta {
		recorded[k] = v
	}
	auth := &cliproxyauth.Auth{ID: provider + "-fixture.json", Provider: provider, Label: rsfixLabel(provider, meta, nil), Attributes: map[string]string{}, Metadata: meta}
	req := cliproxyexecutor.Request{Model: tc.model, Payload: []byte(tc.body)}
	opts := cliproxyexecutor.Options{SourceFormat: tc.source, Stream: tc.stream, Alt: tc.alt, OriginalRequest: []byte(tc.body), Headers: tc.headers}
	ctx, takeCapture := rsfixCapture(context.Background(), tc.headers, srv.URL())
	rsfixResetUsage()
	var down rsfixDownstream
	switch {
	case tc.count:
		resp, err := exec.CountTokens(ctx, auth, req, opts)
		down.ErrStatus, down.ErrBody = rsfixStatus(err)
		down.Body = string(resp.Payload)
	case tc.stream:
		result, err := exec.ExecuteStream(ctx, auth, req, opts)
		down.ErrStatus, down.ErrBody = rsfixStatus(err)
		if result != nil {
			for chunk := range result.Chunks {
				if chunk.Err != nil {
					status, msg := rsfixStatus(chunk.Err)
					down.StreamErr = msg
					if status > 0 {
						down.ErrStatus = status
					}
					continue
				}
				down.Chunks = append(down.Chunks, string(chunk.Payload))
			}
		}
	default:
		resp, err := exec.Execute(ctx, auth, req, opts)
		down.ErrStatus, down.ErrBody = rsfixStatus(err)
		down.Body = string(resp.Payload)
	}
	request := map[string]any{"source": tc.source.String(), "model": tc.model, "stream": tc.stream, "body": tc.body}
	if tc.cfg != "" {
		request["config"] = tc.cfg
	}
	if tc.alt != "" {
		request["alt"] = tc.alt
	}
	if tc.count {
		request["count"] = true
	}
	if len(tc.headers) > 0 {
		var hs [][2]string
		for k, vs := range tc.headers {
			hs = append(hs, [2]string{k, vs[0]})
		}
		request["headers"] = hs
	}
	rsfixWrite(t, provider, rsfixFixture{
		Name:       tc.name,
		Credential: recorded,
		Request:    request,
		Responses:  tc.responses,
		Upstream:   srv.Captured(),
		Downstream: down,
		Extra:      map[string]any{"usage": rsfixTakeUsage(), "capture": takeCapture()},
	})
}

func jsonResp(status int, body string, extra ...[2]string) rsfixResponse {
	return rsfixResponse{Status: status, Headers: append([][2]string{{"Content-Type", "application/json"}}, extra...), Body: body}
}

func sseResp(body string) rsfixResponse {
	return rsfixResponse{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body}
}

func TestRSFixKimi(t *testing.T) {
	rsfixOut(t)
	buildinfo.Version = "0.1.0"
	kimiMeta := map[string]any{"type": "kimi", "access_token": "kimi-access-fixture", "refresh_token": "kimi-refresh-fixture", "device_id": "dev-fixture-1"}
	cases := []rsfixExecCase{
		{
			name: "chat-nonstream-normalize", source: sdktranslator.FormatOpenAI, model: "kimi-k2.5", meta: kimiMeta,
			body:      `{"model":"kimi-k2.5","temperature":0.7,"reasoning_effort":"high","messages":[{"role":"system","content":"sys"},{"role":"user","content":"hi"},{"role":"assistant","content":"","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]},{"role":"tool","call_id":"call_1","content":"result"},{"role":"assistant","content":""},{"role":"user","content":"next"}],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"properties":{"q":{"$ref":"#/$defs/Q"}},"$defs":{"Q":{"type":"string","description":"query"}}}}}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"kimi-k2.5","choices":[{"index":0,"message":{"role":"assistant","content":"done","reasoning_content":"thought"},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}`)},
		},
		{
			name: "chat-stream-suffix", source: sdktranslator.FormatOpenAI, model: "kimi-k2.8(max)", stream: true, meta: kimiMeta,
			body:      `{"model":"kimi-k2.8(max)","stream":true,"temperature":1,"messages":[{"role":"user","content":"hi"}],"functions":[{"name":"legacy","parameters":{"properties":{"a":{"type":"string"}}}}]}`,
			responses: []rsfixResponse{sseResp("data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"kimi-for-coding\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"r\"}}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"kimi-for-coding\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hel\"}}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"kimi-for-coding\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n")},
		},
		{
			name: "chat-error-429-clamped-none", source: sdktranslator.FormatOpenAI, model: "kimi-k2.7-code", meta: kimiMeta,
			body:      `{"model":"kimi-k2.7-code","reasoning_effort":"none","temperature":0.6,"messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{jsonResp(429, `{"error":{"message":"rate limited","type":"rate_limit_error"}}`, [2]string{"Retry-After", "7"})},
		},
		{
			name: "chat-disabled-thinking-temperature", source: sdktranslator.FormatOpenAI, model: "kimi-k2.6", meta: kimiMeta,
			body:      `{"model":"kimi-k2.6","temperature":0.6,"thinking":{"type":"disabled"},"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"prev","reasoning_content":"[reasoning unavailable]","tool_calls":[{"id":"a","type":"function","function":{"name":"f","arguments":"{}"}},{"id":"b","type":"function","function":{"name":"f","arguments":"{}"}}]},{"role":"tool","content":"x"},{"role":"tool","tool_call_id":"b","content":"y"}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"c2","object":"chat.completion","created":2,"model":"kimi-k2.6","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}`)},
		},
		{
			name: "responses-nonstream-reorder-suffix", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3(high)", meta: kimiMeta,
			body:      `{"model":"kimi-k3(high)","temperature":0.5,"reasoning":{"effort":"low","summary":"auto"},"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"function_call","call_id":"call_a","name":"f","arguments":"{}"},{"type":"function_call","call_id":"call_b","name":"f","arguments":"{}"},{"type":"message","role":"developer","content":[{"type":"input_text","text":"interjection"}]},{"type":"function_call_output","call_id":"call_a","output":"A"},{"type":"function_call_output","call_id":"call_b","output":"B"}],"tools":[{"type":"function","name":"f","parameters":{"properties":{"x":{"$ref":"#/definitions/X"}},"definitions":{"X":{"type":"integer"}}}}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"resp_1","object":"response","status":"completed","model":"k3","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}],"usage":{"input_tokens":6,"output_tokens":4,"total_tokens":10}}`)},
		},
		{
			name: "responses-stream-clamp", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      `{"model":"kimi-k3","stream":true,"reasoning":{"effort":"xhigh"},"input":"hi"}`,
			responses: []rsfixResponse{sseResp("event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_2\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n")},
		},
		{
			name: "chat-stream-gemini-client", source: sdktranslator.FormatGemini, model: "kimi-k2", stream: true, meta: kimiMeta,
			body:      `{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}`,
			responses: []rsfixResponse{sseResp("data: {\"id\":\"g1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"k2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"he\"}}]}\n\ndata: {\"id\":\"g1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"k2\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"y\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":2,\"total_tokens\":4}}\n\n")},
		},
		{
			name: "chat-stream-interactions-client", source: sdktranslator.FormatInteractions, model: "kimi-k2", stream: true, meta: kimiMeta,
			body:      `{"model":"kimi-k2","input":"hi"}`,
			responses: []rsfixResponse{sseResp("data: {\"id\":\"i1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"k2\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"yo\"},\"finish_reason\":\"stop\"}]}\n\n")},
		},
		{
			name: "chat-nonstream-gemini-client", source: sdktranslator.FormatGemini, model: "kimi-k2", meta: kimiMeta,
			body:      `{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"g2","object":"chat.completion","created":2,"model":"k2","choices":[{"index":0,"message":{"role":"assistant","content":"hey"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}`)},
		},
		{
			name: "chat-payload-rules", source: sdktranslator.FormatOpenAI, model: "kimi-k3(high)", stream: true, meta: kimiMeta, cfg: "payload:\n  default:\n    - models: [{name: \"kimi-*\", protocol: openai}]\n      params: {temperature: 1.0, n: 1, user: \"default-user\"}\n    - models: [{name: \"kimi-*\", protocol: openai-response}]\n      params: {store: true, parallel_tool_calls: false}\n  override:\n    - models: [{name: \"kimi-k3(high)\"}]\n      params: {requested_hit: true}\n    - models: [{name: \"kimi-*\", protocol: codex}]\n      params: {wrong_protocol: true}\n    - models: [{name: \"kimi-*\", from-protocol: openai}]\n      params: {from_openai: true}\n  filter:\n    - models: [{name: \"kimi-*\", protocol: openai}]\n      params: [max_tokens]\n",
			body:      `{"model":"kimi-k3(high)","messages":[{"role":"user","content":"hi"}],"temperature":0.3,"max_tokens":50}`,
			responses: []rsfixResponse{sseResp("data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"k3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")},
		},
		{
			name: "responses-payload-rules", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3(high)", meta: kimiMeta, cfg: "payload:\n  default:\n    - models: [{name: \"kimi-*\", protocol: openai}]\n      params: {temperature: 1.0, n: 1, user: \"default-user\"}\n    - models: [{name: \"kimi-*\", protocol: openai-response}]\n      params: {store: true, parallel_tool_calls: false}\n  override:\n    - models: [{name: \"kimi-k3(high)\"}]\n      params: {requested_hit: true}\n    - models: [{name: \"kimi-*\", protocol: codex}]\n      params: {wrong_protocol: true}\n    - models: [{name: \"kimi-*\", from-protocol: openai}]\n      params: {from_openai: true}\n  filter:\n    - models: [{name: \"kimi-*\", protocol: openai}]\n      params: [max_tokens]\n",
			body:      `{"model":"kimi-k3(high)","input":"hi","store":false}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"r","object":"response","status":"completed","output":[]}`)},
		},
		{
			name: "responses-stream-data-only-frames", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      `{"model":"kimi-k3","stream":true,"input":"hi"}`,
			responses: []rsfixResponse{sseResp("data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"b\"}\n\ndata: [DONE]\n: keep\ndata: {\"type\":")},
		},
		{
			name: "responses-apply-patch-stream", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{sseResp(rsfixPatchSSE("k3", true))},
		},
		{
			name: "responses-apply-patch-nonstream", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{jsonResp(200, rsfixPatchResponse(true))},
		},
		{
			name: "responses-apply-patch-invalid-stream", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{sseResp(rsfixPatchSSE("k3", false))},
		},
		{
			name: "responses-apply-patch-invalid-nonstream", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{jsonResp(200, rsfixPatchResponse(false))},
		},
		{
			name: "responses-apply-patch-eof-stream", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{sseResp(rsfixPatchEOF())},
		},
		{
			name: "responses-nonstream-nested-usage", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", meta: kimiMeta,
			body:      `{"model":"kimi-k3","input":"hi"}`,
			responses: []rsfixResponse{jsonResp(200, `{"type":"response.completed","response":{"id":"r9","status":"completed","model":"k3-nested","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"x"}]}],"usage":{"input_tokens":3,"output_tokens":2,"total_tokens":5}}}`)},
		},
		{
			name: "responses-stream-incomplete-then-done", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body: `{"model":"kimi-k3","stream":true,"input":"hi"}`,
			responses: []rsfixResponse{sseResp("data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"r\",\"model\":\"k3-a\",\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n" +
				"data: {\"type\":\"response.done\",\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":0}}\n\n" +
				"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"total_tokens\":0}}}\n\n" +
				"{\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"usage\":{\"input_tokens\":50,\"output_tokens\":50,\"total_tokens\":100}}}\n\n")},
		},
		{
			name: "responses-apply-patch-stream-event-lines", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body:      rsfixKimiPatchBody,
			responses: []rsfixResponse{sseResp(rsfixPatchSSEMixed("k3"))},
		},
		{
			name: "responses-stream-tier-merge", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", stream: true, meta: kimiMeta,
			body: `{"model":"kimi-k3","stream":true,"input":"hi"}`,
			responses: []rsfixResponse{sseResp(`data: {"type":"response.incomplete","response":{"id":"r","service_tier":"priority","usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}}}` + "\n\n" +
				`data: {"type":"response.completed","response":{"id":"r","usage":{"input_tokens":7,"output_tokens":2,"total_tokens":9}}}` + "\n\n")},
		},
		{
			name: "chat-stream-usage-then-model", source: sdktranslator.FormatOpenAI, model: "kimi-k2", stream: true, meta: kimiMeta,
			body: `{"model":"kimi-k2","stream":true,"messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{sseResp(`data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m1","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":0}}` + "\n\n" +
				`data: {"id":"c","object":"chat.completion.chunk","created":1,"model":"m2","choices":[]}` + "\n\ndata: [DONE]\n\n")},
		},
		{
			name: "responses-compact-rejected", source: sdktranslator.FormatOpenAIResponse, model: "kimi-k3", alt: "responses/compact", meta: kimiMeta,
			body: `{"model":"kimi-k3","input":"hi"}`,
		},
		{
			name: "claude-nonstream-delegated", source: sdktranslator.FormatClaude, model: "kimi-k3", meta: kimiMeta,
			body:      `{"model":"kimi-k3","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"msg_1","type":"message","role":"assistant","model":"k3","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}`)},
		},
		{
			name: "claude-count-tokens-upstream", source: sdktranslator.FormatClaude, model: "kimi-k2.8", count: true, meta: kimiMeta,
			body:      `{"model":"kimi-k2.8","messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{jsonResp(200, `{"input_tokens":9}`)},
		},
		{
			name: "chat-kimi-ai-metadata-base", source: sdktranslator.FormatOpenAI, model: "kimi-k2", meta: map[string]any{"type": "kimi-ai", "access_token": "kimi-ai-access", "domain": "kimi.ai"},
			body:      `{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}`,
			responses: []rsfixResponse{jsonResp(200, `{"id":"c3","object":"chat.completion","created":3,"model":"kimi-k2","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}`)},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := &config.Config{}
			if tc.cfg != "" {
				parsed, err := config.ParseConfigBytes([]byte(tc.cfg))
				if err != nil {
					t.Fatal(err)
				}
				cfg = parsed
			}
			cfg.RequestLog = true
			rsfixRunExecutor(t, "kimi", NewKimiExecutor(cfg), tc, "base_url", "/coding")
		})
	}
}

func TestRSFixKimiRefresh(t *testing.T) {
	rsfixOut(t)
	buildinfo.Version = "0.1.0"
	for _, tc := range []struct {
		name string
		meta map[string]any
		resp rsfixResponse
	}{
		{"refresh-rotates", map[string]any{"type": "kimi", "access_token": "old", "refresh_token": "kimi-refresh-1", "device_id": "dev-fixture-1", "custom": "kept"}, jsonResp(200, `{"access_token":"new-access","refresh_token":"new-refresh","token_type":"Bearer","expires_in":3600,"scope":"kimi-code"}`)},
		{"refresh-ai-keeps-refresh", map[string]any{"type": "kimi-ai", "access_token": "old", "refresh_token": "kimi-refresh-2"}, jsonResp(200, `{"access_token":"new-access-2","token_type":"Bearer","expires_in":0}`)},
		{"refresh-rejected", map[string]any{"type": "kimi", "access_token": "old", "refresh_token": "kimi-refresh-3"}, jsonResp(401, `{"error":"invalid_grant"}`)},
		{"refresh-500", map[string]any{"type": "kimi", "access_token": "old", "refresh_token": "kimi-refresh-4"}, jsonResp(500, `{"error":"boom"}`)},
	} {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, false, tc.resp)
			ctx := context.WithValue(context.Background(), "cliproxy.roundtripper", http.RoundTripper(rsfixRedirect{host: srv.ln.Addr().String()}))
			meta := map[string]any{}
			for k, v := range tc.meta {
				meta[k] = v
			}
			auth := &cliproxyauth.Auth{ID: "kimi-fixture.json", Provider: tc.meta["type"].(string), Attributes: map[string]string{}, Metadata: meta}
			before := time.Now().Unix()
			updated, err := NewKimiExecutor(&config.Config{}).Refresh(ctx, auth)
			var down rsfixDownstream
			down.ErrStatus, down.ErrBody = rsfixStatus(err)
			extra := map[string]any{"before_unix": before}
			if updated != nil {
				extra["metadata"] = updated.Metadata
			}
			rsfixWrite(t, "kimi", rsfixFixture{Name: tc.name, Credential: tc.meta, Request: map[string]any{"refresh": true}, Responses: []rsfixResponse{tc.resp}, Upstream: srv.Captured(), Downstream: down, Extra: extra})
		})
	}
}

func TestRSFixKimiLogin(t *testing.T) {
	out := rsfixOut(t)
	buildinfo.Version = "0.1.0"
	for _, tc := range []struct {
		name     string
		provider string
	}{{"login-kimi", "kimi"}, {"login-kimi-ai", "kimi-ai"}} {
		t.Run(tc.name, func(t *testing.T) {
			responses := []rsfixResponse{
				jsonResp(200, `{"device_code":"dev-code-1","user_code":"ABCD-EFGH","verification_uri":"https://www.kimi.com/device","verification_uri_complete":"https://www.kimi.com/device?code=ABCD-EFGH","expires_in":600,"interval":1}`),
				jsonResp(200, `{"error":"authorization_pending","error_description":"pending"}`),
				jsonResp(200, `{"access_token":"login-access","refresh_token":"login-refresh","token_type":"Bearer","expires_in":3600,"scope":"kimi-code"}`),
			}
			srv := newRSFixServer(t, false, responses...)
			previous := http.DefaultTransport
			http.DefaultTransport = rsfixRedirect{host: srv.ln.Addr().String()}
			defer func() { http.DefaultTransport = previous }()
			authDir := t.TempDir()
			manager := sdkAuth.NewManager(sdkAuth.NewFileTokenStore(), sdkAuth.NewKimiAuthenticator(), sdkAuth.NewKimiAIAuthenticator())
			before := time.Now()
			record, saved, err := manager.Login(context.Background(), tc.provider, &config.Config{AuthDir: authDir}, &sdkAuth.LoginOptions{NoBrowser: true, Metadata: map[string]string{}})
			if err != nil {
				t.Fatal(err)
			}
			raw, _ := os.ReadFile(saved)
			var parsed map[string]any
			_ = json.Unmarshal(raw, &parsed)
			rsfixWrite(t, "kimi", rsfixFixture{Name: tc.name, Credential: map[string]any{}, Request: map[string]any{"login": tc.provider}, Responses: responses, Upstream: srv.Captured(), Extra: map[string]any{
				"file_name": filepath.Base(saved), "file_raw": string(raw), "file": parsed, "label": record.Label, "before_unix_ms": before.UnixMilli(),
			}})
			_ = url.URL{}
			_ = out
		})
	}
}

func TestRSFixKimiReplay(t *testing.T) {
	rsfixOut(t)
	buildinfo.Version = "0.1.0"
	signed := `{"id":"msg_1","type":"message","role":"assistant","model":"k3","content":[{"type":"thinking","thinking":"plan","signature":"sig-1"},{"type":"text","text":"Calling."},{"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a"}}],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":1}}`
	streamed := "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"k3\",\"content\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n" +
		"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n" +
		"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"again\"}}\n\n" +
		"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-2\"}}\n\n" +
		"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n" +
		"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"read\",\"input\":{}}}\n\n" +
		"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"b\\\"}\"}}\n\n" +
		"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n" +
		"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":2}}\n\n" +
		"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
	history := func(text, id, path string) string {
		textBlock := ""
		if text != "" {
			textBlock = `{"type":"text","text":"` + text + `"},`
		}
		return `{"model":"M","max_tokens":64,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[` + textBlock + `{"type":"tool_use","id":"` + id + `","name":"read","input":{"path":"` + path + `"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"` + id + `","content":"x"}]}]}`
	}
	ok := jsonResp(200, `{"id":"msg_x","type":"message","role":"assistant","model":"k3","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":1}}`)
	steps := []struct {
		Model  string        `json:"model"`
		Stream bool          `json:"stream"`
		Body   string        `json:"body"`
		Resp   rsfixResponse `json:"response"`
	}{
		{"kimi-k3", false, `{"model":"M","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`, jsonResp(200, signed)},
		{"kimi-k3-256k", false, history("Calling.", "toolu_1", "a"), ok},
		{"kimi-k3", true, `{"model":"M","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"go"}]}`, sseResp(streamed)},
		{"kimi-k3", false, history("", "toolu_2", "b"), jsonResp(400, `{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}`)},
		{"kimi-k3", false, history("", "toolu_2", "b"), ok},
		{"kimi-k2.5", false, history("Calling.", "toolu_1", "a"), ok},
	}
	var responses []rsfixResponse
	for _, s := range steps {
		responses = append(responses, s.Resp)
	}
	srv := newRSFixServer(t, false, responses...)
	exec := NewKimiExecutor(&config.Config{})
	meta := map[string]any{"type": "kimi", "access_token": "kimi-access-fixture", "base_url": srv.URL() + "/coding"}
	var down []rsfixDownstream
	for _, s := range steps {
		gin.SetMode(gin.TestMode)
		ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
		ginCtx.Request = httptest.NewRequest(http.MethodPost, "/v1/messages", nil)
		ginCtx.Request.Header.Set("X-Claude-Code-Session-Id", "sess-1")
		ginCtx.Set("userApiKey", "client-key-1")
		ctx := context.WithValue(context.Background(), "gin", ginCtx)
		body := strings.Replace(s.Body, `"M"`, `"`+s.Model+`"`, 1)
		auth := &cliproxyauth.Auth{ID: "kimi-fixture.json", Provider: "kimi", Attributes: map[string]string{}, Metadata: meta}
		req := cliproxyexecutor.Request{Model: s.Model, Payload: []byte(body)}
		opts := cliproxyexecutor.Options{SourceFormat: sdktranslator.FormatClaude, Stream: s.Stream, OriginalRequest: []byte(body), Headers: ginCtx.Request.Header.Clone()}
		var d rsfixDownstream
		if s.Stream {
			result, err := exec.ExecuteStream(ctx, auth, req, opts)
			d.ErrStatus, d.ErrBody = rsfixStatus(err)
			if result != nil {
				for chunk := range result.Chunks {
					d.Chunks = append(d.Chunks, string(chunk.Payload))
				}
			}
		} else {
			resp, err := exec.Execute(ctx, auth, req, opts)
			d.ErrStatus, d.ErrBody = rsfixStatus(err)
			d.Body = string(resp.Payload)
		}
		down = append(down, d)
	}
	rsfixWrite(t, "kimi", rsfixFixture{Name: "claude-replay-sequence", Credential: map[string]any{"type": "kimi", "access_token": "kimi-access-fixture"}, Request: map[string]any{"steps": steps, "session": "sess-1", "api_key": "client-key-1"}, Responses: responses, Upstream: srv.Captured(), Extra: map[string]any{"downstream": down}})
}

func gzipBody(s string) string {
	var buf bytes.Buffer
	w := gzip.NewWriter(&buf)
	_, _ = w.Write([]byte(s))
	_ = w.Close()
	return b64(buf.Bytes())
}

func TestRSFixKimiTransport(t *testing.T) {
	rsfixOut(t)
	buildinfo.Version = "0.1.0"
	meta := map[string]any{"type": "kimi", "access_token": "kimi-access-fixture", "device_id": "dev-fixture-1"}
	okJSON := `{"id":"c9","object":"chat.completion","created":9,"model":"k2","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}`
	cases := []struct {
		name      string
		attrs     map[string]string
		client    map[string]string
		stream    bool
		body      string
		responses []rsfixResponse
	}{
		{
			name:      "transport-custom-headers",
			attrs:     map[string]string{"header:Host": "kimi.internal", "header:Accept-Encoding": "identity", "header:X-Trace": "$X-Client-Trace", "header:Range": "bytes=0-"},
			client:    map[string]string{"X-Client-Trace": "trace-1"},
			body:      `{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{jsonResp(200, okJSON)},
		},
		{
			name:      "transport-redirect-307",
			body:      `{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{{Status: 307, Headers: [][2]string{{"Location", "/coding/v1/chat/completions-next"}}}, jsonResp(200, okJSON)},
		},
		{
			name:      "transport-gzip-response",
			body:      `{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}, {"Content-Encoding", "gzip"}}, BodyB64: gzipBody(okJSON)}},
		},
		{
			name:      "transport-explicit-gzip-not-decoded",
			attrs:     map[string]string{"header:Accept-Encoding": "gzip"},
			body:      `{"model":"kimi-k2","messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}, {"Content-Encoding", "gzip"}}, BodyB64: gzipBody(okJSON)}},
		},
		{
			name:      "transport-json-typed-stream",
			stream:    true,
			body:      `{"model":"kimi-k2","stream":true,"messages":[{"role":"user","content":"hi"}]}`,
			responses: []rsfixResponse{{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"a\\r\\rb\"}}]}\r\n\r\n: keep-alive\n\ndata: [DONE]"}},
		},
		{
			name:   "transport-stream-options-not-object",
			stream: true,
			body:   `{"model":"kimi-k2","stream":true,"stream_options":[],"messages":[{"role":"user","content":"hi"}]}`,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, false, tc.responses...)
			m := map[string]any{}
			for k, v := range meta {
				m[k] = v
			}
			m["base_url"] = srv.URL() + "/coding"
			attrs := map[string]string{}
			for k, v := range tc.attrs {
				attrs[k] = v
			}
			gin.SetMode(gin.TestMode)
			ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
			ginCtx.Request = httptest.NewRequest(http.MethodPost, "/v1/chat/completions", nil)
			for k, v := range tc.client {
				ginCtx.Request.Header.Set(k, v)
			}
			ctx := context.WithValue(context.Background(), "gin", ginCtx)
			auth := &cliproxyauth.Auth{ID: "kimi-fixture.json", Provider: "kimi", Label: rsfixLabel("kimi", m, attrs), Attributes: attrs, Metadata: m}
			req := cliproxyexecutor.Request{Model: "kimi-k2", Payload: []byte(tc.body)}
			opts := cliproxyexecutor.Options{SourceFormat: sdktranslator.FormatOpenAI, Stream: tc.stream, OriginalRequest: []byte(tc.body), Headers: ginCtx.Request.Header.Clone()}
			logCfg := &config.Config{}
			logCfg.RequestLog = true
			exec := NewKimiExecutor(logCfg)
			var down rsfixDownstream
			if tc.stream {
				result, err := exec.ExecuteStream(ctx, auth, req, opts)
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				if result != nil {
					for chunk := range result.Chunks {
						if chunk.Err != nil {
							down.StreamErr = chunk.Err.Error()
							continue
						}
						down.Chunks = append(down.Chunks, string(chunk.Payload))
					}
				}
			} else {
				resp, err := exec.Execute(ctx, auth, req, opts)
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				down.Body = string(resp.Payload)
			}
			var clientHeaders [][2]string
			for k, v := range tc.client {
				clientHeaders = append(clientHeaders, [2]string{k, v})
			}
			rsfixWrite(t, "kimi", rsfixFixture{Name: tc.name, Credential: map[string]any{"type": "kimi", "access_token": "kimi-access-fixture", "device_id": "dev-fixture-1"}, Attributes: attrs,
				Request: map[string]any{"source": "openai", "model": "kimi-k2", "stream": tc.stream, "body": tc.body, "headers": clientHeaders}, Responses: tc.responses, Upstream: srv.Captured(), Downstream: down,
				Extra: map[string]any{"capture": rsfixCaptureOf(ginCtx, srv.URL())}})
		})
	}
}

const rsfixKimiPatchBody = `{"model":"kimi-k3","input":[{"type":"custom_tool_call","call_id":"old","name":"apply_patch","input":"old\n"},{"type":"custom_tool_call_output","call_id":"old","output":"ok"}],"tools":[{"type":"custom","name":"apply_patch","description":"Apply a patch"}],"tool_choice":{"type":"custom","name":"apply_patch"}}`

// rsfixPatchResponse is a non-stream Responses body with one apply_patch function_call.
func rsfixPatchResponse(valid bool) string {
	args := `{\"input\":\"*** Begin Patch\\n+中😀\\n*** End Patch\\n\"}`
	if !valid {
		args = `{\"input\":5}`
	}
	return `{"id":"r1","object":"response","status":"completed","model":"k3","output":[{"type":"function_call","id":"fc1","call_id":"c1","name":"apply_patch","arguments":"` + args + `","status":"completed"}],"usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}`
}
