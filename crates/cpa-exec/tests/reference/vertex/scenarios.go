package main

import (
	"encoding/base64"
	"strings"
)

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
        - name: gemini-2.5-flash-lite
          alias: vertex-lite
          display-name: Vertex Lite
          thinking:
            min: 512
            max: 4096
            zero-allowed: false
            dynamic-allowed: false
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

// keyPrefixConfig: base URLs that already name the models path (2d7f5377).
const keyPrefixConfig = `
api-keys:
  vertex:
    - base-url: http://UPSTREAM/v1/projects/p-1/locations/us-central1/publishers/google/models/
      keys:
        - api-key: vk-fake-prefix
    - base-url: http://UPSTREAM/v1beta1/
      keys:
        - api-key: vk-fake-beta
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

// usageOAConfig names a partner model without an alias (d9451cb6).
const usageOAConfig = `
api-keys:
  vertex:
    - base-url: http://UPSTREAM/api
      models:
        - name: meta/llama-4
      keys:
        - api-key: vk-fake-usage
`

// usageOAPublisherConfig names a partner model by its publisher resource behind an
// alias: the route normalizes it to meta/llama-3.3-70b-instruct-maas.
const usageOAPublisherConfig = `
api-keys:
  vertex:
    - base-url: http://UPSTREAM/api
      models:
        - name: publishers/meta/models/llama-3.3-70b-instruct-maas
          alias: u-llama
      keys:
        - api-key: vk-fake-usage
`

// escapedHostConfig: a Google host spelled with a percent escape passes the
// Google-host check as a relay base.
const escapedHostConfig = `
api-keys:
  vertex:
    - base-url: https://%61iplatform.googleapis.com
      models:
        - name: meta/llama-4
      keys:
        - api-key: vk-fake-escaped
`

// emptyHostConfig: a base URL Go parses as opaque, with no host.
const emptyHostConfig = `
api-keys:
  vertex:
    - base-url: https:relay.example
      models:
        - name: meta/llama-4
        - name: gemini-2.5-flash
      keys:
        - api-key: vk-fake-empty-host
`

// interactionsKeyConfig opts its first key into native Interactions (2d5351b3); the
// second key does not.
const interactionsKeyConfig = `
api-keys:
  vertex:
    - base-url: http://UPSTREAM/api/
      headers:
        X-Static: static-vertex
      models:
        - name: gemini-3-pro-preview
          alias: vertex-int
      keys:
        - api-key: vk-fake-int
          interactions: true
        - api-key: vk-fake-plain
