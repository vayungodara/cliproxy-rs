// Command main writes server-core goldens from CLIProxyAPI at 6fecc6e. It calls exported
// Go functions in-process only; it opens no sockets and calls no provider endpoint.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/interfaces"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/logging"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/redisqueue"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/session"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/usage"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
	"github.com/tidwall/gjson"
)

type pair struct {
	In  string `json:"in"`
	Out string `json:"out"`
}

var sanitizeInputs = []string{
	"",
	"   ",
	"  plain message  ",
	"open /home/user/.cli-proxy-api/claude-a.json: permission denied",
	"copy /tmp/a to /tmp/b: denied",
	"copy /tmp/a TO /tmp/b: denied",
	"request to https://user:pass@api.example.com/v1?key=abc123&x=1 failed",
	"Post \"https://api.example.com/v1/messages?beta=true\": dial tcp 10.0.0.1:443: i/o timeout",
	"Authorization: Bearer sk-ant-api03-abcdefghijkl",
	"invalid api key: sk-abcdefghijklmnop",
	"invalid x-api-key",
	"the api key is abc123, retry later",
	`{"token": "secret-value", "other": 1}`,
	"Cookie: session=abc; other=1",
	`C:\Users\me\auth.json not found`,
	`\\server\share\file failed`,
	"failed reading '/etc/secret.pem'",
	"failed reading \"/etc/secret.pem\" now",
	"failed reading `/etc/secret.pem` now",
	"error at (/var/lib/x/y): no such file or directory",
	"read /a b/c d: is a directory",
	"upstream returned 401: invalid token abc.def.ghi",
	"password=hunter2 and user=bob",
	"client_secret is s3cr3t; next",
	"auth basic dXNlcjpwYXNz",
	"ghp_abcdefghijk1234 leaked",
	"path /usr/local/bin/tool:12 crashed",
	"multiple /a/b and /c/d paths",
	"config at /opt/app/config.yaml loaded",
	"token: XYZ αβγ",
	"connect: connection refused",
	"sessionid=abcd1234",
	"rate limited, please retry in 30s",
	"model claude-sonnet-4 is not supported: use another",
	"see http://example.com/docs/errors: failed",
	"x=/srv/data/file.txt, y=2",
	"Set-Cookie: a=b\nnext line",
	"key: \"quoted value\" rest",
	"bearer abc.def and more",
	"The access token provided is expired via oauth",
	strings.Repeat("é", 300),
	strings.Repeat("ab ", 100),
}

var extractInputs = []string{
	"",
	`{"error":{"code":"x","message":"boom"}}`,
	`{"error":{"type":"overloaded","message":"overloaded now"}}`,
	`{"error":{"type":"rate_limit_error","message":"Rate limited"}}`,
	`{"error":{"code":"insufficient_quota","message":"You exceeded your quota: insufficient_quota"}}`,
	`status 500: {"message":"m at /home/u/f.json: denied"}`,
	`{"error":"flat sk-abcdefghijkl"}`,
	`{"code":"c","message":""}`,
	`{"type":"t"}`,
	`{"error":{"code":42,"message":"numeric"}}`,
	`{"other":1}`,
	"plain text",
	`[1,2]`,
}

// availabilityCases are per-credential projections of one model, as the conductor
// writes them (clientModelProjectionForAuth): "none", "quota" (a 429 model cooldown:
// suspended with reason quota and quota-exceeded), "other" (suspended for another
// reason) and "other_qe" (suspended for another reason and quota-exceeded).
var availabilityCases = [][]string{
	{"none"},
	{"quota"},
	{"other"},
	{"other_qe"},
	{"none", "none"},
	{"quota", "quota"},
	{"other", "other"},
	{"other", "none"},
	{"quota", "none"},
	{"quota", "other"},
	{"other_qe", "none"},
	{"other_qe", "quota"},
	{"other_qe", "other_qe"},
	{"quota", "quota", "other"},
	{"other", "other", "none"},
}

type availability struct {
	States    []string `json:"states"`
	Available bool     `json:"available"`
}

// modelAvailability registers one model per case on the real registry, applies the
// projections and reads GetAvailableModels.
func modelAvailability() []availability {
	reg := registry.GetGlobalRegistry()
	models := make([]string, len(availabilityCases))
	for i, states := range availabilityCases {
		models[i] = fmt.Sprintf("golden-model-%d", i)
		for j, state := range states {
			client := fmt.Sprintf("golden-client-%d-%d", i, j)
			reg.RegisterClient(client, "openai", []*registry.ModelInfo{{ID: models[i], Object: "model", OwnedBy: "golden"}})
			projection := registry.ClientModelProjection{ModelID: models[i]}
			switch state {
			case "quota":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "quota", true
			case "other":
				projection.Suspended, projection.SuspendReason = true, "unauthorized"
			case "other_qe":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "cloudflare_challenge", true
			}
			if !reg.ApplyClientModelProjections(client, reg.ClientRegistrationEpoch(client), 1, []registry.ClientModelProjection{projection}) {
				panic("projection rejected for " + client)
			}
		}
	}
	listed := map[string]bool{}
	for _, m := range reg.GetAvailableModels("openai") {
		listed[m["id"].(string)] = true
	}
	out := make([]availability, len(availabilityCases))
	for i, states := range availabilityCases {
		out[i] = availability{states, listed[models[i]]}
	}
	return out
}

// step is one MarkResult on a fresh claude credential. RetryAfterMs < 0 means no hint.
type step struct {
	Model           string `json:"model"`
	Status          int    `json:"status"`
	Message         string `json:"message"`
	RetryAfterMs    int64  `json:"retry_after_ms"`
	CredentialScope bool   `json:"credential_scope"`
}

type cooldownCase struct {
	Name  string `json:"name"`
	Steps []step `json:"steps"`
	// Seconds until each touched model's NextRetryAfter, rounded; 0 when none.
	Seconds map[string]int64 `json:"seconds"`
	// Quota marks a model state left quota-exceeded.
	Quota map[string]bool `json:"quota"`
}

func s(model string, status int, message string, retryAfterMs int64) step {
	return step{Model: model, Status: status, Message: message, RetryAfterMs: retryAfterMs}
}

var cooldownInputs = []cooldownCase{
	{Name: "429_no_hint", Steps: []step{s("m1", 429, "rate limited", -1)}},
	{Name: "429_twice_reuses_deadline", Steps: []step{s("m1", 429, "rate limited", -1), s("m1", 429, "rate limited", -1)}},
	{Name: "429_zero_hint_floor", Steps: []step{s("m1", 429, "rate limited", 0)}},
	{Name: "429_small_hint_floor", Steps: []step{s("m1", 429, "rate limited", 3000)}},
	{Name: "429_large_hint", Steps: []step{s("m1", 429, "rate limited", 30000)}},
	{Name: "429_hint_then_none", Steps: []step{s("m1", 429, "rate limited", 120000), s("m1", 429, "rate limited", -1)}},
	{Name: "401", Steps: []step{s("m1", 401, "unauthorized", -1)}},
	{Name: "402", Steps: []step{s("m1", 402, "payment required", -1)}},
	{Name: "403", Steps: []step{s("m1", 403, "forbidden", -1)}},
	{Name: "404", Steps: []step{s("m1", 404, "not found", -1)}},
	{Name: "404_hint", Steps: []step{s("m1", 404, "not found", 60000)}},
	{Name: "500", Steps: []step{s("m1", 500, "boom", -1)}},
	{Name: "500_hint", Steps: []step{s("m1", 500, "boom", 5000)}},
	{Name: "503", Steps: []step{s("m1", 503, "overloaded", -1)}},
	{Name: "418_default", Steps: []step{s("m1", 418, "teapot", -1)}},
	{Name: "400_request_fault", Steps: []step{s("m1", 400, "bad request", -1)}},
	{Name: "400_model_support", Steps: []step{s("m1", 400, "The requested model is not supported", -1)}},
	{Name: "404_model_support_hint", Steps: []step{s("m1", 404, "model_not_supported", 90000)}},
	{Name: "400_invalid_grant", Steps: []step{s("m1", 400, `{"error":"invalid_grant"}`, -1)}},
	{Name: "403_cloudflare", Steps: []step{s("m1", 403, "<html>Just a moment... cloudflare</html>", -1)}},
	{Name: "403_cloudflare_twice", Steps: []step{s("m1", 403, "challenge-platform", -1), s("m1", 403, "challenge-platform", -1)}},
	{Name: "500_then_429", Steps: []step{s("m1", 500, "boom", -1), s("m1", 429, "rate limited", -1)}},
	{Name: "401_then_429_short", Steps: []step{s("m1", 401, "unauthorized", -1), s("m1", 429, "rate limited", 5000)}},
	{Name: "credential_scope", Steps: []step{
		s("m2", 500, "boom", -1),
		{Model: "m1", Status: 429, Message: "credential quota", RetryAfterMs: 600000, CredentialScope: true},
	}},
	{Name: "credential_scope_short_keeps_sibling", Steps: []step{
		s("m2", 401, "unauthorized", -1),
		{Model: "m1", Status: 429, Message: "credential quota", RetryAfterMs: 20000, CredentialScope: true},
	}},
}

// cooldowns drives Manager.MarkResult on one fresh credential per case.
func cooldowns() []cooldownCase {
	ctx := context.Background()
	m := auth.NewManager(nil, nil, nil)
	out := make([]cooldownCase, len(cooldownInputs))
	for i, c := range cooldownInputs {
		id := fmt.Sprintf("golden-%d.json", i)
		if _, err := m.Register(ctx, &auth.Auth{ID: id, Provider: "claude", Metadata: map[string]any{"type": "claude"}}); err != nil {
			panic(err)
		}
		for _, st := range c.Steps {
			result := auth.Result{
				AuthID:          id,
				Provider:        "claude",
				Model:           st.Model,
				CredentialScope: st.CredentialScope,
				Error:           &auth.Error{HTTPStatus: st.Status, Message: st.Message},
			}
			if st.RetryAfterMs >= 0 {
				d := time.Duration(st.RetryAfterMs) * time.Millisecond
				result.RetryAfter = &d
			}
			m.MarkResult(ctx, result)
		}
		current, _ := m.GetByID(id)
		c.Seconds = map[string]int64{}
		c.Quota = map[string]bool{}
		for _, st := range c.Steps {
			state := current.ModelStates[st.Model]
			if state == nil || !state.NextRetryAfter.After(time.Now()) {
				c.Seconds[st.Model] = 0
			} else {
				c.Seconds[st.Model] = int64(time.Until(state.NextRetryAfter).Round(time.Second) / time.Second)
			}
			c.Quota[st.Model] = state != nil && state.Quota.Exceeded
		}
		out[i] = c
	}
	return out
}

// sessionCase is one request: headers as name/value pairs (repeatable), the body, the
// execution session, the source format and the client key (for the caller scope).
type sessionCase struct {
	Name      string     `json:"name"`
	Headers   [][]string `json:"headers"`
	Payload   string     `json:"payload"`
	Execution string     `json:"execution"`
	Format    string     `json:"format"`
	ClientKey string     `json:"client_key"`

	Info *session.SessionInfo `json:"info"`
	// From session.Enrich: the derived identity and canonical/parent session metadata.
	Derived   string `json:"derived"`
	Canonical string `json:"canonical"`
	Parent    string `json:"parent"`
	// auth.ExtractSessionID with Enrich's metadata, and with only the execution session
	// (the first-messages hash path when nothing explicit exists).
	SessionID     string `json:"session_id"`
	HashSessionID string `json:"hash_session_id"`
	CallerScope   string `json:"caller_scope"`
}

