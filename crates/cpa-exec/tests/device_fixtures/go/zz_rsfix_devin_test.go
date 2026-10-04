package executor

// Devin fixture generator for cliproxy-rs. Runs the real Go Devin executor, auth service
// and helpers against local capture servers; see zz_rsfix_util_test.go. Upstream bodies
// are Connect frames, recorded as base64.

import (
	"bytes"
	"compress/gzip"
	"context"
	"encoding/json"
	"errors"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	devinauth "github.com/router-for-me/CLIProxyAPI/v8/internal/auth/devin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	translatorcommon "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/common"
	sdkAuth "github.com/router-for-me/CLIProxyAPI/v8/sdk/auth"
	cliproxyauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"google.golang.org/protobuf/encoding/protowire"
)

func pbS(num protowire.Number, s string) []byte {
	return protowire.AppendString(protowire.AppendTag(nil, num, protowire.BytesType), s)
}

func pbB(num protowire.Number, b []byte) []byte {
	return protowire.AppendBytes(protowire.AppendTag(nil, num, protowire.BytesType), b)
}

func pbV(num protowire.Number, v uint64) []byte {
	return protowire.AppendVarint(protowire.AppendTag(nil, num, protowire.VarintType), v)
}

func pbF32(num protowire.Number, v float32) []byte {
	return protowire.AppendFixed32(protowire.AppendTag(nil, num, protowire.Fixed32Type), math.Float32bits(v))
}

func pbMsg(parts ...[]byte) []byte { return bytes.Join(parts, nil) }

func dvData(parts ...[]byte) []byte { return helps.WrapConnectEnvelope(pbMsg(parts...)) }

func dvGzip(parts ...[]byte) []byte {
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	_, _ = zw.Write(pbMsg(parts...))
	_ = zw.Close()
	return helps.WrapConnectEnvelopeWithFlag(helps.ConnectFlagCompressed, buf.Bytes())
}

func dvEOS(trailer string) []byte {
	return helps.WrapConnectEnvelopeWithFlag(helps.ConnectFlagEndStream, []byte(trailer))
}

func dvResp(frames ...[]byte) rsfixResponse {
	return rsfixResponse{Status: 200, Headers: [][2]string{{"Content-Type", "application/connect+proto"}}, BodyB64: b64(bytes.Join(frames, nil))}
}

func dvTool(id, name, args string) []byte {
	var parts [][]byte
	if id != "" {
		parts = append(parts, pbS(1, id))
	}
	if name != "" {
		parts = append(parts, pbS(2, name))
	}
	if args != "" {
		parts = append(parts, pbS(3, args))
	}
	return pbB(6, pbMsg(parts...))
}

func dvUsage(prompt, completion, cached, cacheWrite uint64, model string) []byte {
	return pbB(7, pbMsg(pbV(2, prompt), pbV(3, completion), pbV(4, cacheWrite), pbV(5, cached),
		pbB(8, pbMsg(pbS(1, "x-request-id"), pbS(2, "req_fixture"))), pbS(9, model)))
}

// Rich: thinking split across a UTF-8 boundary, a split signature, content and tool calls
// that arrive while thinking (buffered), and text after the tool call.
func dvRichFrames() [][]byte {
	return [][]byte{
		dvData(pbS(1, "out_1"), pbV(2, 1700000000), pbS(9, "Let me th")),
		dvData(pbS(9, "ink caf\xc3")),
		dvData(pbS(9, "\xa9."), pbB(10, []byte("sig-part-1"))),
		dvData(pbB(10, []byte("|sig-part-2")), pbS(21, "anthropic"), pbS(3, "Answer: ")),
		dvData(dvTool("call_1", "get_weather", `{"city":`)),
		dvData(dvTool("", "", `"Paris"}`)),
		dvData(pbS(3, "Done."), pbS(99, "unknown")),
		dvData(dvUsage(11, 7, 3, 2, "upstream-model-x"), pbV(5, 10)),
		dvEOS("{}"),
	}
}

// Ordering: content and tool calls buffered while thinking, a late signature, a new
// thought that flushes the buffer, a tool name arriving after its ID, invalid JSON args.
func dvOrderingFrames() [][]byte {
	return [][]byte{
		dvData(pbS(9, "plan")),
		dvData(pbS(3, "Hi")),
		dvData(pbB(10, []byte("sig-late")), pbS(21, "openai")),
		dvData(dvTool("c1", "", `{"a"`)),
		dvData(pbS(9, "more")),
		dvData(dvTool("c1", "fn", `:1}`)),
		dvData(pbB(6, pbMsg(pbS(1, "c2"), pbS(2, "g"), pbS(4, "bad{")))),
		dvData(pbS(3, "end")),
		dvData(dvUsage(3, 4, 0, 0, "m"), pbV(5, 10)),
		dvEOS("{}"),
	}
}

func dvPlainFrames(text string) [][]byte {
	return [][]byte{
		dvData(pbS(3, text)),
		dvData(pbS(3, " more")),
		dvData(dvUsage(5, 2, 0, 0, ""), pbV(5, 2)),
		dvEOS("{}"),
	}
}

type dvCase struct {
	name      string
	source    sdktranslator.Format
	model     string
	body      string
	stream    bool
	count     bool
	refresh   bool
	cfg       string
	meta      map[string]any
	attrs     map[string]string
	headers   http.Header
	responses []rsfixResponse
	repeat    int
}