`

const interactionsOK = `{"id":"int_1","model":"gemini-3-pro-preview","status":"completed","outputs":[{"type":"text","text":"ok"}],"usage":{"total_input_tokens":3,"total_output_tokens":1,"total_tokens":4}}`

var interactionsJSON = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: interactionsOK}

var interactionsSSE = reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: strings.Join([]string{
	"event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_1\",\"model\":\"gemini-3-pro-preview\"}}\n\n",
	": ping\n\n",
	"event: step.delta\r\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"hi\"}}\r\n\r\n",
	"{\"event_type\":\"bare\"}\n\n",
	"data: {\"event_type\":\"interaction.completed\",\"interaction\":{\"id\":\"int_1\",\"status\":\"completed\",\"usage\":{\"total_input_tokens\":3,\"total_output_tokens\":2}}}\n\n",
	"event: done\ndata: [DONE]\n\n",
}, "")}

const interactionsInput = `{"model":"vertex-int","input":[{"type":"user_input","id":"s1","content":[{"type":"text","text":"hi","id":"p1"}]},{"type":"function_call","id":"c2","call_id":"c2x","name":"g","arguments":{}}],"generation_config":{"temperature":1}}`

func interactionsKey(name, op, model, payload string, replies ...reply) scenario {
	return scenario{Name: name, Config: interactionsKeyConfig, ConfigAuth: 0, Model: model, RequestedModel: "vertex-int", Payload: payload, Source: "interactions", Op: op, Via: "plain", Replies: replies}
}

func usageKey(name, source, op, payload string, replies ...reply) scenario {
	return scenario{Name: name, Config: usageKeyConfig, ConfigAuth: 0, Model: "gemini-2.5-flash", RequestedModel: "u-vflash", Payload: payload, Source: source, Op: op, Via: "plain", Replies: replies}
}

func with(s scenario, edit func(*scenario)) scenario {
	edit(&s)
	return s
}

// The apply_patch request and answer of Go's executor tests.
const patchRequest = `{"tools":[{"type":"namespace","name":"functions","tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","definition":"start: patch"}}]}],"input":"patch a file"}`
const patchResponse = `{"responseId":"patch","candidates":[{"content":{"parts":[{"functionCall":{"name":"functions__apply_patch","args":{"input":"  *** Begin Patch\n*** End Patch\n "}}}]},"finishReason":"STOP"}]}`

// b64 is unpadded base64url, as JWT segments are.
func b64(s string) string { return base64.RawURLEncoding.EncodeToString([]byte(s)) }

// tokenCases are token-endpoint answers decoded the way jwtSource.Token decodes them:
// struct fields with Go's types, case-insensitive names, and an id_token claim set.
func tokenCases(hi string) []scenario {
	answer := func(name, body string) scenario {
		return sa("token_"+name, "execute", "", "gemini-2.5-flash", hi, reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}, jsonOK)
	}
	idToken := func(name, claims string) scenario {
		return answer("id_"+name, `{"access_token":"ya29.id","id_token":"`+claims+`"}`)
	}
	return []scenario{
		answer("null", `null`),
		answer("fold_later_wins", `{"access_token":"ya29.a","ACCESS_TOKEN":"ya29.b"}`),
		answer("exact_then_fold", `{"ACCESS_TOKEN":"ya29.b","access_token":"ya29.a"}`),
		answer("escaped", `{"access_token":"ya29.\u0041x"}`),
		answer("expires_in_float", `{"access_token":"ya29.x","expires_in":3599.0}`),
		answer("expires_in_exponent", `{"access_token":"ya29.x","expires_in":1e3}`),
		answer("expires_in_overflow", `{"access_token":"ya29.x","expires_in":99999999999999999999}`),
		answer("expires_in_string", `{"access_token":"ya29.x","expires_in":"3599"}`),
		answer("expires_in_negative", `{"access_token":"ya29.x","expires_in":-5}`),
		answer("expiry_offset", `{"access_token":"ya29.x","expiry":"2026-10-01T12:00:00.5+02:00"}`),
		answer("expiry_lowercase", `{"access_token":"ya29.x","expiry":"2026-10-01t12:00:00z"}`),
		answer("expiry_bad_day", `{"access_token":"ya29.x","expiry":"2026-02-29T12:00:00Z"}`),
		answer("expiry_number", `{"access_token":"ya29.x","expiry":5}`),
		answer("nulls", `{"access_token":"ya29.x","expiry":null,"token_type":null,"expires_in":null}`),
		answer("access_token_number", `{"access_token":5}`),
		answer("array", `["ya29.x"]`),
		answer("trailing", `{"access_token":"ya29.x"} x`),
		answer("unknown_member", `{"access_token":"ya29.x","other":{"deep":[1,{"a":null}]}}`),
		idToken("valid", "h."+b64(`{"exp":1700000000,"iss":"x"}`)+".s"),
		idToken("null_claims", "h."+b64(`null`)+".s"),
		idToken("string_exp", "h."+b64(`{"exp":"1"}`)+".s"),
		idToken("trailing_after_claims", "h."+b64(`{"exp":1}garbage`)+".s"),
		idToken("number_claims", "h."+b64(`5`)+".s"),
		idToken("two_segments", "h."+b64(`{"exp":1}`)),
		idToken("four_segments", "h."+b64(`{"exp":1}`)+".s.t"),
		idToken("padded", "h."+b64(`{"exp":12}`)+"==.s"),
		idToken("empty_claims", "h..s"),
		idToken("trailing_bits", "h.eyJleHAiOjEyfR.s"),
	}
}

// keyFileCases edit the service account the executor marshals into
// google.CredentialsFromJSON.
func keyFileCases(hi string) []scenario {
	edit := func(name string, f func(sa map[string]any)) scenario {
		return with(sa("keyfile_"+name, "execute", "", "gemini-2.5-flash", hi, token, jsonOK), func(s *scenario) {
			f(s.Metadata["service_account"].(map[string]any))
		})
	}
	return []scenario{
		edit("client_id_number", func(sa map[string]any) { sa["client_id"] = 5 }),
		edit("project_id_number", func(sa map[string]any) { sa["project_id"] = 5 }),
		edit("universe_domain_bool", func(sa map[string]any) { sa["universe_domain"] = true }),
		edit("delegates_with_null", func(sa map[string]any) { sa["delegates"] = []any{"a", nil} }),
		edit("delegates_string", func(sa map[string]any) { sa["delegates"] = "a" }),
		edit("credential_source_string", func(sa map[string]any) { sa["credential_source"] = "x" }),
		edit("credential_source_object", func(sa map[string]any) { sa["credential_source"] = map[string]any{"file": "f"} }),
		edit("impersonation_null", func(sa map[string]any) { sa["service_account_impersonation"] = nil }),
		edit("type_folded", func(sa map[string]any) {
			delete(sa, "type")
			sa["TYPE"] = "service_account"
		}),
		edit("type_duplicate_sorted_last_wins", func(sa map[string]any) { sa["typE"] = "bogus" }),
		edit("token_uri_folded", func(sa map[string]any) {
			delete(sa, "token_uri")
			sa["Token_URI"] = "https://oauth2.googleapis.com/token?folded=1"
		}),
		edit("audience_folded", func(sa map[string]any) { sa["Audience"] = "https://aud.example/" }),
		edit("type_missing", func(sa map[string]any) { delete(sa, "type") }),
	}
}

func scenarios() []scenario {
	hi := gem("gemini-2.5-flash", userHi, `,"session_id":"sess-1"`)
	return append(append(scenarios1(hi), tokenCases(hi)...), keyFileCases(hi)...)
}

func scenarios1(hi string) []scenario {
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
		// Go builds the model request (url.Parse) before the token exchange: a URL it
		// rejects fails without a token request.
		sa("sa_location_bad_host", "execute", "us central1", "gemini-2.5-flash", hi, token, jsonOK),
		sa("sa_count_location_bad_host", "count", "us central1", "gemini-2.5-flash", hi, token, countOK),
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
		// The client goes away while the executor waits on the upstream: a context
		// canceled failure, unless a terminal chunk was delivered (dbc4aad9).
		with(key("key_stream_client_drop_mid_read", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\n", Missing: 40, Stall: true}), func(s *scenario) { s.Source = "openai"; s.CancelWhenIdle = true }),
		with(key("key_stream_client_drop_after_terminal", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\ndata: " + chunk2 + "\n\n", Missing: 40, Stall: true}), func(s *scenario) { s.Source = "openai"; s.CancelWhenIdle = true }),
		// v8.0.35 (5ec31442): a read error ends the stream without the synthetic [DONE].
		with(key("key_responses_stream_read_error", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi","stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\n", Missing: 40}), func(s *scenario) { s.Source = "openai-response" }),
		with(key("key_claude_count", "count", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, countOK), func(s *scenario) { s.Source = "claude" }),
		with(key("key_codex_client_passthrough", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi"}`, jsonOK), func(s *scenario) { s.Source = "codex" }),
		// gemini_vertex_executor_test.go TestGeminiVertexApplyPatchExecutorReuse: the
		// apply_patch custom tool round trip for a Responses client, both paths.
		with(key("key_apply_patch_execute", "execute", "gemini-3.1-pro-preview", "", patchRequest, reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: patchResponse}), func(s *scenario) { s.Source = "openai-response" }),
		with(key("key_apply_patch_stream", "stream", "gemini-3.1-pro-preview", "", patchRequest, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + patchResponse + "\n\n"}), func(s *scenario) { s.Source = "openai-response" }),
		// v8.0.35 (2d7f5377): a base URL holding /publishers/ is the models prefix; one
		// ending in a version segment gets no /v1.
		with(key("key_publishers_prefix_base", "stream", "gemini-2.5-flash", "", hi, sseOK), func(s *scenario) { s.Config = keyPrefixConfig }),
		with(key("key_publishers_prefix_count", "count", "gemini-2.5-flash", "", hi, countOK), func(s *scenario) { s.Config = keyPrefixConfig }),
		with(key("key_version_suffix_base", "execute", "gemini-2.5-flash", "", hi, jsonOK), func(s *scenario) {
			s.Config = keyPrefixConfig
			s.ConfigAuth = 1
		}),
		// v8.0.35 (ceb4397f): trailing thought-only model turns are dropped, keeping one
		// turn; Execute drops after the leading user turn, ExecuteStream before it.
		key("key_execute_single_thought_turn", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"t","thought":true}]}]`, ""), jsonOK),
		key("key_stream_single_thought_turn", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"model","parts":[{"text":"t","thought":true}]}]`, ""), sseOK),
		key("key_stream_trailing_thought_turns", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"a"}]},{"role":"model","parts":[{"text":"t1","thought":true},{"text":"t2","thought":"true"}]},{"role":"assistant","parts":[{"text":"t3","thought":1}]}]`, ""), sseOK),
		key("key_stream_trailing_thought_dropped", "stream", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"text":"a"}]},{"role":"model","parts":[{"text":"t","thought":true}]}]`, ""), sseOK),
		key("key_execute_thought_function_call_kept", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", `[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"thought":true,"functionCall":{"name":"f","args":{}}}]},{"role":"model","parts":[{"text":"t","thought":true}]}]`, ""), jsonOK),
		// v8.0.35 (7f2f17d7): MALFORMED_FUNCTION_CALL for a Claude client is a 502, or the
		// end of the stream after the api_error event.
		with(key("key_claude_malformed_execute", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"candidates":[{"content":{"role":"model","parts":[]},"finishReason":"MALFORMED_FUNCTION_CALL","index":0}],"usageMetadata":{"promptTokenCount":4,"totalTokenCount":4},"modelVersion":"gemini-2.5-flash"}`}), func(s *scenario) { s.Source = "claude" }),
		with(key("key_claude_malformed_stream", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\ndata: " + `{"candidates":[{"content":{"role":"model","parts":[{"text":"x"}]},"finishReason":"MALFORMED_FUNCTION_CALL","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2,"totalTokenCount":5},"modelVersion":"gemini-2.5-flash"}` + "\n\n"}), func(s *scenario) { s.Source = "claude" }),
		// v8.0.35 (4f2e5270): a Claude client's stream that ends without a finish reason
		// or block reason fails with 502 (API keys and service accounts alike).
		with(key("key_claude_stream_block_reason", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\ndata: " + `{"promptFeedback":{"blockReason":"OTHER"}}` + "\n\n"}), func(s *scenario) { s.Source = "claude" }),
		with(key("key_openai_stream_eof_without_finish", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}],"stream":true}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\n"}), func(s *scenario) { s.Source = "openai" }),
		with(sa("sa_claude_stream_eof_without_finish", "stream", "", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`, token, reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\n"}), func(s *scenario) { s.Source = "claude" }),
		// Every thinking field of a configured Vertex model: range, zero and dynamic.
		key("key_thinking_above_max", "execute", "gemini-2.5-flash-lite(9000)", "vertex-lite(9000)", gem("gemini-2.5-flash-lite", userHi, ""), jsonOK),
		key("key_thinking_zero_not_allowed", "execute", "gemini-2.5-flash-lite(0)", "vertex-lite(0)", gem("gemini-2.5-flash-lite", userHi, ""), jsonOK),
		key("key_thinking_dynamic_not_allowed", "execute", "gemini-2.5-flash-lite(-1)", "vertex-lite(-1)", gem("gemini-2.5-flash-lite", userHi, ""), jsonOK),
		key("key_thinking_below_min", "execute", "gemini-2.5-flash-lite(100)", "vertex-lite(100)", gem("gemini-2.5-flash-lite", userHi, ""), jsonOK),
		// A line ending in "\r\r\n" keeps one "\r" through Go's scanner.
		with(key("key_codex_stream_cr_lines", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":"hi"}`, reply{Status: 200, Body: "data: " + chunk1 + "\r\r\n\r\r\ndata: " + chunk2 + "\r\n\r\n"}), func(s *scenario) { s.Source = "codex" }),
		with(key("key_openai_stream_cr_lines", "stream", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","stream":true,"messages":[{"role":"user","content":"hi"}]}`, reply{Status: 200, Body: "data: " + chunk1 + "\r\r\n\r\r\ndata: " + chunk2 + "\r\n\r\n"}), func(s *scenario) { s.Source = "openai" }),
		// Vertex reports every raw line to the usage reporter: usage on a non-terminal
		// chunk counts (the Gemini executor filters it out).
		usageKey("usage_key_openai_stream_raw_usage", "openai", "stream", `{"model":"u-vflash","stream":true,"messages":[{"role":"user","content":"hi"}],"reasoning_effort":"medium"}`,
			reply{Status: 200, Body: "data: " + `{"candidates":[{"content":{"role":"model","parts":[{"text":"he"}]},"index":0}],"usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":1,"totalTokenCount":8},"modelVersion":"gemini-2.5-flash-004"}` + "\n\ndata: " + `{"candidates":[{"content":{"role":"model","parts":[{"text":"llo"}]},"finishReason":"STOP","index":0}],"modelVersion":"gemini-2.5-flash-004"}` + "\n\n"}),
		usageKey("usage_key_claude_execute_thinking", "claude", "execute", `{"model":"u-vflash","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":2048},"messages":[{"role":"user","content":"hi"}]}`,
			reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"candidates":[{"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":12,"thoughtsTokenCount":6,"cachedContentTokenCount":9,"totalTokenCount":48},"modelVersion":"gemini-2.5-flash-005"}`}),
		// v8.0.35 (2d5351b3): native Vertex Interactions for an Interactions client on a
		// credential that opted in: an API key (no project: /v1beta1/interactions), a
		// service account and a file access token with a project (global location).
		with(interactionsKey("int_key_execute", "execute", "gemini-3-pro-preview(high)", interactionsInput, interactionsJSON), func(s *scenario) {
			s.Headers = map[string]string{"Api-Revision": "client-rev"}
		}),
		interactionsKey("int_key_stream", "stream", "gemini-3-pro-preview", `{"model":"vertex-int","input":"hi","stream":true}`, interactionsSSE),
		interactionsKey("int_key_upstream_error", "execute", "gemini-3-pro-preview", `{"model":"vertex-int","input":"hi"}`, reply{Status: 429, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"code":429,"message":"slow"}}`}),
		// Not opted in, or not an Interactions client: the generateContent path.
		with(interactionsKey("int_key_not_opted_in", "execute", "gemini-3-pro-preview", `{"model":"vertex-int","input":"hi"}`, jsonOK), func(s *scenario) { s.ConfigAuth = 1 }),
		with(interactionsKey("int_key_openai_client_not_native", "execute", "gemini-3-pro-preview", `{"model":"vertex-int","messages":[{"role":"user","content":"hi"}]}`, jsonOK), func(s *scenario) { s.Source = "openai" }),
		with(sa("int_sa_execute", "execute", "us-east5", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, token, interactionsJSON), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["interactions"] = true
		}),
		with(sa("int_sa_stream_native_flag", "stream", "", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi","stream":true}`, token, interactionsSSE), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["native_interactions"] = true
		}),
		// Go's TestGeminiVertexInteractions_RegionalServiceAccount_UsesStandardBridge and
		// _PayloadRulesOrdering: no opt-in keeps a service account on generateContent;
		// payload rules for the interactions protocol apply to the native body.
		with(sa("int_sa_not_opted_in", "execute", "europe-west1", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi regional"}`, token, jsonOK), func(s *scenario) {
			s.Source = "interactions"
		}),
		with(interactionsKey("int_key_payload_rules", "execute", "gemini-3-pro-preview", `{"model":"vertex-int","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}`, interactionsJSON), func(s *scenario) {
			s.Config = interactionsKeyConfig + `
requests:
  payload:
    override:
      - models:
          - name: gemini-3-pro-preview
            protocol: interactions
            from-protocol: interactions
        params:
          custom_rule_param: enforced_after_all_transforms
`
		}),
		// The URL is built and checked before the token exchange; an empty token from the
		// exchange is Go's answer, not a request without credentials.
		with(sa("int_sa_bad_project", "execute", "", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, token, interactionsJSON), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["interactions"] = true
			s.Metadata["project_id"] = "p%zz"
		}),
		with(sa("int_sa_empty_token", "execute", "", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json; charset=utf-8"}}, Body: `{"access_token":"","expires_in":3599,"token_type":"Bearer"}`}, interactionsJSON), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["interactions"] = true
		}),
		with(sa("int_file_access_token_project", "execute", "", "gemini-3-pro-preview", `{"model":"gemini-3-pro-preview","input":"hi"}`, interactionsJSON), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["interactions"] = true
			s.Metadata["access_token"] = "ya29.from-file"
			delete(s.Metadata, "service_account")
		}),
		// d9451cb6: partner and open models, and credentials that opt in with
		// `openapi`, go to the OpenAI-compatible endpoints/openapi route.
		with(sa("oa_sa_execute", "execute", "global", "deepseek-ai/deepseek-v4.1-flash-maas", `{"model":"deepseek-ai/deepseek-v4.1-flash-maas","messages":[{"role":"user","content":"hello maas"}]}`, token, chatOK), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_sa_stream_location_override", "stream", "", "meta/llama-4-maverick-17b-128e-instruct-maas", `{"model":"meta/llama-4-maverick-17b-128e-instruct-maas","messages":[{"role":"user","content":"hi"}],"stream":true}`, token, chatSSE), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["openapi_location"] = "us-east5"
		}),
		with(sa("oa_sa_publisher_resource", "execute", "us-central1", "publishers/Meta/models/llama-3.3-70b-instruct-maas(high)", `{"model":"x","messages":[{"role":"user","content":"hi"}]}`, token, chatOK), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_sa_explicit_optin_gemini", "execute", "", "gemini-2.5-flash", `{"model":"gemini-2.5-flash","messages":[{"role":"user","content":"hi"}]}`, token, chatOK), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["openapi"] = "1"
		}),
		with(sa("oa_sa_explicit_optin_imagen", "execute", "", "imagen-4.0-generate-001", `{"model":"imagen-4.0-generate-001","contents":[{"role":"user","parts":[{"text":"a cat"}]}]}`, token, imagenOK), func(s *scenario) {
			s.Metadata["openapi"] = true
		}),
		with(sa("oa_sa_explicit_optout", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","contents":`+userHi+`}`, token, jsonOK), func(s *scenario) {
			s.Metadata["openapi"] = "false"
		}),
		with(sa("oa_sa_claude_stream", "stream", "", "qwen/qwen3-coder-480b-a35b-instruct-maas", `{"model":"qwen/qwen3-coder-480b-a35b-instruct-maas","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`, token, chatSSE), func(s *scenario) {
			s.Source = "claude"
		}),
		with(sa("oa_sa_before_interactions", "execute", "", "deepseek-ai/deepseek-v4.1-flash-maas", `{"model":"deepseek-ai/deepseek-v4.1-flash-maas","input":"hi"}`, token, chatOK), func(s *scenario) {
			s.Source = "interactions"
			s.Metadata["interactions"] = true
		}),
		with(sa("oa_sa_base_url", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, token, chatOK), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["base_url"] = "https://europe-west4-aiplatform.googleapis.com/v1beta1/"
		}),
		with(sa("oa_sa_token_error", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, reply{Status: 400, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":"invalid_grant","error_description":"bad"}`}), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_sa_empty_token", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json; charset=utf-8"}}, Body: `{"access_token":"","expires_in":3599,"token_type":"Bearer"}`}), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_sa_upstream_error", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, token, reply{Status: 429, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}`}), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_sa_count", "count", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"count these tokens"}]}`), func(s *scenario) {
			s.Source = "openai"
		}),
		with(sa("oa_file_access_token_rejected", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["access_token"] = "ya29.from-file"
			delete(s.Metadata, "service_account")
		}),
		with(key("oa_key_third_party", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, chatOK), func(s *scenario) {
			s.Source = "openai"
		}),
		with(key("oa_key_third_party_stream", "stream", "mistralai/mistral-small-2503", "", `{"model":"mistralai/mistral-small-2503","messages":[{"role":"user","content":"hi"}],"stream":true}`, chatSSE), func(s *scenario) {
			s.Source = "openai"
			s.ConfigAuth = 1
		}),
		with(key("oa_key_publishers_prefix", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, chatOK), func(s *scenario) {
			s.Source = "openai"
			s.Config = keyPrefixConfig
		}),
		with(key("oa_key_version_base", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, chatOK), func(s *scenario) {
			s.Source = "openai"
			s.Config = keyPrefixConfig
			s.ConfigAuth = 1
		}),
		with(key("oa_key_google_host_rejected", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Source = "openai"
			s.ConfigAuth = 2
			s.Via = ""
		}),
		// A base URL whose host carries a percent escape: Go's url.Parse refuses it in
		// http.NewRequest, so the key (or the fresh service-account token) never leaves.
		with(key("oa_key_escaped_host", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Source = "openai"
			s.Config = escapedHostConfig
			s.Via = ""
		}),
		with(sa("oa_sa_escaped_base", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, token), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["base_url"] = "https://%65vil.example/v1"
		}),
		// A base URL Go parses without a host (`https:relay.example`, `https:///v1`):
		// http.NewRequest accepts it and the transport refuses it, after the token
		// exchange on the service-account paths, so nothing reaches a host.
		with(key("oa_key_empty_host", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Source = "openai"
			s.Config = emptyHostConfig
			s.Via = ""
		}),
		with(sa("oa_sa_empty_host", "execute", "", "meta/llama-4", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`, token), func(s *scenario) {
			s.Source = "openai"
			s.Metadata["base_url"] = "https:///v1"
		}),
		with(key("key_empty_host", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, "")), func(s *scenario) {
			s.Config = emptyHostConfig
			s.Via = ""
		}),
		with(interactionsKey("int_key_empty_host", "execute", "gemini-3-pro-preview", `{"model":"vertex-int","input":"hi"}`), func(s *scenario) {
			s.Config = strings.Replace(interactionsKeyConfig, "http://UPSTREAM/api/", "https:relay.example", 1)
			s.Via = ""
		}),
		// A base URL without a scheme (`relay.example`): Go's transport refuses the
		// scheme before it looks for a host.
		with(key("oa_key_schemeless_base", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Source = "openai"
			s.Config = strings.Replace(emptyHostConfig, "https:relay.example", "relay.example", 1)
			s.Via = ""
		}),
		with(key("key_schemeless_base", "execute", "gemini-2.5-flash", "", gem("gemini-2.5-flash", userHi, "")), func(s *scenario) {
			s.Config = strings.Replace(emptyHostConfig, "https:relay.example", "relay.example", 1)
			s.Via = ""
		}),
		with(key("oa_key_payload_rules", "execute", "meta/llama-4", "", `{"model":"meta/llama-4","messages":[{"role":"user","content":"hi"}],"temperature":0.5}`, chatOK), func(s *scenario) {
			s.Source = "openai"
			s.Config = keyConfig + oaPayloadRules
		}),
		// A chat.completion chunk with a finish_reason ends the response model
		// (d9451cb6's response_model.go): the usage-only chunk after it names another model.
		// The key lists the model by name only (vertex_compat.go keeps it without an alias).
		// v8.0.35 (4f2e5270): the truncated Claude stream's failure keeps its usage.
		usageKey("usage_key_claude_stream_eof_without_finish", "claude", "stream", `{"model":"u-vflash","max_tokens":64,"messages":[{"role":"user","content":"hi"}],"stream":true}`,
			reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + chunk1 + "\n\n"}),
		with(usageKey("usage_oa_key_stream_terminal_model", "openai", "stream", `{"model":"meta/llama-4","stream":true,"messages":[{"role":"user","content":"hi"}]}`,
			reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: " + `{"id":"c1","object":"chat.completion.chunk","created":1,"model":"meta/llama-4-served","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}` + "\n\ndata: " + `{"id":"c1","object":"chat.completion.chunk","created":1,"model":"meta/other","choices":[],"usage":{"prompt_tokens":4,"completion_tokens":1,"total_tokens":5}}` + "\n\ndata: [DONE]\n\n"}), func(s *scenario) {
			s.Config = usageOAConfig
			s.Model = "meta/llama-4"
			s.RequestedModel = "meta/llama-4"
		}),
		// Reasoning inside completion_tokens: the record names OpenAICompatExecutor, whose
		// token rule counts it as a subset, and the normalized publisher model.
		with(usageKey("usage_oa_key_reasoning_publisher_model", "openai", "execute", `{"model":"u-llama","messages":[{"role":"user","content":"hi"}]}`,
			reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"c1","object":"chat.completion","created":1,"model":"meta/llama-3.3-70b-instruct-maas","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":50,"total_tokens":60,"completion_tokens_details":{"reasoning_tokens":30}}}`}), func(s *scenario) {
			s.Config = usageOAPublisherConfig
			s.Model = "publishers/meta/models/llama-3.3-70b-instruct-maas"
			s.RequestedModel = "u-llama"
		}),
		// A credential-stage failure keeps the Vertex executor's identity but already
		// names the normalized model.
		with(usageKey("usage_oa_key_google_host_rejected", "openai", "execute", `{"model":"u-llama","messages":[{"role":"user","content":"hi"}]}`), func(s *scenario) {
			s.Config = strings.Replace(usageOAPublisherConfig, "http://UPSTREAM/api", "https://us-central1-aiplatform.googleapis.com", 1)
			s.Model = "publishers/meta/models/llama-3.3-70b-instruct-maas"
			s.RequestedModel = "u-llama"
			s.Via = ""
		}),
		with(key("key_codex_tool_integer_types", "execute", "gemini-2.5-flash", "", `{"model":"gemini-2.5-flash","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[{"type":"function","name":"shell","parameters":{"type":"object","properties":{"timeout_ms":{"type":"number"}}}}]}`, jsonOK), func(s *scenario) {
			s.Source = "openai-response"
			s.Headers = map[string]string{"User-Agent": "codex_cli_rs/0.50.0 (Mac OS 15.0)"}
		}),
	}
}

// The endpoints/openapi answers (d9451cb6).
var chatOK = reply{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"chatcmpl-vertex-maas-1","object":"chat.completion","created":1700000000,"model":"deepseek-ai/deepseek-v4.1-flash-maas","choices":[{"index":0,"message":{"role":"assistant","content":"hello from vertex maas"},"finish_reason":"stop"}],"usage":{"prompt_tokens":8,"completion_tokens":5,"total_tokens":13}}`}

var chatSSE = reply{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"meta/llama-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"he\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"meta/llama-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\ndata: [DONE]\n\n"}

// oaPayloadRules: the openapi route applies rules for the openai protocol of the vertex
// executor, after the built-in edits.
const oaPayloadRules = `
payload:
  default:
    - models: [{name: "meta/*", protocol: openai}]
      params: {max_tokens: 77}
  override:
    - models: [{name: "meta/*", protocol: openai}]
      params: {temperature: 0.1}
    - models: [{name: "meta/*", protocol: gemini}]
      params: {generationConfig.temperature: 0.9}
`

var _ = strings.TrimSpace
