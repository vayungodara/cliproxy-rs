package main

const okBody = `{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4},"modelVersion":"gemini-2.5-flash-002","responseId":"r1"}`

const chunk1 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3},"modelVersion":"gemini-2.5-flash-002","responseId":"r1"}`
const chunk2 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-2.5-flash-002","responseId":"r1"}`

const userHi = `{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}`

func httpResponse(status int, body string) reply {
	return reply{Type: "http_response", Payload: map[string]any{
		"status":  status,
		"headers": map[string]any{"content-type": []any{"application/json"}, "X-Upstream": "1", "x-skip": 5},
		"body":    body,
	}}
}

func streamStart(status int) reply {
	return reply{Type: "stream_start", Payload: map[string]any{"status": status, "headers": map[string]any{"Content-Type": []string{"text/event-stream"}}}}
}

func chunk(data string) reply {
	return reply{Type: "stream_chunk", Payload: map[string]any{"data": data}}
}

var streamEnd = reply{Type: "stream_end"}

// sseOK is a streamed answer as the browser forwards it: SSE text, one event per chunk.
var sseOK = []reply{streamStart(200), chunk("data: " + chunk1 + "\r\n\r\n"), chunk("data: " + chunk2 + "\r\n\r\n"), streamEnd}

// bareOK carries bare JSON chunks, which reach translators as whole values.
var bareOK = []reply{streamStart(200), chunk(chunk1), chunk(chunk2), streamEnd}

func s(name, op, source, model, payload string, replies ...reply) scenario {
	return scenario{Name: name, Op: op, Source: source, Model: model, Payload: payload, Replies: replies}
}

func with(sc scenario, edit func(*scenario)) scenario {
	edit(&sc)
	return sc
}

