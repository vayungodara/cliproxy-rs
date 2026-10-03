package main

import "strings"

const baseConfig = `
requests:
  payload:
    default:
      - models:
          - name: gemini-2.5-flash-lite
            protocol: gemini
        params:
          generationConfig.temperature: 0.25
          generationConfig.topP: 0.5
    override:
      - models:
          - name: gemini-2.5-flash-lite
            from-protocol: gemini
        params:
          generationConfig.topK: 3
      - models:
          - name: gemini-3.1-pro-preview
            protocol: interactions
        params:
          generation_config.seed: 7
    filter:
      - models:
          - name: gemini-2.5-flash-lite
        params:
          - generationConfig.stopSequences
api-keys:
  gemini:
    - name: g1
      base-url: http://UPSTREAM/
      headers:
        X-Static: static-value
        X-Forward: $X-Client-Trace
        X-Missing: $X-Not-Sent
      models:
        - name: gemini-2.5-flash
          alias: flash
        - name: gemini-2.5-pro
          alias: pro-levels
          thinking:
            levels: [low, high]
        - name: gemini-2.5-flash
          alias: flash-compat
          is-compat: true
      keys:
        - api-key: AIza-fake-gemini-1
    - name: g2
      base-url: http://UPSTREAM///
      keys:
        - api-key: AIza-fake-gemini-2
    - name: g3
      base-url: http://UPSTREAM
      prefix: team
      headers:
        X-Session: "sess-$CPA-SESSION-ID"
      models:
        - name: gemini-2.5-pro
          alias: pro-team
          thinking:
            levels: [low]
        - alias: gemini-2.5-flash
      keys:
        - api-key: AIza-fake-gemini-3
  interactions:
    - name: i1
      base-url: http://UPSTREAM
      headers:
        X-Static: static-int
      models:
        - name: gemini-3-pro-preview
          alias: native-pro
      keys:
        - api-key: AIza-fake-int-1
    - name: i2
      base-url: http://UPSTREAM/
      headers:
        Api-Revision: 2099-01-01
      keys:
        - api-key: AIza-fake-int-2
`

// safety is the default list ConvertGeminiRequestToGemini attaches; bodies that carry it
// (and a matching model) are left unchanged by Go's gemini->gemini normalizer.
const safety = `[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]`

const userHi = `[{"role":"user","parts":[{"text":"hi"}]}]`

// gem builds a Gemini body the gemini->gemini normalizer leaves unchanged.
func gem(model, contents, extra string) string {
	return `{"model":"` + model + `","contents":` + contents + `,"safetySettings":` + safety + extra + `}`
}

var jsonOK = &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json; charset=UTF-8"}, {"X-Upstream", "1"}}, Body: `{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`}

const sseChunk1 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`
const sseChunk2 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`

func sse(body string) *upstream {
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body}
}

var geminiSSE = sse("data: " + sseChunk1 + "\r\n\r\n: keepalive\n\nevent: ignored\ndata:" + sseChunk2 + "\n\ndata: [DONE]\n\n")

const countReply = `{"totalTokens":5,"promptTokensDetails":[{"modality":"TEXT","tokenCount":5}]}`

func g(name, op, model, requested, payload string, up *upstream) scenario {
	return scenario{Name: name, Config: baseConfig, Provider: "gemini", ConfigAuth: 0, Model: model, RequestedModel: requested, Payload: payload, Source: "gemini", Op: op, Upstream: up}
}

func with(s scenario, edit func(*scenario)) scenario {
	edit(&s)
	return s
}

const interactionsOK = `{"id":"int_1","model":"gemini-3-pro-preview","status":"completed","outputs":[{"type":"text","text":"ok"}],"usage":{"total_input_tokens":3,"total_output_tokens":1,"total_tokens":4}}`

var interactionsSSE = sse(strings.Join([]string{
	"event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",\"model\":\"gemini-3-pro-preview\"}}\n\n",
	": ping\n\n",
	"event: step.delta\r\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hi\"}}\r\n\r\n",
	"data: {\"event_type\":\"step.delta\",\n",
	"data: \"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"!\"}}\n   \n",
	"{\"event_type\":\"bare\"}\n\n",
	"data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":2}}}\n\n",
	"event: done\ndata: [DONE]\n\n",
	"event: trailing\ndata: {\"event_type\":\"after\"}",
}, ""))

