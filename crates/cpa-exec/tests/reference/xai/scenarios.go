package main

import (
	"fmt"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/tidwall/gjson"
)

// sse frames each event the way xAI streams them: an event line, then its data.
func sse(events ...string) string {
	var b strings.Builder
	for _, e := range events {
		b.WriteString("event: " + gjson.Get(e, "type").String() + "\ndata: " + e + "\n\n")
	}
	return b.String()
}

// dataOnly frames events without event lines, as some upstream streams do.
func dataOnly(events ...string) *upstream {
	var b strings.Builder
	for _, e := range events {
		b.WriteString("data: " + e + "\n\n")
	}
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}}, Body: b.String()}
}

// foldedPatchRequest declares a custom apply_patch inside a namespace large enough to fold.
func foldedPatchRequest() string {
	declarations := []string{`{"type":"custom","name":"apply_patch"}`}
	for i := 0; i < 205; i++ {
		declarations = append(declarations, fmt.Sprintf(`{"type":"function","name":"lookup%d","parameters":{"type":"object"}}`, i))
	}
	tools := `[{"type":"namespace","name":"n","tools":[` + strings.Join(declarations, ",") + `]}]`
	return `{"model":"grok-4","tools":` + tools + `,"input":[{"type":"custom_tool_call","call_id":"old","name":"apply_patch","namespace":"n","input":"old"},{"type":"custom_tool_call_output","call_id":"old","output":"ok"}]}`
}

func foldedPatchTurn() *upstream {
	return dataOnly(
		`{"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"a","name":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.done","item_id":"a","arguments":"{\"name\":\"apply_patch\",\"arguments\":{\"input\":\"p\"}}"}`,
		`{"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"a","call_id":"c","name":"n"}}`,
		`{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"b","name":"n","arguments":""}}`,
		`{"type":"response.function_call_arguments.done","item_id":"b","arguments":"{\"name\":\"lookup0\",\"arguments\":{\"x\":1}}"}`,
		`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"b","call_id":"d","name":"n"}}`,
		`{"type":"response.completed","response":{"output":[]}}`,
	)
}

const patchTool = `{"type":"custom","name":"apply_patch","description":"Patch files.","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}}`

func streamOf(events ...string) *upstream {
	return &upstream{Status: 200, Headers: [][2]string{{"Content-Type", "text/event-stream"}, {"X-Request-Id", "req-1"}}, Body: sse(events...)}
}

func jsonReply(status int, body string) *upstream {
	return &upstream{Status: status, Headers: [][2]string{{"Content-Type", "application/json"}}, Body: body}
}

const usageJSON = `{"input_tokens":12,"input_tokens_details":{"cached_tokens":3},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":17}`

func created(model string) string {
	return fmt.Sprintf(`{"type":"response.created","sequence_number":0,"response":{"id":"resp_x1","object":"response","created_at":1700000000,"status":"in_progress","model":"%s","output":[]}}`, model)
}

func completedWith(model, output string) string {
	return fmt.Sprintf(`{"type":"response.completed","sequence_number":9,"response":{"id":"resp_x1","object":"response","created_at":1700000000,"status":"completed","model":"%s","output":%s,"usage":%s}}`, model, output, usageJSON)
}

const message = `{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"hello there","annotations":[]}]}`

// textTurn is a plain assistant answer whose completed event has an empty output,
// so the executor rebuilds it from output_item.done.
func textTurn(model string) *upstream {
	return streamOf(
		created(model),
		`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}`,
		`{"type":"response.content_part.added","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"part":{"type":"output_text","text":""}}`,
		`{"type":"response.output_text.delta","sequence_number":3,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello there"}`,
		`{"type":"response.output_text.done","sequence_number":4,"item_id":"msg_1","output_index":0,"content_index":0,"text":"hello there"}`,
		`{"type":"response.output_item.done","sequence_number":5,"output_index":0,"item":`+message+`}`,
		completedWith(model, `[]`),
	)
}

// reasoningTurn streams Grok reasoning text, encrypted content and an answer.
func reasoningTurn(model, blob string) *upstream {
	reasoning := `{"type":"reasoning","id":"rs_1","summary":[],"content":[{"type":"reasoning_text","text":"think"}],"encrypted_content":"` + blob + `","status":"completed"}`
	return streamOf(
		created(model),
		`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"status":"in_progress"}}`,
		`{"type":"response.content_part.added","sequence_number":2,"item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":""}}`,
		`{"type":"response.reasoning_text.delta","sequence_number":3,"item_id":"rs_1","output_index":0,"content_index":0,"delta":"think"}`,
		`{"type":"response.reasoning_text.done","sequence_number":4,"item_id":"rs_1","output_index":0,"content_index":0,"text":"think"}`,
		`{"type":"response.content_part.done","sequence_number":5,"item_id":"rs_1","output_index":0,"content_index":0,"part":{"type":"reasoning_text","text":"think"}}`,
		`{"type":"response.output_item.done","sequence_number":6,"output_index":0,"item":`+reasoning+`}`,
		`{"type":"response.output_item.added","sequence_number":7,"output_index":1,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}`,
		`{"type":"response.output_text.delta","sequence_number":8,"item_id":"msg_1","output_index":1,"content_index":0,"delta":"hello there"}`,
		`{"type":"response.output_item.done","sequence_number":9,"output_index":1,"item":`+message+`}`,
		completedWith(model, `[`+reasoning+`,`+message+`]`),
	)
}