func h(pairs ...string) [][]string {
	var out [][]string
	for i := 0; i+1 < len(pairs); i += 2 {
		out = append(out, []string{pairs[i], pairs[i+1]})
	}
	return out
}

var long = strings.Repeat("a", 180) + strings.Repeat("é", 38)

var sessionInputs = []sessionCase{
	{Name: "claude_header", Headers: h("X-Claude-Code-Session-Id", "11111111-2222-3333-4444-555555555555")},
	{Name: "claude_header_agent_parent", Headers: h("X-Claude-Code-Session-Id", "s1", "X-Claude-Code-Agent-Id", "explorer", "X-Claude-Code-Parent-Agent-Id", "planner")},
	{Name: "claude_header_body_agent", Headers: h("X-Claude-Code-Session-Id", "s1"), Payload: `{"metadata":{"agent_id":"worker"},"parent_session_id":"root-1"}`},
	{Name: "claude_header_body_parent", Headers: h("X-Claude-Code-Session-Id", "s1"), Payload: `{"parent_session_id":"root-1"}`},
	{Name: "claude_header_main_agent", Headers: h("X-Claude-Code-Session-Id", "s1", "X-Claude-Code-Agent-Id", "main")},
	{Name: "claude_user_id_json", Payload: `{"metadata":{"user_id":"{\"session_id\":\"sess-9\",\"parent_session_id\":\"sess-1\",\"agent_id\":\"a7\"}"}}`},
	{Name: "claude_user_id_json_no_agent", Payload: `{"metadata":{"user_id":"{\"session_id\":\"sess-9\",\"parent_agent_id\":\"sess-2\"}"}}`},
	{Name: "claude_user_id_legacy", Payload: `{"metadata":{"user_id":"user_abc_account__session_1234-abcd","parent_agent_id":"p-1"}}`},
	{Name: "claude_user_id_legacy_bad", Payload: `{"metadata":{"user_id":"user_x_session_zz"}}`},
	{Name: "codex_session", Headers: h("Session-Id", "019a0000-0000-7000-8000-000000000001")},
	{Name: "codex_session_thread", Headers: h("Session-Id", "sess", "Thread-Id", "thread")},
	{Name: "codex_turn_metadata", Headers: h("X-Codex-Turn-Metadata", `{"session_id":"s","thread_id":"t","agent_name":"/root/explorer","subagent_kind":"thread_spawn"}`)},
	{Name: "codex_turn_fork", Headers: h("Session-Id", "s", "X-Codex-Turn-Metadata", `{"thread_id":"t2","forked_from_thread_id":"t1"}`)},
	{Name: "codex_parent_thread", Headers: h("Session-Id", "s", "X-Codex-Parent-Thread-Id", "p")},
	{Name: "codex_underscore_body_thread", Headers: h("Session_id", "s"), Payload: `{"thread_id":"s"}`},
	{Name: "codex_openai_subagent", Headers: h("Session-Id", "s", "X-Openai-Subagent", "true")},
	{Name: "codex_body_parent", Headers: h("Session-Id", "s"), Payload: `{"parent_id":"root"}`},
	{Name: "agy", Headers: h("X-Http-Session-Id", "a1", "X-Parent-Session-Id", "a0")},
	{Name: "generic_session", Headers: h("X-Session-ID", "g1")},
	{Name: "opencode", Headers: h("X-Session-Affinity", "o1", "X-Parent-Session-Affinity", "o0")},
	{Name: "pi_slot", Headers: h("X-Slot-Session-Id", "slot-1")},
	{Name: "task_header", Headers: h("X-Task-Id", "t1", "X-Parent-Task-Id", "t0")},
	{Name: "conversation_header", Headers: h("X-Conversation-Id", "c1")},
	{Name: "thread_header", Headers: h("X-Thread-Id", "th1", "X-Parent-ID", "th0")},
	{Name: "client_request", Headers: h("X-Client-Request-Id", "cr1")},
	{Name: "gemini_cache", Payload: `{"cachedContent":"cachedContents/abc","contents":[]}`, Format: "gemini"},
	{Name: "nested_request_session", Payload: `{"request":{"session_id":"nested"}}`, Format: "antigravity"},
	{Name: "body_thread_child", Payload: `{"thread_id":"t","parent_id":"p"}`},
	{Name: "body_thread_fork", Payload: `{"thread_id":"t","parent_id":"p","forked_from_id":"p"}`},
	{Name: "body_session_agent", Payload: `{"session_id":"s","metadata":{"agent_id":"helper"}}`},
	{Name: "body_session_numeric", Payload: `{"session_id":12345}`},
	{Name: "body_task", Payload: `{"task_id":"roo-1"}`},
	{Name: "prompt_cache_key", Payload: `{"prompt_cache_key":"pck-1","conversation":{"id":"conv-1"}}`, Format: "openai-response"},
	{Name: "conversation_string", Payload: `{"conversation":"conv-2"}`, Format: "openai-response"},
	{Name: "plain_user_id", Payload: `{"metadata":{"user_id":"u-42"},"messages":[{"role":"user","content":"hi"}]}`, Format: "claude"},
	{Name: "legacy_conversation_id", Payload: `{"conversation_id":"c-3"}`},
	{Name: "execution_only", Execution: "ws-123"},
	{Name: "control_char_rejected", Headers: h("X-Session-ID", "bad\u0001id")},
	{Name: "unicode_trimmed", Headers: h("X-Session-ID", "\u00a0spaced\u00a0")},
	{Name: "long_bounded", Headers: h("X-Claude-Code-Session-Id", long)},
	{Name: "derived_openai", Payload: `{"model":"m","messages":[{"role":"system","content":"You are <helpful> & brief"},{"role":"user","content":[{"type":"text","text":"Hello"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}]}]}`, Format: "openai", ClientKey: "fake-client-key"},
	{Name: "derived_openai_no_key", Payload: `{"messages":[{"role":"user","content":"Hello"},{"role":"assistant","content":"Hi there"}]}`, Format: "openai"},
	{Name: "derived_claude_system_blocks", Payload: `{"system":[{"type":"text","text":"Sys one","cache_control":{"type":"ephemeral"}},{"type":"text","text":"Sys two"}],"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/PNG","data":"QUJD"}},{"type":"tool_result","tool_use_id":"x","cache_control":{"type":"ephemeral"},"is_error":false,"n":1.50}]}]}`, Format: "claude", ClientKey: "k"},
	{Name: "derived_responses", Payload: `{"instructions":"Be terse.","input":[{"role":"developer","content":"dev note"},{"type":"message","role":"user","content":[{"type":"input_text","text":"Question?"}]}]}`, Format: "codex", ClientKey: "k"},
	{Name: "derived_responses_string", Payload: `{"input":"just text"}`, Format: "openai-response"},
	{Name: "derived_gemini_envelope", Payload: `{"request":{"systemInstruction":{"parts":[{"text":"sys"}]},"contents":[{"role":"model","parts":[{"text":"x"}]},{"role":"USER","parts":[{"inlineData":{"mimeType":"image/jpeg","data":"Zm9v"}},{"text":"look"}]}]}}`, Format: "antigravity", ClientKey: "k"},
	{Name: "derived_interactions_steps", Payload: `{"system_instruction":"sys","input":[{"role":"user","steps":[{"type":"text","text":"first"}]}]}`, Format: "interactions"},
	{Name: "derived_interactions_string", Payload: `{"input":"hello there"}`, Format: "interactions"},
	{Name: "no_user_input", Payload: `{"messages":[{"role":"system","content":"only system"}]}`, Format: "openai"},
	{Name: "gemini_hash", Payload: `{"systemInstruction":{"parts":[{"text":"s"}]},"contents":[{"role":"user","parts":[{"text":"u"}]},{"role":"model","parts":[{"text":"m"}]}]}`, Format: "gemini"},
	{Name: "explicit_missed_by_has_explicit", Payload: `{"metadata":{"sessionID":"x"},"messages":[{"role":"user","content":"hi"}]}`},
	{Name: "parent_header_only", Headers: h("X-Parent-ID", "p"), Payload: `{"messages":[{"role":"user","content":"hi"}]}`},
	{Name: "empty"},
}

func sessions() []sessionCase {
	out := make([]sessionCase, len(sessionInputs))
	for i, c := range sessionInputs {
		headers := http.Header{}
		for _, pair := range c.Headers {
			headers.Add(pair[0], pair[1])
		}
		payload := []byte(c.Payload)
		meta := map[string]any{}
		if c.Execution != "" {
			meta[cliproxyexecutor.ExecutionSessionMetadataKey] = c.Execution
		}
		if info, ok := session.ExtractSessionInfo(headers, payload, meta); ok {
			c.Info = &info
		}
		c.HashSessionID = auth.ExtractSessionID(headers, payload, meta)
		format := c.Format
		if format == "" {
			format = "openai"
		}
		optsMeta := map[string]any{}
		for k, v := range meta {
			optsMeta[k] = v
		}
		if c.ClientKey != "" {
			c.CallerScope = session.CallerScope(c.ClientKey)
			optsMeta[cliproxyexecutor.CallerScopeMetadataKey] = c.CallerScope
		}
		_, opts := session.Enrich(
			cliproxyexecutor.Request{Payload: payload},
			cliproxyexecutor.Options{Headers: headers, SourceFormat: sdktranslator.Format(format), Metadata: optsMeta},
		)
		c.Derived, _ = opts.Metadata[cliproxyexecutor.DerivedSessionIDMetadataKey].(string)
		c.Canonical, _ = opts.Metadata[cliproxyexecutor.CanonicalSessionIDMetadataKey].(string)
		c.Parent, _ = opts.Metadata[cliproxyexecutor.ParentSessionIDMetadataKey].(string)
		c.SessionID = auth.ExtractSessionID(headers, payload, opts.Metadata)
		c.Format = format
		out[i] = c
	}
	return out
}

// affinityStep is one Pick (with the available credential IDs) or one OnResult.
type affinityStep struct {
	Op        string     `json:"op"` // pick, ok, fail
	Headers   [][]string `json:"headers,omitempty"`
	Payload   string     `json:"payload,omitempty"`
	Available []string   `json:"available,omitempty"`
	Auth      string     `json:"auth,omitempty"`
	Status    int        `json:"status,omitempty"`
	Message   string     `json:"message,omitempty"`
	Picked    string     `json:"picked,omitempty"`
	// LCP cases: a per-step caller key, and what the pick wrote into the metadata.
	Caller string   `json:"caller,omitempty"`
	LCP    *lcpMeta `json:"lcp,omitempty"`
}

type lcpMeta struct {
	Session    string `json:"session"`
	Parent     string `json:"parent"`
	NodeKind   string `json:"node_kind"`
	Fork       bool   `json:"fork"`
	Compaction bool   `json:"compaction"`
	Generation uint64 `json:"generation"`
}