func i(name, op, model, payload string, up *upstream) scenario {
	return scenario{Name: name, Config: baseConfig, Provider: "gemini-interactions", ConfigAuth: 0, Model: model, Payload: payload, Source: "interactions", Op: op, Upstream: up}
}

const interactionsInput = `{"model":"gemini-3-pro-preview","input":[{"type":"user_input","id":"s1","content":[{"type":"text","text":"hi","id":"p1"},{"type":"text","text":"there"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":{}},{"type":"function_call","id":"c2","call_id":"c2x","name":"g","arguments":{}},{"type":"function_result","id":"r1","call_id":"c1","result":"x","content":"not-array"},{"type":"model_output","id":7}]}`

// usageConfig has one credential per upstream so the usage_* scenarios also replay
// through the Rust router (cpa-server tests/gemini_routes.rs), which compares the queued
// usage record with the one Go's reporter published.
const usageConfig = `
api-keys:
  gemini:
    - base-url: http://UPSTREAM
      models:
        - name: gemini-2.5-flash
          alias: u-flash
      keys:
        - api-key: AIza-fake-usage
  interactions:
    - base-url: http://UPSTREAM
      models:
        - name: gemini-3-pro-preview
          alias: u-pro
      keys:
        - api-key: AIza-fake-usage-int
`

func u(name, provider, source, op, model, requested, payload string, up *upstream) scenario {
	return scenario{Name: name, Config: usageConfig, Provider: provider, ConfigAuth: 0, Model: model, RequestedModel: requested, Payload: payload, Source: source, Op: op, Upstream: up}
}

func usageScenarios() []scenario {
	json := func(body string) *upstream {
		return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
	}
	const nonTerminal = `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":7,"totalTokenCount":12},"modelVersion":"gemini-2.5-flash-003"}`
	const terminalBare = `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"modelVersion":"gemini-2.5-flash-003"}`
	const terminalUsage = `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":9,"thoughtsTokenCount":2,"cachedContentTokenCount":3,"totalTokenCount":16},"modelVersion":"gemini-2.5-flash-003"}`
	interactionsStream := sse(strings.Join([]string{
		"event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_9\",\"model\":\"gemini-3-pro-preview-0901\"}}\n\n",
		"event: step.delta\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hi\"}}\n\n",
		"data: {\"event_type\":\"interaction.completed\",\n",
		"data: \"interaction\":{\"id\":\"int_9\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":11,\"total_output_tokens\":22,\"total_thought_tokens\":4,\"total_cached_tokens\":2,\"total_tokens\":39}}}\n\n",
		"event: done\ndata: [DONE]\n\n",
	}, ""))
	return []scenario{
		u("usage_openai_execute_effort", "gemini", "openai", "execute", "gemini-2.5-flash", "u-flash",
			`{"model":"u-flash","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"low"}`,
			json(`{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":40,"candidatesTokenCount":60,"thoughtsTokenCount":10,"cachedContentTokenCount":8,"totalTokenCount":110},"modelVersion":"gemini-2.5-flash-002"}`)),
		// FilterSSEUsageMetadata hides usage on non-terminal chunks from the reporter.
		u("usage_claude_stream_non_terminal_usage", "gemini", "claude", "stream", "gemini-2.5-flash", "u-flash",
			`{"model":"u-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`,
			sse("data: "+nonTerminal+"\n\ndata: "+terminalBare+"\n\n")),
		u("usage_claude_stream_terminal_usage", "gemini", "claude", "stream", "gemini-2.5-flash", "u-flash",
			`{"model":"u-flash","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":2048},"messages":[{"role":"user","content":"hi"}],"stream":true}`,
			sse("data: "+nonTerminal+"\n\ndata: "+terminalUsage+"\n\n")),
		u("usage_openai_stream_interactions", "gemini-interactions", "openai", "stream", "gemini-3-pro-preview", "u-pro",
			`{"model":"u-pro","stream":true,"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}`,
			interactionsStream),
		u("usage_responses_execute_interactions", "gemini-interactions", "openai-response", "execute", "gemini-3-pro-preview", "u-pro",
			`{"model":"u-pro","input":"hi"}`,
			json(`{"id":"int_8","model":"gemini-3-pro-preview-0902","status":"completed","outputs":[{"type":"text","text":"ok"}],"usage":{"total_input_tokens":20,"total_output_tokens":30,"total_thought_tokens":5,"total_cached_tokens":4,"total_tokens":55}}`)),
	}
}