// callTurn answers with one function call named name.
func callTurn(model, name, args string) *upstream {
	item := fmt.Sprintf(`{"type":"function_call","id":"fc_1","call_id":"call_1","name":"%s","arguments":%q,"status":"completed"}`, name, args)
	return streamOf(
		created(model),
		fmt.Sprintf(`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"%s","arguments":"","status":"in_progress"}}`, name),
		fmt.Sprintf(`{"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","output_index":0,"delta":%q}`, args),
		fmt.Sprintf(`{"type":"response.function_call_arguments.done","sequence_number":3,"item_id":"fc_1","output_index":0,"arguments":%q}`, args),
		`{"type":"response.output_item.done","sequence_number":4,"output_index":0,"item":`+item+`}`,
		completedWith(model, `[`+item+`]`),
	)
}

var apiKey = map[string]string{"api_key": "sk-fake-xai"}

func oauth(extra map[string]any) map[string]any {
	out := map[string]any{"type": "xai", "auth_kind": "oauth", "access_token": "xai-fake-token", "email": "u@example.com"}
	for k, v := range extra {
		out[k] = v
	}
	return out
}

const responsesHello = `{"model":"grok-4.3","input":[{"role":"user","content":"hello"}]}`

func manyFunctions(prefix string, n int) string {
	parts := make([]string, 0, n)
	for i := 0; i < n; i++ {
		parts = append(parts, fmt.Sprintf(`{"type":"function","name":"%s%d","parameters":{"type":"object","properties":{}}}`, prefix, i))
	}
	return strings.Join(parts, ",")
}