type affinityCase struct {
	Name             string            `json:"name"`
	SubagentAffinity bool              `json:"subagent_affinity"`
	Priorities       map[string]string `json:"priorities,omitempty"`
	Steps            []affinityStep    `json:"steps"`
	// LCP cases: selection through the mixed picker (provider "mixed") with a caller
	// scope from Caller, in this source format.
	LCP    bool   `json:"lcp,omitempty"`
	Caller string `json:"caller,omitempty"`
	Format string `json:"format,omitempty"`
}

func pick(hs [][]string, payload string, available ...string) affinityStep {
	return affinityStep{Op: "pick", Headers: hs, Payload: payload, Available: available}
}

func result(op string, hs [][]string, payload, auth string, status int, message string) affinityStep {
	return affinityStep{Op: op, Headers: hs, Payload: payload, Auth: auth, Status: status, Message: message}
}

var (
	s1      = h("X-Session-ID", "s1")
	root    = h("X-Claude-Code-Session-Id", "root")
	worker  = h("X-Claude-Code-Session-Id", "root", "X-Claude-Code-Agent-Id", "worker")
	thread1 = h("Session-Id", "t1")
	fork2   = h("Session-Id", "t1", "X-Codex-Turn-Metadata", `{"thread_id":"t2","forked_from_thread_id":"t1"}`)
	pck1    = `{"prompt_cache_key":"p1","conversation":{"id":"c1"}}`
	pck2    = `{"prompt_cache_key":"p2","conversation":{"id":"c1"}}`
	chat1   = `{"messages":[{"role":"user","content":"hi"}]}`
	chat2   = `{"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"more"}]}`
)

var affinityInputs = []affinityCase{
	{Name: "binding_kept_then_released_on_failure", SubagentAffinity: true, Steps: []affinityStep{
		pick(s1, "", "b"), pick(s1, "", "a", "b"), result("fail", s1, "", "b", 500, "boom"), pick(s1, "", "a", "b"),
	}},
	{Name: "request_fault_keeps_binding", SubagentAffinity: true, Steps: []affinityStep{
		pick(s1, "", "b"), result("fail", s1, "", "b", 400, "bad request"), pick(s1, "", "a", "b"),
	}},
	{Name: "success_from_other_credential_does_not_rebind", SubagentAffinity: true, Steps: []affinityStep{
		pick(s1, "", "b"), result("ok", s1, "", "a", 0, ""), pick(s1, "", "a", "b"),
	}},
	{Name: "subagent_inherits_parent", SubagentAffinity: true, Steps: []affinityStep{
		pick(root, "", "b"), pick(worker, "", "a", "b"), pick(root, "", "a", "b"),
	}},
	{Name: "subagent_affinity_off", SubagentAffinity: false, Steps: []affinityStep{
		pick(root, "", "b"), pick(worker, "", "a", "b"),
	}},
	{Name: "subagent_failure_keeps_parent", SubagentAffinity: true, Steps: []affinityStep{
		pick(root, "", "b"), pick(worker, "", "a", "b"), result("fail", worker, "", "b", 500, "boom"), pick(root, "", "a", "b"), pick(worker, "", "a", "b"),
	}},
	{Name: "fork_inherits_parent", SubagentAffinity: false, Steps: []affinityStep{
		pick(thread1, "", "b"), pick(fork2, "", "a", "b"), result("fail", thread1, "", "b", 500, "boom"), pick(fork2, "", "a", "b"),
	}},
	{Name: "prompt_cache_conversation_alias", SubagentAffinity: true, Steps: []affinityStep{
		pick(nil, pck1, "b"), pick(nil, pck2, "a", "b"), result("fail", nil, pck2, "b", 500, "boom"), pick(nil, pck1, "a", "b"),
	}},
	{Name: "binding_beats_recovered_priority", SubagentAffinity: true, Priorities: map[string]string{"c": "5"}, Steps: []affinityStep{
		pick(s1, "", "a"), pick(s1, "", "a", "c"), pick(nil, "", "a", "c"),
	}},
	{Name: "unavailable_binding_rebinds_highest_tier", SubagentAffinity: true, Priorities: map[string]string{"c": "5"}, Steps: []affinityStep{
		pick(s1, "", "b"), pick(s1, "", "a", "c"), pick(s1, "", "a", "b", "c"),
	}},
	{Name: "derived_identity_survives_new_turns", SubagentAffinity: true, Steps: []affinityStep{
		pick(nil, chat1, "b"), pick(nil, chat2, "a", "b"),
	}},
}

func lcpPick(payload string, available ...string) affinityStep {
	return affinityStep{Op: "pick", Payload: payload, Available: available}
}

// lcpThen picks and reports the outcome on the picked credential with the pick's own
// options, as the conductor does.
func lcpThen(op string, status int, payload string, available ...string) affinityStep {
	return affinityStep{Op: "pick_" + op, Payload: payload, Available: available, Status: status, Message: "boom"}
}

func chat(turns ...string) string {
	msgs := make([]string, 0, len(turns))
	for i, t := range turns {
		role := "user"
		if i%2 == 1 {
			role = "assistant"
		}
		if strings.HasPrefix(t, "system:") {
			role, t = "system", strings.TrimPrefix(t, "system:")
		}
		msgs = append(msgs, fmt.Sprintf(`{"role":%q,"content":%q}`, role, t))
	}
	return `{"messages":[` + strings.Join(msgs, ",") + `]}`
}

const (
	gemFirst     = `{"contents":[{"role":"user","parts":[{"text":"step 1"}]},{"role":"model","parts":[{"text":"ack 1"}]},{"role":"user","parts":[{"text":"step 2"}]},{"role":"model","parts":[{"text":"ack 2"}]},{"role":"user","parts":[{"text":"step 3"}]}]}`
	gemCompacted = `{"contents":[{"role":"user","parts":[{"text":"<summary>Steps 1 and 2 completed</summary>"}]},{"role":"model","parts":[{"text":"ack 2"}]},{"role":"user","parts":[{"text":"step 3"}]},{"role":"user","parts":[{"text":"step 4"}]}]}`
	claudeFirst  = `{"system":"Be brief","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}`
	claudeGrown  = `{"system":"Be brief","messages":[{"role":"user","content":[{"type":"text","text":"hello"}]},{"role":"assistant","content":[{"type":"text","text":"hi"}]},{"role":"user","content":"more"}]}`
	respFirst    = `{"instructions":"Be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}]}`
	respGrown    = `{"instructions":"Be brief","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},{"type":"function_call","call_id":"c1","name":"ls","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"a.txt"}]}`
)

var lcpAffinityInputs = []affinityCase{
	{Name: "lcp_growth_keeps_binding", Steps: []affinityStep{
		lcpPick(chat("hello"), "b"), lcpThen("ok", 0, chat("hello", "hi", "more"), "a", "b"), lcpPick(chat("hello", "hi", "more", "sure", "again"), "a", "b"),
	}},
	{Name: "lcp_fork_derives_lineage", Steps: []affinityStep{
		lcpThen("ok", 0, chat("hello", "hi", "more"), "b"), lcpPick(chat("hello", "hi", "other"), "a", "b"), lcpThen("ok", 0, chat("hello", "hi", "other"), "a", "b"), lcpPick(chat("hello", "hi", "other", "x", "y"), "a", "b"),
	}},
	{Name: "lcp_failure_removes_exact_sequence", Steps: []affinityStep{
		lcpThen("ok", 0, chat("hello"), "b"), lcpThen("fail", 500, chat("hello", "hi", "more"), "a", "b"), lcpPick(chat("hello", "hi", "more"), "a", "b"), lcpThen("fail", 500, chat("hello"), "a", "b"), lcpPick(chat("hello"), "a", "b"),
	}},
	{Name: "lcp_request_fault_keeps_binding", Steps: []affinityStep{
		lcpPick(chat("hello"), "b"), lcpThen("fail", 400, chat("hello"), "a", "b"), lcpPick(chat("hello"), "a", "b"),
	}},
	{Name: "lcp_unavailable_binding_rebinds", Steps: []affinityStep{
		lcpPick(chat("hello"), "b"), lcpThen("ok", 0, chat("hello", "hi", "more"), "a"), lcpPick(chat("hello", "hi", "more"), "a", "b"), lcpPick(chat("hello", "hi", "more", "x", "y"), "a", "b"),
	}},
	{Name: "lcp_binding_beats_recovered_priority", Priorities: map[string]string{"c": "5"}, Steps: []affinityStep{
		lcpPick(chat("hello"), "a"), lcpPick(chat("hello", "hi", "more"), "a", "c"), lcpPick(chat("fresh"), "a", "c"),
	}},
	{Name: "lcp_caller_isolation", Steps: []affinityStep{
		lcpPick(chat("hello"), "b"), func() affinityStep { st := lcpPick(chat("hello"), "a", "b"); st.Caller = "client-key-2"; return st }(), lcpPick(chat("hello"), "a", "b"),
	}},
	{Name: "lcp_explicit_session_wins", Steps: []affinityStep{
		{Op: "pick", Headers: s1, Payload: chat("hello"), Available: []string{"b"}}, lcpPick(chat("hello"), "a", "b"), {Op: "pick", Headers: s1, Payload: chat("hello", "hi", "more"), Available: []string{"a", "b"}},
	}},
	{Name: "lcp_system_only_falls_back", Steps: []affinityStep{
		lcpPick(chat("system:rules"), "b"), lcpPick(chat("system:rules"), "a", "b"),
	}},
	{Name: "lcp_system_prefix_is_not_evidence", Steps: []affinityStep{
		lcpPick(chat("system:rules", "hello"), "b"), lcpPick(chat("system:rules", "other"), "a", "b"), lcpPick(chat("system:rules", "hello", "hi", "more"), "a", "b"),
	}},
	{Name: "lcp_anonymous_caller_uses_session_cache", Caller: "-", Steps: []affinityStep{
		lcpPick(chat("hello"), "b"), lcpPick(chat("hello", "hi", "more"), "a", "b"),
	}},
	{Name: "lcp_gemini_compaction", Format: "gemini", Steps: []affinityStep{
		lcpThen("ok", 0, gemFirst, "b"), lcpPick(gemCompacted, "a", "b"), lcpThen("ok", 0, gemCompacted, "a", "b"),
	}},
	{Name: "lcp_claude_growth", Format: "claude", Steps: []affinityStep{
		lcpPick(claudeFirst, "b"), lcpPick(claudeGrown, "a", "b"),
	}},
	{Name: "lcp_responses_growth", Format: "openai-response", Steps: []affinityStep{
		lcpPick(respFirst, "b"), lcpPick(respGrown, "a", "b"),
	}},
}

func init() {
	for _, c := range lcpAffinityInputs {
		c.LCP = true
		c.SubagentAffinity = true
		switch c.Caller {
		case "":
			c.Caller = "client-key-1"
		case "-":
			c.Caller = ""
		}
		if c.Format == "" {
			c.Format = "openai"
		}
		affinityInputs = append(affinityInputs, c)
	}
}