func TestRSFixDevin(t *testing.T) {
	rsfixOut(t)
	chat := `{"model":"swe-2","messages":[{"role":"system","content":"You are helpful.\nYou are Claude Code, Anthropic's official CLI for Claude.\nKeep it short."},{"role":"user","content":"What's the weather in Paris? %CASE%"},{"role":"assistant","content":"Checking.","tool_calls":[{"id":"call_a","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}}]},{"role":"tool","tool_call_id":"call_a","content":"sunny"},{"role":"user","content":[{"type":"text","text":"Thanks <b>&</b>"},{"type":"image_url","image_url":{"url":"data:image/webp;base64,UklGRg=="}}]}],"tools":[{"type":"function","function":{"name":"get_weather","description":"Get weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}},{"type":"function","function":{"name":"exec_command","description":"Runs a command, returning output or a session ID for ongoing interaction.","parameters":{"type":"object"}}}],"temperature":0.3,"max_tokens":500}`
	responses := `{"model":"glm-5-3","instructions":"Be concise.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hi %CASE%"}]}],"reasoning":{"effort":"medium"},"tools":[{"type":"function","name":"get_weather","description":"Look up","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}],"max_output_tokens":2000}`
	claude := `{"model":"claude-sonnet-4-5","max_tokens":1024,"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":"System rules %CASE%"}],"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/AAAA"}},{"type":"text","text":"Describe"}]},{"role":"assistant","content":[{"type":"thinking","thinking":"hmm","signature":"claude#EqQBCkYIBRgCKkB"},{"type":"text","text":"It is a cat."},{"type":"tool_use","id":"toolu_1","name":"zoom","input":{"level":2}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"text","text":"zoomed"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0K"}}]}]}],"tools":[{"name":"zoom","description":"Zoom in","input_schema":{"type":"object","properties":{"level":{"type":"integer"}}}}],"thinking":{"type":"enabled","budget_tokens":8000},"stream":%STREAM%}`
	interactions := `{"model":"devin/swe-2","session_id":"%SESSION%","system_instruction":"Be helpful %CASE%","input":[{"type":"user_input","content":[{"type":"text","text":"hello"}]},{"type":"model_output","content":[{"type":"text","text":"hi"}],"signature":"gpt#gAAAAB"},{"type":"thought","content":[{"type":"text","text":"pondering"}]},{"type":"function_call","name":"run","id":"fc_1","arguments":{"cmd":"ls"}},{"type":"function_result","call_id":"fc_1","result":[{"type":"text","text":"a.txt"}]},{"type":"function_result","call_id":"orphan_9","result":"lost"},{"type":"function_result","call_id":"","result":{}},{"type":"user_input","content":"next"}],"generation_config":{"temperature":0.7,"max_output_tokens":999999,"thinking_level":"fast"},"tools":[{"type":"function","name":"run","description":"Run","parameters":{"type":"object"}},{"function_declarations":[{"name":"decl_a","description":"Takes a task_id parameter identifying the task","parametersJsonSchema":{"type":"object"}}]},{"type":"namespace","name":"mcp__codex_app","tools":[{"name":"automation_update","description":"x"},{"name":"open","description":"Open"}]}]}`
	gemini := `{"contents":[{"role":"user","parts":[{"text":"Hi %CASE%"}]}],"generationConfig":{"temperature":0.1,"thinkingConfig":{"thinkingBudget":20000}}}`
	applyPatch := `{"model":"gpt-5-6-sol","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"patch %CASE%"}]}],"tools":[{"type":"custom","name":"apply_patch","description":"Apply a patch","format":{"type":"grammar","syntax":"lark","definition":"start: x"}}]}`
	sub := func(body, name string) string {
		body = strings.ReplaceAll(body, "%CASE%", name)
		body = strings.ReplaceAll(body, "%STREAM%", "false")
		return body
	}
	trailerLate := []byte(`{"error":{"code":"permission_denied","message":"Model under high demand"}}`)
	cases := []dvCase{
		{name: "chat-nonstream", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "chat-stream", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "responses-nonstream", source: sdktranslator.FormatOpenAIResponse, model: "glm-5-3", body: responses, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "responses-stream", source: sdktranslator.FormatOpenAIResponse, model: "glm-5-3", body: responses, stream: true, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "claude-nonstream", source: sdktranslator.FormatClaude, model: "claude-sonnet-4-5", body: claude, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "claude-stream", source: sdktranslator.FormatClaude, model: "claude-sonnet-4-5", body: claude, stream: true, responses: []rsfixResponse{dvResp(dvPlainFrames("Hello")...)}},
		{name: "interactions-nonstream", source: sdktranslator.FormatInteractions, model: "devin/swe-2", body: interactions, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "interactions-stream-gzip", source: sdktranslator.FormatInteractions, model: "devin/swe-2", body: interactions, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "Hel")), dvGzip(pbS(3, "lo")), dvData(dvUsage(1, 1, 0, 0, "m")), dvEOS(""))}},
		{name: "gemini-stream", source: sdktranslator.FormatGemini, model: "gemini-3-flash", body: gemini, stream: true, responses: []rsfixResponse{dvResp(dvRichFrames()...)}},
		{name: "gemini-nonstream-dimension-usage", source: sdktranslator.FormatGemini, model: "gemini-3-flash", body: gemini,
			responses: []rsfixResponse{dvResp(
				dvData(pbS(3, "ok")),
				dvData(pbB(28, pbMsg(pbS(1, "Token Usage"),
					pbB(2, pbMsg(pbS(5, "input_tokens"), pbB(4, pbF32(2, 42)))),
					pbB(2, pbMsg(pbS(5, "output_tokens"), pbB(4, pbF32(2, 9)))),
					pbB(2, pbMsg(pbS(5, "cached_input_tokens"), pbB(4, pbF32(2, 4))))))),
				dvEOS("{}"))}},
		{name: "chat-stream-trailer-before-content", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{dvResp(dvEOS(`{"error":{"code":"resource_exhausted","message":"quota exceeded"}}`))}},
		{name: "chat-stream-trailer-after-content", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "partial")), helps.WrapConnectEnvelopeWithFlag(helps.ConnectFlagEndStream, trailerLate))}},
		{name: "responses-stream-trailer-after-content", source: sdktranslator.FormatOpenAIResponse, model: "glm-5-3", body: responses, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "partial")), helps.WrapConnectEnvelopeWithFlag(helps.ConnectFlagEndStream, trailerLate))}},
		{name: "chat-nonstream-trailer-credit", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "x")), dvEOS(`{"error":{"code":"failed_precondition","message":"Credit balance exhausted"}}`))}},
		{name: "chat-nonstream-trailer-internal-invalid", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat,
			responses: []rsfixResponse{dvResp(dvEOS(`{"error":{"code":"invalid_argument","message":"an internal error occurred"}}`))}},
		{name: "chat-http-429-retry-after", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat,
			responses: []rsfixResponse{{Status: 429, Headers: [][2]string{{"Retry-After", "7"}, {"Content-Type", "application/json"}}, Body: `{"code":"resource_exhausted","message":"slow down"}`}}},
		{name: "chat-stream-http-500", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{{Status: 500, Body: "upstream broke"}}},
		{name: "chat-stream-premature-eof", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "cut")))}},
		{name: "chat-nonstream-premature-eof", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "cut")))}},
		{name: "chat-stream-bad-frame-flag", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "ok")), helps.WrapConnectEnvelopeWithFlag(0x04, []byte("x")))}},
		{name: "chat-stream-max-tokens", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, stream: true,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "long")), dvData(pbV(5, 3)), dvEOS("{}"))}},
		{name: "chat-nonstream-content-filter", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat,
			responses: []rsfixResponse{dvResp(dvData(pbS(3, "no")), dvData(pbV(5, 11)), dvEOS("{}"))}},
		{name: "chat-sensitive-words", source: sdktranslator.FormatOpenAI, model: "swe-2", cfg: "devin:\n  sensitive-words: [\"Paris\", \"x\", \"helpful\"]\n",
			body: `{"model":"swe-2","messages":[{"role":"system","content":"x-anthropic-billing-header: cc=1\nYou are helpful. ✓\nNever mention Paris.\nCodex refers to the open-source agentic coding interface\nStay calm"},{"role":"user","content":"Paris %CASE%"}]}`,
			responses: []rsfixResponse{dvResp(dvPlainFrames("ok")...)}},
		{name: "responses-apply-patch-legacy", source: sdktranslator.FormatOpenAIResponse, model: "gpt-5-6-sol", body: applyPatch,
			responses: []rsfixResponse{dvResp(dvData(pbB(6, pbMsg(pbS(1, "call_p"), pbS(2, "apply_patch"), pbS(4, "*** Begin Patch")))), dvEOS("{}"))}},
		{name: "responses-apply-patch-trailer", source: sdktranslator.FormatOpenAIResponse, model: "gpt-5-6-sol", body: applyPatch,
			responses: []rsfixResponse{dvResp(dvEOS(`{"error":{"code":"unavailable","message":"down"}}`))}},
		{name: "chat-custom-headers-attrs", source: sdktranslator.FormatOpenAI, model: "devin/claude-opus-4-6:low", body: chat,
			attrs:     map[string]string{"api_key": "attr-key", "header:X-Extra": "e1", "header:Sentry-Trace": "fixed-trace-1", "device_seed": "attr-seed"},
			responses: []rsfixResponse{dvResp(dvPlainFrames("hi")...)}},
		{name: "turn-index-repeat", source: sdktranslator.FormatInteractions, model: "devin/swe-1-6", body: interactions, repeat: 2,
			responses: []rsfixResponse{dvResp(dvPlainFrames("a")...), dvResp(dvPlainFrames("b")...)}},
		{name: "interactions-stream-ordering", source: sdktranslator.FormatInteractions, model: "devin/swe-2", body: interactions, stream: true, responses: []rsfixResponse{dvResp(dvOrderingFrames()...)}},
		{name: "responses-stream-ordering", source: sdktranslator.FormatOpenAIResponse, model: "glm-5-3", body: responses, stream: true, responses: []rsfixResponse{dvResp(dvOrderingFrames()...)}},
		{name: "interactions-nonstream-ordering", source: sdktranslator.FormatInteractions, model: "devin/swe-2", body: interactions, responses: []rsfixResponse{dvResp(dvOrderingFrames()...)}},
		{name: "count-tokens", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, count: true},
		{name: "missing-credentials", source: sdktranslator.FormatOpenAI, model: "swe-2", body: chat, meta: map[string]any{"type": "devin"}},
	}
	sessions := 0
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, true, tc.responses...)
			sessions++
			body := sub(tc.body, tc.name)
			if tc.stream {
				body = strings.ReplaceAll(strings.ReplaceAll(tc.body, "%CASE%", tc.name), "%STREAM%", "true")
			}
			body = strings.ReplaceAll(body, "%SESSION%", "0f8fad5b-d9cb-469f-a165-70867728950"+string(rune('a'+sessions%26)))
			meta := map[string]any{"type": "devin", "api_key": "devin-session-token$fixture", "session_token": "devin-session-token$fixture", "device_seed": "seed-1", "base_url": srv.URL()}
			if tc.meta != nil {
				meta = map[string]any{}
				for k, v := range tc.meta {
					meta[k] = v
				}
			}
			attrs := map[string]string{}
			for k, v := range tc.attrs {
				attrs[k] = v
			}
			if tc.attrs != nil {
				attrs["base_url"] = srv.URL()
				delete(meta, "base_url")
			}
			recorded := map[string]any{}
			for k, v := range meta {
				recorded[k] = v
			}
			recordedAttrs := map[string]string{}
			for k, v := range attrs {
				recordedAttrs[k] = v
			}
			auth := &cliproxyauth.Auth{ID: "devin-fixture.json", Provider: "devin", Label: rsfixLabel("devin", meta, attrs), Attributes: attrs, Metadata: meta}
			req := cliproxyexecutor.Request{Model: tc.model, Payload: []byte(body)}
			opts := cliproxyexecutor.Options{SourceFormat: tc.source, Stream: tc.stream, OriginalRequest: []byte(body), Headers: tc.headers}
			cfg := &config.Config{}
			if tc.cfg != "" {
				parsed, err := config.ParseConfigBytes([]byte(tc.cfg))
				if err != nil {
					t.Fatal(err)
				}
				cfg = parsed
			}
			cfg.RequestLog = true
			exec := NewDevinExecutor(cfg)
			var takeCapture func() map[string]any
			rsfixResetUsage()
			var down rsfixDownstream
			var execErr error
			runs := tc.repeat
			if runs == 0 {
				runs = 1
			}
			for i := 0; i < runs; i++ {
				// One capture per run: the Rust test keeps the last run's.
				var ctx context.Context
				ctx, takeCapture = rsfixCapture(context.Background(), tc.headers, srv.URL())
				down = rsfixDownstream{}
				switch {
				case tc.count:
					resp, err := exec.CountTokens(ctx, auth, req, opts)
					execErr = err
					down.ErrStatus, down.ErrBody = rsfixStatus(err)
					down.Body = string(resp.Payload)
				case tc.stream:
					result, err := exec.ExecuteStream(ctx, auth, req, opts)
					execErr = err
					down.ErrStatus, down.ErrBody = rsfixStatus(err)
					if result != nil {
						for chunk := range result.Chunks {
							if chunk.Err != nil {
								status, msg := rsfixStatus(chunk.Err)
								down.StreamErr = msg
								down.ErrStatus = status
								execErr = chunk.Err
								continue
							}
							down.Chunks = append(down.Chunks, string(chunk.Payload))
						}
					}
				default:
					resp, err := exec.Execute(ctx, auth, req, opts)
					execErr = err
					down.ErrStatus, down.ErrBody = rsfixStatus(err)
					down.Body = string(resp.Payload)
				}
			}
			extra := map[string]any{"usage": rsfixTakeUsage(), "capture": takeCapture()}
			if execErr != nil {
				type retryAfter interface{ RetryAfter() *time.Duration }
				var ra retryAfter
				if errors.As(execErr, &ra) && ra.RetryAfter() != nil {
					extra["retry_after_secs"] = ra.RetryAfter().Seconds()
				}
			}
			request := map[string]any{"source": string(tc.source), "model": tc.model, "stream": tc.stream, "body": body}
			if tc.cfg != "" {
				request["config"] = tc.cfg
			}
			if tc.count {
				request["count"] = true
			}
			if tc.repeat > 0 {
				request["repeat"] = tc.repeat
			}
			rsfixWrite(t, "devin", rsfixFixture{Name: tc.name, Credential: recorded, Attributes: recordedAttrs, Request: request, Responses: tc.responses, Upstream: srv.Captured(), Downstream: down, Extra: extra})
		})
	}
}

