package main

import (
	"encoding/base64"
	"fmt"
	"sort"
	"strings"
)

// Upstream model used when a case does not choose one.
var defaultModels = map[string]string{
	"claude":       "claude-opus-4-6",
	"openai":       "gpt-test",
	"codex":        "gpt-5.3-codex",
	"gemini":       "gemini-2.5-pro",
	"antigravity":  "gemini-3-flash",
	"interactions": "gemini-2.5-pro",
}

// Models whose static capabilities differ: adaptive with max, adaptive without max
// (a non-Claude catalog entry), budget with a minimum, levels-only, none, and unknown.
var capabilityModels = []string{"claude-opus-4-6", "claude-sonnet-4-5-20250929", "kimi-k2.5", "gemini-2.5-pro", "gpt-5", "unknown-model", " claude-opus-4-6 ", "claude-opus-4-6(high)"}

func req(name, model, input string, stream bool) fixture {
	return fixture{Name: name, Path: "request", Model: model, Input: input, Stream: stream}
}

func streamCase(name, model string, lines ...string) fixture {
	return fixture{Name: name, Path: "stream", Model: model, Lines: lines}
}

func nonStream(name, model, input string) fixture {
	return fixture{Name: name, Path: "non_stream", Model: model, Input: input}
}

// matrix returns the hand-written cases for a pair: client-format request inputs, then
// upstream-format response inputs, then token counts.
func matrix(r registration, model string) []fixture {
	var out []fixture
	switch r.client {
	case "openai":
		out = append(out, openAIChatRequests(model)...)
		if r.upstream == "gemini" {
			out = append(out, openAIGeminiRequests(model)...)
		}
		if r.upstream == "codex" {
			out = append(out, openAICodexRequests(model)...)
		}
		if r.upstream == "claude" {
			for _, f := range openAIChatRequests(model) {
				if strings.HasPrefix(f.Name, "tools/") || strings.HasPrefix(f.Name, "reasoning/") {
					f.Name = "compat/" + f.Name
					f.Path = "request_compat"
					out = append(out, f)
				}
			}
			out = append(out, fixture{Name: "compat/reasoning-content", Path: "request_compat", Model: "deepseek-v4",
				Input: `{"messages":[{"role":"assistant","content":"answer","reasoning_content":"reason"},{"role":"assistant","content":"x","reasoning_content":"  "}]}`})
		}
	}
	switch r.client {
	case "gemini":
		out = append(out, geminiRequests(model)...)
	case "claude":
		out = append(out, claudeRequests(model)...)
		if r.upstream == "openai" || r.upstream == "gemini" {
			for _, f := range claudeRequests(model) {
				if strings.HasPrefix(f.Name, "messages/") || strings.HasPrefix(f.Name, "thinking/") {
					f.Name = "compat/" + f.Name
					f.Path = "request_compat"
					out = append(out, f)
				}
			}
		}
	}
	switch r.client {
	case "openai-response":
		out = append(out, responsesRequests(model)...)
		if r.upstream == "claude" {
			out = append(out, responsesClaudeRequests()...)
		}
	}
	switch r.upstream {
	case "codex":
		if r.client == "openai-response" {
			out = append(out, codexResponses()...)
		} else {
			out = append(out, codexEventStreams()...)
		}
	case "claude":
		out = append(out, claudeResponses()...)
		if r.client == "openai-response" {
			out = append(out, claudeToResponses()...)
		}
	case "gemini":
		out = append(out, geminiUpstreamResponses()...)
	case "openai":
		out = append(out, openAIResponses()...)
		if r.client == "claude" {
			out = append(out, openAIToClaudeResponses()...)
		}
	}
	if r.tokenCount != "" {
		for _, n := range []int64{0, 1, 123456789012} {
			out = append(out, fixture{Name: fmt.Sprintf("token-count/%d", n), Path: "token_count", Model: model, Count: n, Input: `{"upstream":"body"}`})
		}
	}
	return out
}

func openAIChatRequests(model string) []fixture {
	var out []fixture
	for _, effort := range []string{"none", "auto", "minimal", "low", "medium", "high", "xhigh", "max", "invalid", " HIGH ", ""} {
		for _, m := range capabilityModels {
			out = append(out, req("reasoning/"+m+"/"+effort, m, `{"reasoning_effort":"`+effort+`","messages":[{"role":"user","content":"hi"}]}`, false))
		}
	}
	for i, summary := range []string{
		`"include_reasoning":true`, `"include_reasoning":false`, `"reasoning":{"summary":"detailed"}`, `"reasoning":{"summary":null}`,
		`"reasoning":{"exclude":true}`, `"reasoning":{"enabled":true}`, `"extra_body":{"google":{"thinking_config":{"include_thoughts":true}}}`,
		`"reasoning_effort":"high","include_reasoning":false`, `"max_tokens":1024,"include_reasoning":true`, `"max_tokens":1025,"include_reasoning":true`,
		`"reasoning_effort":"none","include_reasoning":true`, `"reasoning_effort":"low","reasoning":{"summary":"concise"}`,
	} {
		for _, m := range []string{"claude-opus-4-6", "claude-sonnet-4-5-20250929", "kimi-k2.5", "unknown-model", "claude-sonnet-4-5-20250929(low)"} {
			out = append(out, req(fmt.Sprintf("summary/%d/%s", i, m), m, `{`+summary+`,"messages":[{"role":"user","content":"hi"}]}`, true))
		}
	}
	// Strings set individually stay raw when plain ASCII and go through encoding/json
	// (HTML-escaped) otherwise; marshaled string arrays are always escaped.
	for i, input := range []string{
		`{"messages":[{"role":"user","content":"a<b>&c"},{"role":"user","content":"é<"},{"role":"system","content":"\u2028 & \u2029"}],"stop":["<x>","plain"]}`,
		`{"messages":[{"role":"user","content":"tab\tq\"uote\\ <tag>"}],"stop":"a&b"}`,
		`{"messages":[{"role":"user","content":[{"type":"text","text":"<b>"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AAA<>"}},{"type":"file","file":{"file_data":"data:application/pdf;base64,QQ=="}}]}]}`,
		`{"tools":[{"type":"function","function":{"name":"émoji.tool<x>","description":"<d>&","parameters":{"type":"object","properties":{"q":{"description":"<&>\u2028","type":"string"}}}}}],"messages":[{"role":"user","content":"x"}]}`,
		`{"response_format":{"type":"json_schema","json_schema":{"name":"N<","description":" D& ","schema":{"type":"object","properties":{"a":{"type":"string","pattern":"<x>"}}}}},"messages":[{"role":"user","content":"x"}]}`,
		`{"response_format":{"type":"json_object"},"messages":[{"role":"system","content":"s"}]}`,
		`{"metadata":{"user_id":"é<user>"},"messages":[]}`,
	} {
		out = append(out, req(fmt.Sprintf("escaping/%d", i), model, input, false))
	}
	// gjson coercions: strings parse as integers only when all digits, floats format
	// without exponents, and Array() wraps a non-array.
	for i, input := range []string{
		`{"max_tokens":"12.75","top_p":"1e3","stop":[1e3,-0,1.5,true,null,{"a":1}],"messages":[]}`,
		`{"max_tokens":"1e3","stop":1e3,"messages":{"role":"user","content":"solo"}}`,
		`{"max_tokens":1e3,"top_p":0.1,"messages":[{"role":"user","content":"x"}]}`,
		`{"max_tokens":12.75,"max_completion_tokens":5,"top_p":"0.5","stop":{"a":1},"messages":[]}`,
		`{"max_completion_tokens":"77","top_p":1e-7,"stop":[],"messages":[]}`,
		`{"max_tokens":-3,"top_p":1e21,"stop":"","messages":[]}`,
		`{"max_tokens":9007199254740993,"top_p":-0,"messages":[]}`,
		`{"max_tokens":true,"top_p":true,"messages":[{"role":"user","content":["a","b"]}]}`,
		`{"max_tokens":null,"top_p":null,"stop":null,"messages":[{"role":"user","content":null},{"role":"user","content":1}]}`,
		`{"top_p":"1_0","max_tokens":"1_0","stop":["a""b"],"messages":[]}`,
		`{"top_p":"1__0","stop":[1e19,-1.5e0],"messages":[]}`,
	} {
		out = append(out, req(fmt.Sprintf("coercion/%d", i), model, input, false))
	}
	// Malformed bytes: invalid UTF-8 is copied raw, or marshaled to \ufffd when Go sets it
	// as a string; malformed JSON follows gjson's scanner.
	for i, input := range []string{
		"{\"messages\":[{\"role\":\"user\",\"content\":\"bad\xff\xfe\"}],\"stop\":[\"\xff\"]}",
		"{\"messages\":[{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"\xc3\"}]}],\"user\":\"u\xff\"}",
		"{\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"t\xff\",\"parameters\":{\"properties\":{\"k\xff\":{}}}}}],\"messages\":[]}",
		`{"messages":[{"role":"user","content":"unterminated`,
		`{"messages":[{"role":"user","content":"x"}],}`,
		`not json at all`, ``, `[]`, `null`, `{"messages":"text"}`,
		`{"messages":[{"role":"user","content":"\ud800 lone \udc00 \u00e9"}]}`,
		`{"tools":[{"type":"function","function":{"name":"f","parameters":{"properties":{"\ud800\u0061":{},"\u00e9<":{"enum":["\ud83d\ude00"]}},"required":["\ud800\u0061"],"allOf":[{"required":["\udc00"]}]}}}],"messages":[]}`,
		"{\"tools\":[{\"type\":\"function\",\"function\":{\"name\":\"f\",\"parameters\":{\"properties\":{\"k\xe2\x82\":{}},\"allOf\":[{\"required\":[\"\xe2\x82\"]}]}}}],\"messages\":[]}",
		` {"model" : "old", "messages":[{"role":"user","content":"x"}]} trailing {}`,
	} {
		out = append(out, req(fmt.Sprintf("malformed/%d", i), model, input, false))
	}
	// Claude user IDs: explicit values, then Go's seed fallbacks in order.
	for i, input := range []string{
		`{"metadata":{"user_id":"explicit"},"user":"u","messages":[]}`,
		`{"metadata":{"user_id":"  "},"user":"u2","messages":[]}`,
		`{"user":5,"prompt_cache_key":" pck ","messages":[]}`,
		`{"session_id":"s1","sessionId":"s2","messages":[]}`,
		`{"sessionId":"s2","conversation":{"id":"c1"},"messages":[]}`,
		`{"conversation":{"id":" "},"conversation_id":"c3","messages":[]}`,
		`{"conversation":"c-str","messages":[]}`,
		`{"conversation":5,"conversation_id":"c4","messages":[]}`,
		`{"messages":[{"role":"system","content":"s"},{"role":"USER","content":[{"type":"text","text":" a "},{"type":"image_url"},{"type":"text","text":"b"}]}]}`,
		`{"input":"  seed text ","messages":[]}`,
		`{"input":[{"role":"developer","content":"dev"},{"type":"message","content":[{"type":"input_text","text":"in"},{"type":"output_text","text":"out"}]}],"messages":[]}`,
		`{"contents":[{"role":"model","parts":[{"text":"m"}]},{"parts":[{"text":"thought","thought":true},{"text":" g1 "},{"text":"g2"}]}],"messages":[]}`,
		`{"model":" m ","instructions":"i","system":["s"],"systemInstruction":{"parts":[]},"system_instruction":null,"messages":[]}`,
		`{"instructions":"only-instructions","messages":[]}`,
		`{"messages":[{"role":"user","content":""}]}`,
	} {
		out = append(out, req(fmt.Sprintf("user-id/%d", i), model, input, false))
	}
	// Tool call and tool result states.
	for i, input := range []string{
		`{"messages":[{"role":"assistant","content":"c","tool_calls":[{"id":"call.1","type":"function","function":{"name":"a.b","arguments":"{\"x\":1}  "}},{"type":"function","function":{"name":"gen","arguments":"[]"}},{"id":"c3","type":"custom","function":{"name":"skip"}},{"id":"c4","type":"function","function":{"name":"","arguments":"not json"}}]},{"role":"tool","tool_call_id":"call.1","content":"r1"},{"role":"tool","tool_call_id":"call.1","content":"r1-dup"},{"role":"tool","content":null},{"role":"tool","tool_call_id":"c4","content":[{"type":"text","text":"t"},"plain",{"type":"image_url","image_url":{"url":"https://x/y.png"}},{"type":"other"}]},{"role":"tool","tool_call_id":"c5","content":{"type":"text","text":"obj"}},{"role":"tool","tool_call_id":"c6","content":[]},{"role":"tool","tool_call_id":"c7","content":[{"type":"other"}]},{"role":"tool","tool_call_id":"c8","content":5}]}`,
		`{"messages":[{"role":"user","content":"u1","cache_control":{"type":"ephemeral"}},{"role":"user","content":[{"type":"text","text":"u2","cache_control":{"type":"ephemeral","ttl":"1h"}}],"cache_control":{"type":"ephemeral"}},{"role":"assistant","content":"a","cache_control":{"type":"other"}},{"role":"assistant","content":[{"type":"text","text":"a2"}],"tool_calls":[{"id":"t","type":"function","function":{"name":"f","arguments":"{}"}}],"cache_control":{"type":"ephemeral"}},{"role":"tool","tool_call_id":"t","content":[{"type":"text","text":"r","cache_control":{"type":"ephemeral"}}]},{"role":"system","content":[{"type":"text","text":"s1"},{"type":"text","text":"s2"}],"cache_control":{"type":"ephemeral"}},{"role":"developer","content":"d","cache_control":{"type":"ephemeral"}},{"role":"other","content":"ignored"}]}`,
		`{"tools":[{"type":"function","function":{"name":"a","parameters":{"anyOf":[{"properties":{"x":{"type":"string"}}},{"type":"string"}],"oneOf":[{"type":["null","object"],"properties":{"y":{}}}],"allOf":[{"properties":{"z":{"default":1e+09}},"required":["z","z"]}],"required":["w"],"$defs":{"q":{"type":"string"}}},"strict":true,"cache_control":{"type":"ephemeral"}}},{"type":"function","function":{"name":"b","parametersJsonSchema":{"type":"object"}},"strict":false,"cache_control":{"type":"ephemeral"}},{"type":"function","function":{"name":"c"}},{"type":"web_search"}],"messages":[{"role":"user","content":"x"}]}`,
		`{"tools":[{"type":"function","function":{"name":"a","parameters":null}},{"type":"function","function":{"name":"b","parameters":[1]}},{"type":"function","function":{"name":"c","parameters":"str"}}],"messages":[]}`,
		`{"tools":[{"type":"web_search"}],"tool_choice":"auto","parallel_tool_calls":false,"messages":[]}`,
		`{"tools":[],"parallel_tool_calls":false,"messages":[]}`,
	} {
		out = append(out, req(fmt.Sprintf("tools/%d", i), model, input, i%2 == 0))
	}
	for _, choice := range []string{`"auto"`, `"none"`, `"required"`, `"any"`, `{"type":"any"}`, `{"type":"none"}`, `{"type":"function","function":{"name":"a.b"}}`,
		`{"type":"function","name":"direct"}`, `{"type":"function"}`, `null`, `5`, `[{"type":"auto"}]`,
		`{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"a.b"}}]}}`,
		`{"type":"allowed_tools","tools":[{"name":"missing"}],"mode":"REQUIRED"}`,
		`{"type":"allowed_tools","allowed_tools":{"tools":[{"name":" a_b "}]}}`} {
		for _, parallel := range []string{`"parallel_tool_calls":false,`, ``} {
			out = append(out, req("tools/choice/"+choice+"/"+parallel, model, `{"tool_choice":`+choice+`,`+parallel+`"tools":[{"type":"function","function":{"name":"a.b","parameters":{"allOf":[{"properties":{"z":{"default":1e+09,"description":"<&>"}},"required":["z"]},{"properties":{"a":{"type":"string"}},"required":["a","z"]}]}}}],"messages":[{"role":"user","content":"test"}]}`, true))
		}
	}
	// Bodies an OpenAI-compatible passthrough must keep byte for byte.
	for i, input := range []string{
		`{ "model" : "old", "number":1e+09, "model":"duplicate", "opaque": {"z":9007199254740993,"a":"\u0061"} }`,
		"{ \"model\":\"gpt-test\", \"opaque\":1e+09 }\n", `{ "opaque":1e+09 }`, `{ }`, `{"model":null}`, `{"model":9}`,
		`{"model":"escaped\u0020model"}`, `{`, `{"opaque":1`, `{"model":`, `{"model":"old"`, ` { } trailing`,
		` {"nested":{"braces":"}\\\"["},"array":[1,{"n":1e+09}]} trailing {}`, "{\"model\":\"x\",\"raw\":\"\xff\"}",
	} {
		out = append(out, req(fmt.Sprintf("passthrough/%d", i), model, input, true))
	}
	return out
}