func lcpMetadata(md map[string]any) *lcpMeta {
	session, _ := md[cliproxyexecutor.LCPAffinitySessionIDMetadataKey].(string)
	if session == "" {
		return nil
	}
	m := &lcpMeta{Session: session}
	m.Parent, _ = md[cliproxyexecutor.ParentSessionIDMetadataKey].(string)
	m.NodeKind, _ = md[cliproxyexecutor.NodeKindMetadataKey].(string)
	m.Fork, _ = md[cliproxyexecutor.IsForkMetadataKey].(bool)
	m.Compaction, _ = md[cliproxyexecutor.IsCompactionMetadataKey].(bool)
	m.Generation, _ = md[cliproxyexecutor.LCPAccessGenerationMetadataKey].(uint64)
	return m
}

func affinities() []affinityCase {
	ctx := context.Background()
	out := make([]affinityCase, len(affinityInputs))
	for i, c := range affinityInputs {
		subagent := c.SubagentAffinity
		selector := auth.NewSessionAffinitySelectorWithConfig(auth.SessionAffinityConfig{
			Fallback:         &auth.FillFirstSelector{},
			TTL:              time.Hour,
			SubagentAffinity: &subagent,
		})
		auths := map[string]*auth.Auth{}
		for _, id := range []string{"a", "b", "c"} {
			attrs := map[string]string{}
			if p := c.Priorities[id]; p != "" {
				attrs["priority"] = p
			}
			auths[id] = &auth.Auth{ID: id, Provider: "claude", Attributes: attrs}
		}
		steps := make([]affinityStep, len(c.Steps))
		for j, st := range c.Steps {
			headers := http.Header{}
			for _, pair := range st.Headers {
				headers.Add(pair[0], pair[1])
			}
			format, provider := "openai", "claude"
			metadata := map[string]any{}
			if c.LCP {
				format, provider = c.Format, "mixed"
				caller := c.Caller
				if st.Caller != "" {
					caller = st.Caller
				}
				if scope := session.CallerScope(caller); scope != "" {
					metadata[cliproxyexecutor.CallerScopeMetadataKey] = scope
				}
			}
			_, opts := session.Enrich(
				cliproxyexecutor.Request{Payload: []byte(st.Payload)},
				cliproxyexecutor.Options{Headers: headers, SourceFormat: sdktranslator.Format(format), OriginalRequest: []byte(st.Payload), Metadata: metadata},
			)
			switch st.Op {
			case "pick", "pick_ok", "pick_fail":
				var candidates []*auth.Auth
				for _, id := range st.Available {
					candidates = append(candidates, auths[id])
				}
				picked, err := selector.Pick(ctx, provider, "m", opts, candidates)
				if err != nil {
					panic(err)
				}
				st.Picked = picked.ID
				if c.LCP {
					st.LCP = lcpMetadata(opts.Metadata)
				}
				if st.Op != "pick" {
					res := auth.Result{AuthID: picked.ID, Provider: provider, Model: "m", Success: st.Op == "pick_ok", Options: opts}
					if st.Op == "pick_fail" {
						res.Error = &auth.Error{HTTPStatus: st.Status, Message: st.Message}
					}
					selector.OnResult(res)
				}
			default:
				res := auth.Result{AuthID: st.Auth, Provider: "claude", Model: "m", Success: st.Op == "ok", Options: opts}
				if st.Op == "fail" {
					res.Error = &auth.Error{HTTPStatus: st.Status, Message: st.Message}
				}
				selector.OnResult(res)
			}
			steps[j] = st
		}
		selector.Stop()
		c.Steps = steps
		out[i] = c
	}
	return out
}

var goTimestamp = regexp.MustCompile(`"20\d\d-\d\d-\d\dT[0-9:.]+Z"`)

// cooldownFiles drives MarkResult with a FileCooldownStateStore rooted at a temporary
// auth dir and returns each .cds file (relative path -> content), with non-zero
// timestamps replaced by "<time>".
func cooldownFiles() map[string]string {
	ctx := context.Background()
	dir, err := os.MkdirTemp("", "cds")
	if err != nil {
		panic(err)
	}
	defer os.RemoveAll(dir)
	m := auth.NewManager(nil, nil, nil)
	m.SetCooldownStateStore(auth.NewFileCooldownStateStoreWithAuthDir(dir, dir))
	register := func(id, path string, disabled bool) {
		attrs := map[string]string{}
		if path != "" {
			attrs["path"] = filepath.Join(dir, path)
		}
		if _, err := m.Register(ctx, &auth.Auth{ID: id, Provider: "claude", Attributes: attrs, Disabled: disabled, Metadata: map[string]any{"type": "claude"}}); err != nil {
			panic(err)
		}
	}
	register("claude-a.json", "claude-a.json", false)
	register("sub/team b.json", "sub/team b.json", false)
	register("cfg:key/0", "", false)
	register("off.json", "off.json", true)
	fail := func(id, model string, status int, message string, retryAfter time.Duration, credential bool) {
		res := auth.Result{AuthID: id, Provider: "claude", Model: model, CredentialScope: credential, Error: &auth.Error{HTTPStatus: status, Message: message}}
		if retryAfter > 0 {
			res.RetryAfter = &retryAfter
		}
		m.MarkResult(ctx, res)
	}
	fail("claude-a.json", "m1", 429, "rate limited", 30*time.Second, false)
	fail("claude-a.json", "m2", 401, "unauthorized", 0, false)
	fail("sub/team b.json", "m2", 500, "boom", 0, false)
	fail("sub/team b.json", "m1", 429, "credential quota", time.Minute, true)
	fail("cfg:key/0", "m3", 403, "challenge-platform", 0, false)
	fail("off.json", "m1", 500, "boom", 0, false)
	m.PersistCooldownStates(ctx)
	out := map[string]string{}
	filepath.WalkDir(dir, func(path string, d fs.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return err
		}
		data, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(dir, path)
		out[filepath.ToSlash(rel)] = goTimestamp.ReplaceAllString(string(data), `"<time>"`)
		return nil
	})
	return out
}

// byProviderClient is one registering credential: provider, projected state and its
// own SupportsWebSearch flag.
type byProviderClient struct {
	Provider string `json:"provider"`
	State    string `json:"state"`
	Search   bool   `json:"search"`
}

type byProviderCase struct {
	Clients []byProviderClient `json:"clients"`
	// Listed reports GetAvailableModelsByProvider("golden-ag"); ListedSearch its flag.
	// ListedSearch is omitted when the provider's credentials disagree (Go picks a
	// random one's info).
	Listed       bool  `json:"listed"`
	ListedSearch *bool `json:"listed_search,omitempty"`
	// InfoSearch is GetModelInfo(model, "").SupportsWebSearch; AgSearch with "golden-ag".
	InfoSearch bool `json:"info_search"`
	AgSearch   bool `json:"ag_search"`
}

// byProvider registers each case's clients (in order) on the real registry and reads
// GetAvailableModelsByProvider and GetModelInfo.
func byProvider() []byProviderCase {
	ag, x := "golden-ag", "golden-x"
	c := func(p, state string, search bool) byProviderClient { return byProviderClient{p, state, search} }
	cases := []byProviderCase{
		{Clients: []byProviderClient{c(ag, "none", true)}},
		{Clients: []byProviderClient{c(ag, "other", true), c(x, "none", true)}},
		{Clients: []byProviderClient{c(ag, "quota", false)}},
		{Clients: []byProviderClient{c(ag, "other", false), c(ag, "none", false)}},
		{Clients: []byProviderClient{c(x, "none", true)}},
		{Clients: []byProviderClient{c(x, "none", true), c(ag, "none", false)}},
		{Clients: []byProviderClient{c(ag, "none", true), c(x, "none", false)}},
		{Clients: []byProviderClient{c(ag, "none", true), c(ag, "none", false)}},
		{Clients: []byProviderClient{c(ag, "other_qe", false), c(x, "other", false)}},
	}
	reg := registry.GetGlobalRegistry()
	for i := range cases {
		model := fmt.Sprintf("golden-bp-model-%d", i)
		for j, cl := range cases[i].Clients {
			client := fmt.Sprintf("golden-bp-client-%d-%d", i, j)
			reg.RegisterClient(client, cl.Provider, []*registry.ModelInfo{{ID: model, Object: "model", OwnedBy: "golden", SupportsWebSearch: cl.Search}})
			projection := registry.ClientModelProjection{ModelID: model}
			switch cl.State {
			case "quota":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "quota", true
			case "other":
				projection.Suspended, projection.SuspendReason = true, "unauthorized"
			case "other_qe":
				projection.Suspended, projection.SuspendReason, projection.QuotaExceeded = true, "cloudflare_challenge", true
			}
			if !reg.ApplyClientModelProjections(client, reg.ClientRegistrationEpoch(client), 1, []registry.ClientModelProjection{projection}) {
				panic("projection rejected for " + client)
			}
		}
		if info := reg.GetModelInfo(model, ""); info != nil {
			cases[i].InfoSearch = info.SupportsWebSearch
		}
		if info := reg.GetModelInfo(model, ag); info != nil {
			cases[i].AgSearch = info.SupportsWebSearch
		}
	}
	for _, info := range reg.GetAvailableModelsByProvider(" GOLDEN-AG ") {
		var i int
		if _, err := fmt.Sscanf(info.ID, "golden-bp-model-%d", &i); err != nil {
			continue
		}
		cases[i].Listed = true
		agree := true
		for _, cl := range cases[i].Clients {
			if cl.Provider == ag && cl.Search != info.SupportsWebSearch {
				agree = false
			}
		}
		if agree {
			search := info.SupportsWebSearch
			cases[i].ListedSearch = &search
		}
	}
	return cases
}

// resolvedConfig is the config every resolved-model case runs against.
const resolvedConfig = `config-version: 8
api-keys:
  claude:
    - name: c1
      base-url: https://c.example
      models:
        - name: claude-sonnet-4-6
          alias: sonnet
          is-compat: true
        - name: claude-opus-4-6(high)
          alias: opus
        - name: claude-custom-x
          alias: custom
          thinking:
            levels: ["HIGH", "none", "high", " auto "]
        - alias: aliasonly
      keys:
        - api-key: ck1
    - name: c2
      base-url: https://dup.example
      headers:
        X-A: "1"
      models:
        - name: claude-sonnet-4-6
          alias: first
      keys:
        - api-key: dupkey
    - name: c3
      base-url: https://dup.example
      headers:
        X-A: "2"
      models:
        - name: claude-opus-4-7
          alias: second
      keys:
        - api-key: dupkey
  codex:
    - name: x1
      base-url: https://codex.example
      prefix: team
      models:
        - name: gpt-6-sol
          alias: sol
          is-compat: true
        - name: gpt-5.5
          alias: five
          support-configuration-update: true
          thinking:
            min: 1
            max: 9
      keys:
        - api-key: xk1
  vertex:
    - name: v1
      base-url: https://vertex.example
      models:
        - name: gemini-2.5-pro
          alias: vpro
      keys:
        - api-key: vk1
  openai-compatibility:
    - name: acme
      base-url: https://acme.example/v1
      models:
        - name: up-model
          alias: acme-m
          is-compat: true
        - name: img-model
          alias: acme-img
          image: true
      keys:
        - api-key: ok1
`

