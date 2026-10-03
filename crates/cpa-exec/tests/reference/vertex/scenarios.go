package main

import "strings"

const keyConfig = `
api-keys:
  vertex:
    - name: v1
      base-url: http://UPSTREAM/api
      headers:
        X-Static: static-vertex
        X-Session: "s-$CPA-SESSION-ID"
      models:
        - name: gemini-2.5-pro
          alias: vertex-pro
          thinking:
            levels: [low, high]
        - name: gemini-2.5-flash
          alias: vertex-flash
      keys:
        - api-key: vk-fake-1
    - name: v2
      base-url: http://UPSTREAM/v2/
      keys:
        - api-key: vk-fake-2
    - name: v3
      proxy-url: http://PROXY
      models:
        - name: gemini-2.5-flash
          alias: flash-default
      keys:
        - api-key: vk-fake-3
`

const safety = `[{"category":"HARM_CATEGORY_HARASSMENT","threshold":"OFF"},{"category":"HARM_CATEGORY_HATE_SPEECH","threshold":"OFF"},{"category":"HARM_CATEGORY_SEXUALLY_EXPLICIT","threshold":"OFF"},{"category":"HARM_CATEGORY_DANGEROUS_CONTENT","threshold":"OFF"},{"category":"HARM_CATEGORY_CIVIC_INTEGRITY","threshold":"BLOCK_NONE"}]`

const userHi = `[{"role":"user","parts":[{"text":"hi"}]}]`

func gem(model, contents, extra string) string {
	return `{"model":"` + model + `","contents":` + contents + `,"safetySettings":` + safety + extra + `}`
}

var token = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json; charset=utf-8"}}, Body: `{"access_token":"ya29.fake-access","expires_in":3599,"token_type":"Bearer"}`}

var jsonOK = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json; charset=UTF-8"}}, Body: `{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":1,"totalTokenCount":4},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`}

const chunk1 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":3,"totalTokenCount":3},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`
const chunk2 = `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-2.5-flash","createTime":"2026-10-01T12:00:00.123456Z","responseId":"r1"}`

// Vertex hands every scanned line to the translator, prefixes and blank lines included.
var sseOK = reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\r\n\r\n: keepalive\n\ndata:" + chunk2 + "\n\ndata: [DONE]\n\n"}

var countOK = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"totalTokens":5,"promptTokensDetails":[{"modality":"TEXT","tokenCount":5}]}`}

var imagenOK = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"predictions":[{"bytesBase64Encoded":"iVBORw0KGgo=","mimeType":"image/jpeg"},{"bytesBase64Encoded":"AAAA"},{"mimeType":"image/png"}]}`}

func saMeta(location string) map[string]any {
	m := map[string]any{
		"type":       "vertex",
		"project_id": "proj-1",
		"email":      "svc@proj-1.iam.gserviceaccount.com",
		"proxy_url":  "http://PROXY",
		"service_account": map[string]any{
			"type":           "service_account",
			"project_id":     "proj-1",
			"private_key_id": "kid-1",
			"private_key":    "SA_KEY",
			"client_email":   "svc@proj-1.iam.gserviceaccount.com",
			"token_uri":      "https://oauth2.googleapis.com/token",
		},
	}
	if location != "" {
		m["location"] = location
	}
	return m
}

func sa(name, op, location, model, payload string, replies ...reply) scenario {
	return scenario{Name: name, ConfigAuth: -1, Metadata: saMeta(location), Key: "pkcs1", Model: model, Payload: payload, Source: "gemini", Op: op, Replies: replies}
}

func key(name, op, model, requested, payload string, replies ...reply) scenario {
	return scenario{Name: name, Config: keyConfig, ConfigAuth: 0, Model: model, RequestedModel: requested, Payload: payload, Source: "gemini", Op: op, Via: "plain", Replies: replies}
}

// usageKeyConfig has a single Vertex key so the usage_* scenarios also replay through
// the Rust router (cpa-server tests/gemini_routes.rs), which compares the queued usage
// record with the one Go's reporter published.
const usageKeyConfig = `
api-keys:
  vertex:
    - base-url: http://UPSTREAM/api
      models:
        - name: gemini-2.5-flash
          alias: u-vflash
      keys:
        - api-key: vk-fake-usage
`

func usageKey(name, source, op, payload string, replies ...reply) scenario {
	return scenario{Name: name, Config: usageKeyConfig, ConfigAuth: 0, Model: "gemini-2.5-flash", RequestedModel: "u-vflash", Payload: payload, Source: source, Op: op, Via: "plain", Replies: replies}
}

func with(s scenario, edit func(*scenario)) scenario {
	edit(&s)
	return s
}