func claudeResponses() []fixture {
	const model = "requested-model"
	start := `data: {"type":"message_start","message":{"id":"msg-x","model":"upstream-model","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}}`
	out := []fixture{
		streamCase("parallel/out-of-order", model, start,
			`data: {"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call-a","name":"first","input":{"ignored":true}}}`,
			`data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-b","name":"second"}}`,
			`data: {"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{\"n\": "}}`,
			`data: {"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"1e+09}"}}`,
			`data: {"type":"content_block_stop","index":2}`, `data: {"type":"content_block_stop","index":7}`,
			`data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4,"cache_read_input_tokens":0}}`,
			`data: {"type":"message_stop"}`, `data: {"type":"message_stop"}`),
		streamCase("text-thinking-escaping", model, "event: message_start", start, "",
			`data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}`,
			`data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"<think> & é"}}`,
			`data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}`,
			`data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"plain <b>"}}`,
			`data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"\u2028 \"q\""}}`,
			`data:{"type":"content_block_delta","index":1,"delta":{"type":"text_delta"}}`,
			"data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"bad\xff\"}}",
			`data: {"type":"message_delta","delta":{},"usage":{"output_tokens":"12.5"}}`, `data: {"type":"message_stop"}`),
		streamCase("error-and-empty", "m", `event: ping`, `data: {"type":"ping"}`, `data: {"type":"error","error":{"type":"overloaded_error","message":"busy <now>"}}`,
			`data: {"type":"error"}`, `data: {"type":"content_block_delta","delta":{"type":"text_delta","text":""}}`, `data: not json`, `: comment`, `data: [DONE]`),
		streamCase("no-usage", "", `data: {"type":"message_start"}`, `data: {"type":"message_start","message":{}}`, `data: {"type":"message_stop"}`),
	}
	for _, reason := range []string{"end_turn", "max_tokens", "refusal", "sensitive", "stop_sequence", "tool_use", "unknown"} {
		out = append(out, streamCase("finish/"+reason, "m",
			`data: {"type":"message_delta","delta":{"stop_reason":"`+reason+`"},"usage":{"input_tokens":13,"output_tokens":4,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}`,
			`data: {"type":"message_stop"}`))
		out = append(out, nonStream("non-stream/finish/"+reason, "", `data: {"type":"message_delta","delta":{"stop_reason":"`+reason+`"}}`))
	}
	for _, f := range out {
		if f.Path == "stream" {
			out = append(out, nonStream("buffered/"+f.Name, f.Model, strings.Join(f.Lines, "\n")+"\n"))
		}
	}
	out = append(out,
		nonStream("buffered/crlf-and-gaps", "", "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"x\"}}\r\n\r\n data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"skipped\"}}\ndata: {\"type\":\"content_block_start\",\"index\":-1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"neg\",\"name\":\"n\"}}\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t3\",\"name\":\"n3\"}}\ndata: {\"type\":\"content_block_stop\",\"index\":3}\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"\"}}"),
		nonStream("buffered/empty", "", ""),
	)
	return out
}

func openAIResponses() []fixture {
	return []fixture{
		streamCase("passthrough/done", "m", `data: {"id":"x","choices":[]}`, "data: [DONE]", `data: {"choices":[],"cost":"0"}`),
		streamCase("passthrough/bare", "m", `{"id":"y"}`, ` data: {"n":1}`, "", "event: x"),
		streamCase("passthrough/bare-done", "m", "[DONE]", `{"id":"after"}`),
		streamCase("passthrough/timestamp", "m", `data: {"created":123,"n":1e+09}`, ` {"created":456,"n":9007199254740993} `, "data:   [DONE]  "),
		streamCase("passthrough/bytes", "m", "data: {\"x\":\"\xff\xfe\"}", "data: \xff"),
		nonStream("passthrough/json", "m", ` {"id":"opaque","created":123,"usage":{"n":1e+09}} `),
		nonStream("passthrough/invalid", "m", `invalid`),
		nonStream("passthrough/bytes", "m", "{\"x\":\"\xff\"}"),
	}
}

