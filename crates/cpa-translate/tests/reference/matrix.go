package main

import (
	"fmt"
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
	switch r.upstream {
	case "claude":
		out = append(out, claudeResponses()...)
	case "openai":
		out = append(out, openAIResponses()...)
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