type resolvedCase struct {
	Name       string            `json:"name"`
	AuthID     string            `json:"auth_id"`
	Provider   string            `json:"provider"`
	Synthetic  bool              `json:"synthetic"`
	Attributes map[string]string `json:"attributes"`
	Metadata   map[string]any    `json:"metadata"`
	ReqModel   string            `json:"req_model"`
	Route      string            `json:"route"`
	Upstream   string            `json:"upstream"`
	Restore    bool              `json:"restore"`
	// Source is "api_key", "codex_oauth" or "" (nothing bound).
	Source      string              `json:"source"`
	Info        *registry.ModelInfo `json:"info"`
	IsCompat    bool                `json:"is_compat"`
	SCU         bool                `json:"support_configuration_update"`
	UserDefined bool                `json:"user_defined"`
}

// resolvedModels binds one execution attempt per case (attachResolvedExecutionModelInfo)
// against config-synthesized auths, mutated config indexes and file-style auths.
func resolvedModels() []resolvedCase {
	cfg, err := config.ParseConfigBytes([]byte(resolvedConfig))
	if err != nil {
		panic(err)
	}
	auths, err := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
		Config: cfg, Now: time.Unix(0, 0), IDGenerator: synthesizer.NewStableIDGenerator(),
	})
	if err != nil {
		panic(err)
	}
	find := func(provider, key, index string) *auth.Auth {
		for _, a := range auths {
			if a.Provider == provider && a.Attributes["api_key"] == key && (index == "" || a.Attributes["config_index"] == index) {
				return a.Clone()
			}
		}
		panic("no auth " + provider + " " + key)
	}
	withIndex := func(a *auth.Auth, index string) *auth.Auth {
		a.Attributes["config_index"] = index
		return a
	}
	file := func(id, provider string, attrs map[string]string, meta map[string]any) *auth.Auth {
		return &auth.Auth{ID: id, Provider: provider, Attributes: attrs, Metadata: meta}
	}
	codexOAuth := func(plan string) *auth.Auth {
		attrs := map[string]string{}
		if plan != "" {
			attrs["plan_type"] = plan
		}
		return file("codex-"+plan+".json", "codex", attrs, map[string]any{"type": "codex", "access_token": "t"})
	}
	type spec struct {
		name                 string
		auth                 *auth.Auth
		synthetic            bool
		req, route, upstream string
		restore              bool
	}
	ck1 := func() *auth.Auth { return find("claude", "ck1", "") }
	dup := func() *auth.Auth { return find("claude", "dupkey", "2") }
	xk1 := func() *auth.Auth { return find("codex", "xk1", "") }
	ok1 := func() *auth.Auth { return find("openai-compatible-acme", "ok1", "") }
	at := func(name string, a *auth.Auth, route, upstream string) spec {
		return spec{name: name, auth: a, synthetic: true, req: route, route: route, upstream: upstream}
	}
	restore := func(name string, a *auth.Auth, req string) spec {
		return spec{name: name, auth: a, synthetic: true, req: req, route: "selection-" + req, upstream: "upstream-" + req, restore: true}
	}
	specs := []spec{
		at("alias", ck1(), "sonnet", "claude-sonnet-4-6"),
		at("suffix falls back to base", ck1(), "sonnet(high)", "claude-sonnet-4-6(high)"),
		at("suffixed configured name exact", ck1(), "opus", "claude-opus-4-6(high)"),
		at("suffixed configured name no fallback", ck1(), "opus(low)", "claude-opus-4-6(low)"),
		at("configured thinking normalized", ck1(), "custom", "claude-custom-x"),
		at("alias-only model", ck1(), "aliasonly", "aliasonly"),
		at("case folded", ck1(), "Sonnet", "CLAUDE-SONNET-4-6"),
		at("route without upstream match", ck1(), "sonnet", "claude-opus-4-6"),
		at("headers twin own entry", dup(), "second", "claude-opus-4-7"),
		at("headers twin not the other entry", dup(), "first", "claude-sonnet-4-6"),
		at("stale index to matching twin", withIndex(dup(), "1"), "first", "claude-sonnet-4-6"),
		at("stale index to other key falls back", withIndex(dup(), "0"), "first", "claude-sonnet-4-6"),
		at("stale index to other key not second", withIndex(dup(), "0"), "second", "claude-opus-4-7"),
		at("stale index out of range", withIndex(dup(), "9"), "first", "claude-sonnet-4-6"),
		at("codex configured with prefix", xk1(), "team/sol", "gpt-6-sol"),
		at("codex configured support and thinking", xk1(), "team/five", "gpt-5.5"),
		at("codex suffix fallback", xk1(), "team/sol(high)", "gpt-6-sol(high)"),
		at("codex unlisted model", xk1(), "team/gpt-6-astra", "gpt-6-astra"),
		at("codex unlisted route configured upstream", xk1(), "team/other", "gpt-6-sol"),
		restore("codex restore", xk1(), "sol"),
		restore("claude restore binds nothing", ck1(), "sonnet"),
		at("compat model", ok1(), "acme-m", "up-model"),
		at("compat image model", ok1(), "acme-img", "img-model"),
		at("compat stale index", withIndex(ok1(), "5"), "acme-m", "up-model"),
		at("vertex model type", find("vertex", "vk1", ""), "vpro", "gemini-2.5-pro"),
		{name: "file api key matches config", auth: file("claude-key.json", "claude",
			map[string]string{"api_key": "ck1", "base_url": "https://c.example"}, map[string]any{"type": "claude"}),
			req: "sonnet", route: "sonnet", upstream: "claude-sonnet-4-6"},
		{name: "claude oauth binds nothing", auth: file("claude-oauth.json", "claude", map[string]string{},
			map[string]any{"type": "claude", "access_token": "t"}), req: "sonnet", route: "sonnet", upstream: "claude-sonnet-4-6"},
		{name: "codex oauth plus", auth: codexOAuth("plus"), req: "gpt-6-astra(high)", route: "gpt-6-astra(high)", upstream: "gpt-6-astra(high)"},
		{name: "codex oauth free lacks model", auth: codexOAuth("free"), req: "gpt-6-sol", route: "gpt-6-sol", upstream: "gpt-6-sol"},
		{name: "codex oauth team case folded", auth: codexOAuth("Team"), req: "GPT-5.5", route: "GPT-5.5", upstream: "GPT-5.5"},
		{name: "codex oauth default plan", auth: codexOAuth(""), req: "gpt-6-sol", route: "gpt-6-sol", upstream: "gpt-6-sol"},
		{name: "codex oauth restore", auth: codexOAuth("plus"), req: "gpt-6-luna", route: "selection", upstream: "upstream", restore: true},
	}
	var out []resolvedCase
	for _, s := range specs {
		info, source := auth.GoldenResolvedModelInfo(cfg, s.auth, s.req, s.route, s.upstream, s.restore)
		c := resolvedCase{
			Name: s.name, AuthID: s.auth.ID, Provider: s.auth.Provider, Synthetic: s.synthetic,
			Attributes: s.auth.Attributes, Metadata: s.auth.Metadata,
			ReqModel: s.req, Route: s.route, Upstream: s.upstream, Restore: s.restore,
			Source: source, Info: info,
		}
		if info != nil {
			c.IsCompat, c.SCU, c.UserDefined = info.IsCompat, info.SupportConfigurationUpdate, info.UserDefined
		}
		out = append(out, c)
	}
	return out
}

// usageCase is one upstream attempt: the client-format response, the request context
// and credential, and the queuedUsageDetail Go's usage queue stores for it.
type usageCase struct {
	Name              string              `json:"name"`
	Format            string              `json:"format"`
	Stream            bool                `json:"stream"`
	Lines             []string            `json:"lines"`
	Provider          string              `json:"provider"`
	ExecutorType      string              `json:"executor_type"`
	Model             string              `json:"model"`
	Alias             string              `json:"alias"`
	AuthID            string              `json:"auth_id"`
	AuthProvider      string              `json:"auth_provider"`
	Attributes        map[string]string   `json:"attributes"`
	Metadata          map[string]any      `json:"metadata"`
	ClientKey         string              `json:"client_key"`
	RequestID         string              `json:"request_id"`
	Endpoint          string              `json:"endpoint"`
	ClientIP          string              `json:"client_ip"`
	ResolvedClientIP  string              `json:"resolved_client_ip"`
	XForwardedFor     string              `json:"x_forwarded_for"`
	UserAgent         string              `json:"user_agent"`
	SessionID         string              `json:"session_id"`
	ParentSessionID   string              `json:"parent_session_id"`
	ReasoningEffort   string              `json:"reasoning_effort"`
	TranslatedPayload string              `json:"translated_payload"`
	TranslatedFormat  string              `json:"translated_format"`
	ServiceTier       string              `json:"service_tier"`
	Generate          bool                `json:"generate"`
	Failed            bool                `json:"failed"`
	FailStatus        int                 `json:"fail_status"`
	FailBody          string              `json:"fail_body"`
	ResponseHeaders   map[string][]string `json:"response_headers"`
	ExecutionID       string              `json:"execution_id"`
	RequestedAt       string              `json:"requested_at"`
	LatencyMs         int64               `json:"latency_ms"`
	TTFTMs            int64               `json:"ttft_ms"`
	Queued            json.RawMessage     `json:"queued"`
}

// usageDetail parses one attempt's client-format response the way Go's executor for
// that format does: whole bodies with Parse*Usage, streams through StreamUsageBuffer
// (Claude merges events, OpenAI keeps the last usage), Codex terminal events with
// ParseCodexUsage, Gemini terminal chunks only (FilterSSEUsageMetadata).
func usageDetail(format string, stream bool, lines []string) usage.Detail {
	if !stream {
		body := []byte(lines[0])
		switch format {
		case "claude":
			return helps.ParseClaudeUsage(body)
		case "openai", "openai-response":
			// A buffered Responses body is the completed event's response object:
			// usage and service_tier sit at the top level.
			return helps.ParseOpenAIUsage(body)
		case "gemini":
			return helps.ParseGeminiUsage(body)
		case "interactions":
			return helps.ParseInteractionsUsage(body)
		}
		panic(format)
	}
	var buffer helps.StreamUsageBuffer
	for _, raw := range lines {
		line := []byte(raw)
		switch format {
		case "claude":
			buffer.ObserveClaudeStream(line)
		case "openai":
			buffer.ObserveOpenAIStream(line)
		case "openai-response":
			payload := helps.JSONPayload(line)
			switch gjson.GetBytes(payload, "type").String() {
			case "response.completed", "response.incomplete", "response.done":
				if detail, ok := helps.ParseCodexUsage(payload); ok {
					if _, seen := buffer.Detail(); !seen {
						buffer.Observe(detail, true)
					}
				}
			}
		case "gemini":
			payload := helps.JSONPayload(helps.FilterSSEUsageMetadata(line))
			if detail, ok := helps.ParseGeminiStreamUsage(payload); ok {
				buffer.Observe(detail, true)
			}
		case "interactions":
			if detail, ok := helps.ParseInteractionsStreamUsage(line); ok {
				buffer.Observe(detail, true)
			}
		}
	}
	detail, _ := buffer.Detail()
	return detail
}