func responsesRequests(model string) []fixture {
	var out []fixture
	for i, input := range []string{
		`{"model":"m","input":"plain <b> & é","stream":false,"store":true,"parallel_tool_calls":false}`,
		`{"input":[{"role":"system","content":[{"type":"input_text","text":"sys <x>"}]},  {"type":"message", "role":"user","content":"u\u2028"}],"stream":"yes","store":null}`,
		`{"input":[{"role":"system","content":"s"},{"role":"user","content":"x",}],"include":["reasoning.encrypted_content"],"stream":true,"store":false,"parallel_tool_calls":true}`,
		`{"input":[{"role":"SYSTEM","content":"s"},"text",5],"include":["a","reasoning.encrypted_content"]}`,
		`{"input":{"role":"system"},"include":"reasoning.encrypted_content"}`,
		`{"input":[],"include":[],"max_output_tokens":5,"max_completion_tokens":6,"temperature":0.2,"top_p":1,"truncation":"auto","prompt_cache_options":{},"prompt_cache_retention":"24h","user":"u","context_management":[{"type":"compaction"}]}`,
		`{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"a","prompt_cache_breakpoint":{"type":"ephemeral"}},{"type":"input_text","text":"b"}],"prompt_cache_breakpoint":true},{"type":"function_call_output","call_id":"c","output":[{"type":"input_text","text":"o","prompt_cache_breakpoint":1}]},{"type":"function_call","call_id":"c2","name":"f","arguments":"  "},{"type":"function_call","call_id":"c3","name":"f","arguments":""},{"type":"function_call","call_id":"c4","name":"f","arguments":5}]}`,
		`{"input":"x","note":"mentions \"prompt_cache_breakpoint\" only in text"}`,
		`{"input":"x","tools":[{"type":"web_search_preview"},{"type":"web_search_preview_2025_03_11","search_context_size":"low"},{"type":"function","name":"f"}],"tool_choice":{"type":"web_search_preview","tools":[{"type":"web_search_preview"}]}}`,
		`{"input":"x","tools":[{"type":"function","name":"f"}],"tool_choice":"auto"}`,
		`{"input":"x","reasoning":{"effort":"high","summary":"detailed"}}`,
		`{"input":"x","reasoning":{"generate_summary":"concise"}}`,
		`{"input":"x","reasoning":{"summary":null}}`,
		"{\"input\":\"bad\xff\",\"instructions\":\"\xfe\"}",
		"{\"input\":[{\"role\":\"system\",\"content\":\"\xff\"}]}",
		`not json`, ``, `[]`, `{"input":"x"} trailing`,
	} {
		for _, tier := range []string{"", `"priority"`, `"fast"`, `" FAST "`, `"ultrafast"`, `"Ultrafast"`, `"flex"`, `5`, `null`} {
			in := input
			if tier != "" {
				if !strings.HasPrefix(strings.TrimSpace(in), "{") || !strings.Contains(in, `"input"`) {
					continue
				}
				in = strings.Replace(in, `{`, `{"service_tier":`+tier+`,`, 1)
			}
			out = append(out, req(fmt.Sprintf("responses/%d/tier%s", i, tier), model, in, i%2 == 0))
		}
	}
	return out
}

func codexResponses() []fixture {
	const model = "gpt-5.3-codex"
	created := `data: {"type":"response.created","sequence_number":0,"response":{"id":"resp_1","status":"in_progress"}}`
	out := []fixture{
		streamCase("events/basic", model, "event: response.created", created, "", "event: response.in_progress",
			`data: {"type":"response.in_progress","response":{"id":"resp_1","model":"upstream"}}`, "",
			`data:{"type":"response.output_text.delta","delta":"<b> é"}`, `data: {"type":"response.completed","response":{"id":"resp_1"}}`, "data: [DONE]"),
		streamCase("events/no-model", "", created, `{"type":"response.in_progress","response":{}}`, `data: {"type":"response.created"}`,
			`data: {"type":"response.created","response":"str"}`, "data: not json", ": keepalive",
			"data: {\"type\":\"response.created\",\"response\":{\"id\":\"\xff\"}}"),
	}
	for i, orig := range []string{`{"model":"client-model"}`, `{"model":"  "}`, `{"request":{"model":"nested"}}`, `invalid`, ``} {
		f := streamCase(fmt.Sprintf("events/original/%d", i), model, created)
		f.Original = orig
		f.Translated = `{"model":"translated-model"}`
		out = append(out, f)
		g := f
		g.Name += "/no-translated"
		g.Translated = ""
		out = append(out, g)
	}
	for i, body := range []string{
		`{"type":"response.completed","response":{"id":"r","output":[{"type":"message"}],"usage":{"input_tokens":1}}}`,
		`{"type":"response.incomplete","response":{"id":"r","status":"incomplete"}}`,
		`{"type":"response.completed"}`,
		`{"id":"r","output":[],"status":"completed"}`,
		`{"id":"r","output":{}}`,
		`{"type":"response.failed","response":{"id":"r"}}`,
		` {"type":"response.completed","response": {"a" : 1} } `,
		`invalid`, ``,
	} {
		out = append(out, nonStream(fmt.Sprintf("non-stream/%d", i), model, body))
	}
	return out
}

const responsesTools = `"tools":[{"type":"function","name":"lookup","description":"<d>","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},{"type":"custom","name":"freeform","description":"raw text"},{"type":"namespace","name":"mcp_srv","tools":[{"type":"function","name":"read","parameters":{}},{"name":"bad name.with/chars"}]},{"type":"web_search","max_uses":3,"filters":{"allowed_domains":["a.com"]},"user_location":{"type":"approximate"}},{"type":"web_search","external_web_access":false,"name":"offline"},{"type":"image_generation"},{"type":"code_interpreter"},{"type":"function","name":"lookup","description":"duplicate loses"},{"type":"mystery","name":"opaque_tool","x":1}]`

func responsesClaudeRequests() []fixture {
	var out []fixture
	models := []string{"claude-opus-4-6", "claude-sonnet-4-5-20250929", "claude-fable-5", "kimi-k2.5", "claude-sonnet-4-6"}
	for i, input := range []string{
		`{` + responsesTools + `,"tool_choice":{"type":"function","name":"read","namespace":"mcp_srv"},"input":"hi"}`,
		`{` + responsesTools + `,"tool_choice":"required","input":[{"type":"additional_tools","tools":[{"type":"function","name":"extra"},{"type":"function","name":"lookup"}]},{"role":"user","content":"x"}]}`,
		`{"tools":[{"type":"custom","name":"apply_patch","description":"Patch. This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"definition":"start: *** Environment ID: x"}}],"tool_choice":{"type":"custom","custom":{"name":"apply_patch"}},"input":"x"}`,
		`{"instructions":"be <terse>","input":[{"role":"system","content":"sys"},{"role":"developer","content":[{"type":"input_text","text":"dev","cache_control":{"type":"ephemeral"}},{"type":"input_image"},{"type":""}],"cache_control":{"type":"ephemeral"}},{"role":"user","content":[{"type":"input_text","text":"q","annotations":[{"encrypted_index":"e1","url":"u"},{"url":"no-index"}]},{"type":"input_image","image_url":"data:image/png;base64,QUJD"},{"type":"input_image","url":"https://x/y.png"},{"type":"input_image","image_url":"data:;base64,"},{"type":"input_file","file_data":"data:application/pdf;base64,UERG"},{"type":"input_file","file_data":"raw"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a"},{"type":"refusal","refusal":"no"}]},{"role":"tool","content":"t"}],"text":{"format":{"type":"json_schema","name":"S","schema":{"type":"object"}}}}`,
		`{"input":[{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{\"q\":1}"},{"type":"function_call","call_id":"c2","name":"read","namespace":"mcp_srv","arguments":"[]"},{"type":"custom_tool_call","call_id":"c3","name":"freeform","input":"raw <x>"},{"type":"function_call_output","call_id":"c1","output":"r1"},{"type":"function_call_output","call_id":"c1","output":"dup"},{"type":"function_call_output","output":[{"type":"input_text","text":"no id"}]},{"type":"custom_tool_call_output","call_id":"c3","output":[{"type":"input_text","text":"a"},{"type":"input_image","image_url":"https://i"}]},{"type":"function_call_output","call_id":"orphan","output":"  "},{"role":"user","content":"next"}],` + responsesTools + `}`,
		`{"input":[{"role":"user","content":"q"},{"type":"reasoning","summary":[{"type":"summary_text","text":"think"}],"encrypted_content":"not-a-signature"},{"type":"reasoning","encrypted_content":"claude-redacted-thinking: REDACTED "},{"type":"reasoning","encrypted_content":"claude-redacted-thinking:"},{"type":"web_search_call","id":"ws_srvtoolu_abc.def","action":{"queries":["q2"]},"results":[{"type":"web_search_result","url":"u","encrypted_content":"enc"},{"url":"skip"},{"type":"web_search_tool_result_error","error_code":"x"}]},{"type":"function_call","call_id":"c9","name":"lookup","arguments":""},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}],` + responsesTools + `}`,
		`{"input":[{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"secret <b>"},{"type":"input_text","text":"t"}]},{"type":"function_call","call_id":"x1","name":"lookup"}],"max_output_tokens":999999,"service_tier":"priority","reasoning":{"effort":"xhigh","summary":"auto"}}`,
		`{"input":[{"role":"user","content":"a"},{"type":"function_call_output","name":"lookup","output":"by name"},{"type":"function_call","call_id":"p1","name":"lookup"},{"type":"function_call","call_id":"p2","name":"read"},{"type":"function_call_output","name":"read","output":"r"},{"type":"function_call_output","output":"o"}],"max_output_tokens":null}`,
		`{"input":[{"role":"assistant","content":"prefill"}],"reasoning":{"effort":"none"}}`,
		`{"input":[{"role":"user","content":"x"},{"type":"reasoning","summary":[]}]}`,
	} {
		for _, m := range models {
			out = append(out, req(fmt.Sprintf("responses-claude/%d/%s", i, m), m, input, i%2 == 1))
		}
	}
	return out
}

func claudeToResponses() []fixture {
	request := `{"model":"client-model","instructions":"inst","max_output_tokens":5,"max_tool_calls":2,"parallel_tool_calls":true,"previous_response_id":"p","prompt_cache_key":"k","reasoning":{"summary":"auto","effort":"high","effort":"low"},"safety_identifier":"s","service_tier":"auto","store":false,"temperature":0.5,"text":{"format":{"type":"text"}},"tool_choice":"auto","top_logprobs":1,"top_p":1e0,"truncation":"auto","user":"u<","metadata":{"b":1,"a":"<&>"},` + responsesTools + `}`
	start := `data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":10,"cache_read_input_tokens":3,"cache_creation_input_tokens":2,"output_tokens":1}}}`
	cases := [][]string{
		{start, `data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}`, `data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan <x> é"}}`, `data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG"}}`, `data: {"type":"content_block_stop","index":0}`,
			`data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}`, `data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hello <b>"}}`, `data: {"type":"content_block_delta","index":1,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","url":"u","encrypted_index":"e","cited_text":"<c>","n":1.50}}}`, `data: {"type":"content_block_stop","index":1}`,
			`data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"lookup","input":{}}}`, `data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{"q":"}}`, `data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":""<x>"}"}}`, `data: {"type":"content_block_stop","index":2}`,
			`data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}`, `data: {"type":"message_stop"}`, `data: {"type":"message_stop"}`},
		{start, `data: {"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_9","name":"web_search","input":{}}}`, `data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{"query":" weather "}"}}`, `data: {"type":"content_block_stop","index":0}`,
			`data: {"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_9","content":[{"type":"web_search_result","url":"https://w","title":"W"},{"type":"other"}]}}`, `data: {"type":"content_block_stop","index":1}`,
			`data: {"type":"content_block_start","index":2,"content_block":{"type":"server_tool_use","id":"srvtoolu_x","name":"code_execution"}}`,
			`data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_c","name":"freeform"}}`, `data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{"input":"raw\ntext \u00e9"}"}}`, `data: {"type":"content_block_stop","index":3}`,
			`data: {"type":"content_block_start","index":4,"content_block":{"type":"tool_use","id":"toolu_n","name":"mcp_srv__read"}}`, `data: {"type":"content_block_stop","index":4}`,
			`data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}`, `data: {"type":"message_stop"}`},
		{start, `data: {"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"RED"}}`, `data: {"type":"content_block_stop","index":0}`,
			`data: {"type":"content_block_delta","index":5,"delta":{"type":"citations_delta","citation":{"url":"early"}}}`, `data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}`, `data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"a"}}`,
			`data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}`, `data: {"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"b"}}`, `data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"","name":""}}`, `data: {"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{"input":"trunc"}}`,
			`data: {"type":"content_block_delta","index":9,"delta":{"type":"input_json_delta","partial_json":"orphan"}}`, `data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}`, `event: message_stop`, `data: {"type":"message_stop"}`},
		{`data: {"type":"message_start","message":{"id":"m2"}}`, `data: {"type":"ping"}`, `data: {"type":"message_stop"}`, `data: {"type":"message_start","message":{"id":"after"}}`},
	}
	var out []fixture
	for i, lines := range cases {
		for j, orig := range []string{request, ``, `{"model":"only-model"}`} {
			f := streamCase(fmt.Sprintf("claude-responses/%d/%d", i, j), "upstream-model", lines...)
			f.Original = orig
			out = append(out, f)
			n := nonStream(fmt.Sprintf("claude-responses/buffered/%d/%d", i, j), "upstream-model", strings.Join(lines, "\n"))
			n.Original = orig
			out = append(out, n)
		}
	}
	return out
}