func scenarios() []scenario {
	hi := gem("gemini-2.5-flash", userHi, `,"session_id":"sess-1"`)
	return []scenario{
		// Service account: JWT bearer exchange at the key's token_uri, then the regional
		// endpoint with the access token.
		sa("sa_generate_default_location", "execute", "", "gemini-2.5-flash", hi, token, jsonOK),
		with(sa("sa_generate_pkcs8_key", "execute", "europe-west4", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) { s.Key = "pkcs8" }),
		with(sa("sa_generate_pasted_key", "execute", "us-east5", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) { s.Key = "pkcs8_crlf_ansi" }),
		with(sa("sa_generate_one_line_key", "execute", "us-east5", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) { s.Key = "pkcs8_one_line" }),
		with(sa("sa_stream_global", "stream", "global", "gemini-2.5-flash", hi, token, sseOK), func(s *scenario) {
			s.Headers = map[string]string{"Authorization": "Bearer client-secret"}
		}),
		with(sa("sa_stream_alt_json", "stream", "us-central1", "gemini-2.5-flash", hi, token, reply{Status: 200, Body: "[" + chunk2 + "]"}), func(s *scenario) { s.Alt = "json" }),
		sa("sa_count", "count", "europe-west4", "gemini-2.5-flash", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"a"}]}]`, `,"tools":[],"generationConfig":{"maxOutputTokens":99999}`), token, countOK),
		sa("sa_imagen_predict", "execute", "", "imagen-3.0-generate-002", `{"contents":[{"parts":[{"text":"a red fox"}]}],"aspectRatio":"16:9","sampleCount":2,"negativePrompt":"blur"}`, token, imagenOK),
		sa("sa_imagen_messages_prompt", "execute", "", "imagen-4.0-ultra", `{"messages":[{"role":"system","content":""},{"role":"user","content":"a cat"}]}`, token, imagenOK),
		sa("sa_imagen_no_prompt", "execute", "", "imagen-4.0-ultra", `{"messages":[]}`),
		sa("sa_imagen_stream", "stream", "", "imagen-3.0-generate-002", hi, token, reply{Status: 200, Body: `{"predictions":[]}`}),
		sa("sa_token_rejected", "execute", "", "gemini-2.5-flash", hi, reply{Status: 400, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":"invalid_grant","error_description":"Invalid JWT Signature."}`}),
		sa("sa_token_without_access_token", "execute", "", "gemini-2.5-flash", hi, reply{Status: 200, Body: `{"token_type":"Bearer","expires_in":10}`}, jsonOK),
		sa("sa_token_bad_expires", "execute", "", "gemini-2.5-flash", hi, reply{Status: 200, Body: `{"access_token":"ya29.x","expires_in":"soon"}`}),
		sa("sa_upstream_429", "execute", "", "gemini-2.5-flash", hi, token, reply{Status: 429, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"code":429,"message":"Quota exceeded","status":"RESOURCE_EXHAUSTED"}}`}),
		sa("sa_stream_upstream_403", "stream", "", "gemini-2.5-flash", hi, token, reply{Status: 403, Body: `{"error":{"code":403,"message":"denied"}}`}),
		with(sa("sa_missing_project", "execute", "", "gemini-2.5-flash", hi), func(s *scenario) {
			delete(s.Metadata, "project_id")
			s.Metadata["project"] = "  "
		}),
		with(sa("sa_project_fallback", "execute", "", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) {
			delete(s.Metadata, "project_id")
			s.Metadata["project"] = " proj-legacy "
		}),
		with(sa("sa_missing_service_account", "execute", "", "gemini-2.5-flash", hi), func(s *scenario) { delete(s.Metadata, "service_account") }),
		with(sa("sa_ec_key", "execute", "", "gemini-2.5-flash", hi), func(s *scenario) { s.Key = "ec" }),
		with(sa("sa_wrong_credential_type", "execute", "", "gemini-2.5-flash", hi), func(s *scenario) {
			s.Metadata["service_account"].(map[string]any)["type"] = "authorized_user"
		}),
		with(sa("sa_default_token_uri", "execute", "", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) {
			delete(s.Metadata["service_account"].(map[string]any), "token_uri")
		}),
		with(sa("sa_responses_tool_ids_stripped", "execute", "", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"},{"type":"function_call_output","call_id":"call_1","output":"done"}],"tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}`, token, jsonOK), func(s *scenario) { s.Source = "openai-response" }),
		with(sa("sa_compact_alt_rejected", "execute", "", "gemini-2.5-flash", hi), func(s *scenario) { s.Alt = "responses/compact" }),
		// A file credential with an access token takes the API-key path.
		with(sa("file_access_token_as_api_key", "execute", "", "gemini-2.5-flash", hi, jsonOK), func(s *scenario) {
			s.Metadata["access_token"] = "ya29.from-file"
			delete(s.Metadata, "service_account")
		}),

		// API keys: simple key auth at base-url + /v1/publishers/google/models.
		with(key("key_generate_alias_headers", "execute", "gemini-2.5-flash", "vertex-flash", hi, jsonOK), func(s *scenario) {
			s.Headers = map[string]string{"X-Session-Id": "client-sess", "X-Goog-Api-Key": "client-key"}
		}),
		with(key("key_stream_trailing_slash_base", "stream", "gemini-2.5-flash", "", hi, sseOK), func(s *scenario) { s.ConfigAuth = 1 }),
		key("key_count", "count", "gemini-2.5-flash", "", hi, countOK),
		with(key("key_default_base_via_proxy", "execute", "gemini-2.5-flash", "flash-default", hi, jsonOK), func(s *scenario) {
			s.ConfigAuth = 2
			s.Via = "proxy"
		}),
		key("key_imagen_untranslated_response", "execute", "imagen-3.0-generate-002", "", hi, imagenOK),
		key("key_imagen_stream", "stream", "imagen-3.0-generate-002", "", hi, reply{Status: 200, Body: `{"predictions":[]}`}),
		key("key_thinking_configured_levels", "execute", "gemini-2.5-pro(medium)", "vertex-pro(medium)", gem("gemini-2.5-pro", userHi, ""), jsonOK),
		key("key_thinking_budget_suffix", "execute", "gemini-2.5-flash(2048)", "", hi, jsonOK),
		key("key_stream_upstream_500", "stream", "gemini-2.5-flash", "", hi, reply{Status: 500, Body: `{"error":{"code":500,"message":"boom"}}`}),
		key("key_empty_body_is_502", "execute", "gemini-2.5-flash", "", hi, reply{Status: 200, Body: ""}),
		key("key_boundary_user_turns", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"a"}]},{"role":"model","parts":[{"text":"b"}]}]`, ""), sseOK),
		// Raw lines reach every translator; Go's Claude, OpenAI and Responses translators
		// see the `data:` prefixes, comments and blank lines.
		with(key("key_claude_stream", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hello there"}],"stream":true}`, sseOK), func(s *scenario) { s.Source = "claude" }),
		with(key("key_openai_stream", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, sseOK), func(s *scenario) { s.Source = "openai" }),
		with(key("key_openai_execute", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"max_tokens":100000}`, jsonOK), func(s *scenario) { s.Source = "openai" }),
		with(key("key_responses_stream", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi","stream":true}`, sseOK), func(s *scenario) { s.Source = "openai-response" }),
		with(key("key_claude_count", "count", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, countOK), func(s *scenario) { s.Source = "claude" }),
		with(key("key_codex_client_passthrough", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi"}`, jsonOK), func(s *scenario) { s.Source = "codex" }),
		// A line ending in "\r\r\n" keeps one "\r" through Go's scanner.
		with(key("key_codex_stream_cr_lines", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi"}`, reply{Status: 200, Body: "data: " + chunk1 + "\r\r\n\r\r\ndata: " + chunk2 + "\r\n\r\n"}), func(s *scenario) { s.Source = "codex" }),
		with(key("key_openai_stream_cr_lines", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, reply{Status: 200, Body: "data: " + chunk1 + "\r\r\n\r\r\ndata: " + chunk2 + "\r\n\r\n"}), func(s *scenario) { s.Source = "openai" }),
		// Vertex reports every raw line to the usage reporter: usage on a non-terminal
		// chunk counts (the Gemini executor filters it out).
		usageKey("usage_key_openai_stream_raw_usage", "openai", "stream", `{"model":"u-vflash","stream":true,"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"medium"}`,
			reply{Status: 200, Body: "data: " + `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1,"totalTokenCount":8},"modelVersion":"gemini-2.5-flash-004"}` + "\n\ndata: " + `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"modelVersion":"gemini-2.5-flash-004"}` + "\n\n"}),
		usageKey("usage_key_claude_execute_thinking", "claude", "execute", `{"model":"u-vflash","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":2048},"messages":[{"role":"user","content":"hi"}]}`,
			reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":12,"thoughtsTokenCount":6,"cachedContentTokenCount":9,"totalTokenCount":48},"modelVersion":"gemini-2.5-flash-005"}`}),
		with(key("key_codex_tool_integer_types", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[{"type":"function","name":"shell","parameters":{"type":"object","properties":{"timeout_ms":{"type":"number"}}}}]}`, jsonOK), func(s *scenario) {
			s.Source = "openai-response"
			s.Headers = map[string]string{"User-Agent": "codex_cli_rs/0.50.0 (Mac OS 15.0)"}
		}),
	}
}

var _ = strings.TrimSpace