func dvUserStatus() []byte {
	plan := pbMsg(
		pbB(1, pbMsg(pbS(2, "Teams Pro"), pbB(33, pbMsg(pbS(4, "org-77"), pbS(8, "Acme Org"))))),
		pbB(2, pbMsg(pbV(1, 1767225600))),
		pbB(3, pbMsg(pbV(1, 1798761600))),
		pbV(14, 63),
		pbV(15, 0),
		pbV(17, 1790000000),
		pbV(19, 5),
	)
	user := pbMsg(pbS(3, "dev.user"), pbS(5, "team-9"), pbS(7, "dev@example.com"), pbB(13, plan), pbS(36, "user-123"), pbV(40, 1))
	return pbMsg(pbB(1, user), pbS(2, "ignored"))
}

func TestRSFixDevinRefresh(t *testing.T) {
	rsfixOut(t)
	cases := []struct {
		name      string
		meta      map[string]any
		quota     map[string]string
		responses []rsfixResponse
	}{
		{name: "refresh-user-status", meta: map[string]any{"type": "devin", "api_key": "devin-session-token$fixture", "device_seed": "seed-r", "email": "old@example.com", "plan": "Free"},
			quota:     map[string]string{"plan_end": "keep-me", "daily_quota_reset_at": "stale"},
			responses: []rsfixResponse{{Status: 200, Headers: [][2]string{{"Content-Type", "application/proto"}}, BodyB64: b64(dvUserStatus())}}},
		{name: "refresh-sparse-status", meta: map[string]any{"type": "devin", "session_token": "tok-only"},
			responses: []rsfixResponse{{Status: 200, Headers: [][2]string{{"Content-Type", "application/proto"}}, BodyB64: b64(pbMsg(pbB(1, pbMsg(pbS(3, "")))))}}},
		{name: "refresh-error", meta: map[string]any{"type": "devin", "api_key": "devin-session-token$fixture"},
			responses: []rsfixResponse{{Status: 403, Body: "forbidden seat"}}},
		{name: "refresh-no-token", meta: map[string]any{"type": "devin"}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, true, tc.responses...)
			meta := map[string]any{"base_url": srv.URL()}
			recorded := map[string]any{}
			for k, v := range tc.meta {
				meta[k] = v
				recorded[k] = v
			}
			auth := &cliproxyauth.Auth{ID: "devin-fixture.json", Provider: "devin", Attributes: map[string]string{}, Metadata: meta}
			if tc.quota != nil {
				auth.Quota.Signals = tc.quota
			}
			exec := NewDevinExecutor(&config.Config{})
			updated, err := exec.Refresh(context.Background(), auth)
			var down rsfixDownstream
			down.ErrStatus, down.ErrBody = rsfixStatus(err)
			extra := map[string]any{}
			if updated != nil {
				delete(updated.Metadata, "base_url")
				extra["metadata_after"] = updated.Metadata
				extra["attributes_after"] = updated.Attributes
				extra["quota_signals"] = updated.Quota.Signals
				extra["same_auth"] = updated == auth
			}
			extra["quota_before"] = tc.quota
			rsfixWrite(t, "devin", rsfixFixture{Name: tc.name, Credential: recorded, Request: map[string]any{"refresh": true}, Responses: tc.responses, Upstream: srv.Captured(), Downstream: down, Extra: extra})
		})
	}
}

