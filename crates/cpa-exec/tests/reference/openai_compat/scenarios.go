package main

import (
	"encoding/base64"
	"encoding/json"
	"strings"
)

const baseConfig = `
api-keys:
  openai-compatibility:
    - name: Acme
      base-url: http://UPSTREAM/v1
      headers:
        X-Static: static-value
        X-Forward: $X-Client-Trace
        X-Missing: $X-Not-Sent
      models:
        - name: acme-chat
          alias: chat
        - name: acme-mct
          alias: mct
          use-max-completion-tokens: true
        - name: acme-text
          alias: text
          input-modalities: [text]
        - name: acme-image
          alias: img
          image: true
        - name: gpt-4o
          alias: four
        - name: acme-compat
          alias: cc
          is-compat: true
      keys:
        - api-key: sk-fake-acme
    - name: cachey
      base-url: http://UPSTREAM/v2/
      support-prompt-cache-key: true
      models:
        - name: cache-model
          alias: cm
      keys:
        - api-key: sk-fake-cache
`

// payloadConfig adds requests.payload rules to baseConfig (helps.ApplyPayloadConfigWithRequest).
const payloadConfig = baseConfig + `
requests:
  payload:
    # One param per rule: Go ranges over each rule's params map, so several params in
    # one rule are written in random order.
    default:
      - models:
          - name: "acme-*"
            protocol: "openai"
        params:
          "temperature": 0.25
      - models:
          - name: "acme-*"
        params:
          "metadata.tier": "gold"
      - models:
          - name: "acme-chat"
        params:
          "max_tokens": 1
    default-raw:
      - models:
          - name: "acme-chat"
            headers:
              X-Tier: "pro*"
        params:
          "response_format": "{\"type\":\"json_object\"}"
    override:
      - models:
          - name: "acme-chat"
            from-protocol: "openai"
        params:
          "top_p": 0.5
      - models:
          - name: "acme-chat"
            protocol: "claude"
        params:
          "never": true
    filter:
      - models:
          - name: "acme-chat"
        params:
          - "user"
`

const claudeThinking = `{"model":"cc","max_tokens":10,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":[{"type":"thinking","thinking":"plan it","signature":"sig-not-gpt"},{"type":"text","text":"ok"}]},{"role":"user","content":"go"}]}`

// applyPatchRequest declares the apply_patch custom tool (apply_patch_bridge_test.go).
const applyPatchRequest = `{"model":"chat","input":[{"role":"user","content":"edit a.txt"}],"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}`

func applyPatchReply(arguments string) *upstream {
	body, _ := json.Marshal(arguments)
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"r1","object":"chat.completion","created":1,"model":"acme-chat","choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"c1","type":"function","function":{"name":"apply_patch","arguments":` + string(body) + `}}]},"finish_reason":"tool_calls"}]}`}
}

func applyPatchStream(arguments string, done bool) *upstream {
	args, _ := json.Marshal(arguments)
	head := `{"id":"r1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"apply_patch","arguments":""}}]},"finish_reason":null}]}`
	part := `{"id":"r1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":` + string(args) + `}}]},"finish_reason":null}]}`
	body := "data: " + head + "\n\ndata: " + part + "\n\n"
	if done {
		body += `data: {"id":"r1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}` + "\n\ndata: [DONE]\n\n"
	}
	return sse(body)
}

var jsonOK = &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"chatcmpl_1","object":"chat.completion","created":1,"model":"acme-chat","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}`}

func sse(body string) *upstream {
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: body}
}

const chunk1 = `{"id":"c1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{"role":"assistant","content":"he"},"finish_reason":null}]}`
const chunk2 = `{"id":"c1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{"content":"llo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}`

func chat(name, model, requested, payload string, up *upstream) scenario {
	return scenario{Name: name, Config: baseConfig, ConfigAuth: 0, Model: model, RequestedModel: requested, Payload: payload, Source: "openai", Op: "execute", Upstream: up}
}