func usageRecords() []usageCase {
	gin.SetMode(gin.ReleaseMode)
	at := time.Date(2026, 10, 3, 8, 0, 0, 123456000, time.UTC).Format(time.RFC3339Nano)
	claudeKey := func() (string, string, map[string]string, map[string]any) {
		return "claude:apikey:0a1b2c3d4e5f", "claude", map[string]string{"api_key": "fake-upstream-key", "auth_kind": "apikey", "source": "config:claude[0a1b]", "config_index": "0"}, map[string]any{}
	}
	base := func(name, format string, stream bool, provider, executor, model, alias string, lines ...string) usageCase {
		id, authProvider, attrs, meta := claudeKey()
		return usageCase{
			Name: name, Format: format, Stream: stream, Lines: lines,
			Provider: provider, ExecutorType: executor, Model: model, Alias: alias,
			AuthID: id, AuthProvider: authProvider, Attributes: attrs, Metadata: meta,
			ClientKey: "fake-client-key", RequestID: "0192f5b4-aaaa-7bbb-8ccc-0123456789ab",
			Endpoint: "POST /v1/messages", ClientIP: "10.0.0.2", ResolvedClientIP: "10.0.0.2",
			UserAgent: "claude-cli/2.1.0 (external, cli)", ServiceTier: "auto", Generate: true,
			ExecutionID: "6f9619ff-8b86-4d01-b42d-00cf4fc964ff", RequestedAt: at, LatencyMs: 1234, TTFTMs: 345,
		}
	}
	oauth := func(c *usageCase, provider string, meta map[string]any, attrs map[string]string) {
		c.AuthID, c.AuthProvider, c.Metadata, c.Attributes = provider+"-user.json", provider, meta, attrs
	}
	var cases []usageCase

	c := base("claude buffered api key", "claude", false, "claude", "ClaudeExecutor", "claude-sonnet-4-6", "sonnet",
		`{"id":"msg_1","type":"message","model":"claude-sonnet-4-6-20260101","usage":{"input_tokens":10,"output_tokens":20,"cache_read_input_tokens":5,"cache_creation_input_tokens":3,"output_tokens_details":{"thinking_tokens":7}}}`)
	c.SessionID, c.ParentSessionID = "claude:5B8E6F3A-1234-4ABC-8DEF-0123456789AB", "claude:11111111-2222-4333-8444-555555555555"
	c.ReasoningEffort = "high"
	c.TranslatedPayload, c.TranslatedFormat = `{"thinking":{"type":"enabled","budget_tokens":8192}}`, "claude"
	c.ResponseHeaders = map[string][]string{"X-Request-Id": {"req-1"}, "Content-Type": {"application/json"}}
	cases = append(cases, c)

	c = base("claude stream oauth", "claude", true, "claude", "ClaudeExecutor", "claude-opus-4-7", "claude-opus-4-7",
		"event: message_start",
		`data: {"type":"message_start","message":{"id":"m","model":"claude-opus-4-7","usage":{"input_tokens":12,"cache_read_input_tokens":100,"output_tokens":1}}}`,
		"event: content_block_delta",
		`data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"<hi>"}}`,
		"event: message_delta",
		`data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":40}}`,
		"event: message_stop",
		`data: {"type":"message_stop"}`)
	oauth(&c, "claude", map[string]any{"type": "claude", "email": "user@example.com", "access_token": "fake-access-token"}, map[string]string{"auth_kind": "oauth"})
	c.ClientKey, c.UserAgent, c.XForwardedFor = "", "", "203.0.113.9, 10.0.0.1"
	c.SessionID, c.ParentSessionID = "header:my-session", "header:my-session"
	cases = append(cases, c)

	c = base("openai buffered compat", "openai", false, "openai-compatible-acme", "OpenAICompatExecutor", "up-model", "acme-m",
		`{"id":"c1","object":"chat.completion","model":"up-model","service_tier":"default","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":50,"total_tokens":150,"prompt_tokens_details":{"cached_tokens":30},"completion_tokens_details":{"reasoning_tokens":20}}}`)
	c.AuthID, c.AuthProvider = "openai-compatibility:acme:0376e65a6eff", "openai-compatible-acme"
	c.Attributes = map[string]string{"api_key": "fake-compat-key", "auth_kind": "apikey", "compat_name": "acme", "provider_key": "openai-compatible-acme", "source": "config:acme[0376]"}
	c.Endpoint, c.ServiceTier = "POST /v1/chat/completions", "default"
	cases = append(cases, c)

	c = base("openai stream last usage", "openai", true, "openai-compatible-acme", "OpenAICompatExecutor", "up-model", "acme-m",
		`data: {"id":"c2","object":"chat.completion.chunk","model":"up-model-0612","service_tier":"flex","choices":[{"index":0,"delta":{"content":"hi"}}]}`,
		`data: {"id":"c2","object":"chat.completion.chunk","model":"up-model-0612","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}`,
		`data: {"id":"c2","object":"chat.completion.chunk","model":"up-model-0612","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}}`,
		"data: [DONE]")
	c.Attributes = map[string]string{"api_key": "fake-compat-key", "auth_kind": "apikey"}
	c.Endpoint = "POST /v1/chat/completions"
	cases = append(cases, c)

	c = base("openai partial usage", "openai", false, "xai", "XAIExecutor", "grok-5", "grok-5",
		`{"id":"c3","object":"chat.completion","model":"grok-5","usage":{"prompt_tokens":5}}`)
	c.Endpoint = "POST /v1/chat/completions"
	cases = append(cases, c)

	c = base("responses buffered codex oauth", "openai-response", false, "codex", "CodexExecutor", "gpt-6-sol", "gpt-6-sol(high)",
		`{"id":"resp_1","object":"response","model":"gpt-6-sol","service_tier":"priority","status":"completed","usage":{"input_tokens":200,"input_tokens_details":{"cached_tokens":50},"output_tokens":80,"output_tokens_details":{"reasoning_tokens":30},"total_tokens":280}}`)
	oauth(&c, "codex", map[string]any{"type": "codex", "email": "dev@example.com", "access_token": "fake-codex-token"}, map[string]string{"plan_type": "plus"})
	c.Endpoint, c.ReasoningEffort = "POST /v1/responses", "high"
	c.TranslatedPayload, c.TranslatedFormat = `{"model":"gpt-6-sol","reasoning":{"effort":"medium"}}`, "codex"
	cases = append(cases, c)

	c = base("responses stream terminal", "openai-response", true, "codex", "CodexExecutor", "gpt-6-sol", "gpt-6-sol",
		"event: response.created",
		`data: {"type":"response.created","response":{"id":"r","model":"gpt-6-sol","service_tier":"auto","usage":null}}`,
		`data: {"type":"response.output_text.delta","delta":"x"}`,
		"event: response.completed",
		`data: {"type":"response.completed","response":{"id":"r","model":"gpt-6-sol-2026","service_tier":"default","usage":{"input_tokens":9,"output_tokens":4,"total_tokens":13}}}`)
	oauth(&c, "codex", map[string]any{"type": "codex", "access_token": "fake-codex-token"}, map[string]string{})
	c.Endpoint, c.ServiceTier, c.Generate = "POST /v1/responses", "flex", false
	cases = append(cases, c)

	c = base("gemini buffered api key", "gemini", false, "gemini", "GeminiExecutor", "gemini-2.5-pro", "gemini-2.5-pro",
		`{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":60,"thoughtsTokenCount":10,"totalTokenCount":110,"cachedContentTokenCount":8},"modelVersion":"gemini-2.5-pro-002"}`)
	c.Attributes = map[string]string{"api_key": "fake-gemini-key", "auth_kind": "apikey"}
	c.Endpoint = "POST /v1beta/models/*action"
	cases = append(cases, c)

	c = base("gemini stream terminal usage", "gemini", true, "gemini", "GeminiExecutor", "gemini-2.5-flash", "gemini-2.5-flash",
		`data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4},"modelVersion":"gemini-2.5-flash"}`,
		`data: {"candidates":[{"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":9,"thoughtsTokenCount":2,"totalTokenCount":14},"modelVersion":"gemini-2.5-flash"}`)
	c.Attributes = map[string]string{"api_key": "fake-gemini-key", "auth_kind": "apikey"}
	c.Endpoint = "POST /v1beta/models/*action"
	cases = append(cases, c)

	c = base("vertex project source", "gemini", false, "vertex", "GeminiVertexExecutor", "gemini-2.5-pro", "gemini-2.5-pro",
		`{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2}}`)
	oauth(&c, "vertex", map[string]any{"type": "vertex", "project_id": "proj-1", "email": "sa@proj-1.iam"}, map[string]string{})
	cases = append(cases, c)

	c = base("interactions buffered", "interactions", false, "gemini-interactions", "GeminiExecutor", "gemini-3-pro", "gemini-3-pro",
		`{"id":"i1","model":"gemini-3-pro","status":"completed","usage":{"total_input_tokens":20,"total_output_tokens":30,"total_thought_tokens":5,"total_tokens":55,"total_cached_tokens":4}}`)
	c.Endpoint = "POST /v1beta/interactions"
	cases = append(cases, c)

	c = base("failure without usage", "claude", false, "claude", "ClaudeExecutor", "claude-sonnet-4-6", "sonnet", `{}`)
	c.Failed, c.FailStatus, c.FailBody = true, 429, "  {\"error\":{\"type\":\"rate_limit_error\",\"message\":\"quota <exceeded>\"}}  "
	cases = append(cases, c)

	c = base("failure after stream usage", "openai", true, "openai-compatible-acme", "OpenAICompatExecutor", "up-model", "acme-m",
		`data: {"id":"c4","object":"chat.completion.chunk","model":"up-model","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":2,"total_tokens":13}}`)
	c.Failed, c.FailStatus, c.FailBody = true, 502, "upstream reset"
	cases = append(cases, c)

	c = base("client key source fallback", "openai", false, "codex", "CodexExecutor", "gpt-5.5", "gpt-5.5",
		`{"object":"chat.completion","model":"gpt-5.5","usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`)
	oauth(&c, "codex", map[string]any{"type": "codex", "refresh_token": "fake-refresh"}, map[string]string{})
	c.ClientKey = " fake-client-key-2 "
	cases = append(cases, c)

	for i := range cases {
		cs := &cases[i]
		ginCtx, _ := gin.CreateTestContext(httptest.NewRecorder())
		if cs.ClientKey != "" {
			ginCtx.Set("userApiKey", cs.ClientKey)
		}
		ctx := context.WithValue(context.Background(), "gin", ginCtx)
		ctx = logging.WithRequestID(ctx, cs.RequestID)
		ctx = logging.WithEndpoint(ctx, cs.Endpoint)
		ctx = logging.WithClientRequestMetadata(ctx, logging.ClientRequestMetadata{
			ClientIP: cs.ClientIP, ResolvedClientIP: cs.ResolvedClientIP, XForwardedFor: cs.XForwardedFor,
			UserAgent: cs.UserAgent, SessionID: cs.SessionID, ParentSessionID: cs.ParentSessionID,
		})
		ctx = logging.WithResponseStatusHolder(ctx)
		ctx = logging.WithResponseHeadersHolder(ctx)
		if cs.ResponseHeaders != nil {
			logging.SetResponseHeaders(ctx, http.Header(cs.ResponseHeaders))
		}
		ctx = usage.WithRequestedModelAlias(ctx, cs.Alias)
		ctx = usage.WithReasoningEffort(ctx, cs.ReasoningEffort)
		ctx = usage.WithServiceTier(ctx, cs.ServiceTier)
		ctx = usage.WithGenerate(ctx, cs.Generate)
		ctx = usage.WithStream(ctx, cs.Stream)
		a := &auth.Auth{ID: cs.AuthID, Provider: cs.AuthProvider, Attributes: cs.Attributes, Metadata: cs.Metadata}
		if strings.HasSuffix(cs.AuthID, ".json") {
			// A file auth's index seed is its absolute path; pin it.
			a.FileName = "/auth/" + cs.AuthID
		}
		detail := usageDetail(cs.Format, cs.Stream, cs.Lines)
		requestedAt, err := time.Parse(time.RFC3339Nano, cs.RequestedAt)
		if err != nil {
			panic(err)
		}
		fail := usage.Failure{StatusCode: cs.FailStatus, Body: cs.FailBody}
		record := helps.GoldenUsageRecord(ctx, cs.ExecutorType, cs.Provider, cs.Model, a, detail, cs.Failed, fail,
			cs.ExecutionID, requestedAt, time.Duration(cs.LatencyMs)*time.Millisecond, time.Duration(cs.TTFTMs)*time.Millisecond,
			cs.Lines, cs.TranslatedPayload, cs.TranslatedFormat)
		cs.Queued = redisqueue.GoldenQueue(ctx, record)
		if cs.Queued == nil {
			panic("nothing queued for " + cs.Name)
		}
	}
	return cases
}