func scenarios() []scenario {
	return []scenario{
		// Non-stream: the relayed request, the translated answer re-encoded by
		// ensureColonSpacedJSON, and the browser's headers.
		s("execute_gemini", "execute", "gemini", "gemini-2.5-flash", userHi, httpResponse(200, okBody)),
		s("execute_openai", "execute", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi <b>&"}],"reasoning_effort":"low"}`, httpResponse(200, okBody)),
		s("execute_claude", "execute", "claude", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`, httpResponse(200, okBody)),
		s("execute_responses", "execute", "openai-response", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","input":"hi"}`, httpResponse(200, okBody)),
		s("execute_codex_no_pair", "execute", "codex", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","input":"hi"}`, httpResponse(200, okBody)),
		// Generation limits, response MIME type and schema are dropped; levels go upper case.
		s("execute_drops_generation_fields", "execute", "gemini", "gemini-3-pro-preview(low)", `{"contents":[{"role":"model","parts":[{"text":"a"}]}],"generationConfig":{"maxOutputTokens":10,"responseMimeType":"application/json","responseJsonSchema":{"type":"object"},"temperature":0.5},"session_id":"s1"}`, httpResponse(200, okBody)),
		with(s("execute_alt_json", "execute", "gemini", "gemini-2.5-flash", userHi, httpResponse(200, "["+okBody+"]")), func(sc *scenario) { sc.Alt = "json" }),
		with(s("execute_custom_headers", "execute", "gemini", "gemini-2.5-flash", userHi, httpResponse(200, okBody)), func(sc *scenario) {
			sc.Attributes = map[string]string{"header:X-Static": "v1", "header:Host": "example.invalid", "header:x-forward": "$X-Client"}
			sc.Headers = map[string]string{"X-Client": "c1"}
		}),
		with(s("execute_payload_rules", "execute", "gemini", "gemini-2.5-flash", userHi, httpResponse(200, okBody)), func(sc *scenario) {
			sc.Config = "requests:\n  payload:\n    override:\n      - models:\n          - name: gemini-2.5-flash\n            protocol: gemini\n        params:\n          generationConfig.topK: 7\n"
		}),
		s("execute_upstream_429", "execute", "gemini", "gemini-2.5-flash", userHi, httpResponse(429, `{"error":{"code":429,"message":"quota"}}`)),
		s("execute_streamed_answer", "execute", "gemini", "gemini-2.5-flash", userHi, streamStart(200), chunk(okBody[:20]), chunk(okBody[20:]), streamEnd),
		s("execute_relay_error", "execute", "gemini", "gemini-2.5-flash", userHi, reply{Type: "error", Payload: map[string]any{"error": "fetch failed", "status": 503}}),
		s("execute_relay_error_bare", "execute", "gemini", "gemini-2.5-flash", userHi, reply{Type: "error"}),
		s("execute_missing_payload", "execute", "gemini", "gemini-2.5-flash", userHi, reply{Type: "http_response"}),
		with(s("execute_disconnected", "execute", "gemini", "gemini-2.5-flash", userHi), func(sc *scenario) { sc.Disconnected = true }),
		with(s("execute_compact_rejected", "execute", "openai-response", "gemini-2.5-flash", `{"input":"hi"}`), func(sc *scenario) { sc.Alt = "responses/compact" }),

		// Streams: every chunk goes to the translator whole, after usage filtering.
		s("stream_gemini_sse", "stream", "gemini", "gemini-2.5-flash", userHi, sseOK...),
		s("stream_gemini_bare", "stream", "gemini", "gemini-2.5-flash", userHi, bareOK...),
		s("stream_openai_bare", "stream", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, bareOK...),
		s("stream_openai_sse", "stream", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, sseOK...),
		s("stream_claude_bare", "stream", "claude", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}`, bareOK...),
		s("stream_responses_bare", "stream", "openai-response", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"input":"hi"}`, bareOK...),
		// Go hands a relay chunk to the translator as one value, events and all.
		s("stream_openai_two_events_one_chunk", "stream", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, streamStart(200), chunk("data: "+chunk1+"\n\ndata: "+chunk2+"\n\n"), streamEnd),
		s("stream_gemini_two_events_one_chunk", "stream", "gemini", "gemini-2.5-flash", userHi, streamStart(200), chunk("data: "+chunk1+"\n\ndata: "+chunk2+"\n\n"), streamEnd),
		s("stream_codex_no_pair", "stream", "codex", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"input":"hi"}`, bareOK...),
		s("stream_first_status_error", "stream", "gemini", "gemini-2.5-flash", userHi, streamStart(400), chunk(`{"error":`), chunk(`"bad"}`), streamEnd),
		s("stream_first_status_then_relay_error", "stream", "gemini", "gemini-2.5-flash", userHi, streamStart(500), reply{Type: "error", Payload: map[string]any{"error": "gone"}}),
		s("stream_error_midway", "stream", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, streamStart(200), chunk(chunk1), reply{Type: "error", Payload: map[string]any{"error": "socket reset", "status": 0}}),
		s("stream_http_response", "stream", "openai", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, httpResponse(200, chunk2)),
		s("stream_http_response_error", "stream", "gemini", "gemini-2.5-flash", userHi, httpResponse(403, `{"error":"denied"}`)),

		// countTokens: no custom headers, no alt, generation config and tools removed.
		with(s("count_basic", "count", "gemini", "gemini-2.5-flash", `{"contents":[{"role":"model","parts":[{"text":"a"}]}],"tools":[{"functionDeclarations":[]}],"generationConfig":{"temperature":1}}`, httpResponse(200, `{"totalTokens":7}`)), func(sc *scenario) {
			sc.Attributes = map[string]string{"header:X-Static": "v1"}
			sc.Alt = "json"
		}),
		s("count_claude", "count", "claude", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, httpResponse(200, `{"totalTokens":7}`)),
		s("count_missing_total", "count", "gemini", "gemini-2.5-flash", userHi, httpResponse(200, `{"totalTokens":0}`)),
	}
}