func scenarios() []scenario {
	grok46 := &registry.ThinkingSupport{Levels: []string{"low", "high"}}
	s := []scenario{
		// --- credentials, base URLs and headers ---
		{Name: "api_key_default_base", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "oauth_default_chat_proxy", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "oauth_default_api_base_goes_to_chat_proxy", ConfigAuth: -1, Metadata: oauth(map[string]any{"base_url": "https://api.x.ai/v1/"}), Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "oauth_custom_base_without_cli_headers", ConfigAuth: -1, Metadata: oauth(map[string]any{"base_url": "https://grok.example.test/v2/"}), Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "oauth_using_api_metadata", ConfigAuth: -1, Metadata: oauth(map[string]any{"using_api": true}), Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "api_key_using_api_false_attribute", ConfigAuth: -1, Attributes: map[string]string{"api_key": "sk-fake-xai", "using_api": "false"}, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "oauth_string_using_api_and_custom_headers", ConfigAuth: -1, Metadata: oauth(map[string]any{"using_api": "TRUE"}),
			Attributes: map[string]string{"header:X-Team": "blue", "header:User-Agent": "custom-agent/1"}, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "config_key_base_url_and_headers", ConfigAuth: 0,
			Config: "xai-api-key:\n  - api-key: sk-fake-cfg\n    base-url: https://xai.example.test/v1\n    headers:\n      X-Org: acme\n", Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "empty_token_omits_authorization", ConfigAuth: -1, Attributes: map[string]string{"base_url": "https://xai.example.test/v1"}, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},

		// --- request shaping ---
		{Name: "shapes_responses_request", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config: "xai:\n  inject-x-search: true\n", ExecutionSession: "conv-xai-1",
			Payload:  `{"model":"grok-4.3","input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"test"}],"content":null,"encrypted_content":null},{"type":"reasoning","summary":[{"type":"summary_text","text":"second"}]},{"role":"user","content":"hello"}],"include":["reasoning.encrypted_content"],"reasoning":{"effort":"high"},"previous_response_id":"resp_old","prompt_cache_retention":"24h","safety_identifier":"u1","stream_options":{"include_obfuscation":false},"tools":[{"type":"tool_search"},{"type":"image_generation"},{"type":"custom","name":"custom_lookup"},{"type":"function","name":"lookup"},{"type":"web_search","external_web_access":true,"search_content_types":["text","image"]},{"type":"namespace","name":"codex_app","description":"Tools in the codex_app namespace.","tools":[{"type":"function","name":"automation_update"},{"type":"custom","name":"namespace_custom"},{"type":"tool_search"}]}],"tool_choice":{"type":"allowed_tools","tools":[{"type":"function","name":"automation_update","namespace":"codex_app"},{"type":"function","name":"lookup"},{"type":"web_search"}]}}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "chat_completions_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "execute",
			Payload:  `{"model":"grok-4.3","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"hi"}],"max_completion_tokens":64,"temperature":0.2,"top_p":0.9,"stop":["END"],"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}]}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "chat_completions_stream_tool_call", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "stream",
			Payload:  `{"model":"grok-4.3","stream":true,"messages":[{"role":"user","content":"hi"}],"max_tokens":32,"tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}}]}`,
			Upstream: callTurn("grok-4.3", "lookup", `{"q":"x"}`)},
		{Name: "claude_stream_message_start_tokens", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "stream",
			Payload:  `{"model":"grok-4.3","max_tokens":100,"system":"sys prompt","messages":[{"role":"user","content":[{"type":"text","text":"hello claude"}]}],"stream":true}`,
			Upstream: streamOf(created("grok-4.3"), `{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}`, `{"type":"response.output_text.delta","sequence_number":2,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"hi"}`, `{"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":`+message+`}`, `{"type":"response.completed","sequence_number":4,"response":{"id":"resp_x1","object":"response","created_at":1700000000,"status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":0,"output_tokens":2,"total_tokens":2}}}`)},
		{Name: "claude_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "execute",
			Payload:  `{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":"hello"}],"tools":[{"name":"get_weather","description":"w","input_schema":{"type":"object","properties":{"city":{"type":"string"}}}}]}`,
			Upstream: callTurn("grok-4.3", "get_weather", `{"city":"Paris"}`)},
		{Name: "responses_stream_passthrough", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "accepts_response_incomplete", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: streamOf(created("grok-4.3"), `{"type":"response.output_item.done","sequence_number":1,"output_index":0,"item":`+message+`}`, `{"type":"response.incomplete","sequence_number":2,"response":{"id":"resp_x1","object":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"grok-4.3","output":[],"usage":`+usageJSON+`}}`)},
		{Name: "stream_accepts_response_incomplete", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "stream",
			Payload: `{"model":"grok-4.3","messages":[{"role":"user","content":"hi"}]}`, Upstream: streamOf(created("grok-4.3"), `{"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"partial"}`, `{"type":"response.incomplete","sequence_number":2,"response":{"id":"resp_x1","object":"response","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"model":"grok-4.3","output":[],"usage":`+usageJSON+`}}`)},
		{Name: "disconnected_before_completed", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: streamOf(created("grok-4.3"), `{"type":"response.output_text.delta","sequence_number":1,"item_id":"msg_1","output_index":0,"content_index":0,"delta":"cut"}`)},
		{Name: "output_controls_preserved_from_chat", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "execute",
			Payload: `{"model":"grok-4.3","messages":[{"role":"user","content":"hi"}],"max_completion_tokens":null,"max_tokens":77,"temperature":1.5,"top_p":null,"top_k":40}`, Upstream: textTurn("grok-4.3")},
		{Name: "payload_override_stop_dropped", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config:  "payload:\n  override:\n    - models:\n        - name: grok-4.3\n      params:\n        stop: [\"X\"]\n        temperature: 0.3\n",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "root_union_branches_and_refs", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":"hi","tools":[{"type":"function","name":"union","parameters":{"anyOf":[{"properties":{"a":{"type":"string"}}},{"properties":{"b":{"$ref":"#/$defs/B"}}}],"$defs":{"B":{"type":"integer"}}}},{"type":"function","name":"refd","parameters":{"type":"object","properties":{"x":{"$ref":"#/definitions/X"}},"definitions":{"X":{"type":"string","enum":["a","b"]}}}},{"type":"function","name":"bad_root","parameters":{"type":"string"}},{"type":"function","name":"num","parameters":{"type":"object","properties":{"n":{"type":"number"},"i":{"type":"integer"}}}}]}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "codex_app_automation_update_simplified", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":"hi","tools":[{"type":"namespace","name":"codex_app","description":"app","tools":[{"type":"function","name":"automation_update","description":"update","parameters":{"type":"object","properties":{"spec":{"oneOf":[{"$ref":"#/$defs/A"},{"type":"null"}]}},"$defs":{"A":{"type":"object","properties":{"k":{"type":"string"}}}}}},{"type":"function","name":"other","parameters":{"type":"object","properties":{}}}]},{"type":"function","name":"mcp__codex_app__automation_update","parameters":{"type":"object","properties":{"z":{"oneOf":[{"type":"string"},{"type":"null"}]}}}}]}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "namespaces_flatten_and_restore", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"hi"},{"type":"function_call","call_id":"call_0","name":"read","namespace":"files","arguments":"{\"path\":\"a\",\"big\":12345678901234567890}"},{"type":"function_call_output","call_id":"call_0","output":"data"}],"tools":[{"type":"namespace","name":"files","description":"File tools","tools":[{"type":"function","name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}},{"type":"function","name":"files__write","parameters":{"type":"object","properties":{}}}]},{"type":"function","name":"read","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"function","name":"read","namespace":"files"}}`,
			Upstream: callTurn("grok-4.3", "files__read", `{"path":"b"}`)},
		{Name: "namespaces_fold_when_over_200_tools", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"hi"},{"type":"function_call","call_id":"call_0","name":"t1","namespace":"big","arguments":"{\"a\":1}"},{"type":"function_call_output","call_id":"call_0","output":"ok"}],"tools":[` + manyFunctions("f", 150) + `,{"type":"namespace","name":"big","description":"Big","tools":[` + manyFunctions("t", 60) + `]}],"tool_choice":{"type":"function","name":"t2","namespace":"big"}}`,
			Upstream: callTurn("grok-4.3", "big", `{"name":"t3","arguments":{"x":1}}`)},
		{Name: "namespaces_fold_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","input":"hi","tools":[` + manyFunctions("f", 150) + `,{"type":"namespace","name":"big","description":"Big","tools":[` + manyFunctions("t", 60) + `]}]}`,
			Upstream: callTurn("grok-4.3", "big", `{"name":"t3","arguments":"{\"x\":1}"}`)},
		{Name: "tools_clamped_to_200", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.3","input":"hi","tools":[` + manyFunctions("f", 205) + `]}`, Upstream: textTurn("grok-4.3")},
		{Name: "additional_tools_promoted_and_restored", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"hi"},{"type":"additional_tools","tools":[{"type":"namespace","name":"mcp__srv","tools":[{"type":"function","name":"go","parameters":{"type":"object","properties":{}}}]},{"type":"function","name":"plain","parameters":{"type":"object","properties":{}}}]}],"tool_choice":"auto"}`,
			Upstream: callTurn("grok-4.3", "mcp__srv__go", `{}`)},
		{Name: "custom_tool_history_normalized", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"run"},{"type":"custom_tool_call","call_id":"c1","name":"shell","input":"ls -la <dir>"},{"type":"custom_tool_call_output","call_id":"c1","output":[{"type":"input_text","text":"a"},{"type":"input_text","text":"b"}]},{"type":"custom_tool_call","call_id":"c2","name":"json_tool","input":{"k":1}},{"type":"custom_tool_call_output","call_id":"c2","output":"done"}],"tools":[{"type":"custom","name":"shell","description":"Run","format":{"type":"grammar","syntax":"lark","definition":"start: /.+/"}},{"type":"custom","name":"json_tool"}]}`,
			Upstream: callTurn("grok-4.3", "shell", `{"input":"pwd"}`)},
		{Name: "x_search_internal_calls_filtered", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: responsesHello,
			Upstream: streamOf(created("grok-4.3"),
				`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"xs_1","call_id":"xs_call","name":"x_keyword_search","arguments":"","status":"in_progress"}}`,
				`{"type":"response.function_call_arguments.done","sequence_number":2,"item_id":"xs_1","output_index":0,"arguments":"{}"}`,
				`{"type":"response.output_item.done","sequence_number":3,"output_index":0,"item":{"type":"function_call","id":"xs_1","call_id":"xs_call","name":"x_keyword_search","arguments":"{}","status":"completed"}}`,
				`{"type":"response.output_item.added","sequence_number":4,"output_index":1,"item":{"type":"message","id":"msg_1","status":"in_progress","role":"assistant","content":[]}}`,
				`{"type":"response.output_item.done","sequence_number":5,"output_index":1,"item":`+message+`}`,
				completedWith("grok-4.3", `[{"type":"function_call","id":"xs_1","call_id":"xs_call","name":"x_keyword_search","arguments":"{}","status":"completed"},`+message+`]`))},
		{Name: "x_search_stream_keeps_client_same_name_tool", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.3","input":"hi","tools":[{"type":"function","name":"x_keyword_search","parameters":{"type":"object","properties":{}}}]}`,
			Upstream: streamOf(created("grok-4.3"),
				`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"xs_1","call_id":"xs_call","name":"x_semantic_search","arguments":"","status":"in_progress"}}`,
				`{"type":"response.output_item.done","sequence_number":2,"output_index":0,"item":{"type":"function_call","id":"xs_1","call_id":"xs_call","name":"x_semantic_search","arguments":"{}","status":"completed"}}`,
				`{"type":"response.output_item.added","sequence_number":3,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"x_keyword_search","arguments":"","status":"in_progress"}}`,
				`{"type":"response.output_item.done","sequence_number":4,"output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"x_keyword_search","arguments":"{}","status":"completed"}}`,
				completedWith("grok-4.3", `[]`))},
		{Name: "tool_search_stream_filtered", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: `{"model":"grok-4.3","input":"hi","tools":[{"type":"tool_search"},{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"tool_search"}}`, Upstream: textTurn("grok-4.3")},
		{Name: "claude_web_search_tool_choice_grok43", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.3","max_tokens":50,"messages":[{"role":"user","content":"search"}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":3},{"name":"lookup","input_schema":{"type":"object","properties":{}}}],"tool_choice":{"type":"tool","name":"web_search"}}`, Upstream: textTurn("grok-4.3")},
		{Name: "claude_web_search_tool_choice_grok46", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.6", Source: "claude", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.6","max_tokens":50,"messages":[{"role":"user","content":"search"}],"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":3},{"name":"lookup","input_schema":{"type":"object","properties":{}}}],"tool_choice":{"type":"tool","name":"web_search"}}`, Upstream: textTurn("grok-4.6")},
		{Name: "image_generation_stripped_before_grok46", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.20-fast", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-4.20-fast","input":"draw","tools":[{"type":"image_generation"},{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"image_generation"}}`, Upstream: textTurn("grok-4.20-fast")},
		{Name: "image_generation_forced_kept_grok46", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.6", Source: "openai-response", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.6","input":"draw","tools":[{"type":"image_generation","output_format":"png"},{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"image_generation"}}`, Upstream: textTurn("grok-4.6")},
		{Name: "allowed_tools_image_only_auto", ConfigAuth: -1, Attributes: apiKey, Model: "grok-5", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-5","input":"draw","tools":[{"type":"image_generation"},{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"allowed_tools","mode":"auto","tools":[{"type":"image_generation"}]}}`, Upstream: textTurn("grok-5")},
		{Name: "allowed_tools_web_search_mixed", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.3","input":"find","tools":[{"type":"web_search"},{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"web_search"},{"type":"function","name":"f"},{"type":"function","name":"gone"}]}}`, Upstream: textTurn("grok-4.3")},
		{Name: "orphaned_tool_choice_dropped", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Config:  "xai:\n  inject-x-search: true\n",
			Payload: `{"model":"grok-4.3","input":"hi","tools":[{"type":"tool_search"}],"tool_choice":{"type":"function","name":"missing"},"parallel_tool_calls":true}`, Upstream: textTurn("grok-4.3")},
		{Name: "client_web_search_function_aliased", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"hi"},{"type":"function_call","call_id":"w0","name":"web_search","arguments":"{\"q\":\"a\"}"},{"type":"function_call_output","call_id":"w0","output":"r"}],"tools":[{"type":"function","name":"web_search","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},{"type":"function","name":"clientfn_web_search","parameters":{"type":"object","properties":{}}}],"tool_choice":{"type":"function","name":"web_search"}}`,
			Upstream: callTurn("grok-4.3", "clientfn_web_search_1", `{"q":"b"}`)},
		{Name: "client_web_search_alias_restored_in_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","input":"hi","tools":[{"type":"function","name":"web_search","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}]}`,
			Upstream: callTurn("grok-4.3", "clientfn_web_search", `{"q":"b"}`)},
		{Name: "thinking_suffix_applied", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3(high)", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "unsupported_reasoning_effort_omitted", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4-fast-non-reasoning", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-4-fast-non-reasoning","input":"hi","reasoning":{"effort":"high"}}`, Upstream: textTurn("grok-4-fast-non-reasoning")},
		{Name: "bound_model_thinking_levels", ConfigAuth: -1, Attributes: apiKey, Model: "grok-x-custom", Source: "openai-response", Op: "execute",
			Bind:    &bind{Name: "grok-x-custom", Thinking: grok46},
			Payload: `{"model":"grok-x-custom","input":"hi","reasoning":{"effort":"medium"}}`, Upstream: textTurn("grok-x-custom")},
		{Name: "bound_compat_model_claude", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "execute",
			Bind:    &bind{Name: "grok-4.3", IsCompat: true},
			Payload: `{"model":"grok-4.3","max_tokens":50,"thinking":{"type":"enabled","budget_tokens":2048},"messages":[{"role":"user","content":"hello"}]}`, Upstream: textTurn("grok-4.3")},
		{Name: "composer_reuses_prompt_cache_key", ConfigAuth: -1, Attributes: apiKey, Model: "grok-composer-1", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-composer-1","input":"hi","prompt_cache_key":"pck-123"}`, Upstream: textTurn("grok-composer-1")},
		{Name: "derived_session_uuid", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			DerivedSession: "ctx:v1:abc", Payload: responsesHello, Upstream: textTurn("grok-4.3")},
		{Name: "reasoning_text_events_normalized_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: responsesHello, Upstream: reasoningTurn("grok-4.3", "GROKENC3")},
		{Name: "reasoning_output_normalized_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: reasoningTurn("grok-4.3", "GROKENC3")},
		{Name: "reasoning_to_claude_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "stream",
			Payload: `{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":"hello"}],"stream":true}`, Upstream: reasoningTurn("grok-4.3", "GROKENC3")},
		{Name: "encrypted_content_sanitized", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"a"}],"encrypted_content":"gAAAA-codex-blob"},{"type":"reasoning","summary":[{"type":"summary_text","text":"b"}]},{"type":"compaction","encrypted_content":"bad"},{"type":"reasoning","summary":[],"encrypted_content":"GROKENC1"},{"role":"user","content":"hi"}]}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "image_refs_in_chat_body", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-4.3","input":[{"role":"user","content":[{"type":"input_text","text":"see"},{"type":"input_image","image_url":"https://img.example/a.png"}]}],"metadata":{"image":{"image_url":{"url":" https://img.example/b.png "}},"n":1.50}}`, Upstream: textTurn("grok-4.3")},

		// --- reasoning replay across turns (one executor, shared cache) ---
		{Name: "replay_claude_turn1", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "stream", CallerKey: "client-key-1",
			Headers: map[string]string{"X-Claude-Code-Session-Id": "sess-replay-1"},
			Payload: `{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":"hello"}],"stream":true}`, Upstream: reasoningTurn("grok-4.3", "GROKENC1")},
		{Name: "replay_other_caller_isolated", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "execute", CallerKey: "client-key-2",
			Headers: map[string]string{"X-Claude-Code-Session-Id": "sess-replay-1"},
			Payload: `{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello there"},{"role":"user","content":"again"}]}`, Upstream: textTurn("grok-4.3")},
		{Name: "replay_claude_turn2_injects", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "execute", CallerKey: "client-key-1",
			Headers: map[string]string{"X-Claude-Code-Session-Id": "sess-replay-1"},
			Payload: `{"model":"grok-4.3","max_tokens":100,"messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"hello there"},{"role":"user","content":"again"}]}`, Upstream: textTurn("grok-4.3")},
		{Name: "replay_responses_turn1_tool_call", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute", CallerKey: "client-key-1",
			Payload: `{"model":"grok-4.3","input":"weather?","prompt_cache_key":"pck-replay","tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]}`, Upstream: callTurn("grok-4.3", "lookup", `{}`)},
		{Name: "replay_websocket_previous_response_skipped", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute", CallerKey: "client-key-1", Websocket: true,
			Payload: `{"model":"grok-4.3","prompt_cache_key":"pck-replay","previous_response_id":"resp_prev","input":[{"type":"function_call_output","call_id":"call_1","output":"sunny"}]}`, Upstream: textTurn("grok-4.3")},
		{Name: "replay_responses_turn2_tool_output", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute", CallerKey: "client-key-1",
			Payload: `{"model":"grok-4.3","prompt_cache_key":"pck-replay","input":[{"role":"user","content":"weather?"},{"type":"function_call_output","call_id":"call_1","output":"sunny"}],"tools":[{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]}`, Upstream: textTurn("grok-4.3")},
		{Name: "replay_execution_session_without_caller", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream", ExecutionSession: "exec-replay-9",
			Payload: responsesHello, Upstream: reasoningTurn("grok-4.3", "GROKENC2")},
		{Name: "replay_execution_session_turn2", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute", ExecutionSession: "exec-replay-9",
			Payload: `{"model":"grok-4.3","input":[{"role":"user","content":"hello"},{"role":"user","content":"more"}]}`, Upstream: textTurn("grok-4.3")},

		// --- compact ---
		{Name: "compact_oauth_uses_official_api", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-4.3", Source: "openai-response", Op: "execute", Alt: "responses/compact",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"long"},{"type":"compaction_trigger"}],"previous_response_id":" resp_9 ","tools":[{"type":"image_generation"}],"tool_choice":{"type":"image_generation"},"max_output_tokens":10,"temperature":1,"top_p":0.5,"stop":"x","parallel_tool_calls":true,"instructions":"be"}`,
			Upstream: jsonReply(200, `{"id":"cmp_77","object":"response.compaction","created_at":1700000001,"model":"grok-4.3","output":[{"type":"compaction","encrypted_content":"opaque"}],"usage":{"input_tokens":9,"output_tokens":1,"total_tokens":10}}`)},
		{Name: "compact_stream_rejected", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream", Alt: "responses/compact",
			Payload: responsesHello},
		{Name: "compaction_trigger_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","instructions":"sys","input":[{"role":"user","content":"long"},{"type":"compaction_trigger"}],"reasoning":{"effort":"low"},"metadata":{"a":"b"}}`,
			Upstream: jsonReply(200, `{"id":"cmp_42","object":"response.compaction","created_at":1700000002,"completed_at":1700000003,"model":"grok-4.3","output":[{"type":"compaction","encrypted_content":"opaque"}],"usage":{"input_tokens":20,"output_tokens":2,"total_tokens":22}}`)},
		{Name: "compact_error", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute", Alt: "responses/compact",
			Payload: responsesHello, Upstream: jsonReply(400, `{"error":"bad compact"}`)},

		// --- errors ---
		{Name: "bad_credentials_403_becomes_401", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: jsonReply(403, `{"code":"Some(bad-credentials)","error":"The access token could not be validated"}`)},
		{Name: "free_usage_exhausted_429", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: responsesHello, Upstream: jsonReply(429, `{"code":"free-usage-exhausted","error":"You have used your included free usage"}`)},
		{Name: "plain_403_kept", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: responsesHello, Upstream: jsonReply(403, `{"error":"forbidden model"}`)},
		{Name: "server_error_500", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "stream",
			Payload: `{"model":"grok-4.3","messages":[{"role":"user","content":"hi"}]}`, Upstream: jsonReply(500, `{"error":"boom"}`)},

		// --- count tokens ---
		{Name: "count_tokens_responses", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "count",
			Payload: `{"model":"grok-4.3","instructions":"You are helpful.","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"What is the weather in Paris today?"},{"type":"input_image","image_url":"https://img.example/x.png"}]},{"type":"function_call","name":"lookup","arguments":{"city":"Paris"}},{"type":"function_call_output","call_id":"c","output":"sunny"},{"type":"reasoning","summary":[{"type":"summary_text","text":"thinking"}]}],"tools":[{"type":"function","name":"lookup","description":"Look up","parameters":{"type":"object","properties":{"city":{"type":"string"}}}},{"type":"web_search"}],"text":{"format":{"type":"json_schema","name":"out","schema":{"type":"object"}}}}`},
		{Name: "count_tokens_claude", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "claude", Op: "count",
			Payload: `{"model":"grok-4.3","system":"sys","messages":[{"role":"user","content":"count these tokens please"}]}`},
		{Name: "count_tokens_chat", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai", Op: "count",
			Payload: `{"model":"grok-4.3","messages":[{"role":"user","content":"count"}]}`},

		// --- images and videos ---
		{Name: "images_generations_api_key", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			Payload:  `{"model":"grok-imagine-image","prompt":"a cat","n":1,"response_format":"b64_json"}`,
			Upstream: jsonReply(200, `{"created":1700000000,"data":[{"b64_json":"AAAA"}],"usage":{"total_tokens":3}}`)},
		{Name: "images_edits_oauth_chat_proxy_refs", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-imagine-image", Source: "openai", Op: "images", RequestPath: "/v1/images/edits",
			Payload:  `{"prompt":"edit","image":{"image_url":"https://img.example/a.png"},"images":[{"image_url":{"url":"https://img.example/b.png"}},{"url":"https://img.example/c.png","image_url":"ignored"},{"type":"image_url","image_url":{"url":"https://keep.example/d.png"}}],"n":2.0,"big":12345678901234567890}`,
			Upstream: jsonReply(200, `{"data":[{"url":"https://out.example/1.png"}]}`)},
		{Name: "images_error", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-image", Source: "openai", Op: "images", RequestPath: "/v1/images/generations",
			Payload: `{"model":"grok-imagine-image","prompt":"x"}`, Upstream: jsonReply(429, `{"code":"free-usage-exhausted"}`)},
		{Name: "videos_create_idempotency", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/v1/videos/generations",
			Headers:  map[string]string{"Idempotency-Key": " idem-1 ", "X-Idempotency-Key": "other"},
			Payload:  `{"model":"grok-imagine-video","prompt":"a wave","reference_images":[{"image_url":"https://img.example/r.png"}]}`,
			Upstream: jsonReply(200, `{"request_id":"vid_1"}`)},
		{Name: "videos_edits_x_idempotency_header", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/openai/v1/videos/edits",
			Headers:  map[string]string{"X-Idempotency-Key": "xi-2"},
			Payload:  `{"model":"grok-imagine-video","prompt":"edit","video":{"url":"https://v.example/a.mp4"}}`,
			Upstream: jsonReply(200, `{"request_id":"vid_2"}`)},
		{Name: "videos_extensions", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/v1/videos/extensions",
			Payload: `{"model":"grok-imagine-video","request_id":"vid_1"}`, Upstream: jsonReply(200, `{"request_id":"vid_3"}`)},
		{Name: "videos_retrieve_by_request_id", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/v1/videos/:request_id",
			Headers: map[string]string{"Idempotency-Key": "ignored-for-get"},
			Payload: `{"model":"grok-imagine-video","request_id":"vid 1/a?b"}`, Upstream: jsonReply(200, `{"status":"done","video":{"url":"https://v.example/out.mp4"}}`)},
		{Name: "videos_default_generations", ConfigAuth: -1, Attributes: apiKey, Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/v1/videos",
			Payload: `{"model":"grok-imagine-video","prompt":"p"}`, Upstream: jsonReply(200, `{"request_id":"vid_4"}`)},
		{Name: "videos_error_bad_credentials", ConfigAuth: -1, Metadata: oauth(nil), Model: "grok-imagine-video", Source: "openai", Op: "videos", RequestPath: "/v1/videos/generations",
			Payload: `{"prompt":"p"}`, Upstream: jsonReply(403, `{"error":{"code":"bad-credentials","message":"nope"}}`)},

		// --- apply_patch (bridge pending in cpa-translate) ---
		{Name: "apply_patch_custom_tool_bridged", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: callTurn("grok-4.3", "apply_patch", `{"input":"*** Begin Patch\n*** End Patch"}`)},
		{Name: "apply_patch_custom_tool_bridged_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: callTurn("grok-4.3", "apply_patch", `{"input":"*** Begin Patch\n*** End Patch"}`)},
		{Name: "apply_patch_invalid_input_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: callTurn("grok-4.3", "apply_patch", `{"input":42}`)},
		{Name: "apply_patch_invalid_input_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: callTurn("grok-4.3", "apply_patch", `{"input":42}`)},
		{Name: "apply_patch_eof_without_completion_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: streamOf(created("grok-4.3"),
				`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":"","status":"in_progress"}}`,
				`{"type":"response.function_call_arguments.delta","sequence_number":2,"item_id":"fc_1","output_index":0,"delta":"{\"input\":"}`)},
		{Name: "apply_patch_eof_without_completion_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-4.3","input":"patch","tools":[` + patchTool + `]}`,
			Upstream: streamOf(created("grok-4.3"),
				`{"type":"response.output_item.added","sequence_number":1,"output_index":0,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"apply_patch","arguments":"","status":"in_progress"}}`)},
		{Name: "apply_patch_folded_namespace_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4", Source: "openai-response", Op: "execute",
			Payload: foldedPatchRequest(), Upstream: foldedPatchTurn()},
		{Name: "apply_patch_folded_namespace_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4", Source: "openai-response", Op: "stream",
			Payload: foldedPatchRequest(), Upstream: foldedPatchTurn()},
		{Name: "apply_patch_declared_history_to_claude", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"x"},{"type":"custom_tool_call","call_id":"p1","name":"apply_patch","input":"*** Begin Patch\n*** End Patch"},{"type":"custom_tool_call_output","call_id":"p1","output":"ok"}],"tools":[` + patchTool + `],"tool_choice":{"type":"custom","name":"apply_patch"}}`,
			Upstream: callTurn("grok-4.3", "apply_patch", `{"input":"*** Begin Patch\n*** End Patch"}`)},
		{Name: "apply_patch_invalid_history_input", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"type":"custom_tool_call","call_id":"p1","name":"apply_patch","input":42}]}`,
			Upstream: textTurn("grok-4.3")},
		{Name: "apply_patch_history_without_declaration", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload:  `{"model":"grok-4.3","input":[{"role":"user","content":"x"},{"type":"custom_tool_call","call_id":"p1","name":"apply_patch","input":"*** Begin Patch\n+<a & b>\n*** End Patch"},{"type":"custom_tool_call_output","call_id":"p1","output":"ok"}],"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{}}}]}`,
			Upstream: textTurn("grok-4.3")},
		// Usage: Go's HTTP paths observe usage on completed and incomplete only, so a
		// response.done carrying usage before a completed without it publishes nothing.
		{Name: "usage_done_ignored_execute", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "execute",
			Payload: `{"model":"grok-4.3","input":"hi"}`, Upstream: streamOf(created("grok-4.3"), `{"type":"response.done","sequence_number":1,"response":{"id":"resp_d","status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":7,"output_tokens":7,"total_tokens":14}}}`, `{"type":"response.completed","sequence_number":2,"response":{"id":"resp_d","object":"response","created_at":1700000000,"status":"completed","model":"grok-4.3","output":[]}}`)},
		{Name: "usage_done_ignored_stream", ConfigAuth: -1, Attributes: apiKey, Model: "grok-4.3", Source: "openai-response", Op: "stream",
			Payload: `{"model":"grok-4.3","input":"hi"}`, Upstream: streamOf(created("grok-4.3"), `{"type":"response.done","sequence_number":1,"response":{"id":"resp_d","status":"completed","model":"grok-4.3","output":[],"usage":{"input_tokens":7,"output_tokens":7,"total_tokens":14}}}`, `{"type":"response.completed","sequence_number":2,"response":{"id":"resp_d","object":"response","created_at":1700000000,"status":"completed","model":"grok-4.3","output":[]}}`)},
	}
	return s
}