// alts runs sdk/api/handlers GetAlt on raw query strings.
func alts() []pair {
	gin.SetMode(gin.ReleaseMode)
	var out []pair
	for _, q := range []string{"", "alt=", "alt", "alt=sse", "alt=json", "$alt=json", "alt=&$alt=json",
		"$alt=sse", "alt=SSE", "alt=a%20b", "alt=%zz&$alt=json", "x=1;alt=json&$alt=raw", "alt=json&alt=sse"} {
		c, _ := gin.CreateTestContext(httptest.NewRecorder())
		c.Request = httptest.NewRequest(http.MethodPost, "/v1beta/models/m:generateContent?"+q, nil)
		out = append(out, pair{q, (&handlers.BaseAPIHandler{}).GetAlt(c)})
	}
	return out
}

type authKindCase struct {
	Attributes map[string]string `json:"attributes"`
	Metadata   map[string]any    `json:"metadata"`
	Kind       string            `json:"kind"`
}

// authKinds runs Auth.AuthKind over attribute and metadata shapes.
func authKinds() []authKindCase {
	cases := []authKindCase{
		{Attributes: map[string]string{"auth_kind": "apikey"}},
		{Attributes: map[string]string{"auth_kind": " API-Key "}},
		{Attributes: map[string]string{"auth_kind": "OAuth2", "api_key": "k"}},
		{Attributes: map[string]string{"auth_kind": "weird", "api_key": "k"}},
		{Attributes: map[string]string{"auth_kind": "weird"}, Metadata: map[string]any{"auth_kind": "api_key"}},
		{Attributes: map[string]string{"auth_kind": "weird"}, Metadata: map[string]any{"auth_kind": "unknown", "email": "a@b"}},
		{Metadata: map[string]any{"auth_kind": "oauth", "api_key": "k"}},
		{Attributes: map[string]string{"api_key": "   "}},
		{Attributes: map[string]string{"api_key": "   "}, Metadata: map[string]any{"refresh_token": "r"}},
		{Metadata: map[string]any{"api_key": "k"}},
		{Metadata: map[string]any{"token": map[string]any{"access_token": "x"}}},
		{Metadata: map[string]any{"token": map[string]any{}}},
		{Metadata: map[string]any{"expired": "2026-01-01T00:00:00Z"}},
		{Metadata: map[string]any{"email": "  "}},
		{Metadata: map[string]any{"auth_kind": 7, "access_token": "t"}},
		{},
	}
	for i := range cases {
		a := &auth.Auth{Attributes: cases[i].Attributes, Metadata: cases[i].Metadata}
		cases[i].Kind = a.AuthKind()
	}
	return cases
}

type goldenExecutor struct{ id string }

func (e goldenExecutor) Identifier() string { return e.id }

// Executor type names reach records through reflection, so each provider gets its
// own named type, as in Go's executors.
type (
	GeminiExecutor struct{ goldenExecutor }
	ClaudeExecutor struct{ goldenExecutor }
	KimiExecutor   struct{ goldenExecutor }
	DevinExecutor  struct{ goldenExecutor }
	CodexExecutor  struct{ goldenExecutor }
)

type goldenStatusError struct {
	code int
	msg  string
}

func (e goldenStatusError) Error() string   { return e.msg }
func (e goldenStatusError) StatusCode() int { return e.code }

type capturePlugin struct{ ch chan usage.Record }

func (p capturePlugin) HandleUsage(_ context.Context, r usage.Record) { p.ch <- r }

type reporterRecord struct {
	Failed          bool   `json:"failed"`
	FailStatus      int    `json:"fail_status"`
	FailBody        string `json:"fail_body"`
	Input           int64  `json:"input"`
	Output          int64  `json:"output"`
	Total           int64  `json:"total"`
	ResponseModel   string `json:"response_model"`
	ReasoningEffort string `json:"reasoning_effort"`
	Model           string `json:"model"`
}

type reporterCase struct {
	Name    string           `json:"name"`
	Records []reporterRecord `json:"records"`
}

// reporterSequences drives real UsageReporter call sequences, written as Go's
// executors make them, and captures what Go publishes. The usage manager delivers in
// order on one worker, so a sentinel record ends each case.
func reporterSequences() []reporterCase {
	ch := make(chan usage.Record, 64)
	usage.RegisterPlugin(capturePlugin{ch})
	const sentinel = "__sentinel__"
	drain := func() []reporterRecord {
		usage.PublishRecord(context.Background(), usage.Record{Model: sentinel})
		out := []reporterRecord{}
		for r := range ch {
			if r.Model == sentinel {
				return out
			}
			out = append(out, reporterRecord{
				Failed: r.Failed, FailStatus: r.Fail.StatusCode, FailBody: r.Fail.Body,
				Input: r.Detail.InputTokens, Output: r.Detail.OutputTokens, Total: r.Detail.TotalTokens,
				ResponseModel: r.ResponseModel, ReasoningEffort: r.ReasoningEffort, Model: r.Model,
			})
		}
		return out
	}
	drain() // anything published before this point
	credential := &auth.Auth{ID: "a.json", Provider: "x", Attributes: map[string]string{"auth_kind": "oauth"}}
	ctx := context.Background()
	var cases []reporterCase
	run := func(name string, f func()) {
		f()
		cases = append(cases, reporterCase{Name: name, Records: drain()})
	}
	gemini := GeminiExecutor{goldenExecutor{"gemini"}}
	claude := ClaudeExecutor{goldenExecutor{"claude"}}
	kimi := KimiExecutor{goldenExecutor{"kimi"}}
	devin := DevinExecutor{goldenExecutor{"devin"}}
	codex := CodexExecutor{goldenExecutor{"codex"}}

	geminiLines := []string{
		`data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1,"totalTokenCount":5}}`,
		`data: {"candidates":[{"content":{"parts":[{"text":"b"}]}}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":3,"totalTokenCount":7}}`,
	}
	// gemini_executor.go stream: the scan error publishes before the deferred buffer.
	// The lines stand for what the executor parses after FilterSSEUsageMetadata
	// (Vertex parses unfiltered chunks like these).
	run("gemini stream usage then scan error", func() {
		r := helps.NewExecutorUsageReporter(ctx, gemini, "gemini-2.5-pro", credential)
		defer r.EnsurePublished(ctx)
		var buf helps.StreamUsageBuffer
		defer buf.Publish(ctx, r)
		for _, line := range geminiLines {
			if d, ok := helps.ParseGeminiStreamUsage([]byte(line)); ok {
				buf.Observe(d, true)
			}
		}
		r.PublishFailure(ctx, errors.New("read: connection reset"))
	})
	run("gemini stream usage", func() {
		r := helps.NewExecutorUsageReporter(ctx, gemini, "gemini-2.5-pro", credential)
		defer r.EnsurePublished(ctx)
		var buf helps.StreamUsageBuffer
		defer buf.Publish(ctx, r)
		for _, line := range geminiLines {
			if d, ok := helps.ParseGeminiStreamUsage([]byte(line)); ok {
				buf.Observe(d, true)
			}
		}
	})
	claudeLines := []string{
		`data: {"type":"message_start","message":{"model":"claude-sonnet-4-6","usage":{"input_tokens":9,"output_tokens":1}}}`,
		`data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":6}}`,
	}
	// claude_executor_stream.go: failures go through the buffer and keep its usage.
	run("claude stream usage then scan error", func() {
		r := helps.NewExecutorUsageReporter(ctx, claude, "claude-sonnet-4-6", credential)
		var buf helps.StreamUsageBuffer
		defer buf.Publish(ctx, r)
		for _, line := range claudeLines {
			buf.ObserveClaudeStream([]byte(line))
		}
		buf.PublishFailure(ctx, r, goldenStatusError{502, "stream broke"})
	})
	kimiNoUsage := []string{`data: {"id":"c","object":"chat.completion.chunk","model":"kimi-k2","choices":[{"index":0,"delta":{"content":"x"}}]}`, "data: [DONE]"}
	kimiUsage := append([]string{}, kimiNoUsage[0], `data: {"id":"c","object":"chat.completion.chunk","model":"kimi-k2","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}}`, "data: [DONE]")
	// kimi_executor.go stream: no EnsurePublished.
	for _, c := range []struct {
		name  string
		lines []string
	}{{"kimi stream without usage", kimiNoUsage}, {"kimi stream with usage", kimiUsage}} {
		run(c.name, func() {
			r := helps.NewExecutorUsageReporter(ctx, kimi, "kimi-k2", credential)
			var buf helps.StreamUsageBuffer
			defer buf.Publish(ctx, r)
			for _, line := range c.lines {
				r.ObserveResponseModel([]byte(line))
				buf.ObserveOpenAIStream([]byte(line))
			}
		})
	}
	// kimi_executor.go native non-stream: publishes only with tokens.
	run("kimi native nonstream without usage", func() {
		r := helps.NewExecutorUsageReporter(ctx, kimi, "kimi-k2", credential)
		data := []byte(`{"id":"r","object":"response","model":"kimi-k2","output":[]}`)
		if u, ok := helps.ParseCodexUsage(data); ok && (u.TotalTokens > 0 || u.InputTokens > 0) {
			r.Publish(ctx, u)
		} else if u := helps.ParseOpenAIUsage(data); u.TotalTokens > 0 || u.InputTokens > 0 {
			r.Publish(ctx, u)
		}
	})
	// kimi_executor.go: SetTranslatedReasoningEffort(body, e.Identifier()).
	for _, payload := range []string{
		`{"model":"kimi-k2","reasoning_effort":"high"}`,
		`{"model":"kimi-k2","thinking":{"type":"enabled"}}`,
		`{"model":"kimi-k2","thinking":{"type":"disabled"}}`,
	} {
		run("kimi translated effort "+payload, func() {
			r := helps.NewExecutorUsageReporter(ctx, kimi, "kimi-k2", credential)
			r.SetTranslatedReasoningEffort([]byte(payload), kimi.Identifier())
			r.Publish(ctx, helps.ParseOpenAIUsage([]byte(`{"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}`)))
		})
	}
	// devin_executor.go stream outcomes.
	run("devin set response model", func() {
		r := helps.NewExecutorUsageReporter(ctx, devin, "devin-chat", credential)
		defer r.EnsurePublished(ctx)
		r.SetResponseModel("  devin-model-x ")
	})
	run("devin truncated before EOS", func() {
		r := helps.NewExecutorUsageReporter(ctx, devin, "devin-chat", credential)
		defer r.EnsurePublished(ctx)
	})
	run("devin trailer error", func() {
		r := helps.NewExecutorUsageReporter(ctx, devin, "devin-chat", credential)
		defer r.EnsurePublished(ctx)
		r.PublishFailure(ctx, errors.New("devin trailer: permission_denied"))
	})
	run("publish then failure", func() {
		r := helps.NewExecutorUsageReporter(ctx, gemini, "gemini-2.5-pro", credential)
		r.Publish(ctx, helps.ParseGeminiUsage([]byte(`{"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":2,"totalTokenCount":4}}`)))
		r.PublishFailure(ctx, goldenStatusError{500, "late"})
	})
	run("failure then publish", func() {
		r := helps.NewExecutorUsageReporter(ctx, gemini, "gemini-2.5-pro", credential)
		r.PublishFailure(ctx, goldenStatusError{429, "slow down"})
		r.Publish(ctx, helps.ParseGeminiUsage([]byte(`{"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":2,"totalTokenCount":4}}`)))
	})
	run("terminal response model then set", func() {
		r := helps.NewExecutorUsageReporter(ctx, codex, "gpt-5.5", credential)
		r.ObserveResponseModel([]byte(`data: {"type":"response.completed","response":{"model":"gpt-5.5-2026-01-01"}}`))
		r.SetResponseModel("other-model")
		r.EnsurePublished(ctx)
	})
	return cases
}