func TestRSFixDevinAuth(t *testing.T) {
	rsfixOut(t)
	statusResp := rsfixResponse{Status: 200, Headers: [][2]string{{"Content-Type", "application/proto"}}, BodyB64: b64(dvUserStatus())}
	cases := []struct {
		name      string
		token     string
		exchange  string
		responses []rsfixResponse
		stale     string
	}{
		{name: "record-profile", token: "eyJhbGciOi.fixture", responses: []rsfixResponse{jsonResp(200, `{"user_name":"Profile Name","user_id":"pid-1","org_id":"porg"}`), statusResp}},
		{name: "record-status-only", token: "devin-session-token$abc", responses: []rsfixResponse{jsonResp(500, `{"user_name":"ignored"}`), statusResp}},
		{name: "record-nothing", token: "plain-token", responses: []rsfixResponse{jsonResp(404, `{}`), {Status: 500, Body: "seat down"}}},
		{name: "record-unsafe-name", token: "eyJx", responses: []rsfixResponse{jsonResp(200, `{"user_name":"../evil name","user_id":"u"}`), {Status: 500, Body: "x"}}},
		{name: "exchange-ok", exchange: "code-1", responses: []rsfixResponse{jsonResp(200, `{"token":" eyJexchanged "}`)}},
		{name: "exchange-error", exchange: "code-2", responses: []rsfixResponse{jsonResp(400, `{"error":"invalid_grant"}`)}},
		{name: "exchange-no-token", exchange: "code-3", responses: []rsfixResponse{jsonResp(200, `{"other":1}`)}},
		{name: "login-save", token: "eyJsave", stale: `{"type":"devin","api_key":"old","custom":"keep","disabled":true,"access_token":"drop","user_name":"stale","x":1.50}`,
			responses: []rsfixResponse{jsonResp(200, `{"user_name":"saver","user_id":"s1","org_id":"o1"}`), statusResp}},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			srv := newRSFixServer(t, true, tc.responses...)
			svc := devinauth.NewDevinAuthService(&http.Client{Timeout: 10 * time.Second})
			svc.SetAPIBaseURL(srv.URL())
			svc.SetServerBaseURL(srv.URL())
			svc.SetAppBaseURL(srv.URL())
			ctx := context.Background()
			var down rsfixDownstream
			extra := map[string]any{}
			switch {
			case tc.exchange != "":
				token, err := svc.ExchangeCodeForToken(ctx, tc.exchange, " verifier-1 ")
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				down.Body = token
			case tc.stale != "" || strings.HasPrefix(tc.name, "login"):
				dir := t.TempDir()
				record, err := svc.CreateAuthRecord(ctx, tc.token)
				if err != nil {
					t.Fatal(err)
				}
				if tc.stale != "" {
					if err = os.WriteFile(filepath.Join(dir, record.FileName), []byte(tc.stale), 0o600); err != nil {
						t.Fatal(err)
					}
				}
				store := sdkAuth.NewFileTokenStore()
				store.SetBaseDir(dir)
				mgr := sdkAuth.NewManager(store, &dvFixedAuthenticator{record: record})
				_, saved, errLogin := mgr.Login(ctx, "devin", &config.Config{AuthDir: dir}, &sdkAuth.LoginOptions{})
				if errLogin != nil {
					t.Fatal(errLogin)
				}
				raw, _ := os.ReadFile(saved)
				extra["file_name"] = filepath.Base(saved)
				extra["file"] = string(raw)
				extra["stale"] = tc.stale
			default:
				record, err := svc.CreateAuthRecord(ctx, tc.token)
				down.ErrStatus, down.ErrBody = rsfixStatus(err)
				if record != nil {
					extra["id"] = record.ID
					extra["file_name"] = record.FileName
					extra["label"] = record.Label
					extra["attributes"] = record.Attributes
					extra["metadata"] = record.Metadata
					extra["quota_signals"] = record.Quota.Signals
				}
			}
			request := map[string]any{"token": tc.token, "exchange": tc.exchange}
			rsfixWrite(t, "devin", rsfixFixture{Name: tc.name, Request: request, Responses: tc.responses, Upstream: srv.Captured(), Downstream: down, Extra: extra})
		})
	}
}