func scenarios() []scenario {
	geminiErr := &upstream{Status: 429, Headers: [][2]string{{"Content-Type", "application/json"}, {"Retry-After", "7"}}, Body: `{"error":{"code":429,"message":"Resource has been exhausted","status":"RESOURCE_EXHAUSTED","details":[{"@type":"type.googleapis.com/google.rpc.RetryInfo","retryDelay":"7s"}]}}`}
	out := []scenario{
		// generateContent: URL, key, custom headers ($Header references), client
		// credentials not forwarded, model written into the body, session_id dropped.
		with(g("gen_basic_headers", "execute", "gemini-2.5-flash", "flash", gem("gemini-2.5-flash", userHi, `,"session_id":"sess-1"`), jsonOK), func(s *scenario) {
			s.Headers = map[string]string{"X-Client-Trace": "trace-1", "Authorization": "Bearer client-secret", "X-Goog-Api-Key": "client-key"}
		}),
		with(g("gen_alt_json", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), jsonOK), func(s *scenario) { s.Alt = "json" }),
		g("gen_model_written_when_missing_needs_normalizer", "execute", "gemini-2.5-flash", "", `{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}`, jsonOK),
		g("gen_boundary_user_turns", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"earlier"}]},{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"a"}]}]`, ""), jsonOK),
		g("gen_trailing_function_response_kept", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"functionResponse":{"name":"f","response":{"x":1}}}]}]`, ""), jsonOK),
		g("gen_cap_max_output_tokens", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, `,"generationConfig":{"maxOutputTokens":100000,"temperature":0.5}`), jsonOK),
		g("gen_cap_uses_max_completion_tokens", "execute", "gemini-3.7-flash", "", gem("gemini-3.7-flash", userHi, `,"generationConfig":{"maxOutputTokens":70000}`), jsonOK),
		g("gen_cap_keeps_small_and_string", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, `,"generationConfig":{"maxOutputTokens":"100000"}`), jsonOK),
		g("gen_cap_unknown_model", "execute", "gemini-unknown-x", "", gem("gemini-unknown-x", userHi, `,"generationConfig":{"maxOutputTokens":100000}`), jsonOK),
		g("gen_image_aspect_ratio_white_image", "execute", "gemini-2.5-flash-image-preview", "", gem("gemini-2.5-flash-image-preview", `[{"role":"user","parts":[{"text":"draw"},{"text":"cat"}]}]`, `,"generationConfig":{"imageConfig":{"aspectRatio":"16:9"},"temperature":1}`), jsonOK),
		g("gen_image_aspect_ratio_unknown", "execute", "gemini-2.5-flash-image-preview", "", gem("gemini-2.5-flash-image-preview", userHi, `,"generationConfig":{"imageConfig":{"aspectRatio":"7:3"}}`), jsonOK),
		g("gen_image_aspect_ratio_with_inline_data", "execute", "gemini-2.5-flash-image-preview", "", gem("gemini-2.5-flash-image-preview", `[{"role":"user","parts":[{"text":"edit"},{"inlineData":{"mimeType":"image/png","data":"AAAA"}}]}]`, `,"generationConfig":{"imageConfig":{"aspectRatio":"1:1","imageSize":"1K"}}`), jsonOK),
		g("gen_thinking_budget_suffix", "execute", "gemini-2.5-flash(8192)", "", gem("gemini-2.5-flash", userHi, ""), jsonOK),
		g("gen_thinking_none_suffix", "execute", "gemini-2.5-flash(none)", "", gem("gemini-2.5-flash", userHi, `,"generationConfig":{"thinkingConfig":{"thinkingBudget":512,"includeThoughts":true}}`), jsonOK),
		g("gen_thinking_level_suffix", "execute", "gemini-3-pro-preview(high)", "", gem("gemini-3-pro-preview", userHi, ""), jsonOK),
		g("gen_thinking_level_to_budget", "execute", "gemini-2.5-pro(high)", "", gem("gemini-2.5-pro", userHi, ""), jsonOK),
		g("gen_thinking_body_config", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, `,"generationConfig":{"thinkingConfig":{"thinkingBudget":99999999}}`), jsonOK),
		g("gen_thinking_invalid_suffix", "execute", "gemini-2.5-flash(warp)", "", gem("gemini-2.5-flash", userHi, ""), jsonOK),
		g("gen_thinking_resolved_config_levels", "execute", "gemini-2.5-pro(medium)", "pro-levels(medium)", gem("gemini-2.5-pro", userHi, ""), jsonOK),
		g("gen_thinking_resolved_config_level_ok", "execute", "gemini-2.5-pro(high)", "pro-levels(high)", gem("gemini-2.5-pro", userHi, ""), jsonOK),
		g("gen_upstream_429", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), geminiErr),
		g("gen_upstream_500_gzip_body", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 500, Headers: [][2]string{{"Content-Encoding", "gzip"}}, Body: `{"error":{"code":500,"message":"internal"}}`, Gzip: true}),
		g("gen_gzip_success", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}, {"Content-Encoding", "gzip"}}, Body: jsonOK.Body, Gzip: true}),
		g("gen_empty_body_is_502", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 200, Body: ""}),
		with(g("gen_compact_alt_rejected", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), nil), func(s *scenario) { s.Alt = "responses/compact" }),
		with(g("gen_base_url_trailing_slashes", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), jsonOK), func(s *scenario) { s.ConfigAuth = 1 }),
		{Name: "gen_attributes_without_key", Provider: "gemini", ConfigAuth: -1, Attributes: map[string]string{"base_url": " http://UPSTREAM/v-custom/ ", "header:X-Attr": "attr-value"}, Model: "gemini-2.5-flash", Payload: gem("gemini-2.5-flash", userHi, ""), Source: "gemini", Op: "execute", Upstream: jsonOK},
		g("gen_original_differs", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), jsonOK),
		// Payload rules: default (only missing, judged on the translated original),
		// override and filter for the upstream model and protocol.
		g("gen_payload_rules", "execute", "gemini-2.5-flash-lite", "", gem("gemini-2.5-flash-lite", userHi, `,"generationConfig":{"topP":0.9,"stopSequences":["x"],"topK":40}`), jsonOK),
		g("stream_payload_rules", "stream", "gemini-2.5-flash-lite", "", gem("gemini-2.5-flash-lite", userHi, ""), geminiSSE),
		g("count_payload_rules_not_applied", "count", "gemini-2.5-flash-lite", "", gem("gemini-2.5-flash-lite", userHi, `,"generationConfig":{"stopSequences":["x"]}`), &upstream{Status: 200, Body: countReply}),
		// Prefixed credential: the route model loses the prefix before capability lookup;
		// configured levels reject what the static model would accept; an alias-only model
		// keeps the static capabilities; $CPA-SESSION-ID expands to the explicit session.
		with(g("gen_prefixed_configured_levels", "execute", "gemini-2.5-pro(medium)", "team/pro-team(medium)", gem("gemini-2.5-pro", userHi, ""), jsonOK), func(s *scenario) { s.ConfigAuth = 2 }),
		with(g("gen_prefixed_alias_only_model", "execute", "gemini-2.5-flash(1024)", "team/gemini-2.5-flash(1024)", gem("gemini-2.5-flash", userHi, ""), jsonOK), func(s *scenario) {
			s.ConfigAuth = 2
			s.Headers = map[string]string{"X-Session-Id": "client-session-1"}
		}),
		// Without an explicit session $CPA-SESSION-ID is the derived canonical session.
		with(g("gen_session_header_derived", "execute", "gemini-2.5-flash", "team/gemini-2.5-flash", gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"derive me"}]}]`, ""), jsonOK), func(s *scenario) { s.ConfigAuth = 2 }),

		// streamGenerateContent: alt=sse, usage stripped from non-terminal chunks,
		// comments, event lines and [DONE] skipped.
		g("stream_basic", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, `,"session_id":"x"`), geminiSSE),
		with(g("stream_alt_json", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), sse("[{\n  \"candidates\": []\n}\n,\n"+sseChunk2+"\n]")), func(s *scenario) { s.Alt = "json" }),
		g("stream_boundary_user_turns", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"earlier"}]},{"role":"model","parts":[{"text":"a"}]}]`, ""), geminiSSE),
		g("stream_usage_trace_id", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), sse("data: {\"traceId\":\"gem-t1\",\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\ndata: {\"traceId\":\"gem-t1\",\"usageMetadata\":{\"totalTokenCount\":9}}\n\ndata: {\"traceId\":\"gem-t2\",\"usageMetadata\":{\"totalTokenCount\":1},\"response\":{\"usageMetadata\":{\"x\":1}}}\n\ndata: not-json\n\n")),
		g("stream_unterminated_last_line", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), sse("data: "+sseChunk1+"\n\ndata: "+sseChunk2)),
		g("stream_upstream_400", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 400, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT"}}`}),
		g("stream_thinking_error_before_upstream", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, `,"generationConfig":{"thinkingConfig":{"thinkingBudget":-7}}`), geminiSSE),
		g("stream_unknown_suffix_ignored", "stream", "gemini-2.5-flash(warp)", "", gem("gemini-2.5-flash", userHi, ""), geminiSSE),
		with(g("stream_compact_alt_rejected", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), nil), func(s *scenario) { s.Alt = "responses/compact" }),

		// countTokens: tools, generationConfig and safetySettings removed, leading user
		// turn only, no alt.
		with(g("count_basic", "count", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"a"}]},{"role":"user","parts":[{"text":"b"}]},{"role":"model","parts":[{"text":"c"}]}]`, `,"tools":[{"function_declarations":[]}],"generationConfig":{"maxOutputTokens":9},"session_id":"s"`), &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: countReply}), func(s *scenario) { s.Alt = "json" }),
		g("count_thinking_suffix_then_stripped", "count", "gemini-2.5-flash(1024)", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 200, Body: countReply}),
		g("count_needs_token_count_shape", "count", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 200, Body: `{"totalTokens":5,"cachedContentTokenCount":1}`}),
		g("count_upstream_403", "count", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, ""), &upstream{Status: 403, Body: `{"error":{"code":403,"message":"denied"}}`}),

		// Cross-format clients on the Gemini upstream (need their translator pairs).
		with(g("openai_chat_to_gemini", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}],"max_tokens":100000,"reasoning_effort":"high"}`, jsonOK), func(s *scenario) { s.Source = "openai" }),
		with(g("openai_chat_stream_to_gemini", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, geminiSSE), func(s *scenario) { s.Source = "openai" }),
		with(g("claude_stream_to_gemini", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","max_tokens":1024,"system":"sys prompt","messages":[{"role":"user","content":[{"type":"text","text":"hello there"}]}],"stream":true}`, geminiSSE), func(s *scenario) { s.Source = "claude" }),
		with(g("claude_to_gemini", "execute", "gemini-2.5-flash", "flash-compat", `{"model":"flash-compat","max_tokens":1024,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":""},{"type":"text","text":"a"}]},{"role":"user","content":"again"}]}`, jsonOK), func(s *scenario) { s.Source = "claude" }),
		with(g("claude_count_to_gemini", "count", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, &upstream{Status: 200, Body: countReply}), func(s *scenario) { s.Source = "claude" }),
		with(g("responses_stream_to_gemini", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi","stream":true}`, geminiSSE), func(s *scenario) { s.Source = "openai-response" }),
		with(g("interactions_client_on_gemini_upstream", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi","stream":true}`, geminiSSE), func(s *scenario) { s.Source = "interactions" }),
		g("gemini_natural_body_stream", "stream", "gemini-2.5-flash", "", `{"contents":[{"parts":[{"text":"hi","thoughtSignature":"bogus"}]}],"tools":[{"functionDeclarations":[{"name":"f","parameters":{"type":"object"}}]}]}`, geminiSSE),

		// Native Interactions: POST /v1beta/interactions, Api-Revision, input ID repair.
		with(i("int_basic", "execute", "gemini-3-pro-preview", interactionsInput, &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: interactionsOK}), func(s *scenario) {
			s.Headers = map[string]string{"Authorization": "Bearer client-secret"}
		}),
		with(i("int_client_revision", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, &upstream{Status: 200, Body: interactionsOK}), func(s *scenario) {
			s.Headers = map[string]string{"Api-Revision": "2030-02-02"}
		}),
		with(i("int_config_header_revision_wins", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, &upstream{Status: 200, Body: interactionsOK}), func(s *scenario) {
			s.ConfigAuth = 1
			s.Headers = map[string]string{"Api-Revision": "2030-02-02"}
		}),
		i("int_model_rewritten_to_base", "execute", "gemini-3-pro-preview(low)", `{"model":"native-pro","input":"hi"}`, &upstream{Status: 200, Body: interactionsOK}),
		i("int_agent_without_model", "execute", "deep-research", `{"agent":"deep-research","input":"hi"}`, &upstream{Status: 200, Body: interactionsOK}),
		i("int_thinking_level", "execute", "gemini-3-pro-preview(high)", `{"model":"gemini-3-pro-preview","input":"hi","generation_config":{"temperature":1}}`, &upstream{Status: 200, Body: interactionsOK}),
		i("int_thinking_invalid", "execute", "gemini-3-pro-preview(warp)", `{"model":"gemini-3-pro-preview","input":"hi"}`, &upstream{Status: 200, Body: interactionsOK}),
		i("int_error_status", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, &upstream{Status: 400, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"code":400,"message":"Invalid revision","status":"INVALID_ARGUMENT"}}`}),
		i("int_stream_frames", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi","stream":"yes"}`, interactionsSSE),
		i("int_stream_ids", "stream", "gemini-3-pro-preview", interactionsInput, interactionsSSE),
		i("int_stream_error", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, &upstream{Status: 503, Headers: [][2]string{{"Content-Encoding", "gzip"}}, Body: `{"error":{"code":503,"message":"overloaded"}}`, Gzip: true}),
		with(i("int_compact_alt_rejected", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, nil), func(s *scenario) { s.Alt = "responses/compact" }),
		i("int_payload_rules", "execute", "gemini-3.1-pro-preview", `{"model":"gemini-3.1-pro-preview","input":"hi","generation_config":{"seed":1}}`, &upstream{Status: 200, Body: interactionsOK}),
		// countTokens always targets generateContent, even on an Interactions key.
		i("int_count_uses_count_tokens", "count", "gemini-3-pro-preview", gem("gemini-3-pro-preview", userHi, ""), &upstream{Status: 200, Body: countReply}),
		with(i("int_stream_alt_ignored", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, interactionsSSE), func(s *scenario) { s.Alt = "json" }),

		// Native Interactions for translated clients (need their translator pairs).
		with(i("int_gemini_client_stream", "stream", "gemini-3-pro-preview", gem("gemini-3-pro-preview", userHi, ""), interactionsSSE), func(s *scenario) { s.Source = "gemini" }),
		with(i("int_openai_client", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","messages":[{"role":"user","content":"hi"}]}`, &upstream{Status: 200, Body: interactionsOK}), func(s *scenario) { s.Source = "openai" }),
		with(i("int_claude_client_stream", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","max_tokens":64,"messages":[{"role":"user","content":"hello there"}],"stream":true}`, interactionsSSE), func(s *scenario) { s.Source = "claude" }),
		with(i("int_responses_client_stream", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi","stream":true}`, interactionsSSE), func(s *scenario) { s.Source = "openai-response" }),
		// A non-native source on an Interactions credential takes generateContent.
		with(i("int_codex_source_uses_generate_content", "execute", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, jsonOK), func(s *scenario) { s.Source = "codex" }),

		// A line ending in "\r\r\n" keeps one "\r" through Go's scanner: Interactions
		// clients get it back inside the frame, Codex clients (no pair) in the chunk.
		i("int_stream_cr_lines", "stream", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`,
			sse("event: step.delta\r\r\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hi\"}}\r\r\n\r\r\nevent: done\r\r\ndata: [DONE]\r\r\n\r\n")),
		with(g("stream_codex_client_cr_lines", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi"}`, sse("data: "+sseChunk1+"\r\r\n\r\r\ndata: "+sseChunk2+"\r\n\r\n")), func(s *scenario) { s.Source = "codex" }),
	}
	out = append(out, usageScenarios()...)
	for i := range out {
		if out[i].Name == "gen_original_differs" {
			out[i].Original = gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"original"}]}]`, "")
		}
	}
	return out
}