type substitutionCase struct {
	Requested   string `json:"requested"`
	Served      string `json:"served"`
	Substituted bool   `json:"substituted"`
}

func substitutions() []substitutionCase {
	pairs := [][2]string{
		{"gpt-5.5", "gpt-5.5"}, {"gpt-5.5", "GPT-5.5"}, {"gpt-5.5", "gpt-5.4"},
		{"gpt-5.5", "gpt-5.5-2026-01-01"}, {"gpt-5.5-2026-01-01", "gpt-5.5"}, {"gpt-5.5", "gpt-5.5-20260101"},
		{"gpt-5.5", "gpt-5.5-001"}, {"gpt-5.5", "gpt-5.5-0001"}, {"gpt-5.5", "gpt-5.5-2026-1-01"},
		{"claude-sonnet-4-6", "anthropic/claude-sonnet-4-6"}, {"openai/gpt-5.5", "gpt-5.5-latest"},
		{"gpt-5.5-latest", "gpt-5.5-2026-01-01"}, {"gpt-5.5(high)", "gpt-5.5"}, {" gpt-5.5 ", "gpt-5.5"},
		{"", "gpt-5.5"}, {"gpt-5.5", ""}, {"kimi-k2", "kimi-k2.5"}, {"a/b/", "b"}, {"gemini-2.5-pro", "models/gemini-2.5-pro"},
	}
	out := make([]substitutionCase, 0, len(pairs))
	for _, p := range pairs {
		out = append(out, substitutionCase{p[0], p[1], helps.IsModelSubstituted(p[0], p[1])})
	}
	return out
}

type headerFilterCase struct {
	In  map[string][]string `json:"in"`
	Out map[string][]string `json:"out"`
}

// upstreamHeaderFilters runs handlers.FilterUpstreamHeaders, what passthrough-headers
// forwards from an upstream response.
func upstreamHeaderFilters() []headerFilterCase {
	inputs := []http.Header{
		{
			"Content-Type":                       {"application/json"},
			"Content-Length":                     {"42"},
			"Content-Encoding":                   {"gzip"},
			"Set-Cookie":                         {"s=1"},
			"X-Request-Id":                       {"req-1"},
			"Connection":                         {"close, X-Custom-Hop"},
			"X-Custom-Hop":                       {"h"},
			"Keep-Alive":                         {"timeout=5"},
			"X-Litellm-Model":                    {"m"},
			"Helicone-Id":                        {"h1"},
			"Cf-Aig-Cache-Status":                {"HIT"},
			"Access-Control-Allow-Origin":        {"*"},
			"X-Cpa-Trace-Id":                     {"t"},
			"Anthropic-Ratelimit-Requests-Limit": {"50"},
			"Retry-After":                        {"30"},
			"X-Multi":                            {"a", "b"},
		},
		{"Transfer-Encoding": {"chunked"}, "Te": {"trailers"}},
	}
	out := make([]headerFilterCase, 0, len(inputs))
	for _, in := range inputs {
		filtered := handlers.FilterUpstreamHeaders(in)
		if filtered == nil {
			filtered = http.Header{}
		}
		out = append(out, headerFilterCase{In: in, Out: filtered})
	}
	return out
}

type errorEventCase struct {
	Name   string           `json:"name"`
	Steps  []step           `json:"steps"`
	Events []map[string]any `json:"events"`
}

// errorEvents captures the events Go's MarkResult publishes on the errors channel
// (sdk/cliproxy/auth/error_events.go) for the cooldown inputs. Clock values become
// "<time>" or whole seconds from now ("+Ns").
func errorEvents() []errorEventCase {
	redisqueue.SetEnabled(true)
	defer redisqueue.SetEnabled(false)
	events, unsubscribe := redisqueue.SubscribeErrors()
	defer unsubscribe()
	ctx := context.Background()
	m := auth.NewManager(nil, nil, nil)
	var normalize func(v any) any
	normalize = func(v any) any {
		switch x := v.(type) {
		case map[string]any:
			for k, val := range x {
				switch k {
				case "timestamp":
					x[k] = "<time>"
				case "next_retry_after", "next_recover_at":
					t, err := time.Parse(time.RFC3339Nano, val.(string))
					if err != nil {
						panic(err)
					}
					x[k] = fmt.Sprintf("+%ds", int64(time.Until(t).Round(time.Second)/time.Second))
				default:
					x[k] = normalize(val)
				}
			}
			return x
		default:
			return v
		}
	}
	out := make([]errorEventCase, len(cooldownInputs))
	for i, c := range cooldownInputs {
		id := fmt.Sprintf("events-%d.json", i)
		// Synthesized credentials start active (watcher/synthesizer file.go, config.go).
		if _, err := m.Register(ctx, &auth.Auth{ID: id, Index: fmt.Sprintf("idx-%d", i), Provider: "claude", Status: auth.StatusActive, Metadata: map[string]any{"type": "claude"}}); err != nil {
			panic(err)
		}
		cs := errorEventCase{Name: c.Name, Steps: c.Steps, Events: []map[string]any{}}
		for _, st := range c.Steps {
			result := auth.Result{
				AuthID:          id,
				Provider:        "claude",
				Model:           st.Model,
				CredentialScope: st.CredentialScope,
				Error:           &auth.Error{HTTPStatus: st.Status, Message: st.Message},
			}
			if st.RetryAfterMs >= 0 {
				d := time.Duration(st.RetryAfterMs) * time.Millisecond
				result.RetryAfter = &d
			}
			m.MarkResult(ctx, result)
			select {
			case payload := <-events:
				var event map[string]any
				if err := json.Unmarshal(payload, &event); err != nil {
					panic(err)
				}
				cs.Events = append(cs.Events, normalize(event).(map[string]any))
			case <-time.After(time.Second):
			}
		}
		out[i] = cs
	}
	return out
}

type keepAliveCase struct {
	Name        string `json:"name"`
	DelayMillis int    `json:"delay_ms"`
	Status      int    `json:"status"`
	ContentType string `json:"content_type"`
	Body        string `json:"body"`
}

// nonStreamKeepAlives runs a non-stream handler's tail (code_handlers.go
// handleNonStreamingResponse) with nonstream-keepalive-interval 1: Content-Type,
// StartNonStreamingKeepAlive, a result after the delay, then the body or
// WriteErrorResponse.
func nonStreamKeepAlives() []keepAliveCase {
	gin.SetMode(gin.ReleaseMode)
	h := handlers.NewBaseAPIHandlers(&config.SDKConfig{NonStreamKeepAliveInterval: 1}, nil)
	upstreamErr := `{"type":"error","error":{"type":"invalid_request_error","message":"bad input"}}`
	okBody := `{"id":"msg_1","type":"message"}`
	cases := []keepAliveCase{
		{Name: "fast_ok"},
		{Name: "slow_ok", DelayMillis: 1500},
		{Name: "fast_error"},
		{Name: "slow_error", DelayMillis: 1500},
	}
	for i := range cases {
		rec := httptest.NewRecorder()
		c, _ := gin.CreateTestContext(rec)
		c.Request = httptest.NewRequest(http.MethodPost, "/v1/messages", nil)
		c.Header("Content-Type", "application/json")
		stop := h.StartNonStreamingKeepAlive(c, context.Background())
		time.Sleep(time.Duration(cases[i].DelayMillis) * time.Millisecond)
		stop()
		if strings.HasSuffix(cases[i].Name, "_error") {
			h.WriteErrorResponse(c, &interfaces.ErrorMessage{StatusCode: http.StatusBadRequest, Error: errors.New(upstreamErr)})
		} else {
			_, _ = c.Writer.Write([]byte(okBody))
		}
		cases[i].Status = rec.Code
		cases[i].ContentType = rec.Header().Get("Content-Type")
		cases[i].Body = rec.Body.String()
	}
	return cases
}

func main() {
	out := map[string]any{}
	out["error_events"] = errorEvents()
	out["upstream_headers"] = upstreamHeaderFilters()
	out["reporter"] = reporterSequences()
	out["substitution"] = substitutions()
	out["nonstream_keepalive"] = nonStreamKeepAlives()
	out["alt"] = alts()
	out["auth_kind"] = authKinds()
	out["by_provider"] = byProvider()
	out["resolved_config"] = resolvedConfig
	out["resolved"] = resolvedModels()
	out["usage"] = usageRecords()
	var sanitized, extracted []pair
	for _, in := range sanitizeInputs {
		sanitized = append(sanitized, pair{in, auth.SanitizeUpstreamErrorSummary(in)})
	}
	for _, in := range extractInputs {
		extracted = append(extracted, pair{in, auth.ExtractUpstreamErrorSummary(in)})
	}
	out["sanitize"] = sanitized
	out["extract"] = extracted
	out["availability"] = modelAvailability()
	out["cooldown"] = cooldowns()
	out["session"] = sessions()
	out["affinity"] = affinities()
	out["cooldown_files"] = cooldownFiles()
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(os.Args[1], append(data, '\n'), 0o644); err != nil {
		panic(err)
	}
}