type dvFixedAuthenticator struct{ record *cliproxyauth.Auth }

func (a *dvFixedAuthenticator) Provider() string { return "devin" }
func (a *dvFixedAuthenticator) Login(context.Context, *config.Config, *sdkAuth.LoginOptions) (*cliproxyauth.Auth, error) {
	return a.record, nil
}
func (a *dvFixedAuthenticator) RefreshLead() *time.Duration { return nil }

func TestRSFixDevinVectors(t *testing.T) {
	dir := rsfixOut(t)
	out := map[string]any{}

	type uidCase struct {
		Model  string `json:"model"`
		Level  string `json:"level"`
		Budget int    `json:"budget"`
		Want   string `json:"want"`
	}
	var uids []uidCase
	models := []string{"", "devin/swe-2", "DEVIN/swe-2-max", "swe-2", "swe-2(xhigh)", "swe-2:low", "swe-2(none)", "claude-haiku-4.5", "gpt-4.1", "claude-sonnet-4-5", "claude-sonnet-4.5(high)", "gemini-3-flash",
		"model_gpt_5_2", "MODEL_GPT_5_2(max)", "model-google-gemini-3-0-flash", "model_claude_4_5_opus", "swe-1-7", "swe-1-6", "glm-5-2", "glm-5-2-1m", "claude-opus-4-6", "claude-opus-4-6-1m", "claude-sonnet-4-6-1m",
		"glm-5-3", "gpt-5-6-sol", "grok-4-6", "deepseek-v4-flash", "kimi-k3", "unknown-model", "gpt-6-astra", "claude-fable-5-1", "swe-1-6-slow", "gemini-3-8-flash", "Glm-5.3", "devin/gpt-6-astra-low-fast"}
	levels := []string{"", "minimal", "low", "medium", "high", "xhigh", "max", "none", "auto", "fast", "bogus"}
	for _, m := range models {
		for _, l := range levels {
			uids = append(uids, uidCase{Model: m, Level: l, Want: helps.ResolveDevinChatModelUID(m, l, 0)})
		}
		for _, b := range []int{1, 4096, 4097, 16384, 30000, 40000} {
			uids = append(uids, uidCase{Model: m, Budget: b, Want: helps.ResolveDevinChatModelUID(m, "", b)})
		}
	}
	out["resolve_uid"] = uids

	catalog, _ := json.Marshal(registry.GetDevinModels())
	out["catalog"] = json.RawMessage(catalog)
	var lookups []map[string]any
	for _, id := range []string{"swe-2", "devin/SWE-2", "claude-opus-5-low-fast", "gpt-6-astra-high", "swe-1-6-slow", "swe-1-6-fast", "nope", "", "glm-5-2-max-1m", "claude-opus-4-6-thinking-1m"} {
		m := registry.LookupDevinModel(id)
		entry := map[string]any{"id": id}
		if m != nil {
			entry["found"] = m.ID
		}
		lookups = append(lookups, entry)
	}
	out["lookup"] = lookups
	var statics []map[string]any
	for _, id := range []string{"devin/swe-2", "devin/swe-1-6-slow", "devin/glm-5-3", "swe-2", "devin/gpt-6-astra"} {
		m := registry.LookupStaticModelInfo(id)
		entry := map[string]any{"id": id}
		if m != nil {
			entry["type"] = m.Type
			entry["max_completion_tokens"] = m.MaxCompletionTokens
		}
		statics = append(statics, entry)
	}
	out["static_lookup"] = statics
	var validations []map[string]any
	for _, raw := range []string{
		``, `[]`, `{"devin":[]}`, `{"models":[{"id":"a"}]}`, `[{"id":"Devin/X"},{"id":"x"}]`, `[null]`, `[{"id":" "}]`, `{"devin":[{"id":"m-low","display_name":"M Low","thinking":{"levels":["priority","low"]}},{"id":"m-high-fast","context_length":5,"max_completion_tokens":9,"supported_input_modalities":["image"]},{"id":"m","display_name":"M Medium Thinking Fast","owned_by":"o"},{"id":"n_LOW"},{"id":"q-thinking-1m"},{"id":"swe-1-6-fast"}]}`, `not json`,
	} {
		models, err := registry.ValidateDevinModelsJSON([]byte(raw))
		entry := map[string]any{"input": raw}
		if err != nil {
			entry["error"] = err.Error()
		} else {
			encoded, _ := json.Marshal(models)
			entry["models"] = json.RawMessage(encoded)
		}
		validations = append(validations, entry)
	}
	out["validate"] = validations

	var trailers []map[string]any
	for _, raw := range []string{``, `{}`, ` {} `, `not json`, `{"error":null}`, `{"error":{"code":"invalid_argument","message":"bad"}}`, `{"error":{"code":"invalid_argument","message":"An Internal Error"}}`, `{"error":{"code":"INTERNAL","message":"x"}}`, `{"error":{"code":"unauthenticated","message":"x"}}`,
		`{"error":{"code":"permission_denied","message":"x"}}`, `{"error":{"code":"permission_denied","message":"High Demand"}}`, `{"error":{"code":"resource_exhausted","message":""}}`, `{"error":{"code":"unavailable"}}`, `{"error":{"code":"canceled","message":"c"}}`, `{"error":{"code":"deadline_exceeded","message":"d"}}`,
		`{"error":{"code":"failed_precondition","message":"ACU limit"}}`, `{"error":{"code":"failed_precondition","message":"nope"}}`, `{"error":{"code":"weird","message":"w"}}`, `{"error":{}}`} {
		code, err := helps.ParseDevinTrailerError([]byte(raw))
		entry := map[string]any{"input": raw, "code": code}
		if err != nil {
			entry["error"] = err.Error()
		}
		trailers = append(trailers, entry)
	}
	out["trailers"] = trailers

	matcher := helps.BuildSensitiveWordMatcher([]string{"secret", " Ab ", "x", "zero\u200bwidth", "Ünïcode"})
	var prompts []map[string]any
	for _, p := range []string{"", "plain", "a\r\nb\n\n", "x-anthropic-billing-header: v\nkeep\n   You are Claude Code, an agent\nreal line", "this is SECRET stuff\nother ab line", "Ünïcode word here", "  - Don’t output ANSI escape codes directly — the CLI renderer applies them.\nok", "Fast mode for Claude Code lets\nauthorized security testing\ndestructive techniques, DoS attacks", "Claude Code is available as a CLI\nend"} {
		prompts = append(prompts, map[string]any{"input": p, "plain": helps.SanitizeDevinSystemPrompt(p, nil), "matched": helps.SanitizeDevinSystemPrompt(p, matcher)})
	}
	out["sanitize"] = prompts

	var tools []map[string]any
	for _, c := range [][2]string{{"exec_command", "Runs X, returning output or a session ID for ongoing interaction."}, {"ns__exec_command", "RETURNING OUTPUT OR A SESSION ID FOR ONGOING INTERACTION"}, {"write_stdin", "Writes characters to an existing unified exec session and returns recent output."}, {"x__write_stdin", "writes characters to an existing unified exec session and returns recent output"}, {"other", "returning output or a session ID for ongoing interaction"}, {"exec_command", ""}} {
		tools = append(tools, map[string]any{"name": c[0], "input": c[1], "want": sanitizeDevinToolDescriptionForFixture(c[0], c[1])})
	}
	out["tool_descriptions"] = tools

	var sigs []map[string]any
	for _, s := range []string{"", "  ", "sealed.v1.abc", "claude#EqQB", "gpt#gAAAA", "gemini#AYxx", "AYabc", "CAQSabc", b64([]byte("sealed.v1.inner")), b64([]byte("CAQSxyz")), b64([]byte("gAAAAqq")), b64([]byte{0x01, 0x02}), "gAAAAplain", "random", b64([]byte("hello")), "EqQBCkYIBRgCKkB"} {
		b, typ := parseSignatureBytes(s)
		sigs = append(sigs, map[string]any{"input": s, "bytes_b64": b64(b), "type": typ})
	}
	out["signatures"] = sigs

	out["fingerprints"] = map[string]string{"seed-1": helps.GenerateDevinDeviceFingerprint("seed-1"), "tok": devinauth.GenerateDeviceFingerprint("tok")}

	var uuids []map[string]string
	for _, s := range []string{"3f2a9c4e-1b7d-4e8a-9c3f-2d1e0b9a8c7d", "3F2A9C4E1B7D4E8A9C3F2D1E0B9A8C7D", "{3f2a9c4e-1b7d-4e8a-9c3f-2d1e0b9a8c7d}", "urn:uuid:3f2a9c4e-1b7d-4e8a-9c3f-2d1e0b9a8c7d", "msg:abc", " lcp:x ", "3f2a9c4e-1b7d-4e8a-9c3f-2d1e0b9a8c7"} {
		uuids = append(uuids, map[string]string{"input": s, "want": normalizeDevinUUID(s)})
	}
	out["uuids"] = uuids


	svc := devinauth.NewDevinAuthService(nil)
	out["auth_urls"] = []string{
		svc.BuildAuthorizationURL("http://127.0.0.1:5555/callback", "chal+/=", "st ate"),
		svc.BuildAuthorizationURL("  ", "chal", ""),
		svc.BuildAuthorizationURL("", "c", "s"),
	}
	out["format_tokens"] = map[string]string{"eyJa": devinauth.FormatSessionToken("eyJa"), " devin-session-token$x ": devinauth.FormatSessionToken(" devin-session-token$x "), "other": devinauth.FormatSessionToken("other"), "": devinauth.FormatSessionToken("")}

	status, err := devinauth.ParseGetUserStatusResponse(dvUserStatus())
	if err != nil {
		t.Fatal(err)
	}
	statusJSON, _ := json.Marshal(status)
	out["user_status"] = map[string]any{"input_b64": b64(dvUserStatus()), "parsed": json.RawMessage(statusJSON)}
	out["user_status_request_b64"] = b64(devinauth.BuildGetUserStatusRequest("tok", "fp"))

	var buf helps.UTF8SplitBuffer
	var feeds []map[string]string
	for _, chunk := range [][]byte{[]byte("ab\xe2"), []byte("\x82"), []byte("\xac!"), []byte("\xff\xfe x"), []byte("\xf0\x9f"), {}, []byte("\x98\x80")} {
		feeds = append(feeds, map[string]string{"in_b64": b64(chunk), "out_b64": b64([]byte(buf.Feed(chunk)))})
	}
	out["utf8_feeds"] = feeds

	// parseInteractionsPayload with message IDs dropped.
	var parsed []map[string]any
	for _, c := range [][2]string{
		{`{"system_instruction":"  sys  ","generation_config":{"temperature":0.2,"max_output_tokens":10,"thinking_level":"high","thinking_config":{"thinking_budget":99}},"conversation_id":"conv-1","input":[{"type":"user_input","text":"t"},{"type":"thought","text":"a","signature":"AYsig"},{"type":"thought","content":"b"},{"type":"model_output","content":[{"type":"text","text":"x"},{"type":"text","text":"y"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"},{"type":"function_call","id":"c2","name":"g"},{"type":"function_result","result":{"type":"text","text":"r1"}},{"type":"function_result","id":"c2","output":[{"type":"input_image","image_url":"data:image/gif;base64,R0lG"},{"content":"wrapped"},{"type":"text","text":"t","extra":1},{"k":"v"},"s"]}]}`, `{"temperature":1.5}`},
		{`{"systemInstruction":"S","generationConfig":{"max_output_tokens":0},"temperature":0.9,"messages":[{"role":"developer","content":"dev"},{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:,abc"}},{"type":"text","text":"hi"}]},{"role":"assistant","content":"ok","tool_calls":[{"id":"t1","function":{"name":"fn","arguments":{"z":1}}}]},{"role":"tool","tool_call_id":"t1","content":""},{"role":"tool","tool_call_id":"zz","content":[{"type":"tool_result","content":"inner"}]}]}`, `{"session_id":" orig-sess ","messages":[{"role":"user","content":[{"type":"image","source":{"data":"QUJD","media_type":"image/gif"}}]},{"role":"assistant","content":[{"type":"thinking","thinking":"th","signature":"gpt#gAAAAZ"}]},{"role":"tool","tool_call_id":"zz","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,UE5H"}}]}]}`},
		{`{"input":[{"type":"function_result","call_id":"x","content":[{"type":"tool_result","tool_use_id":"x","content":[{"type":"text","text":"deep"}],"is_error":false}]},{"type":"function_result","call_id":"y","result":[{"type":"text","text":"   "}]},{"type":"user_input","content":[{"type":"image","data":"SU1H","mime_type":"image/jpeg"},{"type":"image","inline_data":{"data":"SU1I"}}]}]}`, ``},
	} {
		sys, prompts, tools, temp, maxTokens, sessionID, cascadeID, level, budget := parseInteractionsPayload([]byte(c[0]), []byte(c[1]))
		for i := range prompts {
			prompts[i].MessageID = ""
		}
		entry := map[string]any{"payload": c[0], "original": c[1], "system": sys, "prompts": prompts, "tools": tools, "max_tokens": maxTokens, "session_id": sessionID, "cascade_id": cascadeID, "level": level, "budget": budget}
		if temp != nil {
			entry["temperature"] = *temp
		}
		parsed = append(parsed, entry)
	}
	out["parse_interactions"] = parsed

	var frames []map[string]any
	for _, f := range [][]byte{
		pbMsg(pbS(1, "o"), pbV(2, 5), pbS(3, "a"), pbS(3, "b"), pbV(4, 2), pbV(5, 10), pbB(6, pbMsg(pbS(1, "i"), pbS(2, "n"), pbS(4, "bad"), pbS(5, "err"), pbV(6, 1), pbV(9, 3))), pbS(9, "t"), pbB(10, []byte{1, 2}), pbB(10, []byte{3}), pbS(17, "mid"), pbS(21, "sealed"), pbS(30, "?"),
			protowire.AppendFixed64(protowire.AppendTag(nil, 12, protowire.Fixed64Type), math.Float64bits(1.25)), pbF32(13, 1)),
		pbMsg(pbB(2, pbMsg(pbV(1, 77), pbV(2, 9))), pbB(7, pbMsg(pbV(2, 3), pbV(2, 4), pbV(4, 1), pbV(4, 1), pbB(8, pbMsg(pbS(1, "Request-Id"), pbS(2, "rid"))), pbB(8, []byte("plain-id")), pbV(6, 200)))),
		pbMsg(pbB(7, pbMsg(pbB(8, []byte("printable")), pbB(8, pbMsg(pbS(1, "x-other"), pbS(2, "v")))))),
		{0x08},
		pbMsg(pbS(3, "ok"), protowire.AppendTag(nil, 4, protowire.StartGroupType)),
	} {
		res, errParse := helps.ParseDevinFrame(f)
		resJSON, _ := json.Marshal(res)
		entry := map[string]any{"input_b64": b64(f), "result": json.RawMessage(resJSON)}
		if errParse != nil {
			entry["error"] = errParse.Error()
		}
		frames = append(frames, entry)
	}
	out["frames"] = frames

	raw, _ := json.MarshalIndent(out, "", "  ")
	if err = os.MkdirAll(filepath.Join(dir, "devin"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err = os.WriteFile(filepath.Join(dir, "devin", "vectors.json"), append(raw, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func sanitizeDevinToolDescriptionForFixture(name, desc string) string {
	return translatorcommon.SanitizeDevinToolDescription(name, desc)
}