// gptSignature is a structurally valid GPT reasoning signature (Fernet-like envelope).
func gptSignature() string {
	raw := make([]byte, 1+8+16+16+32)
	raw[0] = 0x80
	raw[8] = 1
	for i := 9; i < len(raw); i++ {
		raw[i] = byte(i)
	}
	return base64.URLEncoding.EncodeToString(raw)
}

// claudeRequests are Claude Messages client bodies: thinking configs, system shapes,
// message and tool states, schemas, tool choices, coercions, escaping and malformed bytes.
func claudeRequests(model string) []fixture {
	var out []fixture
	for i, thinking := range []string{
		`"thinking":{"type":"enabled","budget_tokens":-5}`, `"thinking":{"type":"enabled","budget_tokens":-1}`,
		`"thinking":{"type":"enabled","budget_tokens":0}`, `"thinking":{"type":"enabled","budget_tokens":1}`,
		`"thinking":{"type":"enabled","budget_tokens":512}`, `"thinking":{"type":"enabled","budget_tokens":513}`,
		`"thinking":{"type":"enabled","budget_tokens":1024}`, `"thinking":{"type":"enabled","budget_tokens":1025}`,
		`"thinking":{"type":"enabled","budget_tokens":8192}`, `"thinking":{"type":"enabled","budget_tokens":8193}`,
		`"thinking":{"type":"enabled","budget_tokens":24576}`, `"thinking":{"type":"enabled","budget_tokens":24577}`,
		`"thinking":{"type":"enabled","budget_tokens":"12.75"}`, `"thinking":{"type":"enabled","budget_tokens":1e3}`,
		`"thinking":{"type":"enabled","budget_tokens":null}`,
		`"thinking":{"type":"enabled"},"output_config":{"effort":" HIGH "}`, `"thinking":{"type":"enabled"},"output_config":{"effort":"  "}`,
		`"thinking":{"type":"enabled"},"output_config":{"effort":5}`, `"thinking":{"type":"enabled"}`,
		`"thinking":{"type":"enabled","budget_tokens":2000},"output_config":{"effort":"max"}`,
		`"thinking":{"type":"adaptive"}`, `"thinking":{"type":"adaptive"},"output_config":{"effort":"Max"}`,
		`"thinking":{"type":"auto"},"output_config":{"effort":"\u00c9LEV\u00c9"}`, `"thinking":{"type":"adaptive"},"output_config":{"effort":""}`,
		`"thinking":{"type":"disabled"}`, `"thinking":{"type":"unknown"}`, `"thinking":{"budget_tokens":100}`,
		`"thinking":"enabled"`, `"thinking":null`, `"thinking":{"type":"Enabled","budget_tokens":100}`,
		"\"thinking\":{\"type\":\"enabled\"},\"output_config\":{\"effort\":\"H\xffIGH\"}",
	} {
		for _, m := range []string{"gpt-5", "gpt-test", "kimi-k2.5", "claude-opus-4-6", "unknown-model", "gemini-2.5-pro", "gemini-3-pro-preview"} {
			out = append(out, req(fmt.Sprintf("thinking/%d/%s", i, m), m, `{`+thinking+`,"max_tokens":100,"messages":[{"role":"user","content":"hi"}]}`, i%2 == 0))
		}
	}
	sig := gptSignature()
	for i, input := range []string{
		`{"system":"plain system","messages":[{"role":"user","content":"hi"}]}`,
		`{"system":"  x-anthropic-billing-header: cc_version=1","messages":[{"role":"user","content":"hi"}]}`,
		`{"system":"","messages":[]}`,
		`{"system":[{"type":"text","text":"x-anthropic-billing-header: a"},{"type":"text","text":"  "},{"type":"text","text":"keep <me> & é"},{"type":"image","source":{"type":"base64","data":"QQ=="}},{"type":"image","source":{"type":"url","url":"https://i/x.png"}},{"type":"image","url":"https://fallback"},{"type":"image","source":{"type":"base64","media_type":"image/png"}},{"type":"other","text":"skip"}],"messages":[]}`,
		`{"system":{"type":"text","text":"object system"},"messages":[]}`,
		`{"system":5,"messages":[{"role":"user","content":"x"}]}`,
		`{"messages":[{"role":"user","content":[{"type":"text","text":"Hello"}]},{"role":"system","content":"mid rule"},{"role":"assistant","content":[{"type":"text","text":"Hi"}]},{"role":"system","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"},{"type":"text","text":"x-anthropic-billing-header: z"}]},{"role":"system","content":"   "},{"role":"system","content":5}]}`,
		`{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":[{"type":"text","text":"calling"},{"type":"tool_use","id":"t1","name":"Read","input":{"path":"<a>"}},{"type":"tool_use","id":"t2","name":"Write","input":{"b":1e3,"a":"\u00e9"}}]},{"role":"system","content":"deferred reminder"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"second"},{"type":"text","text":"after"},{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"first"},"str",{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"/9j/"}},{"type":"image"},{"other":1},{"text":"loose"}]}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"A"},{"type":"tool_use","name":"noid","input":[1]},{"type":"tool_use","id":"t3","input":"str"}]},{"role":"system","content":"r1"},{"role":"user","content":[{"type":"text","text":"no results"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":{"type":"image","source":{"type":"url","url":"https://img"}}}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"A","input":{}},{"type":"tool_use","id":"b","name":"B","input":{}}]},{"role":"system","content":"held"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"image","source":{"type":"base64","data":"QQ=="}}]},{"type":"tool_result","tool_use_id":"b","content":{"text":"obj text"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"zz","content":[]},{"type":"tool_result","tool_use_id":"yy","content":{"x":1}},{"type":"tool_result","tool_use_id":"ww"},{"type":"tool_result","tool_use_id":"vv","content":5},{"type":"tool_result","tool_use_id":"uu","content":[{"type":"image"}]}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"gpt <state>","signature":"` + sig + `"},{"type":"thinking","thinking":"claude state","signature":"claude#EjQ="},{"type":"thinking","thinking":"no sig"},{"type":"thinking","thinking":"  ","signature":"` + sig + `"},{"type":"thinking","text":"text field","signature":"` + sig + `"},{"type":"thinking","thinking":{"text":"nested"},"signature":"` + sig + `"},{"type":"redacted_thinking","data":"x"}]},{"role":"user","content":[{"type":"thinking","thinking":"user thinking","signature":"` + sig + `"},{"type":"tool_use","id":"u1","name":"inject"}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"only reasoning","signature":"` + sig + `"}]},{"role":"assistant","content":[{"type":"text","text":"  "}]},{"role":"assistant","content":[]},{"role":"developer","content":[{"type":"text","text":"dev"}]},{"role":"user","content":null},{"role":"user"},{"role":"user","content":{"type":"text","text":"obj"}},{"role":"other","content":"s"}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"x1","name":"A","input":{}}]},{"role":"user","content":"string reply"},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x1","content":"late"}]},{"role":"assistant","content":[{"type":"tool_use","id":"x1","name":"A","input":{}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x1","content":"dup"}]}]}`,
		`{"messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"early","content":"before call"}]},{"role":"assistant","content":[{"type":"tool_use","id":"early","name":"E","input":{}},{"type":"text","text":"t"}]},{"role":"user","content":[{"type":"text","text":"x"}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"p","name":"P","input":{}},{"type":"tool_use","id":"q","name":"Q","input":{}}]},{"role":"user","content":[{"type":"text","text":"interject"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"q","content":"Q"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"p","content":"P"}]}]}`,
	} {
		out = append(out, req(fmt.Sprintf("messages/%d", i), model, input, i%2 == 1))
	}
	for i, schema := range []string{
		`"input_schema":{"type":"object"}`, `"input_schema":{"type":"object","properties":{"a":true,"b":false}}`,
		`"input_schema":null`, ``, `"input_schema":true`, `"input_schema":false`, `"input_schema":"str<"`, `"input_schema":1e3`, `"input_schema":-0`,
		`"input_schema":[true,{"type":"object"}]`,
		`"input_schema":{"type":"object","properties":{"s":{"type":"string","pattern":"\\p{L}+"},"t":{"type":"string","pattern":"\\\\p{L}"},"u":{"pattern":"\\0"},"v":{"pattern":"a\\P{Lu}"},"w":{"pattern":5}},"patternProperties":{"^\\p{L}$":{"type":"string"},"^x$":true,"^\\d$":{"type":"object"}}}`,
		`"input_schema":{"type":"object","properties":{"list":{"type":"array","items":true,"prefixItems":[true,false,{"type":"object"}],"contains":{"type":"object"}}},"additionalProperties":true,"propertyNames":true,"unevaluatedProperties":false,"anyOf":[true,{"type":"object","properties":{"z":{"type":"object"}}}],"not":{"type":"object"},"if":true,"then":{"type":"object"},"else":false,"$defs":{"d":{"type":"object"},"e":true},"definitions":{"f":{"type":"object"}},"dependentSchemas":{"g":true},"dependencies":{"h":["x"],"i":{"type":"object"}}}`,
		`"input_schema":{"type":"object","type":"string","properties":{"dup":1},"properties":{"other":2},"description":"<b>&\u2028","n":1.50,"big":1e400}`,
		`"input_schema":{"type":"object","properties":{"big":1e21,"small":1e-7,"neg":-0.0,"int":9007199254740993}}`,
		`"input_schema":{"type":["object","null"],"additionalProperties":{"type":"object"}}`,
		"\"input_schema\":{\"type\":\"object\",\"properties\":{\"k\xff\":{\"description\":\"\xfe\"}}}",
	} {
		comma := ""
		if schema != "" {
			comma = ","
		}
		out = append(out, req(fmt.Sprintf("tools/schema/%d", i), model, `{"tools":[{"name":"t<1>","description":"d&"`+comma+schema+`}],"messages":[{"role":"user","content":"x"}]}`, false))
	}
	for i, input := range []string{
		`{"tools":[{"name":"a"},{"description":"no name"},{"type":"web_search_20250305","name":"web_search"}],"messages":[]}`,
		`{"tools":[],"messages":[]}`, `{"tools":{"name":"obj"},"messages":[]}`,
	} {
		out = append(out, req(fmt.Sprintf("tools/list/%d", i), model, input, false))
	}
	for _, choice := range []string{`{"type":"auto"}`, `{"type":"any"}`, `{"type":"none"}`, `{"type":"tool","name":"Read"}`, `{"type":"tool"}`, `{"type":"tool","name":""}`,
		`{"type":"other"}`, `{}`, `"auto"`, `"any"`, `"tool"`, `"bogus"`, `5`, `null`, `[]`, `{"type":"auto","disable_parallel_tool_use":true}`,
		`{"type":"any","disable_parallel_tool_use":"true"}`, `{"type":"tool","name":"R<x>","disable_parallel_tool_use":false}`} {
		out = append(out, req("tools/choice/"+choice, model, `{"tool_choice":`+choice+`,"tools":[{"name":"Read","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"x"}]}`, true))
	}
	for i, input := range []string{
		`{"max_tokens":"12.75","temperature":"1e3","stop_sequences":[1e3,-0,1.5,true,null,{"a":1},"<x>"],"messages":[]}`,
		`{"max_tokens":1e3,"top_p":0.1,"stop_sequences":"single","messages":[]}`,
		`{"max_tokens":-3,"temperature":0,"top_p":0.5,"stop_sequences":[],"messages":[]}`,
		`{"max_tokens":9007199254740993,"top_p":1e21,"messages":[]}`,
		`{"max_tokens":true,"temperature":null,"top_p":1e-7,"messages":[]}`,
		`{"max_tokens":null,"stop_sequences":null,"user":5,"messages":[]}`,
		`{"user":{"id":"u"},"messages":[]}`, `{"user":"plain","messages":[]}`, `{"user":"<u>&é","messages":[]}`,
		`{"metadata":{"user_id":"meta"},"messages":[]}`,
	} {
		out = append(out, req(fmt.Sprintf("coercion/%d", i), model, input, i%2 == 0))
	}
	for i, input := range []string{
		`{"messages":[{"role":"user","content":"a<b>&c \u2028 \u2029"},{"role":"assistant","content":"plain"}]}`,
		`{"messages":[{"role":"user","content":[{"type":"text","text":"tab\tq\"uote\\ <tag>"},{"type":"text","text":"\u00e9"}]}]}`,
		"{\"messages\":[{\"role\":\"user\",\"content\":\"bad\xff\xfe\"},{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"\xc3\"}]}],\"stop_sequences\":[\"\xff\"],\"user\":\"u\xff\"}",
		"{\"messages\":[{\"role\":\"assistant\",\"content\":[{\"type\":\"tool_use\",\"id\":\"i\xff\",\"name\":\"n\xfe\",\"input\":{\"k\":\"\xff<\"}}]},{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"i\xff\",\"content\":\"r\xff\"}]}]}",
		`{"messages":[{"role":"user","content":"unterminated`, `{"messages":[{"role":"user","content":"x"}],}`, `not json`, ``, `[]`, `null`,
		`{"messages":"text"}`, `{"messages":[{"role":"user","content":"\ud800 lone \udc00"}]}`,
		` {"model" : "old", "messages":[{"role":"user","content":"x"}]} trailing {}`,
	} {
		out = append(out, req(fmt.Sprintf("malformed/%d", i), model, input, false))
	}
	return out
}