func stream(name, payload string, up *upstream) scenario {
	s := chat(name, "acme-chat", "chat", payload, up)
	s.Op = "stream"
	s.Stream = true
	return s
}

func scenarios() []scenario {
	hi := `{"model":"chat","messages":[{"role":"user","content":"hi"}]}`
	out := []scenario{
		func() scenario {
			s := chat("chat_basic_alias_headers", "acme-chat", "chat", `{"model":"chat","messages":[{"role":"user","content":"hi"}],"max_tokens":64}`, jsonOK)
			s.Headers = map[string]string{"X-Client-Trace": "trace-1", "Authorization": "Bearer client-secret", "X-Api-Key": "client-secret"}
			return s
		}(),
		chat("chat_max_tokens_to_max_completion_tokens", "acme-mct", "mct", `{"model":"mct","max_tokens":128,"messages":[{"role":"user","content":"hi"}]}`, jsonOK),
		chat("chat_both_limits_keep_max_completion_tokens", "acme-mct", "mct", `{"model":"mct","max_tokens":128,"max_completion_tokens":256,"messages":[]}`, jsonOK),
		chat("chat_max_completion_tokens_to_max_tokens", "acme-chat", "chat", `{"model":"chat","messages":[],"max_completion_tokens":32}`, jsonOK),
		chat("chat_requested_alias_selects_limit_mode", "unlisted-upstream", "mct", `{"model":"mct","max_tokens":9,"messages":[]}`, jsonOK),
		chat("chat_text_only_tool_results", "acme-text", "text", `{"model":"text","messages":[{"role":"user","content":"look"},{"role":"assistant","tool_calls":[{"id":"t1","type":"function","function":{"name":"shot","arguments":"{}"}}]},{"role":"tool","tool_call_id":"t1","content":[{"type":"text","text":"captured"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAAA"}},"plain"]},{"role":"tool","tool_call_id":"t2","content":"[Tool returned image content; the images follow in the next user message.]"},{"role":"user","content":[{"type":"text","text":"Images returned by the preceding tool call(s):"},{"type":"image_url","image_url":{"url":"data:image/png;base64,BBBB"}}]},{"role":"tool","tool_call_id":"t3","content":"done"},{"role":"user","content":[{"type":"text","text":"Images returned by the preceding tool call(s):"},{"type":"input_image","image_url":"x"},{"type":"text","text":"keep me"}]}]}`, jsonOK),
		chat("chat_image_model_keeps_tool_images", "acme-chat", "chat", `{"model":"chat","messages":[{"role":"tool","tool_call_id":"t1","content":[{"type":"image_url","image_url":{"url":"u"}}]}]}`, jsonOK),
		chat("chat_video_input_passthrough", "acme-chat", "chat", `{"model":"chat","messages":[{"role":"user","content":[{"type":"text","text":"Describe the videos."},{"type":"video_url","video_url":{"url":"https://example.com/clip.mp4?part=1&name=a%20b","processing":"agentic"}}]}]}`, jsonOK),
		func() scenario {
			s := chat("chat_prompt_cache_key_trimmed", "cache-model", "cm", `{"model":"cm","prompt_cache_key":"  pk-1 ","messages":[]}`, jsonOK)
			s.ConfigAuth = 1
			return s
		}(),
		func() scenario {
			s := chat("chat_prompt_cache_key_from_original", "cache-model", "cm", `{"model":"cm","messages":[]}`, jsonOK)
			s.ConfigAuth = 1
			s.Original = `{"model":"cm","prompt_cache_key":"orig-key","messages":[]}`
			return s
		}(),
		func() scenario {
			s := chat("chat_prompt_cache_key_absent_without_session", "cache-model", "cm", `{"model":"cm","messages":[{"role":"user","content":"x"}]}`, jsonOK)
			s.ConfigAuth = 1
			return s
		}(),
		func() scenario {
			s := chat("chat_prompt_cache_key_from_execution_session", "cache-model", "cm", `{"model":"cm","messages":[]}`, jsonOK)
			s.ConfigAuth = 1
			s.ExecutionSession = " ws-session-1 "
			return s
		}(),
		func() scenario {
			s := chat("chat_prompt_cache_key_from_derived_session", "cache-model", "cm", `{"model":"cm","messages":[]}`, jsonOK)
			s.ConfigAuth = 1
			s.DerivedSession = " ctx:v1:0123abcd "
			return s
		}(),
		func() scenario {
			s := chat("chat_execution_session_beats_derived_session", "cache-model", "cm", `{"model":"cm","messages":[]}`, jsonOK)
			s.ConfigAuth = 1
			s.ExecutionSession = "ws-session-1"
			s.DerivedSession = "ctx:v1:0123abcd"
			return s
		}(),
		func() scenario {
			s := chat("chat_execution_session_without_prompt_cache_support", "acme-chat", "chat", `{"model":"chat","messages":[]}`, jsonOK)
			s.ExecutionSession = "ws-session-1"
			return s
		}(),
		chat("chat_prompt_cache_key_not_supported_kept", "acme-chat", "chat", `{"model":"chat","prompt_cache_key":"keep","messages":[]}`, jsonOK),
		chat("chat_upstream_201_and_extra_fields", "acme-chat", "chat", hi, &upstream{Status: 201, Headers: [][2]string{{"Content-Type", "application/json"}, {"X-Upstream", "1"}}, Body: `{"id":"x","choices":[],"extra":{"nested":[1,2]}}`}),
		chat("chat_gzip_response", "acme-chat", "chat", hi, &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}, {"Content-Encoding", "gzip"}}, Body: `{"id":"gzip"}`, Gzip: true}),
		chat("error_400_json_body", "acme-chat", "chat", hi, &upstream{Status: 400, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"error":{"message":"bad request","type":"invalid_request_error"}}`}),
		chat("error_429_retry_after_seconds", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Content-Type", "application/json"}, {"Retry-After", "7"}}, Body: `{"error":{"code":"rate_limit","message":"try later"}}`}),
		chat("error_429_retry_after_zero", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "0"}}, Body: `{}`}),
		chat("error_429_retry_after_past_date", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Wed, 21 Oct 2015 07:28:00 GMT"}}, Body: `{}`}),
		chat("error_429_retry_after_fraction_falls_back_to_tpm", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "1.5"}}, Body: `{"error":{"code":"TPMRateLimitExceeded","message":"x"}}`}),
		chat("error_429_tpm_message", "acme-chat", "chat", hi, &upstream{Status: 429, Body: `{"error":{"message":"Rate limit: Tokens per minute LIMIT was exceeded"}}`}),
		chat("error_429_plain", "acme-chat", "chat", hi, &upstream{Status: 429, Body: `slow down`}),
		chat("error_503_retry_after_ignored", "acme-chat", "chat", hi, &upstream{Status: 503, Headers: [][2]string{{"Retry-After", "9"}}, Body: `busy`}),
		chat("error_500_empty_body", "acme-chat", "chat", hi, &upstream{Status: 500, Body: ``}),
		{Name: "missing_base_url", ConfigAuth: -1, Provider: "openai-compatibility", Attributes: map[string]string{"api_key": "sk-fake"}, Model: "m", Payload: hi, Source: "openai", Op: "execute"},
		{Name: "attributes_only_no_key_custom_headers", ConfigAuth: -1, Provider: "openai-compatible-solo", Attributes: map[string]string{"base_url": " http://UPSTREAM/base/ ", "header:user-agent": "custom-agent", "header:x-b": "2", "header:X-A": "1", "header:content-type": "application/vnd.test+json", "header:  ": "skip", "header:X-Empty": "  "}, Model: "solo", Payload: hi, Source: "openai", Op: "execute", Upstream: jsonOK},
		stream("stream_basic", hi, sse("data: "+chunk1+"\n\ndata: "+chunk2+"\n\ndata: [DONE]\n\n")),
		stream("stream_include_usage_false_replaced", `{"model":"chat","stream":true,"stream_options":{"include_usage":false},"messages":[],"max_completion_tokens":5}`, sse("data: [DONE]\n\n")),
		stream("stream_comments_ids_crlf_and_spacing", hi, sse(": keep-alive\r\n\r\nid: 7\r\nretry: 10\r\ndata:"+chunk1+"\r\n\r\n   \ndata:   "+chunk2+"  \n\ndata: [DONE]\n\n")),
		stream("stream_drops_chunks_after_done", hi, sse("data: "+chunk1+"\n\ndata: [DONE]\n\ndata: "+chunk2+"\n\n")),
		stream("stream_eof_without_done", hi, sse("data: "+chunk1+"\n\n")),
		stream("stream_unterminated_final_frame", hi, sse("data: "+chunk1)),
		stream("stream_incomplete_frame_at_eof", hi, sse("data: "+chunk1+"\n\ndata: {\"id\":")),
		func() scenario {
			s := stream("stream_multiline_data", hi, sse("data: {\"id\":\"m\",\ndata: \"choices\":[]}\n\ndata: [DONE]\n\n"))
			// Go passes the joined payload, newline included, as one data line.
			return s
		}(),
		stream("stream_multiline_with_done", hi, sse("data: "+chunk1+"\ndata: [DONE]\n\n")),
		stream("stream_named_error_event", hi, sse("data: "+chunk1+"\n\nevent: error\ndata: {\"message\":\"boom\"}\n\n")),
		stream("stream_error_event_without_data", hi, sse("event: response.failed\n\n")),
		stream("stream_error_event_done", hi, sse("event: error\ndata: [DONE]\n\n")),
		stream("stream_data_error_with_status", hi, sse("data: {\"error\":{\"message\":\"rate\",\"status\":429}}\n\n")),
		stream("stream_data_error_null_is_ok", hi, sse("data: {\"error\":null,\"id\":\"n\",\"choices\":[]}\n\ndata: [DONE]\n\n")),
		stream("stream_data_type_failed", hi, sse("data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"status_code\":503}}}\n\n")),
		stream("stream_data_top_level_code_message", hi, sse("data: {\"code\":\"x\",\"message\":\"y\",\"status\":700}\n\n")),
		stream("stream_plain_json_after_blank_lines", hi, sse("\n\n{\"error\":\"bad\"}\n")),
		stream("stream_invalid_json_frame", hi, sse("data: not-json\n\n")),
		stream("stream_http_429", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "3"}}, Body: `{"error":{"message":"later"}}`}),
		stream("stream_json_content_type", hi, &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: "data: " + chunk1 + "\n\ndata: [DONE]\n\n"}),
		{Name: "compact_passthrough_sanitized", Config: baseConfig, ConfigAuth: 1, Model: "cache-model", RequestedModel: "cm", Source: "openai-response", Op: "execute", Alt: "responses/compact",
			Payload:  `{"model":"cm","stream":true,"input":[{"role":"user","content":"hi"},{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"step one"},{"type":"reasoning_text","text":""},{"type":"other","text":"x"}]},{"type":"reasoning","id":"rs_2","summary":[{"type":"summary_text","text":"kept"}]},{"type":"message","role":"assistant","content":[]}],"max_tokens":5,"prompt_cache_key":"x"}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"resp_1","object":"response.compaction","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}`}},
		{Name: "compact_store_true_keeps_ids", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai-response", Op: "execute", Alt: "responses/compact",
			Payload:  `{"model":"chat","store":true,"input":[{"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"s"}]}]}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"id":"resp_2","object":"response","usage":{"input_tokens":1,"output_tokens":2,"output_tokens_details":null,"input_tokens_details":{"other":1}}}`}},
		{Name: "compact_invalid_encrypted_content", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai-response", Op: "execute", Alt: "responses/compact", Needs: []string{"signature"},
			Payload:  `{"model":"chat","input":[{"type":"reasoning","id":"rs_1","encrypted_content":"not-a-signature","summary":[]},{"type":"reasoning","id":"rs_2","encrypted_content":null,"summary":[]},{"type":"reasoning","id":"rs_3","encrypted_content":" pad ","summary":[]},{"type":"reasoning","encrypted_content":7,"summary":[]}]}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"object":"response.compaction"}`}},
		{Name: "stream_with_compact_alt_skips_limits_and_cache", Config: baseConfig, ConfigAuth: 1, Model: "cache-model", RequestedModel: "cm", Source: "openai", Op: "stream", Stream: true, Alt: "responses/compact",
			Payload: `{"model":"cm","prompt_cache_key":" spaced ","max_tokens":4,"messages":[]}`, Upstream: sse("data: [DONE]\n\n")},
		{Name: "count_tokens_default_encoding", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai", Op: "count",
			Payload: `{"model":"chat","messages":[{"role":"system","content":"You are terse."},{"role":"user","name":"bob","content":[{"type":"text","text":"Count these tokens, please."},{"type":"image_url","image_url":{"url":"https://example.com/a.png"}}]},{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"x\"}"}}]}],"tools":[{"type":"function","function":{"name":"lookup","description":"Find things","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}],"tool_choice":"auto","response_format":{"type":"json_object"}}`},
		{Name: "count_tokens_gpt4o_encoding", Config: baseConfig, ConfigAuth: 0, Model: "gpt-4o", Source: "openai", Op: "count", Payload: `{"model":"four","messages":[{"role":"user","content":"tokenize ünïcödé text"}]}`},
		{Name: "count_tokens_empty", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai", Op: "count", Payload: `{"model":"chat","messages":[]}`},
		{Name: "images_generations_json", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			Payload:  `{"model":"img","prompt":"a cat","stream":true,"n":1}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"created":1,"data":[{"b64_json":"AAAA"}],"usage":{"total_tokens":3}}`}},
		{Name: "images_edits_json_path", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/openai/v1/images/edits", ContentType: "application/json",
			Payload:  `{"model":"acme-image","prompt":"edit","images":[{"image_url":"data:image/png;base64,AA"}]}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_default_path", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/other",
			Payload:  `{"prompt":"p"}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_stream_raw_passthrough", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images_stream", RequestPath: "/v1/images/generations",
			Payload:  `{"model":"img","prompt":"a cat","stream":false}`,
			Upstream: sse("event: image_generation.partial_image\ndata: {\"b64_json\":\"AA\",\"partial_image_index\":0}\n\nevent: image_generation.completed\ndata: {\"b64_json\":\"BB\"}\n\n")},
		{Name: "images_stream_error_status", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images_stream", RequestPath: "/v1/images/generations",
			Payload:  `{"prompt":"x"}`,
			Upstream: &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "5"}}, Body: `{"error":{"message":"busy"}}`}},
		{Name: "images_error_status", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			Payload:  `{"prompt":"x"}`,
			Upstream: &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "5"}}, Body: `{"error":{"message":"busy"}}`}},
		{Name: "images_edits_multipart_rewritten", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=client-boundary",
			Payload:     "--client-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nimg\r\n--client-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"in put.png\"\r\nContent-Type: image/png\r\n\r\nPNG-bytes\r\n\x00\x01\r\n--client-boundary--\r\n",
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_edits_multipart_stream_default_type", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images_stream", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\nfalse\r\n--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x\"\r\n\r\nraw\r\n--b--\r\n",
			Upstream:    sse("data: {}\n\n")},
		{Name: "images_multipart_missing_boundary", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data", Payload: "not json"},
		{Name: "images_non_json_non_multipart_passthrough", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "text/plain", Payload: "raw body",
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		stream("stream_depth_10000_is_valid", hi, sse("data: "+strings.Repeat("[", 10000)+"0"+strings.Repeat("]", 10000)+"\n\ndata: [DONE]\n\n")),
		stream("stream_depth_10001_is_invalid", hi, sse("data: "+strings.Repeat("[", 10001)+"0"+strings.Repeat("]", 10001)+"\n\n")),
		stream("stream_unicode_space_line_ends_frame", hi, sse("data: {}\n\u00a0\ndata: [DONE]\n\n")),
		stream("stream_data_error_fractional_status_string", hi, sse("data: {\"status\":\"429.5\",\"error\":{\"status\":503}}\n\n")),
		stream("stream_data_error_float_status", hi, sse("data: {\"status\":429.9}\n\n")),
		chat("error_429_retry_after_inconsistent_weekday", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Mon, 01 Jan 1970 00:00:00 GMT"}}, Body: `{"error":{"code":"TPMRateLimitExceeded"}}`}),
		chat("error_429_retry_after_rfc850", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Sunday, 06-Nov-94 08:49:37 GMT"}}, Body: `{}`}),
		chat("error_429_retry_after_ansic", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Sun Nov  6 08:49:37 1994"}}, Body: `{}`}),
		chat("error_429_retry_after_plus_sign", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "+4"}}, Body: `{}`}),
		{Name: "compact_reasoning_numeric_text", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai-response", Op: "execute", Alt: "responses/compact",
			Payload:  `{"model":"chat","input":[{"type":"reasoning","summary":null,"content":[{"type":"reasoning_text","text":1e3},{"type":"reasoning_text","text":true}]}]}`,
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"object":"response.compaction"}`}},
		{Name: "count_tokens_numeric_parts", Config: baseConfig, ConfigAuth: 0, Model: "acme-chat", Source: "openai", Op: "count",
			Payload: `{"model":"chat","messages":[{"role":"user","content":[{"type":"text","text":1e3},12.50,"plain",[{"type":"text","text":"nested"}]]}],"input":{"a":1},"prompt":2.5e-3}`},
		{Name: "images_multipart_boundary_prefix_in_body", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x.png\"\r\n\r\nA\r\n--bXYZ\r\nB\r\n--b--\r\n",
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_multipart_rfc2231_filename", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"image\"; filename*=utf-8''dir%2F%C3%A9.png\r\nX-Extra: kept\r\n\r\nraw\r\n--b--\r\n",
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_multipart_bad_final_delimiter", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"p\"\r\n\r\nv\r\n--b--garbage\r\n"},
		{Name: "images_json_non_utf8", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			ContentType: "application/json",
			PayloadB64:  base64.StdEncoding.EncodeToString([]byte("{\"model\":\"old\",\"prompt\":\"\xff\",\"stream\":true}")),
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_json_valid_stand_in_range_character", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			ContentType: "application/json", Payload: "{\"model\":\"acme-image\",\"prompt\":\"\U0010FF22\"}",
			Upstream: &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_multipart_rfc2231_split_utf8", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"image\"; filename*0*=utf-8''%C3; filename*1*=%A9.png\r\n\r\nx\r\n--b--\r\n",
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		{Name: "images_multipart_header_name_with_space", Config: baseConfig, ConfigAuth: 0, Model: "acme-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			ContentType: "multipart/form-data; boundary=b",
			Payload:     "--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x\"\r\nX Note: ok\r\n\r\nx\r\n--b--\r\n",
			Upstream:    &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: `{"data":[]}`}},
		stream("stream_data_error_overflowing_status_string", hi, sse("data: {\"error\":{},\"status\":\"18446744073709552045\"}\n\n")),
		chat("error_429_retry_after_double_space_past", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Fri,  02 Oct 2015 12:00:30 GMT"}}, Body: `{"error":{"code":"TPMRateLimitExceeded"}}`}),
		chat("error_429_retry_after_bad_zone_falls_back", "acme-chat", "chat", hi, &upstream{Status: 429, Headers: [][2]string{{"Retry-After", "Friday, 02-Oct-15 12:00:30 ABCDEF"}}, Body: `{"error":{"code":"TPMRateLimitExceeded"}}`}),
		func() scenario {
			s := chat("needs_thinking_suffix_level", "acme-chat(high)", "chat", hi, jsonOK)
			s.Needs = []string{"thinking"}
			return s
		}(),
		func() scenario {
			s := chat("needs_thinking_body_level_clamped", "acme-chat", "chat", `{"model":"chat","reasoning_effort":"xhigh","messages":[]}`, jsonOK)
			s.Needs = []string{"thinking"}
			return s
		}(),
		func() scenario {
			// A file credential is not configured-model routing: Go binds no capabilities and
			// the unknown model's effort passes through unclamped.
			s := chat("thinking_file_credential_not_bound", "acme-chat", "chat", `{"model":"chat","reasoning_effort":"xhigh","messages":[]}`, jsonOK)
			s.ConfigAuth = -1
			s.Provider = "openai-compatible-acme"
			s.Attributes = map[string]string{"base_url": "http://UPSTREAM/v1", "compat_name": "Acme", "provider_key": "openai-compatible-acme", "auth_kind": "oauth"}
			return s
		}(),
		func() scenario {
			s := chat("chat_payload_rules", "acme-chat", "chat", `{"model":"chat","messages":[],"max_tokens":7,"user":"u-1","metadata":{"a":1}}`, jsonOK)
			s.Config = payloadConfig
			s.Headers = map[string]string{"X-Tier": "pro-plus"}
			return s
		}(),
		func() scenario {
			s := chat("chat_payload_rules_header_gate_misses", "acme-chat", "chat", `{"model":"chat","messages":[]}`, jsonOK)
			s.Config = payloadConfig
			s.Headers = map[string]string{"X-Tier": "free"}
			return s
		}(),
		{Name: "custom_header_cpa_session_id", ConfigAuth: -1, Provider: "openai-compatible-solo",
			Attributes: map[string]string{"base_url": "http://UPSTREAM/v1", "api_key": "sk-fake-solo", "header:X-Sess": "$CPA-SESSION-ID", "header:X-Mix": "pre-$cpa-session-id-post", "header:X-Echo": "$X-Claude-Code-Session-Id"},
			Headers:    map[string]string{"X-Claude-Code-Session-Id": "sess-hdr-1"},
			Model:      "solo", Payload: hi, Source: "openai", Op: "execute", Upstream: jsonOK},
		// Go expands $CPA-SESSION-ID to CanonicalSessionID, message-hash fallback included;
		// the shared Rust helper passes only the explicit session, by design.
		{Name: "custom_header_session_from_payload_without_original", ConfigAuth: -1, Provider: "openai-compatible-solo",
			Attributes: map[string]string{"base_url": "http://UPSTREAM/v1", "api_key": "sk-fake-solo", "header:X-Sess": "$CPA-SESSION-ID"},
			Model:      "solo", Payload: `{"model":"solo","messages":[{"role":"user","content":"hi"}],"prompt_cache_key":"pc-1"}`, Source: "openai", Op: "execute", Upstream: jsonOK},
		{Name: "custom_header_cpa_session_id_absent", Needs: []string{"session"}, ConfigAuth: -1, Provider: "openai-compatible-solo",
			Attributes: map[string]string{"base_url": "http://UPSTREAM/v1", "api_key": "sk-fake-solo", "header:X-Sess": "$CPA-SESSION-ID", "header:X-Mix": "pre-$CPA-SESSION-ID-post"},
			Model:      "solo", Payload: hi, Source: "openai", Op: "execute", Upstream: jsonOK},
		func() scenario {
			// is-compat keeps assistant thinking whose signature is not GPT-compatible.
			s := chat("claude_is_compat_keeps_thinking", "acme-compat", "cc", claudeThinking, jsonOK)
			s.Source = "claude"
			return s
		}(),
		func() scenario {
			s := chat("claude_not_compat_drops_thinking", "acme-chat", "chat", strings.Replace(claudeThinking, `"cc"`, `"chat"`, 1), jsonOK)
			s.Source = "claude"
			return s
		}(),
		func() scenario {
			// Codex clients get integer parameter types restored before translation.
			s := chat("codex_user_agent_tool_integers", "acme-chat", "chat", `{"model":"chat","messages":[],"tools":[{"type":"function","function":{"name":"exec_command","parameters":{"type":"object","properties":{"timeout_ms":{"type":"number"},"cmd":{"type":"string"}}}}}]}`, jsonOK)
			s.Headers = map[string]string{"User-Agent": "codex_cli_rs/0.50.0"}
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_valid_call", "acme-chat", "chat", applyPatchRequest, applyPatchReply(`{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch\n"}`))
			s.Source = "openai-response"
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_invalid_arguments", "acme-chat", "chat", applyPatchRequest, applyPatchReply(`{"input":"x","extra":"RAW_SECRET"}`))
			s.Source = "openai-response"
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_stream_valid", "acme-chat", "chat", applyPatchRequest, applyPatchStream(`{"input":"*** Begin Patch\n*** End Patch\n"}`, true))
			s.Source, s.Op, s.Stream = "openai-response", "stream", true
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_stream_invalid_arguments", "acme-chat", "chat", applyPatchRequest, applyPatchStream(`{"input":7}`, true))
			s.Source, s.Op, s.Stream = "openai-response", "stream", true
			return s
		}(),
		func() scenario {
			// The failing frame ends the stream with 502 before the later error frame.
			up := applyPatchStream(`{"input":7}`, false)
			up.Body += `data: {"id":"r1","object":"chat.completion.chunk","created":1,"model":"acme-chat","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}` + "\n\n" + `data: {"error":{"message":"late","status":429}}` + "\n\n"
			s := chat("apply_patch_stream_failure_stops_before_later_frames", "acme-chat", "chat", applyPatchRequest, up)
			s.Source, s.Op, s.Stream = "openai-response", "stream", true
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_stream_truncated", "acme-chat", "chat", applyPatchRequest, applyPatchStream(`{"input":"unfinished"`, false))
			s.Source, s.Op, s.Stream = "openai-response", "stream", true
			return s
		}(),
		func() scenario {
			s := chat("apply_patch_stream_chat_client_ignores_bridge", "acme-chat", "chat", `{"model":"chat","messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"apply_patch","parameters":{"type":"object"}}}]}`, applyPatchStream(`{"input":7}`, true))
			s.Op, s.Stream = "stream", true
			return s
		}(),
		func() scenario {
			s := chat("needs_translator_responses_source", "acme-chat", "chat", `{"model":"chat","input":"hello","max_output_tokens":20}`, jsonOK)
			s.Source = "openai-response"
			s.Needs = []string{"translator"}
			return s
		}(),
		func() scenario {
			s := chat("needs_translator_responses_eof_without_done", "acme-chat", "chat", `{"model":"chat","input":"hello","stream":true}`, sse("data: "+chunk1+"\n\n"))
			s.Source = "openai-response"
			s.Op = "stream"
			s.Stream = true
			s.Needs = []string{"translator"}
			return s
		}(),
		func() scenario {
			s := chat("needs_translator_claude_code_prompt_cache", "cache-model", "cm", `{"model":"cm","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}`, jsonOK)
			s.ConfigAuth = 1
			s.Source = "claude"
			s.Headers = map[string]string{"X-Claude-Code-Session-Id": "sess-1"}
			return s
		}(),
	}
	return out
}