// openAIToClaudeResponses are OpenAI Chat Completions upstream chunks and bodies for a
// Claude client: block ordering, interleaving, tool-call states, finish reasons, usage.
func openAIToClaudeResponses() []fixture {
	const streaming = `{"stream":true,"tools":[{"name":"Read"},{"name":"  __Write_File "},{"function":{"name":"Fn"}},{"name":"read"}]}`
	chunk := func(delta string) string {
		return `data: {"id":"chatcmpl-1","model":"gpt-x","created":5,"choices":[{"index":0,"delta":` + delta + `}]}`
	}
	finish := func(reason string) string {
		return `data: {"id":"chatcmpl-1","choices":[{"index":0,"delta":{},"finish_reason":"` + reason + `"}]}`
	}
	usage := `data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":100,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":0,"cache_creation_tokens":20}}}`
	cases := map[string][]string{
		"text":                {chunk(`{"role":"assistant","content":""}`), chunk(`{"content":"Hello <b>"}`), chunk(`{"content":" & \u2028é"}`), finish("stop"), usage, "data: [DONE]"},
		"reasoning-then-text": {chunk(`{"reasoning_content":"think "}`), chunk(`{"reasoning_content":"more"}`), chunk(`{"content":"answer"}`), chunk(`{"reasoning":{"text":"back"}}`), chunk(`{"content":"again"}`), finish("length"), "data: [DONE]"},
		"reasoning-shapes":    {chunk(`{"reasoning":"str"}`), chunk(`{"reasoning_details":[{"type":"reasoning.text","text":"d1"},{"text":""},"d2",5,[{"text":"nested"}]]}`), chunk(`{"reasoning_content":"","reasoning":{"x":1},"reasoning_details":[{"text":"wins"}]}`), chunk(`{"reasoning_content":5}`), "data: [DONE]"},
		"tools-split": {chunk(`{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]}`),
			chunk(`{"content":"while open"}`), chunk(`{"reasoning_content":"thinking while open"}`), chunk(`{"content":" more"}`),
			chunk(`{"tool_calls":[{"index":0,"function":{"arguments":"{\"path\":"}}]}`), chunk(`{"tool_calls":[{"index":1,"id":"call_2","function":{"name":"__write_file","arguments":"{'a': 'it\\'s \"q\"'}"}}]}`),
			chunk(`{"tool_calls":[{"index":0,"function":{"name":"renamed","arguments":"\"/tmp/<x>\"}"}}]}`), finish("tool_calls"), usage, "data: [DONE]"},
		"tools-late-id":        {chunk(`{"tool_calls":[{"index":2,"function":{"name":"fn"}}]}`), chunk(`{"tool_calls":[{"index":2,"id":"call.2!"}]}`), chunk(`{"tool_calls":[{"index":2,"id":"","function":{"arguments":"{}"}}]}`), finish("stop"), "data: [DONE]"},
		"tools-unnamed":        {chunk(`{"tool_calls":[{"id":"call_a","function":{"arguments":"{\"x\":1}"}},{"function":{"arguments":"[1]"}}]}`), chunk(`{"tool_calls":[{"index":5}]}`), chunk(`{"tool_calls":[{"index":7,"id":5,"function":{"name":5,"arguments":"not json"}}]}`), finish("stop"), "data: [DONE]"},
		"tools-invalid-args":   {chunk(`{"tool_calls":[{"index":0,"id":"c","function":{"name":"Read","arguments":"{\"unterminated"}}]}`), finish("tool_calls"), `data: {"usage":{"prompt_tokens":"12","completion_tokens":1.5}}`, "data: [DONE]"},
		"tools-blank-args":     {chunk(`{"tool_calls":[{"index":0,"id":"c","function":{"name":"Read","arguments":"   "}}]}`), "data: [DONE]"},
		"tools-negative-index": {chunk(`{"tool_calls":[{"index":-1,"id":"neg","function":{"name":"n","arguments":"{}"}}]}`), chunk(`{"tool_calls":[{"index":0,"id":"zero","function":{"name":"z","arguments":"{}"}}]}`), chunk(`{"content":"after"}`), finish("content_filter"), "data: [DONE]"},
		"tools-second-open":    {chunk(`{"tool_calls":[{"index":0,"id":"a","function":{"name":"A","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"B","arguments":"{\"k\":1}"}}]}`), chunk(`{"tool_calls":[{"index":3,"id":"c","function":{"name":"C"}}]}`), finish("tool_calls"), "data: [DONE]"},
		"usage-trailing":       {chunk(`{"content":"x"}`), `data: {"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":3}}}`, chunk(`{"content":"late"}`), "data: [DONE]", "data: [DONE]"},
		"usage-only":           {`data: {"choices":[],"usage":{"prompt_tokens":5}}`, `data: {"usage":null}`, "data: [DONE]"},
		"usage-overflow":       {chunk(`{"content":"x"}`), `data: {"choices":[{"delta":{},"finish_reason":"function_call"}],"usage":{"prompt_tokens":5,"prompt_tokens_details":{"cached_tokens":9223372036854775807,"cache_write_tokens":9223372036854775807}}}`},
		"finish-other":         {chunk(`{"content":"x"}`), finish("weird"), `data: {"choices":[{"finish_reason":"stop"}],"usage":{"prompt_tokens":1}}`, "data: [DONE]"},
		"no-delta":             {`data: {"id":"x","choices":[{"index":0,"finish_reason":"stop"}]}`, "data: [DONE]"},
		"framing":              {"event: ping", ": comment", "", "data:" + `{"id":"nospace","choices":[{"delta":{"content":"a"}}]}`, " data: ignored", "data:   [DONE]  ", "data: not json", "data: {\"choices\":[{\"delta\":{\"content\":\"bad\xff\"}}]}"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		for j, orig := range []string{streaming, `{"stream":false}`, ``, `{"stream":"yes","tools":"bad"}`} {
			if j > 0 && name != "text" && name != "tools-split" && name != "framing" {
				continue
			}
			f := streamCase(fmt.Sprintf("openai-claude/%s/%d", name, j), "m", cases[name]...)
			f.Original = orig
			out = append(out, f)
		}
	}
	tools := `{"tools":[{"name":"Read"},{"name":"_Write"}]}`
	for i, body := range []string{
		`{"id":"chatcmpl-1","model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","content":"Hello <b> é","reasoning_content":"why"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":2,"cache_creation_tokens":1}}}`,
		`{"id":"c","choices":[{"message":{"content":[{"type":"reasoning","text":"r1"},{"type":"reasoning","text":"r2"},{"type":"text","text":"t1"},{"type":"text","text":"t2"},{"type":"reasoning"},{"type":"tool_calls","tool_calls":[{"id":"call 1","function":{"name":"read","arguments":"{'a':1}"}},{"function":{"name":"write","arguments":"[1]"}}]},{"type":"image"},{"type":"text","text":""},{"type":"reasoning","text":"tail"}],"tool_calls":[{"id":"c2","function":{"name":"WRITE","arguments":"{\"b\": 2}  "}}],"reasoning":{"text":"after content"}}}]}`,
		`{"id":"c","choices":[{"message":{"content":"","tool_calls":[{"id":"t","function":{"name":"x","arguments":""}}]}}]}`,
		`{"id":"c","choices":[{"message":{"content":null}}],"usage":null}`,
		`{"id":"c","choices":[{"message":{"content":5,"reasoning_details":[{"text":"d"}]},"finish_reason":"tool_calls"}]}`,
		`{"id":"c","choices":[]}`, `{"choices":{"message":{"content":"obj"}}}`, `{"id":"c","choices":[{"finish_reason":null}]}`,
		`{"id":"c","choices":[{"message":{"content":"x"},"finish_reason":"content_filter"},{"message":{"content":"second"}}]}`,
		`not json`, ``, "{\"id\":\"c\xff\",\"choices\":[{\"message\":{\"content\":\"bad\xfe\"}}]}",
	} {
		for j, orig := range []string{tools, ``} {
			n := nonStream(fmt.Sprintf("openai-claude/non-stream/%d/%d", i, j), "m", body)
			n.Original = orig
			out = append(out, n)
		}
	}
	return out
}

// geminiRequests are Gemini generateContent client bodies: roles, tool declarations,
// thinking configs across capability models, signatures, schemas, coercions and bytes.
func geminiRequests(model string) []fixture {
	var out []fixture
	for i, thinking := range []string{
		`"thinkingConfig":{"thinkingBudget":0}`, `"thinkingConfig":{"thinkingBudget":-1}`, `"thinkingConfig":{"thinkingBudget":1024}`,
		`"thinkingConfig":{"thinkingBudget":24577,"includeThoughts":true}`, `"thinkingConfig":{"thinkingBudget":"512"}`,
		`"thinkingConfig":{"thinkingLevel":"high"}`, `"thinkingConfig":{"thinkingLevel":" LOW "}`, `"thinkingConfig":{"thinkingLevel":"minimal","includeThoughts":false}`,
		`"thinkingConfig":{"thinking_budget":2048,"include_thoughts":true}`, `"thinkingConfig":{"includeThoughts":true}`, `"thinkingConfig":{}`,
		`"thinkingConfig":{"thinkingLevel":"xhigh"}`, `"thinkingConfig":{"thinkingBudget":1e3}`,
	} {
		for _, m := range []string{"gemini-2.5-pro", "gemini-3-pro-preview", "claude-opus-4-6", "gpt-5", "kimi-k2.5", "unknown-model", "gemini-2.5-flash(8192)"} {
			out = append(out, req(fmt.Sprintf("gemini-thinking/%d/%s", i, m), m, `{"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"maxOutputTokens":100,`+thinking+`}}`, i%2 == 0))
		}
	}
	for i, input := range []string{
		`{"contents":[{"parts":[{"text":"no role"}]},{"role":"bogus","parts":[{"text":"x"}]},{"role":"MODEL","parts":[{"text":"y"}]},{"role":"model","parts":[{"text":"z"}]},{"parts":[{"functionResponse":{"name":"f","response":{"r":1}}}]},{"role":"","parts":[{"function_response":{"name":"g"}}]}]}`,
		`{"contents":[{"role":"user","parts":[{"text":"ok"}]},{"role":"model","parts":[{"text":"fine"}]}],"safetySettings":[]}`,
		`{"contents":{"a":{"parts":[{"text":"obj"}]},"b":{"role":"model"}}}`, `{"contents":"str"}`, `{"contents":null}`, `{"contents":[]}`,
		`{"systemInstruction":{"parts":[{"text":"sys <b>"}]},"generationConfig":{"temperature":0.5}}`,
		`{"tools":[{"functionDeclarations":[{"name":"a","description":"d","parameters":{"type":"OBJECT","properties":{"q":{"type":"STRING"}}}},{"name":"b","parametersJsonSchema":{"type":"object"}}]},{"googleSearch":{}},{"function_declarations":[{"name":"c","parameters":{"type":"object"}}]},{"codeExecution":{}}],"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		`{"tools":[{"functionDeclarations":[{"name":"keep","parametersJsonSchema":{}}]},{"function_declarations":"bad"}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["keep"]}},"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		`{"tools":{"functionDeclarations":[]},"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		`{"generationConfig":{"responseSchema":{"type":"OBJECT"},"responseMimeType":"application/json","stopSequences":["<s>","b"],"candidateCount":2,"topK":40,"topP":0.9,"seed":7,"presencePenalty":0.1,"frequencyPenalty":0.2,"responseModalities":["TEXT","IMAGE"]},"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		`{"generationConfig":{"responseJsonSchema":{"type":"object"},"responseSchema":{"type":"object","x":1}},"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		`{"contents":[{"role":"model","parts":[{"functionCall":{"name":"Bash","args":{"cmd":"ls"}}},{"functionCall":{"name":"Read","args":{}}}]},{"role":"user","parts":[{"functionResponse":{"name":"","response":{"ok":1}}},{"functionResponse":{"name":"  ","response":{}}},{"functionResponse":{"response":{}}}]},{"role":"model","parts":[{"functionCall":{"name":"Once"}}]},{"role":"user","parts":[{"text":"between"}]},{"role":"user","parts":[{"functionResponse":{"name":""}}]}]}`,
		`{"contents":[{"role":"model","parts":{"functionCall":{"name":"ObjParts"}}},{"role":"user","parts":[{"functionResponse":{"name":""}},{"functionResponse":{"name":""}}]}]}`,
		`{"contents":[{"role":"model","parts":[{"functionCall":{"name":"A"}}]},{"role":"function","parts":[{"functionResponse":{"name":"","response":{"r":"<x>"}}}]}]}`,
		`{"contents":[{"role":"model","parts":[{"text":"t","thought":true,"thoughtSignature":"bad-sig"},{"functionCall":{"name":"f","args":{}},"thoughtSignature":"skip_thought_signature_validator"},{"text":"x","thought_signature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"}]},{"role":"user","parts":[{"functionResponse":{"name":"f","response":{}},"thoughtSignature":"claude#EjQ="}]}]}`,
		`{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"iVBOR"}},{"fileData":{"mimeType":"application/pdf","fileUri":"gs://b/f.pdf"}},{"text":"what <is> this & é"}]}]}`,
		`{"contents":[{"role":"model","parts":[{"executableCode":{"language":"PYTHON","code":"print(1)"}},{"codeExecutionResult":{"outcome":"OUTCOME_OK","output":"1"}}]}]}`,
		`{"contents":[{"role":"user","parts":[{"text":"a"}]},{"role":"user","parts":[{"text":"b"}]},{"role":"model","parts":[]},{"role":"model"},{"role":"user","parts":[{"text":""}]}]}`,
		`{"model":"old","contents":[{"role":"user","parts":[{"text":"x"}]}],"cachedContent":"cachedContents/abc","labels":{"k":"v"}}`,
		"{\"contents\":[{\"role\":\"user\",\"parts\":[{\"text\":\"bad\xff\xfe\"}]},{\"role\":\"r\xff\",\"parts\":[{\"text\":\"\xc3\"}]}]}",
		`{"contents":[{"role":"user","parts":[{"text":"\ud800 lone \u2028 \u00e9 <&>"}]}]}`,
		`not json`, ``, `[]`, `{"contents":[{"role":"user","parts":[{"text":"x"}]}],}`, `{"contents":[{"role":"user","parts":[{"text":"unterminated`,
		` {"contents" : [ {"parts" : [ {"text" : "spaced"} ] } ] } trailing`,
	} {
		out = append(out, req(fmt.Sprintf("gemini/%d", i), model, input, i%2 == 1))
	}
	return out
}

// geminiUpstreamResponses are Gemini generateContent stream lines and bodies: text,
// thoughts with signatures, function calls, inline data, finish reasons and usage.
func geminiUpstreamResponses() []fixture {
	usage := `"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":4,"totalTokenCount":20}`
	chunk := func(parts string, extra string) string {
		return `data: {"candidates":[{"content":{"role":"model","parts":` + parts + `},"index":0` + extra + `}],"modelVersion":"gemini-2.5-pro","responseId":"resp-1","createTime":"2025-01-01T00:00:00Z"}`
	}
	cases := map[string][]string{
		"text":            {chunk(`[{"text":"Hello <b>"}]`, ``), chunk(`[{"text":" & é \u2028"}]`, ``), chunk(`[{"text":""}]`, `,"finishReason":"STOP"`) + ``, `data: {"candidates":[],` + usage + `}`, "data: [DONE]"},
		"thoughts":        {chunk(`[{"text":"think","thought":true}]`, ``), chunk(`[{"text":"","thought":true,"thoughtSignature":"EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"}]`, ``), chunk(`[{"text":"answer"}]`, `,"finishReason":"STOP"`), `data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],` + usage + `}`},
		"function-calls":  {chunk(`[{"text":"calling"},{"functionCall":{"name":"lookup","args":{"q":"<x>","n":1e3}},"thoughtSignature":"sig"}]`, ``), chunk(`[{"functionCall":{"id":"call-2","name":"Read","args":{}}},{"functionCall":{"name":"noargs"}},{"functionCall":{"name":"a.b_c_d","args":[1]}}]`, `,"finishReason":"STOP"`), `data: {` + usage + `}`, "data: [DONE]"},
		"create-time":     {`data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"createTime":"2025-01-02T03:04:05.678+01:00"}`, `data: {"candidates":[{"content":{"parts":[{"text":"b"}]}}],"createTime":"not a time"}`, `data: {"candidates":[{"content":{"parts":[{"text":"c"},{"text":"d","thought":true},{"text":"e"}]},"finishReason":"stop"}],"createTime":5,` + usage + `}`, `data: {"candidates":[{"content":{"parts":[{"thoughtSignature":"only-sig"},{"thought_signature":"","text":"keep"},{"audioTranscription":{"text":"spoken"}},{"inline_data":{"mime_type":"audio/wav","data":"UklG"}},{"inlineData":{"data":""}}]},"index":2,"finishReason":"MAX_TOKENS"}],"usageMetadata":{"promptTokenCount":1}}`},
		"finish-reasons":  {chunk(`[{"text":"a"}]`, `,"finishReason":"MAX_TOKENS"`), chunk(`[{"text":"b"}]`, `,"finishReason":"SAFETY","safetyRatings":[{"category":"HARM_CATEGORY_HATE_SPEECH","probability":"HIGH"}]`), chunk(`[]`, `,"finishReason":"MALFORMED_FUNCTION_CALL"`), chunk(`[{"text":"c"}]`, `,"finishReason":"RECITATION"`), chunk(`[{"text":"d"}]`, `,"finishReason":"OTHER"`), "data: [DONE]"},
		"inline-data":     {chunk(`[{"inlineData":{"mimeType":"image/png","data":"iVBOR"}},{"text":"caption"},{"executableCode":{"language":"PYTHON","code":"x"}},{"codeExecutionResult":{"outcome":"OUTCOME_OK","output":"1"}}]`, `,"finishReason":"STOP"`), "data: [DONE]"},
		"grounding":       {chunk(`[{"text":"grounded"}]`, `,"groundingMetadata":{"webSearchQueries":["q"],"groundingChunks":[{"web":{"uri":"https://a","title":"A"}}]},"finishReason":"STOP"`), "data: [DONE]"},
		"multi-candidate": {`data: {"candidates":[{"content":{"parts":[{"text":"c0"}]},"index":0},{"content":{"parts":[{"text":"c1"}]},"index":1,"finishReason":"STOP"}],"responseId":"r"}`, "data: [DONE]"},
		"usage-only":      {`data: {"usageMetadata":{"promptTokenCount":"7","candidatesTokenCount":1.5}}`, `data: {"candidates":[{"finishReason":"STOP"}]}`, "data: [DONE]"},
		"framing":         {"", ": keepalive", "event: x", `{"candidates":[{"content":{"parts":[{"text":"bare"}]}}]}`, "data:   [DONE]  ", "data: not json", "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"bad\xff\"}]}}]}", `data: {"error":{"code":429,"message":"quota <x>"}}`},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		f := streamCase("gemini-up/"+name, "gemini-2.5-pro", cases[name]...)
		f.Original = `{"model":"client-model","stream":true,"tools":[{"name":"Read"},{"function":{"name":"Lookup"}},{"name":"a.b/c d"},{"name":"a.b c/d"}]}`
		out = append(out, f)
	}
	for i, body := range []string{
		`{"candidates":[{"content":{"role":"model","parts":[{"text":"think","thought":true,"thoughtSignature":"sig"},{"text":"Hello <b> é"},{"functionCall":{"name":"lookup","args":{"q":1}}}]},"finishReason":"STOP","index":0}],` + usage + `,"modelVersion":"gemini-2.5-pro","responseId":"resp-1"}`,
		`{"candidates":[{"content":{"parts":[{"functionCall":{"id":"c1","name":"Read","args":{"path":"/a"}}},{"functionCall":{"name":"look_up"}},{"text":"t1"},{"text":"t2"},{"text":"r","thought":true},{"inlineData":{"data":"AA"}},{"inline_data":{"mime_type":"image/webp","data":"BB"}},{"audioTranscription":{"text":"heard"}}]},"finishReason":"MAX_TOKENS","index":1}],"usageMetadata":{"promptTokenCount":3,"cachedContentTokenCount":2},"createTime":"2025-06-01T00:00:00Z"}`,
		`{"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/jpeg","data":"/9j/"}}]},"finishReason":"SAFETY"}]}`,
		`{"candidates":[]}`, `{"candidates":[{"content":{"parts":[{"text":""}]}}]}`, `{"promptFeedback":{"blockReason":"SAFETY"}}`,
		`{"candidates":[{"content":{"parts":[{"text":"a"}]}},{"content":{"parts":[{"text":"b"}]},"index":1}]}`,
		`not json`, ``, "{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"bad\xfe\"}]}}]}",
		`[{"candidates":[{"content":{"parts":[{"text":"array body"}]}}]}]`,
	} {
		n := nonStream(fmt.Sprintf("gemini-up/non-stream/%d", i), "gemini-2.5-pro", body)
		n.Original = `{"model":"client-model","tools":[{"name":"Read"},{"name":"look up"}]}`
		out = append(out, n)
	}
	return out
}

// openAIGeminiRequests exercise the OpenAI Chat -> Gemini branches: generation settings,
// modalities, response formats, media parts, demoted system messages, tool turns,
// declarations with sanitized names and strictness, built-in tools and tool choices.
func openAIGeminiRequests(model string) []fixture {
	var out []fixture
	sig := "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"
	for i, input := range []string{
		`{"messages":[{"role":"user","content":"x"}],"temperature":0.2,"top_p":1,"top_k":40,"max_tokens":12.75,"n":3,"reasoning_effort":"AUTO","generationConfig":{"seed":1,"maxOutputTokens":5}}`,
		`{"messages":[{"role":"user","content":"x"}],"temperature":"0.2","top_k":"4","max_completion_tokens":1e3,"n":1,"reasoning_effort":" Medium "}`,
		`{"messages":[{"role":"user","content":"x"}],"max_tokens":"9","max_completion_tokens":7,"n":"2","reasoning_effort":""}`,
		`{"messages":[{"role":"user","content":"x"}],"modalities":["Text","IMAGE","audio",5],"image_config":{"aspect_ratio":"16:9","image_size":"2K"}}`,
		`{"messages":[{"role":"user","content":"x"}],"modalities":"text","image_config":{"aspect_ratio":5,"image_size":"<1K>"}}`,
		`{"messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_schema","json_schema":{"name":"n","schema":{"type":"object","properties":{"a":{"type":"string"}}}}},"generationConfig":{"responseSchema":{"x":1}}}`,
		`{"messages":[{"role":"user","content":"x"}],"response_format":{"type":" JSON_OBJECT "}}`,
		`{"messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_schema"}}`,
		`{"messages":[{"role":"system","content":"sys1"},{"role":"developer","content":{"type":"text","text":"dev obj"}},{"role":"system","content":[{"type":"text","text":"a"},{"type":"image_url"},{"text":"c"}]},{"role":"user","content":"hello"},{"role":"system","content":"late rule"},{"role":"developer","content":"  "},{"role":"system","content":[{"type":"text","text":"arr late"},{"type":"text","text":""}]}]}`,
		`{"messages":[{"role":"system","content":"only system"}]}`,
		`{"messages":[{"role":"user","content":[{"type":"text","text":"look"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBOR"}},{"type":"image_url","image_url":{"url":"https://x/y.png"}},{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,"}},{"type":"video_url","video_url":{"url":"data:video/mp4;base64,AAAA"}},{"type":"file","file":{"filename":"doc.PDF","file_data":"UERG"}},{"type":"file","file":{"filename":"a.unknownext","file_data":"QQ=="}},{"type":"file","file":{"file_data":"data:text/plain;charset=utf-8;BASE64,aGk="}},{"type":"file","file":{"file_data":"DATA:;base64,x"}},{"type":"file","file":{"filename":"dir.x/noext","file_data":"Zg=="}},{"type":"input_audio","input_audio":{"data":"UklG","format":"mp3"}},{"type":"input_audio","input_audio":{"data":"AA","format":"opus"}},{"type":"input_audio","input_audio":{"data":"AA"}},{"type":"input_audio","input_audio":{"format":"wav"}}]}]}`,
		`{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"thinking out loud","reasoning_content":"r <x>"},{"role":"assistant","content":[{"type":"text","text":"multi"},{"type":"image_url","image_url":{"url":"data:image/gif;base64,R0lG"}},{"type":"text","text":""}],"reasoning_content":5},{"role":"assistant","content":"","reasoning_content":""},{"role":"user","content":"next"},{"role":"assistant","content":"trailing model dropped"}]}`,
		`{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"calling","tool_calls":[{"id":"c1","type":"function","function":{"name":"get weather!","arguments":"{\"city\":\"<P>\"}"},"extra_content":{"google":{"thought_signature":"` + sig + `"}}},{"id":"c2","type":"function","function":{"name":"Read","arguments":"not json"},"thoughtSignature":"claude#EjQ="},{"id":"c3","type":"custom","function":{"name":"skip"}},{"id":"c4","type":"function","function":{"name":"","arguments":"{}"}},{"id":"c5","type":"function","function":{"name":"9lives","arguments":""},"function":{"extra_content":{"google":{"thought_signature":"skip_thought_signature_validator"}}}}]},{"role":"tool","tool_call_id":"c1","content":"sunny <&>"},{"role":"tool","tool_call_id":"c2","content":[{"type":"text","text":"file"}]},{"role":"tool","tool_call_id":"c1","content":{"override":true}},{"role":"user","content":"thanks"},{"role":"tool","tool_call_id":"c5","content":""}]}`,
		`{"messages":[{"role":"assistant","tool_calls":[{"id":"a","type":"function","function":{"name":"A","arguments":"{}"}}]},{"role":"assistant","content":"second"},{"role":"tool","tool_call_id":"a","content":"late"}]}`,
		`{"messages":[{"role":"assistant","content":"x","tool_calls":[]},{"role":"assistant","tool_calls":[{"type":"other"}]},{"role":"assistant","tool_calls":{"id":"obj","type":"function","function":{"name":"O","arguments":"{}"}}}]}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"get weather","description":"d <b>","parameters":{"type":"object","properties":{"city":{"type":["string","null"],"format":"city","minLength":1},"unit":{"enum":["c","f"]}},"required":["city","missing"],"additionalProperties":false,"$schema":"x"},"strict":true}},{"type":"function","function":{"name":"noparams"},"strict":false},{"type":"function","function":{"name":5,"parameters":{}}},{"type":"function","function":"bad"},{"type":"web_search"},{"google_search":{"x":1}},{"type":"function","function":{"name":"both"},"code_execution":{},"url_context":{"u":1}}],"tool_choice":"auto"}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"a b","parameters":{"type":"object"}}},{"type":"function","function":{"name":"a_b","parameters":{"type":"object"}}}],"tool_choice":{"type":"function","function":{"name":"a b"}}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"},"strict":true}}],"tool_choice":{"type":"function","name":" Read "},"parallel_tool_calls":true}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"}}}],"tool_choice":{"type":"function","function":{"name":"Missing"}}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"}}},{"type":"function","function":{"name":"Write"},"strict":true}],"tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"Write"}},{"name":"Read"}]}}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read","strict":true}},{"type":"function","function":{"name":"Write"}},{"google_search":{}}],"tool_choice":{"type":"allowed_tools","tools":[{"name":"Read"}],"mode":"AUTO"}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read"}}],"tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":{"name":"Read"}}}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read"}}],"tool_choice":{"type":"allowed_tools","allowed_tools":{"tools":[{"name":"Nope"}],"mode":"any"}}}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read","strict":true}}]}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read"}}],"tool_choice":"REQUIRED","parallel_tool_calls":false}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read"}}],"tool_choice":{"type":"any"},"parallel_tool_calls":"false"}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[{"type":"function","function":{"name":"Read"}}],"tool_choice":5}`,
		`{"messages":[{"role":"user","content":"x"}],"tools":[],"tool_choice":"none","safetySettings":[{"category":"x"}]}`,
	} {
		out = append(out, req(fmt.Sprintf("openai-gemini/%d", i), model, input, i%2 == 0))
	}
	return out
}

// openAICodexRequests exercise OpenAI Chat -> Codex branches: custom tools and the
// apply_patch envelope, call-ID repair and ambiguity, tool outputs with images, name
// shortening, structured output, verbosity and service tiers.
func openAICodexRequests(model string) []fixture {
	long := strings.Repeat("x", 70)
	var out []fixture
	for i, input := range []string{
		`{"messages":[{"role":"system","content":"sys"},{"role":"user","content":[{"type":"text","text":"hi <b>"},{"type":"image_url","image_url":{"url":"https://x/y.png"}},{"type":"image_url"},{"type":"file","file":{"file_data":"data:application/pdf;base64,UERG","filename":"a.pdf"}},{"type":"file","file":{}},{"type":"input_audio","input_audio":{"data":"UklG","format":"wav"}},{"type":"input_audio","input_audio":{"data":"AA"}}]},{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"image_url","image_url":{"url":"skip"}}]},{"role":"developer","content":"dev"}],"reasoning_effort":"high","service_tier":" FAST "}`,
		`{"messages":[{"role":"user","content":"x"}],"reasoning_effort":5,"service_tier":"ultrafast","response_format":{"type":"json_schema","json_schema":{"name":"N<","strict":true,"schema":{"type":"object"}}},"text":{"verbosity":"low"}}`,
		`{"messages":[{"role":"user","content":"x"}],"service_tier":"flex","response_format":{"type":"text"}}`,
		`{"messages":[{"role":"user","content":"x"}],"response_format":{"type":"json_object"},"text":{"other":1}}`,
		`{"messages":[{"role":"user","content":"x"}],"text":{"verbosity":"high"}}`,
		`{"messages":[{"role":"user","content":""},{"role":"assistant","content":""},{"role":"assistant","content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"lookup","arguments":"{\"q\":1}"}},{"type":"function","function":{"name":"noid","arguments":"{}"}},{"type":"function","function":{"name":"noid2","arguments":"{}"}},{"id":"call_missing_2_1","type":"function","function":{"name":"taken","arguments":"{}"}},{"id":"x","type":"other"}]},{"role":"tool","tool_call_id":"c1","content":"r1 <x>"},{"role":"tool","content":"anon"},{"role":"tool","tool_call_id":"unknown","content":"orphan"},{"role":"tool","tool_call_id":"call_missing_2_2","content":[{"type":"text","text":"t"},{"type":"image_url","image_url":{"url":"https://img","detail":"high"}},{"type":"input_image","image_url":"https://i2","file_id":"f1","detail":"low"},{"type":"image_url","image_url":{}},{"type":"file","file":{"file_id":"fid","filename":"n.txt"}},{"type":"file","file":{}},{"type":"other","x":1},"bare"]}]}`,
		`{"messages":[{"role":"assistant","tool_calls":[{"id":"dup","type":"function","function":{"name":"a","arguments":"{}"}},{"id":"dup","type":"function","function":{"name":"b","arguments":"{}"}},{"id":"ok","type":"function","function":{"name":"c","arguments":"{}"}}]},{"role":"tool","tool_call_id":"dup","content":"d"},{"role":"tool","tool_call_id":"ok","content":"[{\"type\":\"input_image\",\"image_url\":\"https://s\"}]"},{"role":"tool","tool_call_id":"ok","content":"second"}]}`,
		`{"tools":[{"type":"custom","name":"apply_patch","description":"Patch. This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"type":"grammar","definition":"start: x"}},{"type":"function","function":{"name":"lookup","description":"<d>","parameters":{"type":"object"}}},{"type":"function","function":{"name":"strict_one","strict":true}},{"type":"function"},{"type":"web_search","search_context_size":"low"},{"name":"untyped"},{"type":"custom","name":"lookup"}],"tool_choice":{"type":"function","function":{"name":"apply_patch"}},"messages":[{"role":"assistant","tool_calls":[{"id":"p1","type":"function","function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\\n\"}"}},{"id":"p2","type":"function","function":{"name":"apply_patch","arguments":"not json"}},{"id":"p3","type":"custom","custom":{"name":"apply_patch","input":"{\"input\":\"raw\"}"}},{"id":"p4","type":"function","function":{"name":"lookup","arguments":"{}"}}]},{"role":"tool","tool_call_id":"p1","content":"done"},{"role":"tool","tool_call_id":"p3","content":"done3"}]}`,
		`{"tools":[{"type":"function","function":{"name":"mcp__server__` + long + `"}},{"type":"function","function":{"name":"` + long + `a"}},{"type":"function","function":{"name":"` + long + `b"}},{"type":"function","function":{"name":"bad name.é"}}],"tool_choice":{"type":"function","function":{"name":"` + long + `b"}},"messages":[{"role":"assistant","tool_calls":[{"id":"m","type":"function","function":{"name":"mcp__server__` + long + `","arguments":"{}"}},{"id":"h","type":"custom","custom":{"name":"history only tool"}}]}]}`,
		`{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":"required","messages":[]}`,
		`{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"custom","name":"free form"},"messages":[]}`,
		`{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"web_search"},"messages":[]}`,
		`{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"type":"function"},"messages":[]}`,
		`{"tools":[{"type":"function","function":{"name":"a"}}],"tool_choice":{"mode":"x"},"messages":[]}`,
		`{"tools":[],"tool_choice":5,"messages":[{"role":"tool","tool_call_id":"x","content":"no pending"}]}`,
	} {
		out = append(out, req(fmt.Sprintf("openai-codex/%d", i), model, input, i%2 == 0))
	}
	return out
}

// codexEventStreams are Codex (Responses) events as non-Responses clients consume them:
// text and reasoning deltas, function and custom (apply_patch) calls with and without
// added/delta events, images, terminal states, usage and service tiers.
func codexEventStreams() []fixture {
	ev := func(body string) string { return "data: " + body }
	created := ev(`{"type":"response.created","response":{"id":"resp_1","created_at":1700000000,"model":"gpt-5.3-codex","service_tier":"priority"}}`)
	usage := `"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2},"output_tokens_details":{"reasoning_tokens":4}}`
	cases := map[string][]string{
		"text": {created, ev(`{"type":"response.reasoning_summary_text.delta","delta":"think <x>"}`), ev(`{"type":"response.reasoning_summary_text.done"}`), ev(`{"type":"response.reasoning_text.delta","delta":"raw"}`), ev(`{"type":"response.output_text.delta","delta":"Hello é"}`), ev(`{"type":"response.output_text.delta"}`), ev(`{"type":"response.output_text.done","text":"Hello é"}`), ev(`{"type":"response.completed","response":{"id":"resp_1",` + usage + `}}`)},
		"function-calls": {created, ev(`{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"lookup"}}`), ev(`{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"q\":"}`), ev(`{"type":"response.function_call_arguments.delta","output_index":1,"delta":"1}"}`), ev(`{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"q\":1}"}`), ev(`{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call_1","name":"lookup","arguments":"{\"q\":1}"}}`),
			ev(`{"type":"response.output_item.added","output_index":2,"item":{"id":"fc_2","type":"function_call","call_id":"call_2","name":"mcp__srv__read"}}`), ev(`{"type":"response.function_call_arguments.done","item_id":"fc_2","arguments":"{}"}`), ev(`{"type":"response.output_item.done","item":{"id":"fc_3","type":"function_call","call_id":"call_3","name":"skipped_added","arguments":"{\"z\":true}"}}`), ev(`{"type":"response.output_item.done","item":{"id":"msg","type":"message"}}`), ev(`{"type":"response.completed","response":{"id":"resp_1","service_tier":" flex "}}`)},
		"apply-patch": {created, ev(`{"type":"response.output_item.added","output_index":0,"item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_p","name":"apply_patch"}}`), ev(`{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"*** Begin Patch\n<\"q\">"}`), ev(`{"type":"response.custom_tool_call_input.delta","item_id":"ctc_1","delta":"\n*** End Patch"}`), ev(`{"type":"response.custom_tool_call_input.done","item_id":"ctc_1","input":"ignored"}`), ev(`{"type":"response.output_item.done","item":{"id":"ctc_1","type":"custom_tool_call","call_id":"call_p","name":"apply_patch","input":"x"}}`),
			ev(`{"type":"response.output_item.added","output_index":1,"item":{"id":"ctc_2","type":"custom_tool_call","call_id":"call_q","name":"apply_patch"}}`), ev(`{"type":"response.output_item.done","item":{"id":"ctc_2","type":"custom_tool_call","call_id":"call_q","name":"apply_patch","input":"full <patch>"}}`),
			ev(`{"type":"response.output_item.done","item":{"id":"ctc_3","type":"custom_tool_call","call_id":"call_r","name":"apply_patch","input":"only done"}}`), ev(`{"type":"response.output_item.done","item":{"id":"ctc_4","type":"custom_tool_call","call_id":"call_s","name":"freeform","input":"plain"}}`), ev(`{"type":"response.completed","response":{"id":"resp_1"}}`)},
		"images":     {created, ev(`{"type":"response.image_generation_call.partial_image","item_id":"ig_1","partial_image_b64":"AAA","output_format":"webp"}`), ev(`{"type":"response.image_generation_call.partial_image","item_id":"ig_1","partial_image_b64":"AAA","output_format":"webp"}`), ev(`{"type":"response.image_generation_call.partial_image","partial_image_b64":"BBB","output_format":"image/avif"}`), ev(`{"type":"response.image_generation_call.partial_image","item_id":"ig_2"}`), ev(`{"type":"response.output_item.done","item":{"id":"ig_1","type":"image_generation_call","result":"AAA"}}`), ev(`{"type":"response.output_item.done","item":{"id":"ig_1","type":"image_generation_call","result":"CCC","output_format":"JPG"}}`), ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call","result":""}}`)},
		"incomplete": {ev(`{"type":"response.output_text.delta","delta":"no created","model":"evt-model"}`), ev(`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":"7","output_tokens":1.5,"input_tokens_details":{"cache_write_tokens":1.5}}}}`), ev(`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"usage":{"input_tokens_details":{"cache_write_tokens":-1}}}}`), ev(`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"other"}}}`)},
		"framing":    {"event: response.created", created, "", ": keepalive", `{"type":"response.output_text.delta","delta":"bare"}`, "data: [DONE]", "data: not json", "data: {\"type\":\"response.output_text.delta\",\"delta\":\"bad\xff\"}"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	original := `{"tools":[{"type":"custom","name":"apply_patch"},{"type":"custom","name":"freeform"},{"type":"function","function":{"name":"mcp__srv__read"}},{"type":"function","function":{"name":"lookup"}}]}`
	var out []fixture
	for _, name := range names {
		for j, orig := range []string{original, `{"tools":[{"type":"custom","name":"apply_patch"},{"type":"function","function":{"name":"apply_patch"}}]}`, ``} {
			if j > 0 && name != "apply-patch" && name != "function-calls" {
				continue
			}
			f := streamCase(fmt.Sprintf("codex-events/%s/%d", name, j), "requested-model", cases[name]...)
			f.Original = orig
			out = append(out, f)
		}
	}
	for i, body := range []string{
		`{"type":"response.completed","response":{"id":"resp_1","created_at":1700000000,"model":"gpt-5.3-codex","status":"completed","service_tier":"priority",` + usage + `,"output":[{"type":"reasoning","summary":[{"type":"summary_text","text":"s1"},{"type":"summary_text","text":"s2"}],"content":[{"type":"reasoning_text","text":"r1"},{"type":"reasoning_text","text":"r2"}]},{"type":"message","content":[{"type":"refusal"},{"type":"output_text","text":"t1"},{"type":"output_text","text":"t2"}]},{"type":"function_call","call_id":"c1","name":"lookup","arguments":"{}"},{"type":"custom_tool_call","call_id":"c2","name":"apply_patch","input":"patch <x>"},{"type":"custom_tool_call","name":"freeform","input":"raw"},{"type":"image_generation_call","result":"AAA","output_format":"gif"},{"type":"image_generation_call","result":""}]}}`,
		`{"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"plain"}]}]}}`,
		`{"type":"response.incomplete","response":{"id":"r","created_at":5,"status":"incomplete","incomplete_details":{"reason":"max_tokens"},"output":[]}}`,
		`{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"weird"}}}`,
		`{"type":"response.completed","response":{"status":"failed"},"service_tier":"top-level"}`,
		`{"type":"response.created","response":{}}`, `{"id":"r","output":[]}`, `not json`, ``,
	} {
		n := nonStream(fmt.Sprintf("codex-events/non-stream/%d", i), "requested-model", body)
		n.Original = original
		out = append(out, n)
	}
	return out
}
