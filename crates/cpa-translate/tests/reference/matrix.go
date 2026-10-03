package main

import (
	"encoding/base64"
	"fmt"
	"sort"
	"strings"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/signature"
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
		if r.upstream == "interactions" {
			out = append(out, openAIInteractionsRequests(model)...)
		}
		if r.upstream == "antigravity" {
			out = append(out, openAIAntigravityRequests(model)...)
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
	case "interactions":
		out = append(out, interactionsRequests(model)...)
		if r.upstream == "gemini" {
			out = append(out, interactionsGeminiRequests(model)...)
		}
		if r.upstream == "openai" {
			out = append(out, interactionsOpenAIRequests(model)...)
		}
		if r.upstream == "openai-response" {
			out = append(out, interactionsResponsesRequests(model)...)
		}
		if r.upstream == "claude" {
			out = append(out, interactionsClaudeRequests(model)...)
		}
		if r.upstream == "antigravity" {
			out = append(out, interactionsAntigravityRequests(model)...)
		}
	case "gemini":
		out = append(out, geminiRequests(model)...)
		if r.upstream == "interactions" {
			out = append(out, geminiInteractionsRequests(model)...)
		}
		if r.upstream == "claude" {
			out = append(out, geminiClaudeRequests(model)...)
		}
		if r.upstream == "openai" {
			out = append(out, geminiOpenAIRequests(model)...)
		}
		if r.upstream == "codex" {
			out = append(out, geminiCodexRequests(model)...)
		}
		if r.upstream == "antigravity" {
			out = append(out, geminiAntigravityRequests(model)...)
		}
	case "claude":
		out = append(out, claudeRequests(model)...)
		if r.upstream == "codex" {
			out = append(out, claudeCodexRequests(model)...)
		}
		if r.upstream == "interactions" {
			out = append(out, claudeInteractionsRequests(model)...)
		}
		if r.upstream == "antigravity" {
			out = append(out, claudeAntigravityRequests(model)...)
		}
		if r.upstream == "openai" || r.upstream == "gemini" || r.upstream == "codex" || r.upstream == "interactions" {
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
		if r.upstream == "gemini" {
			out = append(out, responsesGeminiRequests(model)...)
		}
		if r.upstream == "openai" {
			out = append(out, responsesOpenAIRequests(model)...)
		}
		if r.upstream == "interactions" {
			out = append(out, responsesInteractionsRequests(model)...)
		}
		if r.upstream == "antigravity" {
			out = append(out, responsesAntigravityRequests(model)...)
		}
	}
	switch r.upstream {
	case "codex":
		if r.client == "openai-response" {
			out = append(out, codexResponses()...)
		} else {
			out = append(out, codexEventStreams()...)
		}
		if r.client == "claude" {
			out = append(out, codexToClaude()...)
		}
		if r.client == "gemini" {
			out = append(out, codexToGemini()...)
		}
		if r.client == "interactions" {
			out = append(out, codexToInteractions()...)
		}
	case "claude":
		out = append(out, claudeResponses()...)
		if r.client == "openai-response" {
			out = append(out, claudeToResponses()...)
			out = append(out, claudeApplyPatchToResponses()...)
		}
		if r.client == "interactions" {
			out = append(out, claudeToInteractions()...)
		}
		if r.client == "gemini" {
			out = append(out, claudeToGemini()...)
		}
	case "gemini":
		out = append(out, geminiUpstreamResponses()...)
		if r.client == "openai-response" {
			out = append(out, geminiToResponses()...)
		}
		if r.client == "interactions" {
			out = append(out, geminiToInteractions()...)
		}
	case "interactions":
		out = append(out, interactionsUpstreamResponses()...)
		if r.client == "openai" {
			out = append(out, interactionsToOpenAI()...)
		}
		if r.client == "openai-response" {
			out = append(out, interactionsToResponses()...)
		}
		if r.client == "claude" {
			out = append(out, interactionsToClaude()...)
		}
	case "openai-response":
		out = append(out, responsesToInteractions()...)
	case "antigravity":
		out = append(out, antigravityUpstreamResponses()...)
		if r.client == "openai-response" {
			out = append(out, antigravityToResponses()...)
		}
		if r.client == "interactions" {
			out = append(out, antigravityToInteractions()...)
		}
		if r.client == "claude" {
			out = append(out, antigravityToClaude()...)
		}
	case "openai":
		out = append(out, openAIResponses()...)
		if r.client == "claude" {
			out = append(out, openAIToClaudeResponses()...)
		}
		if r.client == "openai-response" {
			out = append(out, openAIToResponses()...)
		}
		if r.client == "gemini" {
			out = append(out, openAIToGemini()...)
		}
		if r.client == "interactions" {
			out = append(out, openAIToInteractions()...)
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

// geminiSig is a Gemini thought signature from Go's tests; carrier encodes it the way the
// Gemini Responses translator does.
const geminiSig = "EjQKMgEMOdbHO0Gd+c9Mxk4ELwPGbpCEcp2mFfYYLix2UVtBH3fL8GECc4+JITVnHF4qZDsA"

func carrier(direction, target, sig string) string {
	return "cpa-gemini-responses-carrier-v1:" + direction + ":" + target + ":" + base64.RawStdEncoding.EncodeToString([]byte(sig))
}

// responsesGeminiRequests exercise ConvertOpenAIResponsesRequestToGemini: tool
// declarations (namespaces, collisions, apply_patch, web search capability), tool choices,
// system/developer placement, media parts, tool outputs, reasoning carriers and
// generation settings.
func responsesGeminiRequests(model string) []fixture {
	tools := `[{"type":"function","name":"get.weather","description":"d <b>","parameters":{"type":"object","properties":{"city":{"type":"string","format":"city"}},"required":["city"],"additionalProperties":false}},{"type":"function","name":"get_weather"},{"type":"custom","name":"run","description":"r"},{"type":"custom","name":"apply_patch","description":"Use this. This is a FREEFORM tool, so do not wrap the patch in JSON.","format":{"type":"grammar"}},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list","parameters":{"type":"object"}},{"type":"custom","name":"exec"},{"type":"web_search"}]},{"type":"namespace","name":"ns__","children":[{"name":"child","function":{"description":"fd","parameters":{"type":"object","properties":{}}}}]},{"type":"web_search","filters":{"allowed_domains":[" a.com ","","b<c>.com"]}},{"type":"file_search"}]`
	simple := `[{"type":"function","name":"f"},{"type":"custom","name":"run"},{"type":"web_search_preview"}]`
	sigCarrierPrev := carrier("previous", "text", geminiSig)
	inputs := map[string]string{
		"tools/all":                  `{"input":"hi","tools":` + tools + `,"tool_choice":{"type":"function","name":"list","namespace":"mcp"}}`,
		"tools/additional":           `{"input":[{"type":"additional_tools","tools":[{"type":"function","name":"f","parameters":{"type":"object"}},{"type":"function","name":"g"}]},{"role":"user","content":"x"}],"tools":[{"type":"function","name":"f","description":"top"}]}`,
		"tools/collide":              `{"input":"x","tools":[{"type":"function","name":"a.b"},{"type":"function","name":"a_b"},{"type":"function","name":"a b"},{"type":"function","name":"` + strings.Repeat("x", 70) + `"},{"type":"function","name":"` + strings.Repeat("x", 69) + `."}]}`,
		"tools/no-functions-choice":  `{"input":"x","tools":[{"type":"web_search"}],"tool_choice":"required"}`,
		"tools/search-disallowed":    `{"input":"x","tools":[{"type":"web_search"}],"tool_choice":"none"}`,
		"messages/system-developer":  `{"instructions":"inst <i>","input":[{"role":"developer","content":[{"type":"input_text","text":"d1"},{"text":"d2"}]},{"role":"system","content":"s"},{"role":"user","content":"u"},{"role":"developer","content":"late dev"},{"role":"system","content":[{"type":"input_text","text":"  "}]},{"role":"Developer","content":["x",{"text":"y"}]},{"role":"user","content":"u2"}]}`,
		"messages/developer-pending": `{"input":[{"role":"user","content":"u"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"role":"developer","content":"while pending"},{"type":"function_call_output","call_id":"c1","output":"ok"},{"role":"user","content":"after"}]}`,
		"messages/roles":             `{"input":[{"role":"user","content":[{"type":"input_text","text":"a"},{"type":"output_text","text":"b"},{"text":"c"},{"type":"input_text"}]},{"role":"assistant","content":"plain"},{"role":"model","content":"m"},{"role":"tool","content":"t"},{"role":"ASSISTANT","content":[{"type":"output_text","text":"o"}]},{"type":"input_text","text":"flat1"},{"type":"input_image","image_url":"https://x/y.png"},{"type":"text","text":"flat2","role":"user"},{"type":"unknown"},{"role":"user","content":{"type":"input_text","text":"obj"}},{"role":"user","content":"tail"}]}`,
		"messages/prefill":           `{"input":[{"role":"user","content":"u"},{"role":"assistant","content":[{"type":"output_text","text":"prefill"}]}]}`,
		"messages/only-prefill":      `{"input":[{"role":"assistant","content":"only"}]}`,
		"media/images":               `{"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/jpeg;base64,/9j/4AAQ"},{"type":"input_image","image_url":{"url":"data:;base64,iVBORw0KGgo="},"filename":"pic.GIF"},{"type":"input_image","image_url":"data:image/png;base64,***"},{"type":"input_image","image_url":"data:image/png,raw"},{"type":"input_image","image_url":"HTTPS://h/a/b.JPG?x=1#f"},{"type":"input_image","image_url":"https://h/%zz.png"},{"type":"input_image","image_url":"gs://bucket/dir/"},{"type":"image","source":{"type":"base64","media_type":"image/webp","data":"UklG"}},{"type":"input_image","data":"AAAA","format":"heic"},{"type":"input_image","file_id":"file-1"},{"type":"input_image","image_url":"relative.bmp","mime_type":"application/octet-stream"}]}]}`,
		"media/audio-video":          `{"input":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"UklGRg==","format":"mp3"}},{"type":"input_audio","audio_url":"data:application/octet-stream;base64,AAAA","filename":"x.flac"},{"type":"audio","audio_url":{"url":"local.WAV"}},{"type":"input_audio","data":"AAAA","mime_type":"audio/ogg"},{"type":"input_audio","source":{"type":"base64","data":"AAAA","media_type":"opus"}},{"type":"input_audio","audio_url":"https://h/a.m4a"},{"type":"input_video","video_url":"data:;base64,AAAA","format":"mov"},{"type":"video","video":{"data":"AAAA","format":"MKV"}},{"type":"video_url","video_url":{"url":"https://h/v.webm"}},{"type":"input_video","url":"clip.3gpp"}]}]}`,
		"media/files":                `{"input":[{"role":"user","content":[{"type":"input_file","file_data":"data:application/octet-stream;base64,JVBERi0=","filename":"doc.pdf"},{"type":"input_file","file_data":"SGVsbG8=","filename":"a.TXT"},{"type":"input_file","file_url":"https://x/y/report.pdf?x=1"},{"type":"file","file":{"file_data":"AAAA","filename":"data","mime_type":"binary/octet-stream","format":"csv"}},{"type":"input_file","file_id":"file-2"},{"type":"input_file","url":"data:text/plain;BASE64,aGk"},{"type":"input_file","file_url":"https://x/"}]}]}`,
		"calls/outputs":              `{"tools":[{"type":"function","name":"f"},{"type":"custom","name":"run"},{"type":"namespace","name":"mcp","tools":[{"name":"list"}]}],"input":[{"role":"user","content":"go"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{\"a\":1}"},{"type":"function_call","call_id":"c2","name":"list","namespace":"mcp","arguments":"[1,2]"},{"type":"custom_tool_call","call_id":"c3","name":"run","input":"echo <x>"},{"type":"custom_tool_call","call_id":"c4","name":"run","input":{"k":1}},{"type":"custom_tool_call","call_id":"c5","name":"run"},{"type":"function_call","call_id":"c6","name":"f","arguments":"plain"},{"type":"function_call_output","call_id":"c3","output":"three"},{"type":"function_call_output","call_id":"c1","output":[{"type":"input_text","text":"t1"},{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="},{"type":"input_text","text":"t2"}]},{"type":"custom_tool_call_output","call_id":"c2","output":[{"type":"x","v":1},"s"]},{"type":"function_call_output","call_id":"c4","output":{"type":"input_image","image_url":"data:image/png;base64,iVBORw0KGgo="}},{"type":"function_call_output","call_id":"c5","output":{"$ref":"#/x","a":1}},{"type":"function_call_output","call_id":"c6","output":"null"},{"type":"function_call_output","call_id":"zz","name":"orph","output":[{"text":"orphan <o>"},{"text":" "}]},{"type":"function_call_output","output":5},{"role":"user","content":"done"}]}`,
		"calls/interrupted":          `{"input":[{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"type":"function_call","call_id":"c2","name":"g","arguments":"{}"},{"type":"function_call_output","call_id":"c2","output":""},{"role":"user","content":"next"},{"type":"function_call","call_id":"c3","name":"h.i","arguments":"{}"},{"role":"user","content":"again"},{"type":"function_call","name":"noid","arguments":"{}"},{"type":"function_call_output","call_id":"c9","output":true},{"type":"function_call_output","call_id":"c9","output":"dup"}]}`,
		"reasoning/paired":           `{"input":[{"role":"user","content":"u"},{"type":"reasoning","summary":[{"type":"summary_text","text":"think <t>"}],"encrypted_content":"` + geminiSig + `"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"ok"},{"type":"reasoning","summary":[{"type":"summary_text","text":"why"}],"encrypted_content":"` + geminiSig + `"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]},{"type":"reasoning","summary":[],"encrypted_content":"garbage"},{"type":"reasoning","summary":[{"type":"summary_text","text":"bypass"}],"encrypted_content":"skip_thought_signature_validator"},{"role":"user","content":"more"}]}`,
		"reasoning/carriers":         `{"input":[{"role":"user","content":"u"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"said"}]},{"type":"reasoning","id":"rs_1_detached_after_2","summary":[],"encrypted_content":"` + sigCarrierPrev + `"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"type":"reasoning","summary":[],"encrypted_content":"` + carrier("previous", "function", geminiSig) + `"},{"type":"function_call_output","call_id":"c1","output":"ok"},{"type":"reasoning","summary":[],"encrypted_content":"` + carrier("next", "function", geminiSig) + `"},{"type":"function_call","call_id":"c2","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"c2","output":"ok"},{"type":"reasoning","summary":[{"type":"summary_text","text":"s"}],"encrypted_content":"` + carrier("standalone", "any", geminiSig) + `"},{"type":"reasoning","summary":[],"encrypted_content":"` + carrier("sideways", "text", geminiSig) + `"},{"type":"reasoning","summary":[{"type":"summary_text","text":"bad b64"}],"encrypted_content":"cpa-gemini-responses-carrier-v1:next:text:!!!"},{"type":"reasoning","summary":[],"encrypted_content":"` + carrier("next", "text", "cpa-gemini-responses-carrier-v1:x") + `"},{"role":"user","content":"end"}]}`,
		"reasoning/internal-fields":  `{"input":[{"role":"user","content":"u"},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}","_cpa_reasoning_signature":"x","z":"<&>","a":{"b": 1},"a":2},{"type":"function_call_output","call_id":"c1","output":"ok","_cpa_reasoning_direction":"next"},{"type":"reasoning","summary":[{"type":"summary_text","text":"r"}],"_cpa_reasoning_target":"text"}]}`,
		"generation/settings":        `{"input":"x","max_output_tokens":"12","temperature":"0.5","top_p":1e0,"stop_sequences":["a","<b>",1e3],"text":{"format":{"type":"json_schema","schema":{"type":"object"}}},"reasoning":{"effort":" HIGH "}}`,
		"generation/variants":        `{"input":"x","stop_sequences":[],"text":{"format":{"type":"JSON_SCHEMA","json_schema":{"schema":{"a":1}}}},"reasoning":{"effort":"auto"}}`,
		"generation/others":          `{"input":"x","stop_sequences":"x","text":{"format":{"type":"json_object"}},"reasoning":{"effort":""},"instructions":""}`,
		"invalid/number-input":       `{"input":5,"instructions":"i"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, req("responses-gemini/"+name, model, inputs[name], false))
	}
	for _, m := range []string{"claude-opus-4-6", "unknown-model", "gemini-3-pro-preview(high)"} {
		for _, name := range []string{"tools/all", "reasoning/paired", "reasoning/carriers"} {
			out = append(out, req("responses-gemini/"+name+"@"+m, m, inputs[name], true))
		}
	}
	for i, choice := range []string{`"none"`, `"AUTO"`, `"required"`, `"any"`, `"bogus"`, `{"type":"custom","custom":{"name":"run"}}`, `{"type":"tool","function":{"name":"f","namespace":"n"}}`, `{"type":"allowed_tools","tools":[{"type":"web_search"}]}`, `{"type":"web_search_preview"}`, `{}`, `5`, `{"type":"function"}`, `{"type":"NONE"}`} {
		out = append(out, req(fmt.Sprintf("responses-gemini/tool-choice/%d", i), model, `{"input":"x","tools":`+simple+`,"tool_choice":`+choice+`}`, false))
	}
	return out
}

// geminiToResponses exercise ConvertGeminiResponseToOpenAIResponses(NonStream): thought
// signature placement, tool identities, apply_patch input bridging (success, invalid
// input, conflicts, EOF without a terminator), web search grounding and citations.
func geminiToResponses() []fixture {
	usage := `"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"thoughtsTokenCount":3,"cachedContentTokenCount":4,"totalTokenCount":20}`
	chunk := func(id, parts, extra string) string {
		return `data: {"candidates":[{"content":{"role":"model","parts":` + parts + `}` + extra + `}],"responseId":"` + id + `","createTime":"2025-01-01T00:00:00Z"}`
	}
	tools := `"tools":[{"type":"function","name":"f"},{"type":"function","name":"a.b"},{"type":"custom","name":"run"},{"type":"custom","name":"apply_patch"},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list"}]}]`
	original := `{"model":"client-model","instructions":"i","input":"q","max_output_tokens":9,"reasoning":{"effort":"high"},"metadata":{"k":"<v>"},` + tools + `}`
	plain := `{"model":"client-model","input":"q","tools":[{"type":"function","name":"f"}]}`
	search := `{"model":"client-model","input":[{"role":"user","content":"find <x>"}],"tools":[{"type":"web_search"}]}`
	patch := `{"input":"*** Begin Patch\n*** Add File: a.txt\n+hi \u00e9 <x>\n*** End Patch"}`
	stop := `,"finishReason":"STOP"`
	grounding := func(queries, chunks, supports string) string {
		return `,"groundingMetadata":{"webSearchQueries":` + queries + `,"groundingChunks":` + chunks + `,"groundingSupports":` + supports + `}`
	}
	type sc struct {
		original string
		finalize bool
		lines    []string
	}
	cases := map[string]sc{
		"text":              {original, false, []string{chunk("t1", `[{"text":"Hel"}]`, ``), chunk("t1", `[{"text":"lo <b> \u2028"}]`, ``), `data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"responseId":"t1",` + usage + `}`}},
		"thought-text":      {original, false, []string{chunk("t2", `[{"text":"think","thought":true}]`, ``), chunk("t2", `[{"text":"more","thought":true,"thoughtSignature":"`+geminiSig+`"}]`, ``), chunk("t2", `[{"text":"answer"}]`, ``), chunk("t2", `[{"text":""}]`, stop)}},
		"trailing-cache":    {original, false, []string{chunk("trail-1", `[{"text":"Hello"}]`, ``), chunk("trail-1", `[{"text":"","thoughtSignature":"`+geminiSig+`"}]`, stop)}},
		"trailing-detached": {original, false, []string{chunk("t3", `[{"text":"think","thought":true}]`, ``), chunk("t3", `[{"text":"Hi","thoughtSignature":"other-signature-value"}]`, ``), chunk("t3", `[{"functionCall":{"name":"f","args":{"a":1}},"thoughtSignature":"`+geminiSig+`"}]`, ``), chunk("t3", `[{"text":"","thoughtSignature":"sig-two"}]`, stop)}},
		"tools":             {original, false, []string{chunk("t4", `[{"functionCall":{"name":"f","args":{"q":"<x>&"}}},{"functionCall":{"name":"a_b","args":{}}},{"functionCall":{"name":"run","args":{"input":"ls"}}},{"functionCall":{"name":"mcp__list"}},{"functionCall":{"args":{"x":1}}},{"functionCall":{"name":"unknown_tool","args":[1]}}]`, ``), "data: [DONE]"}},
		"patch-ok":          {original, false, []string{chunk("p1", `[{"text":"patching"},{"functionCall":{"id":"up-1","name":"apply_patch","args":`+patch+`}}]`, ``), chunk("p1", `[{"functionCall":{"id":"up-1","name":"apply_patch","args":`+patch+`}}]`, stop)}},
		"patch-invalid":     {original, false, []string{chunk("p2", `[{"functionCall":{"name":"apply_patch","args":{"input":5}}}]`, ``), chunk("p2", `[{"text":"after"}]`, stop)}},
		"patch-conflict":    {original, false, []string{chunk("p3", `[{"functionCall":{"id":"u","name":"apply_patch","args":{"input":"a"}}}]`, ``), chunk("p3", `[{"functionCall":{"id":"u","name":"apply_patch","args":{"input":"b"}}}]`, stop)}},
		"patch-nameless":    {original, false, []string{chunk("p4", `[{"functionCall":{"args":{"input":"a"}}}]`, ``), chunk("p4", `[]`, stop)}},
		"patch-eof":         {original, true, []string{chunk("p5", `[{"text":"no terminator"}]`, ``)}},
		"plain-eof":         {plain, true, []string{chunk("p6", `[{"text":"no terminator"}]`, ``)}},
		"search":            {search, false, []string{chunk("s1", `[{"text":"Answer "}]`, ``), chunk("s1", `[{"text":"é text"}]`, grounding(`["q1"]`, `[{"web":{"uri":"https://a","title":""}},{"web":{"uri":"https://b","title":"B"}}]`, `[{"segment":{"startIndex":0,"endIndex":6},"groundingChunkIndices":[0,1]}]`)), chunk("s1", `[]`, grounding(`["q1","q2"]`, `[{"web":{"uri":"https://a","title":"A"}},{"web":{"uri":"https://c"}}]`, `[{"segment":{"partIndex":1,"startIndex":0,"endIndex":7},"groundingChunkIndices":[3]}]`)+stop)}},
		"search-multi":      {search, false, []string{chunk("s2", `[{"text":"one"}]`, ``), chunk("s2", `[{"text":"t","thought":true}]`, ``), chunk("s2", `[{"text":"two"}]`, grounding(`[]`, `[{"web":{"uri":"https://x","title":"X"}}]`, `[{"segment":{"startIndex":0,"endIndex":3},"groundingChunkIndices":[0,9,-1]}]`)), chunk("s2", `[]`, stop)}},
		"wrapped":           {"", false, []string{`data: {"response":{"candidates":[{"content":{"parts":[{"text":"w"}]},"finishReason":"STOP"}],"responseId":"resp_w"}}`}},
		"done-only":         {original, false, []string{"data: [DONE]", chunk("d1", `[{"text":"x"}]`, ``), "data: [DONE]", chunk("d1", `[{"text":"late"}]`, ``)}},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		c := cases[name]
		f := streamCase("gemini-responses/"+name, "gemini-2.5-pro", c.lines...)
		f.Original = c.original
		f.Finalize = c.finalize
		out = append(out, f)
	}
	// The trailing signature cached above restores on the next request for this message.
	out = append(out, req("gemini-responses/trailing-restore", "gemini-2.5-pro", `{"input":[{"role":"user","content":"hi"},{"type":"message","id":"msg_resp_trail-1_0","role":"assistant","content":[{"type":"output_text","text":"Hello"}]},{"type":"reasoning","summary":[],"encrypted_content":"`+carrier("previous", "text", geminiSig)+`"},{"role":"user","content":"next"}]}`, false))
	body := func(id, parts, extra string) string {
		return `{"candidates":[{"content":{"role":"model","parts":` + parts + `},"finishReason":"STOP"` + extra + `}],` + usage + `,"modelVersion":"gemini-2.5-pro","responseId":"` + id + `","createTime":"2025-01-01T00:00:00Z"}`
	}
	for i, b := range []struct{ original, body string }{
		{original, body("n1", `[{"text":"think","thought":true},{"text":"Hi","thoughtSignature":"`+geminiSig+`"},{"text":" there"},{"thoughtSignature":"sig-x"},{"functionCall":{"name":"f","args":{"a":"<b>"}},"thoughtSignature":"sig-f"},{"thoughtSignature":"sig-after-call"}]`, ``)},
		{original, body("n2", `[{"text":"t1","thought":true,"thoughtSignature":"s1"},{"text":"t2","thought":true,"thoughtSignature":"s2"},{"text":"x","thoughtSignature":"s3"},{"text":"y","thoughtSignature":"s3"},{"text":"z","thoughtSignature":"s4"},{"functionCall":{"name":"run","args":{"input":"ls"}}},{"functionCall":{"name":"a_b"}},{"functionCall":{"name":"mcp__list","args":{}}}]`, ``)},
		{original, body("n3", `[{"functionCall":{"id":"u1","name":"apply_patch","args":`+patch+`}}]`, ``)},
		{original, body("n4", `[{"functionCall":{"name":"apply_patch","args":{"input":5}}}]`, ``)},
		{original, body("n5", `[{"functionCall":{"args":{"input":"x"}}}]`, ``)},
		{search, body("n6", `[{"text":"Answer é"},{"text":" more"}]`, grounding(`[" q "]`, `[{"web":{"uri":"https://a","title":"A"}},{"web":{"uri":"https://a"}}]`, `[{"segment":{"partIndex":1,"startIndex":0,"endIndex":3},"groundingChunkIndices":[0]},{"segment":{"startIndex":0,"endIndex":8},"groundingChunkIndices":[1]}]`))},
		{`{"input":"no model"}`, `{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"modelVersion":"mv","usageMetadata":{"promptTokenCount":1}}`},
		{``, `{"response":{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"responseId":"resp_r"},"modelVersion":"outer"}`},
		{plain, `{"candidates":[{"content":{"parts":[{"thoughtSignature":"lonely"}]}}]}`},
	} {
		n := nonStream(fmt.Sprintf("gemini-responses/non-stream/%d", i), "gemini-2.5-pro", b.body)
		n.Original = b.original
		out = append(out, n)
	}
	return out
}

// grokContent is high-entropy unpadded base64, shaped like Grok encrypted reasoning.
func grokContent() string {
	raw := make([]byte, 64)
	for i := range raw {
		raw[i] = byte(i*37 + 11)
	}
	return base64.RawStdEncoding.EncodeToString(raw)
}

// claudeCodexRequests exercise ConvertClaudeRequestToCodex beyond the shared Claude
// matrix: name shortening, long call IDs, web search tools, service tiers, structured
// output strictness, documents and Grok reasoning signatures.
func claudeCodexRequests(model string) []fixture {
	long := strings.Repeat("x", 70)
	longID := "toolu " + strings.Repeat("y", 80)
	grok := grokContent()
	inputs := map[string]string{
		"names":       `{"tools":[{"name":"mcp__server_with_a_very_long_name_that_goes_on_and_on__read_file_contents_now"},{"name":"` + long + `"},{"name":"` + long + `b"},{"name":"short"},{"name":"short"},{"type":"custom","name":"c"},{"name":5}],"tool_choice":{"type":"tool","name":"` + long + `"},"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"` + longID + `","name":"` + long + `","input":{"a":1}},{"type":"tool_use","id":"t","name":"unknown_` + long + `","input":"str"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"` + longID + `","content":[{"type":"text","text":"r"},{"type":"image","source":{"type":"base64","mime_type":"image/gif","base64":"R0lG"}},{"type":"image","source":{"data":"AA"}},{"type":"document"}]},{"type":"tool_result","tool_use_id":"t","content":[{"type":"other"}]}]}]}`,
		"web-search":  `{"tools":[{"type":"web_search_20250305","name":"web_search","allowed_domains":["a.com"],"user_location":{"type":"approximate","city":"X"}},{"type":"web_search_20260209","name":"ws2","allowed_domains":"notarray","user_location":"str"},{"name":"f","input_schema":{"type":"object"},"strict":true,"cache_control":{"type":"ephemeral"},"defer_loading":true,"type":"custom"},{"name":"g","input_schema":{"$schema":"x","$id":"y","type":"object","properties":null},"strict":false}],"tool_choice":{"type":"tool","name":"ws2"},"messages":[]}`,
		"documents":   `{"messages":[{"role":"user","content":[{"type":"document","source":{"type":"base64","media_type":"application/pdf","data":"JVBE"}},{"type":"document","source":{"type":"base64","media_type":" Application/PDF ","base64":"QQ"}},{"type":"document","source":{"type":"url","url":"https://x/a.pdf"}},{"type":"document","source":{"type":"base64","media_type":"text/plain","data":"aGk"}},{"type":"image","source":{"type":"base64","data":"iVBO"}}]}]}`,
		"format/miss": `{"output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"object","properties":{"c":{}},"required":["c"]}},"required":["a"]}}},"messages":[]}`,
		"format/full": `{"output_config":{"format":{"type":"json_schema","name":"n<1>","schema":{"type":"object","properties":{"a":{"type":"array","items":{"type":"object","properties":{"z":{}},"required":["z"]}}},"required":["a",5]}}},"messages":[]}`,
		"format/off":  `{"output_config":{"format":{"type":"json_schema","strict":false,"schema":{"type":"object"}}},"messages":[]}`,
		"format/deep": `{"output_config":{"format":{"type":"json_schema","strict":"no","schema":{"$defs":{"d":{"properties":{"q":{}}}},"anyOf":[{"properties":{"r":{}},"required":[]}]}}},"messages":[]}`,
		"format/bad":  `{"output_config":{"format":{"type":"json_schema","schema":[1]}},"messages":[]}`,
		"grok":        `{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"g","signature":"` + grok + `"},{"type":"thinking","thinking":"short","signature":"QUJD"},{"type":"text","text":"t"}]}]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		for _, m := range []string{model, "grok-4.7(high)"} {
			out = append(out, req("codex-claude/"+name+"@"+m, m, inputs[name], false))
		}
	}
	out = append(out, fixture{Name: "compat/codex-claude/grok", Path: "request_compat", Model: "grok-4.7", Input: inputs["grok"]})
	for i, tier := range []string{`"service_tier":"fast"`, `"service_tier":" Priority "`, `"service_tier":"flex"`, `"service_tier":5`, `"service_tier":"flex","speed":"fast"`, `"speed":"FAST"`, `"speed":true`} {
		out = append(out, req(fmt.Sprintf("codex-claude/tier/%d", i), model, `{`+tier+`,"messages":[]}`, false))
	}
	return out
}

// codexToClaude exercise ConvertCodexResponseToClaude(NonStream): reasoning summary parts
// and signatures, events deferred behind an open function call, terminal-only calls,
// web search blocks, errors and stop reasons.
func codexToClaude() []fixture {
	ev := func(body string) string { return "data: " + body }
	long := strings.Repeat("x", 70)
	created := ev(`{"type":"response.created","response":{"id":"resp_c","model":"gpt-5.3-codex"}}`)
	usage := `"usage":{"input_tokens":20,"output_tokens":5,"input_tokens_details":{"cached_tokens":3,"cache_creation_tokens":4},"output_tokens_details":{"reasoning_tokens":9}}`
	cases := map[string][]string{
		"reasoning": {created,
			ev(`{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","encrypted_content":"early"}}`),
			ev(`{"type":"response.reasoning_summary_part.added","output_index":0}`), ev(`{"type":"response.reasoning_summary_text.delta","delta":"part one <x>"}`), ev(`{"type":"response.reasoning_summary_part.done"}`),
			ev(`{"type":"response.reasoning_summary_part.added","output_index":0}`), ev(`{"type":"response.reasoning_summary_text.delta","delta":"part two"}`),
			ev(`{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"final-sig"}}`),
			ev(`{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"only-sig"}}`), ev(`{"type":"response.output_item.done","item":{"type":"reasoning"}}`),
			ev(`{"type":"response.output_item.added","item":{"type":"reasoning"}}`), ev(`{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":""}}`),
			ev(`{"type":"response.content_part.added","part":{"type":"output_text"}}`), ev(`{"type":"response.output_text.delta","delta":"Hello é"}`), ev(`{"type":"response.content_part.done","part":{"type":"output_text"}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"ignored"}]}}`),
			ev(`{"type":"response.completed","response":{"stop_reason":"stop","stop_sequence":"<END>",` + usage + `}}`)},
		"message-done": {created, ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"refusal"},{"type":"output_text","text":"a"},{"type":"output_text","text":"b"}]}}`), ev(`{"type":"response.output_item.done","item":{"type":"message","content":"str"}}`), ev(`{"type":"response.completed","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":2,"input_tokens_details":{"cached_tokens":5}}}}`)},
		"deferred": {created,
			ev(`{"type":"response.output_item.added","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call 1 ` + long + `","name":"` + long + `"}}`),
			ev(`{"type":"response.output_text.delta","delta":"while open"}`),
			ev(`{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"a\":"}`),
			ev(`{"type":"response.output_item.added","output_index":2,"item":{"id":"fc_2","type":"function_call","call_id":"call_2"}}`),
			ev(`{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{}"}`),
			ev(`{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"a\":1}"}`),
			ev(`{"type":"response.output_item.done","output_index":1,"item":{"id":"fc_1","type":"function_call","call_id":"call 1 ` + long + `","arguments":"{\"a\":1}"}}`),
			ev(`{"type":"response.output_item.done","output_index":2,"item":{"id":"fc_2","type":"function_call","call_id":"call_2","name":"mcp__server_with_a_very_long_name_that_goes_on_and_on__read_file_contents_now","arguments":"{\"b\":2}"}}`),
			ev(`{"type":"response.function_call_arguments.delta","delta":"orphan"}`),
			ev(`{"type":"response.completed","response":{"output":[{"type":"function_call","call_id":"call_3","name":"late","arguments":"{}"},{"type":"function_call","call_id":"call_4","arguments":"{}"}]}}`)},
		"terminal-calls": {created, ev(`{"type":"response.output_item.added","output_index":0,"item":{"id":"fc_9","type":"function_call","call_id":"c9"}}`), ev(`{"type":"response.output_text.delta","delta":"held"}`), ev(`{"type":"response.incomplete","response":{"output":{"x":{"type":"function_call","call_id":"c9","name":"named_late","arguments":"{\"k\":1}"}}}}`)},
		"web-search": {created,
			ev(`{"type":"response.web_search_call.searching","item_id":"ws_1"}`),
			ev(`{"type":"response.output_item.done","item":{"id":"ws_1","type":"web_search_call","action":{"type":"search","query":"q <1>"},"results":[{"url":" https://a ","title":""},{"url":""},{"url":"https://b","title":"B"}]}}`),
			ev(`{"type":"response.output_item.done","item":{"id":"ws_1","type":"web_search_call","action":{"query":"again"}}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"web_search_call","action":{}}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"web_search_call"},"query":"root q","results":[]}`),
			ev(`{"type":"response.completed","response":{"stop_reason":"pause_turn"}}`)},
		"errors":       {ev(`{"type":"error","error":{"type":"","code":"cyber_policy","message":"  "}}`), ev(`{"type":"error","error_type":"invalid_request","message":"m <x>"}`), ev(`{"type":"error"}`), ev(`{"type":"error","error":{"code":"rate","message":"slow"}}`)},
		"stop-reasons": {created, ev(`{"type":"response.completed","response":{"stop_reason":"content_filter"}}`), ev(`{"type":"response.completed","response":{"stop_reason":"tool_calls","stop_sequence":""}}`), ev(`{"type":"response.incomplete","response":{"stop_sequence":5,"usage":{"output_tokens":3,"output_tokens_details":{"reasoning_tokens":-1}}}}`), ev(`{"type":"response.completed","response":{"stop_reason":"weird","usage":{"output_tokens":"8","output_tokens_details":{"reasoning_tokens":2.5}}}}`)},
		"framing":      {"event: x", "", created, "data:{\"type\":\"response.output_text.delta\",\"delta\":\"tight\"}", "data: not json", "data: {\"type\":\"response.output_text.delta\",\"delta\":\"bad\xff\"}", "data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	original := `{"tools":[{"name":"` + long + `"},{"name":"mcp__server_with_a_very_long_name_that_goes_on_and_on__read_file_contents_now"},{"name":"late"}],"messages":[]}`
	var out []fixture
	for _, name := range names {
		f := streamCase("codex-claude/"+name, "gpt-5.3-codex", cases[name]...)
		f.Original = original
		out = append(out, f)
	}
	for i, body := range []string{
		`{"type":"response.completed","response":{"id":"r1","model":"m",` + usage + `,"stop_reason":"stop","stop_sequence":"\n\n","output":[{"type":"reasoning","encrypted_content":"sig","summary":[{"type":"summary_text","text":"s1"},"s2",{"x":1}]},{"type":"reasoning","summary":"plain summary"},{"type":"reasoning","summary":[],"content":[{"text":"c1"},{"type":"reasoning_text","text":"c2"}]},{"type":"reasoning","content":"str content"},{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"t <1>"},{"type":"output_text","text":""}]},{"type":"message","content":"string message"},{"type":"web_search_call","id":"ws","action":{"query":"q"},"results":[{"url":"https://a"}]},{"type":"web_search_call","id":"ws","action":{"query":"dup"}},{"type":"web_search_call","action":{"query":"no id"}},{"type":"web_search_call","id":"ws2"},{"type":"function_call","call_id":"call id ` + long + `","name":"` + long + `","arguments":"{\"a\":1}  "},{"type":"function_call","call_id":"c2","name":"x","arguments":"[1]"},{"type":"function_call","name":"y","arguments":"not json"}]}}`,
		`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"content_filter"},"output":[]}}`,
		`{"type":"response.completed","response":{"output":{"type":"message"}}}`,
		`{"type":"response.created","response":{}}`, `{"type":"response.completed"}`, `not json`, ``,
	} {
		n := nonStream(fmt.Sprintf("codex-claude/non-stream/%d", i), "gpt-5.3-codex", body)
		n.Original = original
		out = append(out, n)
	}
	return out
}

// geminiCodexRequests exercise ConvertGeminiRequestToCodex: call ID pairing, media
// parts, thinking configs, tool configs and nested type lowercasing.
func geminiCodexRequests(model string) []fixture {
	long := strings.Repeat("f", 70)
	inputs := map[string]string{
		"pairing":  `{"contents":[{"role":"model","parts":[{"functionCall":{"name":"a","args":{"x":1}}},{"functionCall":{"name":"b","id":" given "}},{"functionCall":{"name":"c","call_id":"cid"}}]},{"role":"user","parts":[{"functionResponse":{"name":"c","call_id":"cid","response":{"result":"rc"}}},{"functionResponse":{"name":"a","response":{"result":{"k":"<v>"}}}},{"functionResponse":{"name":"b","response":{"other":1}}},{"functionResponse":{"name":"z"}},{"functionResponse":{"name":"y","id":"nope","response":"s"}}]},{"role":"model","parts":[{"thought":true,"text":"hidden"},{"text":"visible","thoughtSignature":"s"},{"functionCall":{"name":"` + long + `","args":"str"}}]}],"tools":[{"functionDeclarations":[{"name":"` + long + `"},{"name":"` + long + `"},{"name":""},{"description":"no name"}]}]}`,
		"media":    `{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"IMAGE/PNG","data":"iVBO"}},{"inline_data":{"mime_type":"audio/x-wav","data":"UklG"}},{"inlineData":{"mimeType":"audio/L16","data":"AA"}},{"inlineData":{"mimeType":"audio/mpeg","data":"AA"}},{"inlineData":{"mimeType":"application/pdf","data":"JVBE"}},{"inlineData":{"mimeType":" Text/CSV ","data":"YQ"}},{"inlineData":{"mimeType":"video/mp4","data":"AA"}},{"inlineData":{"mimeType":"","data":"AA"}},{"inlineData":{"mimeType":"image/png"}},{"fileData":{"mimeType":"image/jpeg","fileUri":"gs://b/i.jpg"}},{"file_data":{"mime_type":"video/webm","file_uri":"https://v"}},{"fileData":{"mimeType":"text/xml","fileUri":"u"}},{"fileData":{"mimeType":"model/gltf","fileUri":"m <x>"}},{"fileData":{"fileUri":"nomime"}},{"fileData":{"mimeType":"image/png"}},{"executableCode":{"code":"x"}}]},{"parts":"notarray"},{"role":"function","parts":[{"text":"fn role"}]}]}`,
		"system":   `{"systemInstruction":{"parts":[{"text":"s1 <b>"},{"text":"t","thought":true},{"inlineData":{}},{"text":""}]},"system_instruction":{"parts":[{"text":"snake wins"}]},"contents":[]}`,
		"system2":  `{"systemInstruction":{"parts":[{"text":"camel"}]},"contents":[]}`,
		"tools":    `{"tools":[{"functionDeclarations":[{"name":"a","description":"d","parameters":{"$schema":"x","type":"OBJECT","properties":{"p":{"type":"STRING","items":{"type":"Number"}}},"additionalProperties":true}},{"name":"b","parametersJsonSchema":{"type":"object","additionalProperties":false}},{"name":"c","parameters":"str"}]},{"googleSearch":{}},{"functionDeclarations":"x"}],"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["` + long + `"]}},"contents":[]}`,
		"no-tools": `{"toolConfig":{"functionCallingConfig":{"mode":"AUTO"}},"service_tier":" FAST ","contents":[]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, req("gemini-codex/"+name, model, inputs[name], false))
	}
	for i, cfg := range []string{`{"mode":"NONE"}`, `{"mode":"AUTO"}`, `{"mode":"ANY"}`, `{"mode":"ANY","allowedFunctionNames":["a","b"]}`, `{"mode":"ANY","allowedFunctionNames":"a"}`, `{"mode":"any"}`, `{}`} {
		out = append(out, req(fmt.Sprintf("gemini-codex/tool-config/%d", i), model, `{"tools":[{"functionDeclarations":[{"name":"a"}]}],"toolConfig":{"functionCallingConfig":`+cfg+`},"contents":[]}`, false))
	}
	for i, gen := range []string{`{"thinkingLevel":" HIGH "}`, `{"thinking_level":"low","thinkingConfig":{"thinkingBudget":0}}`, `{"thinkingLevel":""}`, `{"thinkingConfig":{"thinkingLevel":"Medium"}}`, `{"thinkingConfig":{"thinking_level":"  "}}`, `{"thinkingConfig":{"thinkingBudget":-1}}`, `{"thinkingConfig":{"thinkingBudget":0}}`, `{"thinkingConfig":{"thinking_budget":30000}}`, `{"thinkingConfig":{"thinkingBudget":"5000"}}`, `{"thinkingConfig":"x"}`, `{}`, `null`} {
		out = append(out, req(fmt.Sprintf("gemini-codex/thinking/%d", i), model, `{"generationConfig":`+gen+`,"contents":[]}`, false))
	}
	return out
}

// codexToGemini exercise ConvertCodexResponseToGemini(NonStream): stored function calls,
// image dedupe, final-message fallback, created_at and incomplete reasons.
func codexToGemini() []fixture {
	ev := func(body string) string { return "data: " + body }
	long := strings.Repeat("f", 70)
	cases := map[string][]string{
		"calls": {ev(`{"type":"response.created","response":{"id":"r1","model":"gpt-5","created_at":1700000000}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"function_call","name":"` + long[:64] + `","call_id":" c1 ","arguments":"{\"a\":\"<b>\"}"}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"function_call","name":"second","id":"i2","arguments":"[1]"}}`),
			ev(`{"type":"response.output_text.delta","delta":"after"}`),
			ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"skipped"}]}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"function_call","name":"last","arguments":""}}`),
			ev(`{"type":"response.incomplete","response":{"created_at":-1,"usage":{"input_tokens":3,"output_tokens":"4"},"incomplete_details":{"reason":"content_filter"}}}`)},
		"fallback": {ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"a"},{"type":"refusal","text":"r"},{"type":"output_text","text":""},{"type":"output_text","text":"b"}]}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"again"}]}}`),
			ev(`{"type":"response.reasoning_summary_text.delta","delta":"th"}`), ev(`{"type":"response.unknown"}`),
			ev(`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_tokens"}}}`)},
		"images": {ev(`{"type":"response.image_generation_call.partial_image","item_id":"ig","partial_image_b64":"AAA","output_format":"WEBP"}`), ev(`{"type":"response.image_generation_call.partial_image","item_id":"ig","partial_image_b64":"AAA"}`),
			ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call","id":"ig","result":"AAA"}}`), ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call","id":"ig","result":"BBB","output_format":"image/heic"}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"CCC","output_format":"tiff"}}`), ev(`{"type":"response.completed","response":{"created_at":253402300800}}`)},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	original := `{"tools":[{"functionDeclarations":[{"name":"` + long + `"}]}]}`
	var out []fixture
	for _, name := range names {
		f := streamCase("codex-gemini/"+name, "gemini-2.5-pro", cases[name]...)
		f.Original = original
		out = append(out, f)
	}
	for i, body := range []string{
		`{"type":"response.completed","response":{"id":"r","created_at":1700000000,"usage":{"input_tokens":5,"output_tokens":7},"output":[{"type":"function_call","name":"` + long[:64] + `","call_id":"c","arguments":"{\"q\":1}"},{"type":"function_call","name":"g","arguments":"5"},{"type":"reasoning","content":[{"text":"r"}]},{"type":"reasoning"},{"type":"message","content":[{"type":"output_text","text":"t"},{"type":"output_text"}]},{"type":"function_call","name":"h","id":"hid"},{"type":"image_generation_call","result":"AAA","output_format":"gif"},{"type":"image_generation_call","result":""}]}}`,
		`{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"output":[]}}`,
		`{"type":"response.incomplete"}`, `{"type":"response.created"}`, `not json`,
	} {
		n := nonStream(fmt.Sprintf("codex-gemini/non-stream/%d", i), "gemini-2.5-pro", body)
		n.Original = original
		out = append(out, n)
	}
	return out
}

// interactionsRequests are Interactions client bodies: input shapes and steps, content
// parts, system instructions, generation configs, tools and top-level fields.
func interactionsRequests(model string) []fixture {
	long := strings.Repeat("n", 70)
	inputs := map[string]string{
		"input/string":   `{"input":"hello <b>","stream":true}`,
		"input/steps":    `{"input":["plain",{"type":"user_input","content":"u"},{"type":"user_input","content":[{"type":"text","text":"t1"},{"type":"image","url":"https://i/a.png"},{"text":"t2"}]},{"type":"model_output","content":[{"type":"text","text":"m"}]},{"type":"thought","content":[{"text":"a"},{"text":""},{"text":"b"}],"id":"th1"},{"type":"reasoning","text":"r"},{"type":"thought"},{"type":"function_call","name":"` + long + `","id":" fc1 ","arguments":{"a":"<x>"}},{"type":"function_call","name":"g","call_id":"c2","args":"{\"s\":1}"},{"type":"function_call"},{"type":"function_result","call_id":"c2","result":{"ok":true}},{"type":"function_call_output","id":"fc1","output":"done"},{"type":"function_result","name":"x"},{"type":"message","role":"system","content":"sys"},{"type":"MESSAGE","role":" Model ","text":"txt"},{"role":"developer","steps":[{"type":"user_input","content":"nested dev"},{"role":"user","steps":["deep"]}]},{"type":"other","role":"assistant","content":{"type":"text","text":"obj"}},{"type":"other"}]}`,
		"input/object":   `{"input":{"role":"model","steps":[{"type":"user_input","content":"as model"},"bare"]}}`,
		"input/single":   `{"input":{"type":"user_input","content":[{"type":"text","text":"one"}]}}`,
		"input/none":     `{"input":5}`,
		"parts/media":    `{"input":[{"type":"user_input","content":[{"type":"image","file_uri":"gs://f"},{"type":"image","fileUri":"","mime_type":"image/png","data":"iVBO"},{"type":"image","mimeType":"image/gif"},{"type":"image_url","image_url":{"url":"https://u"}},{"type":"audio","mime_type":"audio/flac","data":"ZkxhQw"},{"type":"audio","data":"x"},{"type":"input_audio","input_audio":{"data":"d","format":"wav"}},{"type":"input_audio"},{"type":"video","file":{"file_data":"AA","filename":"v.mp4"}},{"type":"document","uri":"x","url":"https://d/a.pdf","mimeType":"application/pdf"},{"type":"file","file_uri":"","url":"u","mime_type":"text/csv"},{"type":"file","mime_type":"application/json","data":"e30"},{"type":"file"}]}]}`,
		"parts/inline":   `{"input":[{"type":"user_input","content":[{"type":"blob","inline_data":{"mime_type":"image/png","data":"iVBO"}},{"type":"blob","inlineData":{"mimeType":"audio/x-wav","data":"UklG"}},{"type":"blob","inline_data":{"mime_type":"application/pdf","data":"JVBE"}},{"type":"blob","inline_data":{"mime_type":"image/svg+xml; name=\"<a>\u00e9\u2028\"","data":"PHN2Zz4="}},{"type":"blob","inline_data":{"mime_type":"image/png"}},{"type":"blob","file_data":{"mime_type":"image/jpeg","file_uri":"gs://i.jpg"}},{"type":"blob","fileData":{"mimeType":"video/mp4","fileUri":"gs://v"}},{"type":"blob","file_data":{"mime_type":"text/plain"}},{"inline_data":{"mime_type":"image/png","data":"dropped"}},{"type":"text"},{"type":"unknown"}]}]}`,
		"system/string":  `{"system_instruction":"sys <x>","input":"x"}`,
		"system/text":    `{"systemInstruction":{"text":"camel text","parts":[{"text":"ignored"}]},"input":"x"}`,
		"system/parts":   `{"system_instruction":{"text":5,"parts":[{"text":"p1"},{"text":""},{"inline":1},{"text":"p2"}]},"input":"x"}`,
		"system/empty":   `{"system_instruction":{"parts":[{"text":""}]},"input":"x"}`,
		"tools/decls":    `{"tools":[{"function_declarations":[{"name":"a <b>","description":"d & e","parameters":{"$schema":"x","type":"object","additionalProperties":true,"properties":{"q":{"type":"string","description":"<q>"}}}},{"name":"` + long + `","parametersJsonSchema":{"type":"object","additionalProperties":false}},{"name":"c","parameters_json_schema":{"type":"object"}},{"description":"no name"}]},{"functionDeclarations":[{"name":"d","description":5}]},{"name":"direct","parameters":{"type":"object"}},{"googleSearch":{}},{"function_declarations":"notarray"}],"input":"x"}`,
		"tools/badparam": `{"tools":[{"name":"x","parameters":"str"}],"input":"x"}`,
		"tools/nonames":  `{"tools":[{"googleSearch":{}}],"input":"x"}`,
		"tools/object":   `{"tools":{"name":"obj"},"tool_choice":"none","input":"x"}`,
		"top-level":      `{"service_tier":" Fast ","tool_choice":{"type":"function","name":"a"},"parallel_tool_calls":false,"store":true,"metadata":{"k":"<v>"},"include":["reasoning.encrypted_content"],"truncation":"auto","input":"x","tools":[{"name":"a"}]}`,
		"top-level/2":    `{"service_tier":"flex","generation_config":{"service_tier":"priority","tool_choice":"auto"},"tool_choice":"auto","input":"x"}`,
		"reasoning/top":  `{"reasoning":{"effort":"high"},"input":"x"}`,
		"invalid":        `not json`,
	}
	for i, cfg := range []string{
		`{"thinking_level":" HIGH "}`, `{"thinkingLevel":"","thinking_config":{"thinking_level":"low"}}`, `{"thinkingConfig":{"thinkingLevel":"Medium"}}`, `{"reasoning":{"effort":"minimal","summary":"detailed"}}`,
		`{"thinking_budget":0}`, `{"thinkingBudget":"x","thinking_config":{"thinkingBudget":30000}}`, `{"thinkingConfig":{"thinking_budget":-1}}`,
		`{"thinking_summaries":" AUTO "}`, `{"thinkingSummaries":"detailed","include_thoughts":false}`, `{"reasoning":{"summary":"none"}}`, `{"thinkingConfig":{"includeThoughts":true}}`,
		`{"max_output_tokens":10}`, `{"maxOutputTokens":"11"}`, `{"max_tokens":12}`, `{"temperature":0.5}`, `{"topP":1e0}`, `{"presence_penalty":-1}`, `{"frequencyPenalty":2}`,
		`{"parallelToolCalls":false}`, `{"response_format":{"type":"json_object"}}`, `{"text":{"format":{"type":"text"}}}`, `{"verbosity":"low"}`, `{"truncation":"auto"}`, `{"toolChoice":"required"}`, `{"serviceTier":"flex"}`,
		`{"max_output_tokens":1,"maxOutputTokens":2,"temperature":0.1,"top_p":0.2,"text":{"a":1},"verbosity":"high"}`,
	} {
		inputs[fmt.Sprintf("config/%02d", i)] = `{"generation_config":` + cfg + `,"input":"x"}`
	}
	inputs["config/camel"] = `{"generationConfig":{"temperature":1},"reasoning":{"effort":"ignored"},"input":"x"}`
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("interactions/"+name, model, inputs[name], i%3 == 0))
	}
	return out
}

// codexToInteractions exercise ConvertCodexResponseToInteractions(NonStream).
func codexToInteractions() []fixture {
	ev := func(body string) string { return "data: " + body }
	cases := map[string][]string{
		"text": {ev(`{"type":"response.created","response":{"id":"resp_i","model":"gpt-5","created_at":1700000000}}`), ev(`{"type":"response.output_item.added","item":{"type":"message"}}`), ev(`{"type":"response.output_text.delta","delta":"Hi <b>"}`), ev(`{"type":"response.output_text.delta","delta":" é"}`), ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"ignored"}]}}`), ev(`{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":3,"output_tokens":4,"output_tokens_details":{"reasoning_tokens":1},"input_tokens_details":{"cached_tokens":2}}}}`), "data: [DONE]"},
		"steps": {ev(`{"type":"response.output_item.added","item":{"type":"reasoning"}}`), ev(`{"type":"response.reasoning_summary_text.delta","delta":"think"}`), ev(`{"type":"response.reasoning_text.delta","delta":"raw"}`), ev(`{"type":"response.output_item.done","item":{"type":"reasoning","summary":[{"text":"s1"},{"text":"s2"}]}}`),
			ev(`{"type":"response.output_item.added","item":{"type":"function_call","name":"f","call_id":"c1"}}`), ev(`{"type":"response.function_call_arguments.delta","delta":"{\"a\""}`), ev(`{"type":"response.output_item.done","item":{"type":"function_call","name":"f","call_id":"c1","arguments":"{\"a\":1}"}}`),
			ev(`{"type":"response.function_call_arguments.delta","item":{"name":"g","id":"i2"},"delta":"{}"}`), ev(`{"type":"response.output_item.done","item":{"type":"tool_call","name":"g","arguments":"{}"}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"fallback"},{"content":"c"},{"text":5}]}}`),
			ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call","result":"AAA","output_format":"jpg"}}`), ev(`{"type":"response.output_item.done","item":{"type":"image_generation_call"}}`), ev(`{"type":"response.output_item.done","item":{"type":"web_search_call"}}`),
			ev(`{"type":"response.incomplete","response":{"id":"late","usage":{"prompt_tokens":5,"completion_tokens":6,"reasoning_tokens":2,"cached_tokens":1}}}`), ev(`{"type":"response.output_text.delta","delta":"after"}`), "data: [DONE]"},
		"done-only": {"", "data:", "data: [DONE]", "data: [DONE]"},
		"framing":   {"  data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}  ", `{"type":"response.output_text.delta","delta":"bare"}`, "data: not json", "data: {\"type\":\"response.output_text.delta\",\"delta\":\"bad\xff\"}", "event: x", "data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("codex-interactions/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		`{"type":"response.completed","response":{"id":"r","model":"gpt-5","status":"incomplete","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":9,"output_tokens_details":{"reasoning_tokens":3},"input_tokens_details":{"cached_tokens":4}},"output":[{"type":"message","content":[{"type":"output_text","text":"t <1>"},{"content":"c"},{"type":"refusal"}]},{"type":"message","content":[]},{"type":"reasoning","content":"str"},{"type":"reasoning","content":[{"summary_text":"st"},{"text":"x"}]},{"type":"reasoning","content":{"a":1},"summary":"sum"},{"type":"reasoning"},{"type":"function_call","name":"f","call_id":"c","arguments":"{\"a\":1}"},{"type":"tool_call","name":"g","id":" i ","arguments":"[1]"},{"type":"function_call","name":"h","arguments":{"o":1}},{"type":"function_call","arguments":5},{"type":"image_generation_call","result":"AAA"},{"type":"image_generation_call"}]}}`,
		`{"id":"bare","output":[{"type":"message","content":[{"text":"x"}]}],"usage":{"prompt_tokens":7}}`,
		`{"response":{"output":{"k":{"type":"reasoning","content":"in object"}}}}`, `{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("codex-interactions/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// responsesOpenAIRequests exercise ConvertOpenAIResponsesRequestToOpenAIChatCompletions:
// tool name indexes (namespaces, long names, collisions), reasoning carry-over, tool
// output pairing and image outputs.
func responsesOpenAIRequests(model string) []fixture {
	long := strings.Repeat("a", 60)
	tools := `[{"type":"function","name":"lookup","description":"d <x>","parameters":{"type":"object"}},{"type":"custom","name":"run","description":"r"},{"type":"custom","name":"apply_patch"},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list"},{"type":"custom","name":"exec"},{"type":"web_search"}]},{"type":"namespace","name":"` + long + `","tools":[{"name":"` + long + `x"},{"name":"` + long + `y"}]},{"type":"function","function":{"name":"nested","description":"fd","parameters":{"type":"object","properties":{}}}},{"type":"function","name":"list"},{"type":"web_search"},{"type":"custom"}]`
	inputs := map[string]string{
		"tools/index":      `{"input":[{"role":"user","content":"x"},{"type":"function_call","call_id":"c1","name":"list","namespace":"mcp","arguments":"{}"},{"type":"function_call","call_id":"c2","name":"lookup","arguments":"{\"a\":1}"},{"type":"custom_tool_call","call_id":"c3","name":"run","input":"ls <x>"},{"type":"custom_tool_call","call_id":"c4","name":"exec","namespace":"mcp","input":{"k":1}},{"type":"function_call","call_id":"c5","name":"unknown_` + long + `","arguments":"{}"},{"type":"function_call","call_id":"c6","name":"` + long + `x","namespace":"` + long + `"},{"type":"function_call_output","call_id":"c1","output":"one"},{"type":"function_call_output","call_id":"c2","output":[{"type":"input_text","text":"t"},{"type":"input_image","image_url":"https://i/a.png","detail":"Original"}]},{"type":"custom_tool_call_output","call_id":"c3","output":[{"type":"input_text","text":"a"},"b",{"x":1}]},{"type":"custom_tool_call_output","call_id":"c4","output":"[{\"type\":\"input_image\",\"image_url\":\"https://j\"}]"},{"type":"function_call_output","call_id":"c5","output":"{\"not\":\"images\"}"}],"tools":` + tools + `,"tool_choice":{"type":"function","name":"list","namespace":"mcp"},"parallel_tool_calls":"true"}`,
		"tools/choice":     `{"input":"x","tools":` + tools + `,"tool_choice":{"type":"custom","custom":{"name":"run"}}}`,
		"tools/choice2":    `{"input":"x","tools":` + tools + `,"tool_choice":{"type":"function","function":{"name":"` + long + `x","namespace":"` + long + `"}}}`,
		"tools/choice3":    `{"input":"x","tools":` + tools + `,"tool_choice":{"type":"function"}}`,
		"tools/none":       `{"input":"x","tools":[{"type":"web_search"}],"tool_choice":"required","parallel_tool_calls":false}`,
		"reasoning/carry":  `{"reasoning":{"effort":" High "},"input":[{"role":"user","content":"q"},{"type":"reasoning","summary":[{"type":"summary_text","text":"think <1>"}]},{"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},{"type":"function_call_output","call_id":"c1","output":"r"},{"type":"function_call","call_id":"c2","name":"f","arguments":"{}","reasoning_content":"inline"},{"type":"function_call_output","call_id":"c2","output":"r2"},{"role":"assistant","content":[{"type":"output_text","text":"answer"}],"reasoning_content":"own"},{"type":"function_call","call_id":"c3","name":"f"},{"type":"function_call_output","call_id":"c3"},{"type":"reasoning","summary":[]},{"role":"user","content":"next"},{"type":"reasoning","summary":[{"type":"summary_text","text":"tail"}]}]}`,
		"reasoning/flag":   `{"reasoning":"none","input":[{"type":"function_call","call_id":"c1","name":"f"},{"type":"function_call_output","call_id":"c1","output":"r"}]}`,
		"reasoning/item":   `{"input":[{"type":"reasoning","summary":[{"type":"summary_text","text":"x"}]},{"type":"function_call","call_id":"c1","name":"f"},{"type":"function_call_output","call_id":"c1","output":"r"}]}`,
		"reasoning/effort": `{"reasoning_effort":"low","input":[{"type":"function_call","call_id":"c1","name":"f"},{"type":"function_call_output","call_id":"c1","output":"r"}]}`,
		"outputs/order":    `{"input":[{"type":"function_call","call_id":"a","name":"f"},{"type":"function_call","call_id":"b","name":"g"},{"role":"user","content":"between"},{"type":"function_call_output","call_id":"b","output":"B"},{"type":"function_call_output","call_id":"a","output":"A"},{"type":"function_call_output","call_id":"a","output":"dup"},{"type":"function_call_output","call_id":"zz","output":"   "},{"type":"function_call_output","call_id":"yy","output":[]},{"type":"function_call_output","output":"no id"},{"type":"custom_tool_call_output","output":{"text":"obj"}}]}`,
		"outputs/missing":  `{"input":[{"type":"function_call","call_id":"a","name":"f"},{"type":"function_call","call_id":"b","name":"f"},{"type":"function_call_output","output":"one"},{"type":"function_call_output","output":"two"}]}`,
		"messages/parts":   `{"instructions":"sys <s>","max_output_tokens":1e3,"text":{"format":{"type":"json_schema","name":"n","description":"d <x>","strict":"yes","schema":{"type":"object"}}},"input":[{"role":"developer","content":[{"type":"input_text","text":"dev"}]},{"role":"user","content":[{"text":"no type"},{"type":"input_image","image_url":"data:image/png;base64,AA","detail":"LOW"},{"type":"input_image","detail":5},{"type":"input_video","video_url":{"url":"https://v"},"processing":{"fps":1}},{"type":"video_url","video_url":"https://w"},{"type":"input_file","file_id":"f"}]},{"type":"message","role":"assistant","content":"plain"},{"type":"other"},{"content":"no role or type"}]}`,
		"messages/format1": `{"input":"x","text":{"format":{"type":"json_object"}}}`,
		"messages/format2": `{"input":"x","text":{"format":{"type":"grammar"}}}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("responses-openai/"+name, model, inputs[name], i%2 == 0))
	}
	return out
}

// openAIToResponses exercise ConvertOpenAIChatCompletionsResponseToOpenAIResponses
// (NonStream): reasoning, parallel text and tool calls, custom tools and apply_patch.
func openAIToResponses() []fixture {
	chunk := func(body string) string {
		return `data: {"id":"chatcmpl-1","object":"chat.completion.chunk","created":1700000000,"model":"gpt-x","choices":[` + body + `]}`
	}
	usage := `data: {"id":"chatcmpl-1","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":3},"completion_tokens_details":{"reasoning_tokens":2}}}`
	patch := `{\"input\":\"*** Begin Patch\\n*** Add File: a\\n+<x>\\n*** End Patch\"}`
	tools := `"tools":[{"type":"function","name":"lookup"},{"type":"custom","name":"run"},{"type":"custom","name":"apply_patch"},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list"}]}]`
	original := `{"model":"client-model","instructions":"i","input":"q","reasoning":{"effort":"high"},"metadata":{"k":"<v>"},` + tools + `}`
	single := `{"model":"client-model","input":"q","tools":[{"type":"custom","name":"run"}]}`
	type sc struct {
		original string
		finalize bool
		lines    []string
	}
	cases := map[string]sc{
		"text":           {original, false, []string{chunk(`{"index":0,"delta":{"role":"assistant","reasoning_content":"think <t>"}}`), chunk(`{"index":0,"delta":{"reasoning":"more"}}`), chunk(`{"index":0,"delta":{"content":"Hello é"}}`), chunk(`{"index":1,"delta":{"content":"second choice"}}`), chunk(`{"index":0,"delta":{"content":""},"finish_reason":"stop"}`), usage, "data: [DONE]"}},
		"tools":          {original, false, []string{chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"lookup","arguments":"{\"q\":"}},{"index":1,"function":{"name":"list","arguments":""}}]}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"<x>\"}"}},{"index":1,"id":"call_b","function":{"arguments":"{}"}},{"index":2,"id":"call_c","function":{"name":"run","arguments":"{\"input\":\"ls\"}"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"tool_calls"}`), "data: [DONE]"}},
		"patch":          {original, false, []string{chunk(`{"index":0,"delta":{"content":"patching"}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_p","function":{"name":"apply_patch","arguments":"{\"input\":\"*** Begin"}}]}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":" Patch\\n+<x>\\n*** End Patch\"}"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"tool_calls"}`), "data: [DONE]"}},
		"patch-bad":      {original, false, []string{chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_p","function":{"name":"apply_patch","arguments":"{\"input\":5}"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"tool_calls"}`), "data: [DONE]"}},
		"patch-conflict": {original, false, []string{chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_p","function":{"name":"apply_patch","arguments":""}}]}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_q","function":{"arguments":"{}"}}]}}`), "data: [DONE]"}},
		"patch-eof":      {original, true, []string{chunk(`{"index":0,"delta":{"content":"open"}}`)}},
		"plain-eof":      {single, true, []string{chunk(`{"index":0,"delta":{"content":"open"}}`)}},
		"single-custom":  {single, false, []string{chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"input\":\"x\"}"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"length"}`), "data: [DONE]"}},
		"incomplete":     {original, false, []string{chunk(`{"index":0,"delta":{"content":"cut"}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"lookup"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"content_filter"}`), "data: [DONE]"}},
		"unfinished":     {original, false, []string{chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"lookup","arguments":"{\"a\":"}}]}}`), "data: [DONE]"}},
		"reasoning-only": {original, false, []string{chunk(`{"index":0,"delta":{"reasoning_content":"r"}}`), "data: [DONE]"}},
		"framing":        {original, false, []string{"", `{"object":"chat.completion","choices":[]}`, `data: {"choices":{}}`, chunk(`{"delta":{"content":"no index"}}`), "data: not json", "data: [DONE]", chunk(`{"index":0,"delta":{"content":"after"}}`)}},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		c := cases[name]
		f := streamCase("openai-responses/"+name, "gpt-x", c.lines...)
		f.Original = c.original
		f.Finalize = c.finalize
		out = append(out, f)
	}
	for i, b := range []struct{ original, translated, body string }{
		{original, `{"model":"chat-model","max_tokens":7,"reasoning":{}}`, `{"id":"chatcmpl-n1","object":"chat.completion","created":1700000000,"model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","content":"hi <b>","reasoning_content":"r","tool_calls":[{"id":"c1","type":"function","function":{"name":"lookup","arguments":"{\"a\":\"<x>\"}"}},{"type":"function","function":{"name":"run","arguments":"{\"input\":\"ls\"}"}},{"id":"c3","function":{"name":"list","arguments":"{}"}},{"id":"c4","function":{"name":"apply_patch","arguments":"` + patch + `"}}]},"finish_reason":"tool_calls"},{"index":1,"message":{"content":"second"}}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":1},"output_tokens_details":{"reasoning_tokens":2}}}`},
		{original, `{"model":"chat-model"}`, `{"choices":[{"index":0,"message":{"tool_calls":[{"id":"c4","function":{"name":"apply_patch","arguments":"{\"input\":5}"}}]}}]}`},
		{``, ``, `{"id":"resp_x","choices":[{"message":{"content":""},"finish_reason":"length"}],"usage":{"input_tokens":5},"model":"m"}`},
		{single, `{"messages":[]}`, `{"choices":[{"message":{"reasoning":"alt"}}],"usage":{"total_tokens":2}}`},
		{``, ``, `not json`},
	} {
		n := nonStream(fmt.Sprintf("openai-responses/non-stream/%d", i), "gpt-x", b.body)
		n.Original = b.original
		n.Translated = b.translated
		out = append(out, n)
	}
	return out
}

// geminiOpenAIRequests exercise ConvertGeminiRequestToOpenAI: generation config, call ID
// pairing (explicit, queued by name, deterministic), media parts and tool config.
func geminiOpenAIRequests(model string) []fixture {
	inputs := map[string]string{
		"config":  `{"generationConfig":{"temperature":"0.5","maxOutputTokens":1e3,"topP":1,"topK":"40","stopSequences":["a","<b>",5],"candidateCount":2,"responseModalities":[" TEXT ","Image","audio","video"],"thinkingConfig":{"thinkingLevel":" HIGH "}},"service_tier":"flex","contents":[]}`,
		"config2": `{"generationConfig":{"stopSequences":[],"responseModalities":["x"],"thinkingConfig":{"thinking_budget":0}},"service_tier":5,"contents":[]}`,
		"config3": `{"generationConfig":{"thinkingConfig":{"thinkingLevel":"","thinkingBudget":30000}},"contents":[]}`,
		"config4": `{"generationConfig":{"thinkingConfig":{"thinking_budget":-1}},"contents":[]}`,
		"system":  `{"systemInstruction":{"parts":[{"text":"s <1>"},{"text":"t","thought":true},{"inlineData":{"mimeType":"image/png","data":"iVBO"}},{"fileData":{"mimeType":"text/plain","fileUri":"gs://f"}}]},"system_instruction":{"parts":[{"text":"ignored"}]},"contents":[]}`,
		"system2": `{"system_instruction":{"parts":[{"thought":true,"text":"only thought"}]},"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		"pairing": `{"contents":[{"role":"user","parts":[{"text":"go"}]},{"role":"model","parts":[{"text":"calling"},{"functionCall":{"name":"f","args":{"a":"<x>"}}},{"functionCall":{"name":"f","args":{"a":2}}},{"functionCall":{"name":"g","id":" gid "}},{"functionCall":{"name":"h"}}]},{"role":"user","parts":[{"functionResponse":{"name":"g","id":"gid","response":{"content":"c <1>"}}},{"functionResponse":{"name":"f","response":{"result":1}}},{"functionResponse":{"name":"f","callId":"nope","response":{}}},{"functionResponse":{"name":"f"}},{"functionResponse":{"name":"z","response":"str"}}]},{"role":"model","parts":[{"thought":true,"text":"t"}]},{"role":"model","parts":[{"thought":true,"text":"t"},{"text":"kept"}]},{"parts":"notarray"},{"role":"tool","parts":[]}]}`,
		"media":   `{"contents":[{"role":"user","parts":[{"text":"a"},{"inlineData":{"data":"AA"}},{"inline_data":{"mime_type":"Audio/X-WAV","data":"UklG"}},{"inlineData":{"mimeType":"audio/l16","data":"AA"}},{"inlineData":{"mimeType":"video/mp4","data":"AA"}},{"inlineData":{"mimeType":"application/pdf","data":"JV"}},{"inlineData":{"mimeType":"image/png"}},{"fileData":{"mimeType":"image/jpeg","fileUri":"gs://i"}},{"file_data":{"mime_type":"video/webm","file_uri":"gs://v"}},{"fileData":{"mimeType":"text/csv","fileUri":"gs://c"}},{"fileData":{"mimeType":"model/x","fileUri":"m <x>"}},{"fileData":{"fileUri":"nomime"}},{"text":"b","inlineData":{"mimeType":"image/gif","data":"R0"}}]},{"role":"user","parts":[{"text":"only "},{"text":"text"}]}]}`,
		"tools":   `{"tools":[{"functionDeclarations":[{"name":"a","description":"<d>","parameters":{"type":"object"}},{"name":"b","parametersJsonSchema":{"type":"object","properties":{}}},{"description":"no name"}]},{"googleSearch":{}},{"functionDeclarations":[]}],"contents":[]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("gemini-openai/"+name, model, inputs[name], i%2 == 0))
	}
	for i, cfg := range []string{`{"mode":"NONE"}`, `{"mode":"AUTO"}`, `{"mode":"ANY"}`, `{"mode":"ANY","allowedFunctionNames":["x<1>"]}`, `{"mode":"ANY","allowedFunctionNames":["a","b"]}`, `{"mode":"any"}`, `{}`} {
		out = append(out, req(fmt.Sprintf("gemini-openai/tool-config/%d", i), model, `{"toolConfig":{"functionCallingConfig":`+cfg+`},"contents":[]}`, false))
	}
	out = append(out, req("gemini-openai/tool-config/none", model, `{"toolConfig":{},"contents":[]}`, false))
	return out
}

// openAIToGemini exercise ConvertOpenAIResponseToGemini(NonStream): reasoning shapes,
// tool call accumulation, tolerant argument recovery and usage.
func openAIToGemini() []fixture {
	chunk := func(choices string) string {
		return `data: {"id":"c","object":"chat.completion.chunk","model":"gpt-x","choices":[` + choices + `]}`
	}
	args := []string{
		`{"a":1}`, `[1,2]`, `"str"`, ``, `  {}  `, `{"a":1,`, `{"a": "x", "b": tru}`, `x {"k": "v", "n": 1e3, "u": 18446744073709551615, "big": 1e400, "h": 0x1p4, "neg": -0, "plus": +5} y`,
		`{"s": "unterminated`, `{"o": {"x": [1, "}"]}, "bad": {x}, "arr": [1, 2`, `{"esc\"key": 1, "dot.key": 2, "back\\slash": 3, "*": 4, "": 5}`, `{nokey, "after": 1}`, `{"a" 1}`, `{"nan": nan, "inf": inf, "t": true , "nul": null}`, "{\"bad\xff\": \"v\xfe\"}", `{"a":"\u00e9<"}`,
	}
	var lines []string
	for i, a := range args {
		escaped := strings.ReplaceAll(strings.ReplaceAll(a, `\`, `\\`), `"`, `\"`)
		lines = append(lines, chunk(fmt.Sprintf(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_%d","type":"function","function":{"name":"f%d","arguments":"%s"}}]}}`, i, i, escaped)), chunk(`{"index":0,"delta":{},"finish_reason":"tool_calls"}`))
	}
	cases := map[string][]string{
		"text":      {chunk(`{"index":0,"delta":{"role":"assistant","content":""}}`), chunk(`{"index":0,"delta":{"reasoning_content":["a",{"text":"b"},{"x":1},5,["c"]]}}`), chunk(`{"index":0,"delta":{"reasoning_content":"r","content":"Hello <b>"}}`), chunk(`{"index":0,"delta":{"content":"x"},"finish_reason":"stop"}`), chunk(`{"index":0,"delta":{},"finish_reason":"length"}`), chunk(`{"index":0,"delta":{},"finish_reason":null}`), `data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"completion_tokens_details":{"reasoning_tokens":2},"prompt_tokens_details":{"cached_tokens":1}},"model":"m"}`, `data: {"choices":[],"usage":{"input_tokens":5}}`, `data: {"choices":[]}`, "data: [DONE]"},
		"tools":     {chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}},{"index":1,"type":"custom","function":{"name":"x"}},{"index":2}]}}`), chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}`), chunk(`{"index":0,"delta":{"tool_calls":[]},"finish_reason":"stop"}`), chunk(`{"index":0,"delta":{},"finish_reason":"content_filter"}`), chunk(`{"index":0,"delta":{},"usage":{"total_tokens":9}}`)},
		"multi":     {chunk(`{"index":0,"delta":{"content":"a"}},{"index":1,"delta":{"content":"b"},"finish_reason":"stop"}`), "{\"choices\":[{\"delta\":{\"content\":\"bare\"}}]}", "data: not json"},
		"arguments": lines,
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("openai-gemini/"+name, "gpt-x", cases[name]...))
	}
	// Two calls: Go emits them in map order, so every order it produced is recorded.
	unordered := streamCase("openai-gemini/two-calls", "gpt-x", chunk(`{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c0","function":{"name":"first","arguments":"{}"}},{"index":1,"id":"c1","function":{"name":"second","arguments":"{\"b\":1}"}}]}}`), chunk(`{"index":0,"delta":{},"finish_reason":"tool_calls"}`))
	unordered.Unordered = true
	out = append(out, unordered)
	for i, body := range []string{
		`{"id":"x","model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":["r1",{"text":"r2"}],"content":"hi <b>","tool_calls":[{"id":"t1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}},{"type":"function","function":{"name":"g","arguments":"{\"broken\": 1,"}},{"type":"custom","function":{"name":"skip"}}]},"finish_reason":"tool_calls"},{"index":3,"message":{"role":"user","content":"overlay"},"finish_reason":"length"}],"usage":{"prompt_tokens":1,"output_tokens":2,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":4}}}`,
		`{"choices":[{"message":{"content":""},"finish_reason":null}]}`, `{"choices":[]}`, `{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("openai-gemini/non-stream/%d", i), "gpt-x", body))
	}
	return out
}

// interactionsGeminiRequests exercise ConvertInteractionsRequestToGemini: system
// instruction forms, snake-to-camel generation config keys (odd keys, arrays, scalars),
// thinking moves, modalities, tool choices, tool normalization (built-ins, declarations,
// decoded maps, Marshal failures) and thought-signature carrying across input steps.
func interactionsGeminiRequests(model string) []fixture {
	inputs := map[string]string{
		"system/text-and-parts": `{"system_instruction":{"text":"t","parts":[{"text":"p"}]},"input":"x"}`,
		"system/text-number":    `{"system_instruction":{"text":5},"input":"x"}`,
		"system/scalar":         `{"system_instruction":7,"input":"x"}`,
		"system/null":           `{"system_instruction":null,"input":"x"}`,
		"config/keys":           `{"generation_config":{"max_output_tokens":5,"stop_sequences":["a","b"],"response_json_schema":{"type":"object","properties":{"snake_key":{"type":"string"}}},"a.b":1,"c*d":2,"e#f":3,"g_\u00e9":4,"h_\u00e9x":5,"nested":[[1,{"x_y":2}],[]],"empty_obj":{},"null_v":null,"__":true,"top_k":1,"top_k":2,"a\u002eb_c":6,"thinking_level":"high","thinking_budget":10,"include_thoughts":true,"tool_choice":"auto","thinking_summaries":"none"},"input":"x"}`,
		"config/array":          `{"generation_config":[1,{"a_b":2}],"input":"x"}`,
		"config/string":         `{"generation_config":"str","input":"x"}`,
		"config/null":           `{"generation_config":null,"input":"x"}`,
		"config/camel":          `{"generationConfig":{"thinkingLevel":"low","thinkingBudget":0,"includeThoughts":false,"thinkingSummaries":"auto","toolChoice":{"x":1},"snake_kept":1},"input":"x"}`,
		"config/summaries-num":  `{"generationConfig":{"thinkingSummaries":5,"thinkingConfig":{"includeThoughts":true}},"input":"x"}`,
		"config/summaries-odd":  `{"generation_config":{"thinking_summaries":" Detailed ","include_thoughts":true},"input":"x"}`,
		"config/override":       `{"generation_config":{"thinking_config":{"thinking_level":"x"},"thinking_level":"high"},"input":"x"}`,
		"modalities/snake":      `{"response_modalities":["Text"," IMAGE ","audio","video",5],"input":"x"}`,
		"modalities/camel":      `{"responseModalities":"TEXT","input":"x"}`,
		"modalities/both":       `{"response_modalities":["x"],"responseModalities":["text"],"input":"x"}`,
		"modalities/camel-arr":  `{"responseModalities":["AUDIO"],"input":"x"}`,
		"tools/all":             `{"tools":[{"type":"url_context"},{"type":"url_context","url_context":"str","urlContext":{"a":"<b>"}},{"type":"code_execution","code_execution":{}},{"type":"google_search","googleSearch":{"x":1}},{"type":"web_search","google_search":{"y":2}},{"function_declarations":[{"name":"f","parameters":{ "type" : "object" }}]},{"name":"n","description":"d <&>","parameters":{"type":"object","n":1e3}},{"name":"bare"},{"url_context":{},"code_execution":{"k":1.50},"google_search":{},"web_search":{"w":1},"extra_key":[1,2.0,12345678901234567890,-0.0,1e-7]},{"type":"custom","url_context":{}},"str",5,null,[1],{}],"input":"x"}`,
		"tools/bad-raw":         `{"tools":[{"name":"x","parameters":{"a":}}],"input":"x"}`,
		"tools/native":          `{"tools":[{"name":"a"},{"functionDeclarations":[]}],"input":"x"}`,
		"tools/dropped":         `{"tools":["s",null],"input":"x"}`,
		"tools/object":          `{"tools":{"a":1},"input":"x"}`,
		"tools/overflow":        `{"tools":[{"x":1e400}],"input":"x"}`,
		"tools/utf8":            "{\"tools\":[{\"k\":\"\xff<\",\"k\":\"dup\u2028\",\"z\":\"\xc3\"}],\"input\":\"x\"}",
		"choice/strings":        `{"tool_choice":"Required","input":"x"}`,
		"choice/any":            `{"tool_choice":" any ","input":"x"}`,
		"choice/bogus":          `{"tool_choice":"bogus","input":"x"}`,
		"choice/function":       `{"tool_choice":{"type":"function","function":{"name":" f <x> "}},"input":"x"}`,
		"choice/tool":           `{"tool_choice":{"type":"Tool","name":"t"},"input":"x"}`,
		"choice/tool-blank":     `{"tool_choice":{"type":"tool","name":"  "},"input":"x"}`,
		"choice/upper":          `{"tool_choice":{"type":"ANY"},"input":"x"}`,
		"choice/number":         `{"tool_choice":5,"input":"x"}`,
		"choice/config":         `{"generation_config":{"tool_choice":"none"},"input":"x"}`,
		"choice/camel":          `{"generationConfig":{"toolChoice":{"type":"auto"}},"input":"x"}`,
		"service-tier/number":   `{"service_tier":5,"input":"x"}`,
		"model":                 `{"model":"orig","input":"x"}`,
		"signatures":            `{"input":[{"type":"thought","signature":" s1 ","content":[{"text":"t"}]},{"type":"model_output","content":"answer"},{"type":"user_input","content":"q"},{"type":"thought","thought_signature":"s2"},{"type":"thought","thoughtSignature":"s3","summary":"sum"},{"type":"function_call","name":"f","call_id":"c1","arguments":{"a":1}},{"type":"function_call","name":"g","id":"c2","signature":"s4"},{"type":"thought","signature":"s5"},{"type":"function_call","name":"h","signature":"s5"},{"type":"thought","signature":"s6"},{"type":"function_call","name":"i","signature":"s7"},{"type":"function_result","call_id":"c1","name":"f","result":"ok"},{"type":"function_result","id":"c2","result":{"$ref":"#/x"}},{"type":"function_result","name":"h"},{"type":"thought","signature":"s8"}]}`,
		"signatures/trailing":   `{"input":[{"type":"user_input","content":"u"},{"type":"thought","signature":"z"}]}`,
		"signatures/output":     `{"input":[{"type":"thought","signature":"a"},{"type":"thought","signature":"b","text":"tt"},{"type":"model_output"},{"type":"thought","signature":"c"},"plain",{"type":"thought","signature":"d"},{"type":"model_output","content":[{"type":"text","text":"m"}]}]}`,
		"model-turns":           `{"input":[{"type":"model_output","content":[{"type":"text","text":"a"}]},{"type":"model_output","text":"b"},"str",{"type":"model_output","content":{"text":"c"}},{"type":"thought","content":{"type":"image","mime_type":"image/png","data":"AA"}},{"type":"thought","content":7}]}`,
		"native":                `{"input":[{"type":"user_input","role":"Model","parts":[{"text":"t"},{"functionCall":{"name":"f"}},{"functionResponse":{"name":"f"}},{"inlineData":{"mimeType":"image/png","data":"AA"}},{"fileData":{"mime_type":"a/b","file_uri":"u"}},{"inline_data":{"mime_type":"x/y"}},{"file_data":{"mimeType":"m","fileUri":"f"}},{"other":1}]},{"role":"assistant","parts":[]},{"type":"x","parts":{"text":"obj"}},{"type":"x","role":"user","parts":[{"text":"p"}]},{"type":"x","role":"bogus","parts":[{"text":"q"}]}]}`,
		"object-steps":          `{"input":{"role":"assistant","steps":[{"type":"thought","signature":"q"},{"type":"x","text":"t"}]}}`,
		"nested-roles":          `{"input":[{"role":"model","steps":[{"type":"user_input","content":"m"},{"role":"user","steps":["u"]},{"role":"other","steps":["o"]},{"role":"assistant","steps":[{"type":"default","content":"d"}]}]}]}`,
		"results":               `{"input":[{"type":"function_call","name":"a","id":"1"},{"type":"function_result","name":"a","call_id":"1","result":{"r":1}},{"type":"function_result","name":"b","result":"x"},{"type":"user_input","content":"t"},{"type":"function_result","name":"c"}]}`,
		"requote":               "{\"input\":[{\"type\":\"user_input\",\"content\":[{\"type\":\"image\",\"mime_type\":\"image/p\xffng\",\"data\":\"AA\"},{\"type\":\"audio\",\"mime_type\":\"a\\u0007b\",\"data\":\"x\"},{\"type\":\"image\",\"mime_type\":\"image/png\",\"data\":\"A\\tA\"},{\"type\":\"video\",\"fileUri\":\"gs://\u00e9\\u2028\",\"mimeType\":\"\\u0000v\"},{\"type\":\"image\",\"url\":\"data:image/png;base64,\\u0001z\"},{\"type\":\"document\",\"mime_type\":\"\",\"data\":\"d\"}]}]}",
		"files":                 `{"input":[{"type":"user_input","content":[{"type":"file","file":{"filename":"a.PDF","file_data":"JVBE"}},{"type":"file","file":{"file_data":"data:text/csv;base64,YQ"}},{"type":"file","file":{"filename":"x.unknownext","file_data":"AA"}},{"type":"input_audio","input_audio":{"format":" FLAC ","data":"ZkxhQw"}},{"type":"input_audio","input_audio":{"format":"pcm16"}},{"type":"image_url","image_url":{"url":"data:image/gif;base64,R0lG"}},{"type":"image_url","image_url":"https://x"}]}]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("interactions-gemini/"+name, model, inputs[name], i%2 == 0))
	}
	return out
}

// geminiToInteractions exercise ConvertGeminiResponseToInteractions(NonStream) with the
// bare JSON payloads the Gemini executor passes: step switching, signatures, function
// calls and results, finish and usage ordering, and the done marker.
func geminiToInteractions() []fixture {
	cases := map[string][]string{
		"text":           {`{"candidates":[{"content":{"parts":[{"text":"Hi <b>"}]}}]}`, `{"candidates":[{"content":{"parts":[{"text":" é","thoughtSignature":" sig1 "}]}}]}`, `{"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}]}`, `{"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":9,"thoughtsTokenCount":2,"cachedContentTokenCount":1}}`, "[DONE]"},
		"thoughts-calls": {`{"candidates":[{"content":{"parts":[{"text":"think","thought":true},{"text":"more","thought":true,"thought_signature":"ts"},{"thoughtSignature":"only"},{"text":"answer"},{"functionCall":{"id":"fc1","name":"f","args":{"q":"<x>"}},"thoughtSignature":"fsig"},{"functionCall":{"call_id":"fc2","name":"g"}},{"functionCall":{"name":"h","id":""}},{"functionResponse":{"name":"f","response":{"ok":1}}},{"functionResponse":{"name":"g"}},{"text":"x","extra_content":{"google":{"thought_signature":" ex "}}},{"inlineData":{"mimeType":"image/png","data":"AA"}}]},"finishReason":"STOP"}],"usage_metadata":{"prompt_token_count":5,"cached_content_token_count":2,"cachedContentTokenCount":0}}`, "[DONE]", "[DONE]"},
		"no-finish":      {`{"candidates":[{"content":{"parts":[{"text":"a"}]}}]}`, "[DONE]"},
		"usage-first":    {`{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"totalTokenCount":1}}`, `{"candidates":[{"finishReason":"STOP"}]}`, `{"usageMetadata":{"promptTokenCount":2}}`, "[DONE]"},
		"usage-coerce":   {`{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{}}`, `{"usage_metadata":{"prompt_token_count":"7","total_token_count":1.5,"thoughts_token_count":"x"}}`},
		"framing":        {"", "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"prefixed\"}]}}]}", "not json", `{"candidates":[{"content":{"parts":{"a":{"text":"obj"}}}}]}`, `{"candidates":[{"content":{"parts":"scalar"}}]}`, " [DONE] "},
		"done-first":     {"[DONE]", `{"candidates":[{"content":{"parts":[{"text":"late"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1}}`},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("gemini-interactions/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		`{"responseId":"r1","candidates":[{"content":{"parts":[{"text":"t <b>"},{"text":"th","thought":true,"thoughtSignature":"s1"},{"functionCall":{"id":"c1","name":"f","args":{"a":1}},"thoughtSignature":"s2"},{"functionCall":{"call_id":"c2","name":"g"}},{"functionResponse":{"call_id":"c3","name":"h","response":{"r":"<v>"}}},{"functionResponse":{"name":"k"}},{"inlineData":{"mimeType":"IMAGE/png","data":"AA"},"thoughtSignature":"s3"},{"inlineData":{"mime_type":"audio/wav","data":"BB"}},{"inline_data":{"mime_type":"video/mp4","data":"CC"}},{"inlineData":{"mimeType":"application/pdf","data":"DD"}},{"text":"","thoughtSignature":"s4"},{"text":""},{"thought_signature":"s5"},{"extra_content":{"google":{"thought_signature":"s6"}}},{"other":1},{"text":"tail","thoughtSignature":"s7"}]}}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":2,"totalTokenCount":3,"thoughtsTokenCount":4,"cachedContentTokenCount":5}}`,
		`{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"usage_metadata":{"prompt_token_count":"9","total_token_count":2.5}}`,
		`{}`, `not json`, `{"candidates":[]}`, `{"responseId":"","candidates":[{"content":{"parts":[]}}],"usageMetadata":{}}`,
	} {
		out = append(out, nonStream(fmt.Sprintf("gemini-interactions/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// geminiInteractionsRequests exercise ConvertGeminiRequestToInteractions: system text
// forms, camel-to-snake generation config with thinking normalization, tool entries and
// part-to-step conversion.
func geminiInteractionsRequests(model string) []fixture {
	inputs := map[string]string{
		"system/string":    `{"systemInstruction":"plain <s>","contents":[]}`,
		"system/text":      `{"system_instruction":{"text":"snake text","parts":[{"text":"p"}]},"contents":[]}`,
		"system/text-num":  `{"systemInstruction":{"text":5,"parts":[{"text":"a"},{"text":""},{"inline":1},{"text":"b"}]},"contents":[]}`,
		"system/empty":     `{"systemInstruction":{"parts":[{"text":""}]},"contents":[]}`,
		"system/object":    `{"systemInstruction":{"parts":"x"},"contents":[]}`,
		"config/thinking":  `{"generationConfig":{"maxOutputTokens":5,"thinkingConfig":{"thinkingLevel":" HIGH ","thinkingBudget":"12","includeThoughts":true},"responseMimeType":"application/json","stopSequences":["<a>"],"ABc":1,"a.b":2,"xY*z":3,"\u00c9t\u00e9":4},"contents":[]}`,
		"config/summaries": `{"generationConfig":{"thinkingSummaries":"auto","thinkingConfig":{"includeThoughts":false}},"contents":[]}`,
		"config/include":   `{"generationConfig":{"thinkingConfig":{"includeThoughts":"false"}},"contents":[]}`,
		"config/snake-in":  `{"generationConfig":{"thinking_config":{"thinking_level":"Low","thinking_budget":0}},"contents":[]}`,
		"config/array":     `{"generationConfig":[{"aB":1}],"contents":[]}`,
		"config/scalar":    `{"generationConfig":5,"contents":[]}`,
		"tools/builtins":   `{"tools":[{"urlContext":{}},{"url_context":{"k":"<v>"}},{"codeExecution":{"a":1},"googleSearch":{}},{"code_execution":"s"},{"google_search":{"t":{"u":1}}},{"googleSearch":{},"functionDeclarations":[{"name":"after","description":"d"}]}],"contents":[]}`,
		"tools/functions":  `{"tools":[{"functionDeclarations":[{"name":"a","description":"<d>","parameters":{"type":"OBJECT"}},{"name":"b","parametersJsonSchema":{ "type" : "object" }},{"description":"no name"},{"name":"c","parameters":{"x":1},"parametersJsonSchema":{"y":2}}]},{"function_declarations":[{"name":"d"}]},{"name":"top","parametersJsonSchema":{"z":1e2}},{"functionDeclarations":"bad"},{}],"contents":[]}`,
		"tools/bad-raw":    `{"tools":[{"functionDeclarations":[{"name":"a","parameters":{"x":}}]}],"contents":[]}`,
		"tools/none":       `{"tools":[{"x":1}],"contents":[]}`,
		"tools/object":     `{"tools":{"urlContext":{}},"contents":[]}`,
		"contents/parts":   `{"contents":[{"role":"user","parts":[{"text":"hi <b>"},{"inlineData":{"mimeType":"image/png","data":"AA"}},{"inline_data":{"mime_type":"Audio/WAV","data":"BB"}},{"inlineData":{"mime_type":"video/mp4","data":"CC"}},{"inlineData":{"data":"DD"}},{"fileData":{"mimeType":"a/b","fileUri":"u"}},{"text":"","thoughtSignature":"s0"},{"text":""},{"functionResponse":{"id":"c1","name":"f","response":{"ok":true}},"thoughtSignature":"ignored"},{"functionResponse":{"call_id":"c2","name":"g"}},{"text":"user thought","thought":true}]},{"role":"model","parts":[{"text":"t","thought":true,"thoughtSignature":"s1"},{"text":"answer","thoughtSignature":"s2"},{"functionCall":{"id":"c1","name":"f","args":{"a":1}},"thoughtSignature":"s3"},{"functionCall":{"call_id":"c2","name":"g"}},{"functionCall":{"name":"h"},"extra_content":{"google":{"thought_signature":" s4 "}}},{"inlineData":{"mimeType":"image/jpeg","data":"EE"},"thought":true}]},{"role":"MODEL","parts":[{"text":"upper role"}]},{"parts":[{"text":"no role"}]},{"role":"model","parts":{"a":{"text":"obj"}}}]}`,
		"contents/odd":     `{"contents":{"a":{"role":"model","parts":[{"text":"x"}]}},"model":"orig"}`,
		"contents/string":  `{"contents":"str"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("gemini-interactions/"+name, model, inputs[name], i%2 == 1))
	}
	return out
}

// interactionsUpstreamResponses are Gemini Interactions events and bodies as the Gemini
// Interactions executor passes them: joined `data:` payloads (some still SSE-shaped),
// step lifecycles, failures, usage forms and the done marker.
func interactionsUpstreamResponses() []fixture {
	ev := func(body string) string { return body }
	cases := map[string][]string{
		"basic": {ev(`{"event_type":"interaction.created","interaction":{"id":"int_1","model":"gemini-x","status":"in_progress"}}`), ev(`{"event_type":"interaction.status_update","interaction_id":"int_1","status":"in_progress"}`),
			ev(`{"event_type":"step.start","index":0,"step":{"type":"thought","signature":"s0"}}`), ev(`{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"type":"text","text":"think <b>"}}}`), ev(`{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"s0b"}}`), ev(`{"event_type":"step.stop","index":0}`),
			ev(`{"event_type":"step.start","index":1,"step":{"type":"model_output"}}`), ev(`{"event_type":"step.delta","index":1,"delta":{"type":"text","text":"Hi é"}}`), ev(`{"event_type":"step.delta","index":1,"delta":{"type":"text","content":{"text":"via content"}}}`), ev(`{"event_type":"step.delta","index":1,"delta":{"type":"text","text":"  "}}`), ev(`{"event_type":"step.stop","index":1}`),
			ev(`{"event_type":"step.start","index":2,"step":{"type":"function_call","name":"lookup","call_id":"c1","id":"other","thoughtSignature":"fs"}}`), ev(`{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":" {\"q\":\"<x>\"} "}}`), ev(`{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"q\":"}}`), ev(`{"event_type":"step.delta","index":7,"step":{"name":"fallback"},"delta":{"type":"arguments_delta","arguments":"[1]"}}`),
			ev(`{"event_type":"step.start","index":3,"step":{"type":"function_call","name":"","id":"i3","signature":"","thought_signature":"ts3"}}`), ev(`{"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta"}}`), ev(`{"event_type":"step.delta","index":3,"delta":{"type":"thought_signature","thoughtSignature":"late"}}`), ev(`{"event_type":"step.delta","index":3,"delta":{"type":"arguments_delta","arguments":"{}"}}`),
			ev(`{"event_type":"step.delta","index":4,"delta":{"type":"unknown"}}`), ev(`{"event_type":"step.delta","index":4,"delta":{"type":"thought_signature","signature":" "}}`),
			ev(`{"event_type":"interaction.completed","interaction":{"id":"int_2","model":"","service_tier":"priority","usage":{"input_tokens":3,"output_tokens":4,"total_tokens":9,"reasoning_tokens":2,"cached_tokens":1}}}`), ev(`{"event_type":"done"}`)},
		"sse-shaped":  {"event: interaction.created\ndata: {\"event_type\":\"interaction.created\",\"interaction\":{\"id\":\"int_s\"}}", "data: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"a\"}}", ": keepalive\ndata: {\"event_type\":\"step.delta\",\n data: \"index\":0}", "data: [DONE]", "[DONE]", "", "event: done", "not json", `{"event_type":"finish","usage":{"total_input_tokens":5,"total_output_tokens":2,"total_thought_tokens":1,"total_cached_tokens":3}}`},
		"failures":    {ev(`{"event_type":"interaction.failed","interaction":{"error":{"code":"RESOURCE_EXHAUSTED","message":"quota <x>"}}}`), ev(`{"event_type":"response.failed","error":{"status":"404"}}`), ev(`{"event_type":"response.failed","code":" 503 "}`), ev(`{"event_type":"interaction.failed","error":{"code":418,"message":""}}`), ev(`{"event_type":"response.failed","error":{"code":"700"}}`), ev(`{"event_type":"response.failed","error":{"code":"+451"}}`), ev(`{"event_type":"interaction.failed"}`)},
		"usage-forms": {ev(`{"event_type":"interaction.completed","metadata":{"total_usage":{"output_tokens":4}}}`), ev(`{"event_type":"interaction.completed","usage":{"total_input_tokens":"6","total_tokens":1.5}}`), ev(`{"event_type":"interaction.completed","interaction":{"metadata":{"usage":{"input_tokens":2,"total_output_tokens":3}}}}`), ev(`{"event_type":"interaction.completed"}`)},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("interactions-up/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		`{"interaction":{"id":"i1","model":"m1","service_tier":"priority","steps":[{"type":"thought","content":[{"type":"text","text":"t"}]},{"type":"model_output","content":[{"type":"text","text":"a <b>"},{"type":"image","mime_type":"image/png","data":"AA"}]},{"type":"function_call","name":"f","call_id":"c1","arguments":"{\"a\":1}","signature":"s"},{"type":"function_call","name":"g","id":"c2","args":{"b":2}},{"type":"function_call","name":"h","arguments":"not json"},{"type":"function_call","name":"i"},{"type":"function_result","name":"f","call_id":"c1","result":"{\"ok\":true}"},{"type":"function_result","name":"g","response":{"$ref":"#/a"}},{"type":"function_result","name":"h","result":"  "},{"type":"function_result","name":"k"},{"type":"function_result","name":"m","result":"{\"$ref\":\"x\"}"},{"type":"model_output","content":"str"},{"type":"model_output","content":{"text":"obj"}},{"type":"x"}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3,"reasoning_tokens":4,"cached_tokens":5}}}`,
		`{"id":"r","model":"","steps":[{"type":"model_output","content":[{"text":"x"}]}],"usage":{"total_input_tokens":9}}`,
		`{}`, `not json`, `{"interaction":{"id":" ","steps":[]},"id":"outer","service_tier":" "}`, `{"interaction":{"steps":{"k":{"type":"model_output","content":"in object"}}},"steps":[{"type":"model_output","content":"outer"}]}`,
	} {
		out = append(out, nonStream(fmt.Sprintf("interactions-up/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// openAIInteractionsRequests exercise ConvertOpenAIRequestToInteractions: message roles,
// content parts, tool calls and results (with name recovery), generation settings and
// the antigravity variants (tool renames, agent token budget).
func openAIInteractionsRequests(model string) []fixture {
	messages := `"messages":[{"role":"System","content":"s1 <x>"},{"role":" developer ","content":[{"type":"text","text":"d1"},{"text":""},{"text":"d2"}]},{"role":"system","content":{"text":"obj"}},{"role":"system","content":5},{"role":"user","content":""},{"role":"user","content":"hi é"},{"role":"user","content":[{"type":"text","text":"t"},{"text":"untyped"},{"type":"INPUT_TEXT","text":"it"},{"type":"output_text"},{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBO"}},{"type":"image_url","image_url":{"url":"https://img"}},{"type":"input_image","image_url":"data:image/gif;BASE64,R0lG"},{"type":"image","data":"AA","mime_type":"image/webp"},{"type":"image","data":"BB"},{"type":"image","image_url":{"detail":"low"}},{"type":"image"},{"type":"input_audio","input_audio":{"data":"UklG","format":" WAV "}},{"type":"audio","data":"ZkxhQw","format":"flac"},{"type":"audio","input_audio":{"format":"mp3"}},{"type":"audio","data":"x"},{"type":"file","file":{"filename":"a.pdf","file_data":"JVBE"}},{"type":"input_file","file":{"file_data":"data:text/csv;base64,YQ"},"filename":"b.csv"},{"type":"document","mime_type":"text/plain","data":"aGk"},{"type":"file","file":{"file_url":"https://f"},"mimeType":"x/y"},{"type":"file","file":{"filename":"c.unknownext","file_data":"AA"}},{"type":"file"},{"type":"refusal","refusal":"no"}]},{"role":"user","content":{"type":"text","text":"single"}},{"role":"assistant","reasoning_content":"r1","content":"answer","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"p\":1}"}},{"id":"c2","function":{"name":"Lookup","arguments":"not json"}},{"id":"","function":{"name":"anon","arguments":{"o":1}}},{"type":"custom","function":{"name":"x"}},{"id":"c3"},{"id":"c4","function":{"name":"noargs"}},{"id":"c5","function":{"name":"empty","arguments":""}}]},{"role":"assistant","reasoning_content":[{"text":"a"},{"content":"b"},{"text":""},"s"],"content":[{"type":"text","text":"x"}]},{"role":"assistant","reasoning_content":{"text":"obj"},"content":null},{"role":"tool","tool_call_id":"c1","content":"file body"},{"role":"tool","tool_call_id":"c2","name":"Explicit","content":{"k":"<v>"}},{"role":"function","id":"c4"},{"role":"tool","tool_call_id":"unknown","content":[1]},{"role":"tool"}]`
	inputs := map[string]string{
		"messages":         `{` + messages + `}`,
		"top-level":        `{"model":"m","stream":false,"previous_response_id":" ","previous_interaction_id":"prev","environment":{"id":"env"},"agent_config":{"k":1},"messages":"notarray"}`,
		"top-level/2":      `{"previous_response_id":"p1","environment_id":"e1","environment":{"id":"e2"},"messages":[]}`,
		"generation":       `{"max_completion_tokens":10,"max_tokens":20,"temperature":0.5,"top_p":1e0,"presence_penalty":-1,"frequency_penalty":"2","n":2,"stop":["<s>"],"tool_choice":{"type":"function","function":{"name":"read_file"}},"reasoning_effort":" HIGH ","response_format":{"type":"json_object"},"modalities":["text"],"service_tier":"flex","messages":[{"role":"user","content":"x"}]}`,
		"generation/2":     `{"max_tokens":20,"stop":"x","tool_choice":"auto","reasoning_effort":5,"service_tier":5,"messages":[]}`,
		"tools":            `{"tools":[{"type":"function","function":{"name":"read_file","description":"d <&>","parameters":{"type":"object"}}},{"type":" Function ","name":"flat","description":5,"parameters":{"a":1}},{"name":"untyped"},{"type":"web_search"},{"type":"function","function":{"name":"  "}},{"function":{"description":"no name"}}],"messages":[]}`,
		"tools/object":     `{"tools":{"type":"function","function":{"name":"x"}},"messages":[]}`,
		"tools/empty":      `{"tools":[{"type":"other"}],"messages":[]}`,
		"antigravity/msgs": `{` + messages + `}`,
		"antigravity/gen":  `{"max_output_tokens":30,"temperature":1,"stop":["x"],"tool_choice":{"type":"function","function":{"name":"write_file"}},"tools":[{"type":"function","function":{"name":"execute_code"}},{"type":"function","function":{"name":"Execute_Code"}}],"messages":[]}`,
		"antigravity/gen2": `{"max_completion_tokens":7,"agent_config":{"max_total_tokens":99},"tool_choice":{"type":"tool","name":"read_file"},"messages":[]}`,
		"antigravity/gen3": `{"max_tokens":"8","tool_choice":{"type":"tool","name":"other","function":{"name":""}},"messages":[]}`,
		"antigravity/gen4": `{"tool_choice":"required","agent_config":"str","max_tokens":9,"messages":[]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		m := model
		if strings.HasPrefix(name, "antigravity/") {
			m = "Gemini-3-AntiGravity-agent"
		}
		out = append(out, req("openai-interactions/"+name, m, inputs[name], i%2 == 0))
	}
	out = append(out, req("openai-interactions/model-from-body", "", `{"model":"body-model","messages":[]}`, true))
	return out
}

// interactionsToOpenAI exercise ConvertInteractionsResponseToOpenAI(NonStream): tool call
// indexes per step, environment IDs, finish reasons, failures and antigravity names.
func interactionsToOpenAI() []fixture {
	cases := map[string][]string{
		"tools":   {`{"event_type":"step.start","index":0,"step":{"type":"function_call","name":"external_read_file","id":"i0"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"{\"p\":"}}`, `{"event_type":"step.start","index":2,"step":{"type":"function_call","name":"b","call_id":"c2","arguments":{"x":1}}}`, `{"event_type":"step.start","index":0,"step":{"type":"function_call","name":"again"}}`, `{"event_type":"step.delta","index":5,"delta":{"type":"arguments_delta","arguments":"<orphan>"}}`, `{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta"}}`, `{"event_type":"step.start","index":3,"step":{"type":"model_output"}}`, `{"event_type":"step.delta","index":3,"delta":{"type":"thought_signature","signature":"s"}}`, `{"event_type":"interaction.completed","interaction":{"status":"incomplete","environment":{"id":"env-done"}}}`, `{"event_type":"interaction.completed"}`},
		"created": {`{"event_type":"interaction.created","interaction":{"id":"int_9","model":"gemini-antigravity-x","environment_id":"env1"}}`, `{"event_type":"step.start","index":0,"step":{"type":"function_call","name":"external_write_file"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"via text"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary"}}`, `{"event_type":"finish","finish_reason":"content_filter","usage":{"input_tokens":1}}`},
		"reasons": {`{"event_type":"interaction.created","interaction":{"id":" ","model":""},"environment":{"id":"root-env"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"x"}}`, `{"event_type":"interaction.completed","interaction":{"finish_reason":"max_tokens"},"status":"completed"}`},
		"errors":  {`{"event_type":"response.failed","error":{"message":"bad <x>","code":"429","type":"rate_limit"}}`, `{"event_type":"interaction.failed","interaction":{"error":{"code":5}}}`, `{"event_type":"interaction.failed"}`, `{"event_type":"done"}`, "data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("interactions-openai/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		`{"interaction":{"id":"i1","model":"gemini-antigravity-1","status":"incomplete","environment":{"id":"e"},"steps":[{"type":"thought","content":[{"text":"r1"},{"content":{"text":"r2"}}]},{"type":"model_output","content":"plain "},{"type":"model_output","content":[{"type":"text","text":"a <b>"},{"text":""}]},{"type":"function_call","name":"external_execute_code","arguments":{"c":1}},{"type":"function_call","call_id":"c2","name":"f","arguments":"{\"s\":1}"},{"type":"function_call","id":"c3","name":"g"}],"usage":{"total_input_tokens":3,"total_output_tokens":4,"total_tokens":7,"total_cached_tokens":1,"total_thought_tokens":2}}}`,
		`{"id":"r","steps":[{"type":"model_output","content":5}],"finish_reason":"content_filter","environment_id":"top","interaction":{"environment_id":"ignored-nested"}}`,
		`{"interaction":{"model":"","steps":[{"type":"thought","content":"t"}]},"finish_reason":"length","interaction.environment_id":"x"}`,
		`{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("interactions-openai/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// interactionsOpenAIRequests exercise ConvertInteractionsRequestToOpenAI: system text,
// every step type, content part conversions, tool declarations and the generation
// config fallbacks to top-level fields.
func interactionsOpenAIRequests(model string) []fixture {
	steps := `"input":[{"type":"user_input","content":"plain"},{"type":"user_input","content":[{"type":"text","text":"a"},{"text":"b"},{"type":"image","data":"AA","mime_type":"image/png"},{"type":"image","url":"https://i"},{"type":"image","image_url":"data:x","file_data":"y"},{"type":"image"},{"type":"audio","data":"UklG","mime_type":"Audio/X-WAV"},{"type":"audio","mime_type":"audio/ogg"},{"type":"audio"},{"type":"video","data":"AA"},{"type":"video","url":"gs://v"},{"type":"document","data":"JVBE","mime_type":"application/pdf"},{"type":"file","filename":"n.txt","file_url":"https://f","data":"x"},{"type":"document","mime_type":"image/svg+xml"},{"type":"document","mime_type":"weird"},{"type":"Text","text":"case"},{"type":"thought"}]},{"type":"model_output","content":[{"type":"text","text":"m1"},{"type":"text","text":"m2"}]},{"type":"model_output","content":{"type":"text","text":"single"}},{"type":"model_output","content":{"type":"image","data":"QQ"}},{"type":"model_output"},{"type":"thought","content":[{"text":"t1"},{"content":{"text":"t2"}}]},{"type":"thought","content":{"text":"t3"}},{"type":"thought"},{"type":"function_call","name":"external_read_file","call_id":"c1","arguments":{"p":"<x>"}},{"type":"function_call","name":"g","id":"c2","arguments":"{\"s\":1}"},{"type":"function_call","name":"h"},{"type":"function_result","call_id":"c1","result":{"ok":true}},{"type":"function_result","id":"c2","output":"done"},{"type":"function_result"},"bare string",5,{"type":"unknown","content":"x"}]`
	inputs := map[string]string{
		"steps":             `{` + steps + `}`,
		"antigravity/steps": `{` + steps + `,"tools":[{"name":"external_write_file"},{"function_declarations":[{"name":"external_execute_code"}]}]}`,
		"input/object":      `{"input":{"type":"model_output","content":"obj"}}`,
		"input/string":      `{"input":"hello","stream":true}`,
		"input/number":      `{"input":5}`,
		"system/object":     `{"system_instruction":{"text":"st"},"input":"x"}`,
		"system/content":    `{"system_instruction":{"content":[{"text":"a"},{"content":{"text":"b"}},{}]},"input":"x"}`,
		"system/parts":      `{"system_instruction":{"content":"x","parts":[{"text":"p"}]},"input":"x"}`,
		"system/none":       `{"system_instruction":{"other":1},"input":"x"}`,
		"tools":             `{"tools":[{"name":"a","description":"d <x>","parameters":{"type":"object"}},{"function":{"name":"b","description":"fd","parameters":{"p":1}}},{"name":"c","parametersJsonSchema":{"j":1}},{"function_declarations":[{"name":"d"},{"description":"none"}],"functionDeclarations":[{"name":"ignored"}]},{"functionDeclarations":[{"name":"e","parameters":{}}]},{"name":"  "},{"type":"google_search"}],"input":"x"}`,
		"tools/none":        `{"tools":[{"type":"google_search"}],"input":"x"}`,
		"gen/snake":         `{"generation_config":{"temperature":0.1,"max_output_tokens":5,"top_p":0.2,"top_k":3,"candidate_count":2,"stop_sequences":["<s>"],"tool_choice":{"type":"auto"},"reasoning_effort":" Low ","thinking_level":"high"},"temperature":9,"response_modalities":["TEXT"],"input":"x"}`,
		"gen/camel":         `{"generationConfig":{"maxOutputTokens":6,"topP":0.3,"topK":4,"candidateCount":1,"stopSequences":"x","thinkingLevel":"MEDIUM"},"input":"x"}`,
		"gen/root":          `{"generation_config":{"thinking_config":{"thinking_level":"minimal"}},"temperature":0.7,"max_tokens":8,"max_completion_tokens":9,"top_p":0.4,"n":3,"stop":["y"],"tool_choice":"none","reasoning_effort":"high","input":"x"}`,
		"gen/effort":        `{"generation_config":{"reasoning_effort":5,"thinkingConfig":{"thinkingLevel":"x"}},"input":"x"}`,
		"gen/effort-root":   `{"generationConfig":{"thinking_level":7},"reasoning_effort":" XHIGH ","max_completion_tokens":4,"input":"x"}`,
		"gen/both":          `{"generation_config":{},"generationConfig":{"temperature":1},"input":"x"}`,
		"top-level":         `{"response_format":{"type":"json_object"},"service_tier":"flex","previous_interaction_id":" ","previous_response_id":"p","environment":{"id":"e"},"agent_config":{"a":1},"parallel_tool_calls":false,"seed":42,"user":"u<1>","input":"x"}`,
		"top-level/2":       `{"service_tier":3,"previous_interaction_id":"pi","environment_id":"ei","stream":"yes","input":"x"}`,
		"model":             `{"model":"body","input":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		m := model
		if strings.HasPrefix(name, "antigravity/") {
			m = "antigravity-agent"
		}
		out = append(out, req("interactions-openai/"+name, m, inputs[name], i%2 == 1))
	}
	out = append(out, req("interactions-openai/model-from-body", "", `{"model":"body-model","input":"x"}`, false))
	return out
}

// openAIToInteractions exercise ConvertOpenAIResponseToInteractions(NonStream): reasoning,
// text and tool-call deltas with step switching, finish reasons, usage-only chunks,
// stored usage, the done marker and antigravity names.
func openAIToInteractions() []fixture {
	chunk := func(delta, extra string) string {
		return `data: {"id":"chatcmpl-1","model":"gpt-x","choices":[{"index":0,"delta":` + delta + extra + `}]}`
	}
	cases := map[string][]string{
		"text":            {chunk(`{"role":"assistant","content":""}`, ``), chunk(`{"content":"Hi <b>"}`, ``), chunk(`{"content":" é"}`, `,"finish_reason":null`), chunk(`{"content":"more"}`, ``), chunk(`{}`, `,"finish_reason":"stop"`), `data: {"id":"chatcmpl-1","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7,"prompt_tokens_details":{"cached_tokens":1},"completion_tokens_details":{"reasoning_tokens":2}}}`, "data: [DONE]"},
		"reasoning-tools": {chunk(`{"reasoning_content":"think"}`, ``), chunk(`{"reasoning_content":[{"text":"a"},{"content":"b"},{"text":""}]}`, ``), chunk(`{"content":"x","tool_calls":[{"index":0,"id":"call_a","function":{"name":"read_file","arguments":""}}]}`, ``), chunk(`{"tool_calls":[{"index":0,"function":{"arguments":"{\"p\":"}}]}`, ``), chunk(`{"tool_calls":[{"index":0,"function":{"arguments":"1}"}},{"index":1,"function":{"name":"second","arguments":"{}"}}]}`, ``), chunk(`{"tool_calls":[{"index":1,"id":"late_id","function":{"arguments":"x"}}]}`, ``), chunk(`{}`, `,"finish_reason":"tool_calls"`), "data: [DONE]", "data: [DONE]"},
		"usage-stored":    {`data: {"id":"u1","choices":[{"delta":{"content":"a"}}],"usage":{"prompt_tokens":"5"}}`, `data: {"choices":[{"delta":{"content":"b"}}]}`, "data: [DONE]"},
		"framing":         {"", ": keepalive", "event: x\ndata: {\"choices\":[{\"delta\":{\"content\":\"multi\"}}]}", `{"choices":[{"delta":{"content":"bare"}}]}`, "data: not json", `data: {"choices":{"0":{"delta":{"content":"obj"}}}}`, `data: {"choices":[]}`, `data: {"object":"x"}`, "data: [DONE]"},
		"done-only":       {"data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("openai-interactions/"+name, "gpt-test", cases[name]...))
		if name == "reasoning-tools" {
			out = append(out, streamCase("openai-interactions/antigravity-"+name, "antigravity-x", cases[name]...))
		}
	}
	for i, body := range []string{
		`{"id":"chatcmpl-9","model":"gpt-x","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":[{"text":"r"}],"content":"answer <b>","tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{\"p\":1}"}},{"type":"custom","function":{"name":"skip"}},{"function":{"name":"noid","arguments":{"o":1}}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}`,
		`{"choices":[{"message":{"content":[{"type":"text","text":"array"}]},"finish_reason":null},{"message":{"reasoning_content":"second"},"finish_reason":"stop"}]}`,
		`{"choices":[{"message":{"content":""}}],"usage":{"completion_tokens_details":{"reasoning_tokens":"4"}}}`,
		`{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("openai-interactions/non-stream/%d", i), "gpt-test", body))
		if i == 0 {
			out = append(out, nonStream("openai-interactions/non-stream/antigravity", "antigravity-x", body))
		}
	}
	return out
}

// responsesInteractionsRequests exercise ConvertOpenAIResponsesRequestToInteractions:
// winning tool declarations (namespaces, custom tools, apply_patch, additional_tools),
// Devin filtering and description obfuscation, tool choices, input items and the
// antigravity variants.
func responsesInteractionsRequests(model string) []fixture {
	execDesc := `Runs a command, returning output or a session ID for ongoing interaction.`
	stdinDesc := `WRITES characters to an existing unified exec session and returns recent output`
	tools := `"tools":[{"type":"function","name":"lookup","description":"d <x>","parameters":{"type":"object"}},{"type":"function","function":{"name":"nested","description":"fd","parameters":{"p":1}}},{"type":"custom","name":"run","description":"r"},{"type":"custom","name":"apply_patch","description":"Patch. This is a FREEFORM tool, so do not wrap the patch in JSON."},{"type":"namespace","name":"mcp__codex_app","tools":[{"type":"function","name":"automation_update"},{"type":"function","name":"keep"}]},{"type":"function","name":"mcp__codex_app__automation_update"},{"type":"namespace","name":"shell","tools":[{"type":"function","name":"exec_command","description":"` + execDesc + `"},{"type":"function","name":"write_stdin","description":"` + stdinDesc + `."}]},{"type":"function","name":"exec_command","description":"` + execDesc + `"},{"type":"function","name":"lookup","description":"dup"},{"type":"web_search"},{"type":"function","name":"read_file","input_schema":{"s":1}}]`
	input := `"input":[{"type":"message","role":"user","content":"hi <b>"},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a"},{"type":"input_image","image_url":"data:image/png;base64,AA"},{"type":"input_image","image_url":"data:;base64,BB"},{"type":"input_image","url":"https://img"},{"type":"output_image","data":"CC","mime_type":"image/gif"},{"type":"input_image"},{"text":"untyped"},{"type":"refusal"}]},{"type":"message","role":"model","content":{"type":"input_text","text":"obj"}},{"type":"message","role":"system","content":5},{"type":"function_call","call_id":"c1","name":"list","namespace":"mcp","arguments":"{\"a\":1}"},{"type":"function_call","id":"c2","name":"read_file","arguments":" {\"b\":2} "},{"type":"function_call","name":"noid","arguments":"not json"},{"type":"function_call","name":"objargs","arguments":{"o":1}},{"type":"function_call","name":"noargs"},{"type":"custom_tool_call","call_id":"c3","name":"run","input":"ls <x>"},{"type":"custom_tool_call","call_id":"c4","name":"apply_patch","arguments":"{\"input\":\"p\"}"},{"type":"function_call_output","call_id":"c1","output":"{\"ok\":true}"},{"type":"function_call_output","call_id":"c2","output":"plain"},{"type":"custom_tool_call_output","call_id":"c3","result":{"r":1}},{"type":"function_call_output","call_id":"zz","name":"explicit","namespace":"ns","output":5},{"type":"function_call_output"},{"type":"input_text","text":"it"},{"type":"output_text","text":"ot"},{"type":"text"},{"type":"input_image","image_url":"data:image/jpeg;base64,DD"},{"type":"output_image","url":"u"},{"type":"reasoning","summary":[]},{"type":"other","content":[{"type":"output_text","text":"o"}]},{"type":"additional_tools","tools":[{"type":"function","name":"extra"},{"type":"function","name":"lookup","description":"loses"}]}]`
	inputs := map[string]string{
		"full":              `{"instructions":"sys <i>",` + tools + `,` + input + `,"tool_choice":{"type":"function","function":{"name":"read_file"}},"reasoning":{"effort":" HIGH ","summary":"auto"},"text":{"format":{"type":"json_object"}},"max_output_tokens":10,"temperature":0.5,"top_p":1,"presence_penalty":"0.25","frequency_penalty":-1,"stop":["<s>"],"stream":false,"previous_response_id":"p","environment":{"id":"e"},"agent_config":{"a":1}}`,
		"antigravity":       `{` + tools + `,` + input + `,"tool_choice":{"type":"function","name":"read_file"},"max_tokens":7,"temperature":1,"response_format":{"type":"text"}}`,
		"antigravity/agent": `{"max_completion_tokens":3,"agent_config":{"max_total_tokens":9},"tool_choice":{"type":"custom","custom":{"name":"execute_code","namespace":" "}},"input":"x"}`,
		"choice/namespace":  `{"tool_choice":{"type":"function","name":"list","namespace":"mcp"},"input":"x"}`,
		"choice/custom":     `{"tool_choice":{"type":"custom","custom":{"name":"apply_patch"}},"input":"x"}`,
		"choice/devin":      `{"tool_choice":{"type":"function","function":{"name":"automation_update","namespace":"mcp__codex_app"}},"input":"x"}`,
		"choice/devin2":     `{"tool_choice":{"type":"function","name":"MCP__codex_app__automation_update"},"input":"x"}`,
		"choice/string":     `{"tool_choice":"required","input":"x"}`,
		"choice/blank":      `{"tool_choice":{"type":"auto"},"input":"x"}`,
		"instructions/obj":  `{"instructions":{"text":"t"},"input":"x"}`,
		"instructions/arr":  `{"instructions":{"content":[{"text":"a"},{"text":""},{"text":"b"}]},"input":"x"}`,
		"instructions/num":  `{"instructions":5,"input":{"type":"input_text","text":"obj input"}}`,
		"instructions/null": `{"instructions":null,"input":{"type":"unknown"}}`,
		"reasoning/odd":     `{"reasoning":{"effort":5,"summary":7},"input":"x","stream":true}`,
		"tools/array-root":  `[{"type":"function","name":"from_array"}]`,
		"tools/none":        `{"tools":[{"type":"web_search"}],"input":"x"}`,
		"model":             `{"model":"body-model","input":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		m := model
		if strings.HasPrefix(name, "antigravity") {
			m = "gemini-antigravity-1"
		}
		if name == "model" {
			m = " "
		}
		out = append(out, req("responses-interactions/"+name, m, inputs[name], i%2 == 0))
	}
	out = append(out, req("responses-interactions/devin-model", "Devin/agent", `{`+tools+`,"input":"x"}`, false))
	return out
}

// interactionsToResponses exercise ConvertInteractionsResponseToOpenAIResponses(NonStream):
// step items, identities, the apply_patch bridge (streamed, late-named, snapshots,
// invalid input, conflicts, failures, EOF) and completion states.
func interactionsToResponses() []fixture {
	original := `{"model":"client-model","input":"q","tools":[{"type":"function","name":"f"},{"type":"custom","name":"run"},{"type":"custom","name":"apply_patch"},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list"},{"type":"custom","name":"exec"}]}]}`
	plain := `{"model":"client-model","input":"q","tools":[{"type":"function","name":"f"},{"type":"namespace","name":"mcp","tools":[{"type":"function","name":"list"}]},{"type":"custom","name":"run"}]}`
	created := `{"event_type":"interaction.created","interaction":{"id":"int_1","model":"m","environment":{"id":"env"}}}`
	start := func(index int, step string) string {
		return fmt.Sprintf(`{"event_type":"step.start","index":%d,"step":%s}`, index, step)
	}
	args := func(index int, a string) string {
		return fmt.Sprintf(`{"event_type":"step.delta","index":%d,"delta":{"type":"arguments_delta","arguments":%q}}`, index, a)
	}
	stop := func(index int) string { return fmt.Sprintf(`{"event_type":"step.stop","index":%d}`, index) }
	done := `{"event_type":"interaction.completed","interaction":{"id":"int_1","usage":{"total_input_tokens":3,"total_output_tokens":2}}}`
	patchStart := `{"type":"function_call","name":"apply_patch","id":"ap_1","call_id":"call_ap","arguments":{}}`
	type sc struct {
		original string
		finalize bool
		lines    []string
	}
	cases := map[string]sc{
		"basic": {plain, false, []string{created, start(0, `{"type":"thought","id":"th_1"}`), `{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":" ` + geminiSig + ` "}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","text":"think <b>"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"more"}}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary"}}`, stop(0),
			start(1, `{"type":"model_output"}`), `{"event_type":"step.delta","index":1,"delta":{"type":"text","text":"Hello é"}}`, `{"event_type":"step.delta","index":1,"delta":{"type":"text"}}`, stop(1),
			start(2, `{"type":"function_call","name":"mcp__list","id":"fc_2","call_id":"call_2","arguments":{}}`), args(2, `{"q":`), args(2, `1}`), stop(2),
			start(3, `{"type":"function_call","name":"run","call_id":"call_3"}`), args(3, `{"input":"ls -la"}`), stop(3),
			start(4, `{"type":"function_call","name":"unknown_fn","id":"u4","arguments":{"x":"<y>"}}`), stop(4), stop(9), start(5, `{"type":"thought"}`), `{"event_type":"step.delta","index":5,"delta":{"type":"thought_signature","signature":"not-a-signature"}}`,
			`{"event_type":"interaction.completed","interaction":{"status":"incomplete","usage":{"total_input_tokens":3,"total_output_tokens":2,"cached_tokens":1,"reasoning_tokens":4}}}`, "[DONE]", start(6, `{"type":"model_output"}`)}},
		"patch-ok":        {original, false, []string{created, start(0, patchStart), args(0, `{"input":"*** Begin Patch\n`), args(0, `+hi <x> é\n*** End Patch"}`), stop(0), done, "[DONE]"}},
		"patch-late-name": {original, false, []string{created, start(0, `{"type":"function_call","id":"ap_2","call_id":"call_2"}`), args(0, `{"input":"a`), args(0, `b"}`), `{"event_type":"step.delta","index":0,"step":{"name":"apply_patch"},"delta":{"type":"arguments_delta","arguments":""}}`, stop(0), done}},
		"patch-snapshot":  {original, false, []string{created, start(0, `{"type":"function_call","name":"apply_patch","id":"ap_3","call_id":"call_3","arguments":{"input":"full"}}`), stop(0), `{"event_type":"interaction.completed","interaction":{"steps":[{"type":"function_call","name":"apply_patch","id":"ap_3","call_id":"call_3","arguments":{"input":"full"}}]}}`}},
		"patch-invalid":   {original, false, []string{created, start(0, patchStart), args(0, `{"input":5}`), stop(0), done}},
		"patch-conflict":  {original, false, []string{created, start(0, patchStart), `{"event_type":"step.delta","index":1,"step":{"id":"ap_1"},"delta":{"type":"arguments_delta","arguments":"x"}}`}},
		"patch-eof":       {original, true, []string{created, start(0, patchStart), args(0, `{"input":"partial`)}},
		"plain-eof":       {plain, true, []string{created, start(0, `{"type":"model_output"}`)}},
		"patch-failed":    {original, false, []string{created, `{"event_type":"interaction.failed","error":{"message":"m"}}`}},
		"failed":          {plain, false, []string{created, `{"event_type":"response.failed","interaction":{"id":"","error":{"message":"bad <x>","code":"429","type":"rate_limit"}}}`, start(0, `{"type":"model_output"}`)}},
		"failed-bare":     {plain, false, []string{`{"event_type":"interaction.failed"}`}},
		"invalid-step":    {original, false, []string{created, start(0, patchStart), `{"event_type":"step.delta","index":0,"delta":{"type":"arguments_delta","arguments":"x"}`}},
		"invalid-env":     {plain, false, []string{created, `{"event_type":"interaction.completed","status":"incomplete"`}},
		"unresolved":      {original, false, []string{created, start(0, `{"type":"function_call","id":"x0"}`), "data: [DONE]"}},
		"done-event":      {plain, false, []string{created, start(0, `{"type":"model_output","id":"m0"}`), `{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"x"}}`, `{"event_type":"done"}`, `{"event_type":"step.delta","index":0,"delta":{"type":"text","text":"late"}}`}},
		"completed-steps": {original, false, []string{created, `{"event_type":"finish","finish_reason":"content_filter","steps":[{"type":"function_call","name":"apply_patch","arguments":"{\"input\":\"c\"}"},{"type":"model_output"}],"environment_id":"e2"}`}},
		"resolve":         {plain, false, []string{created, `{"event_type":"step.start","step":{"type":"function_call","name":"f","id":"i1","call_id":"c1"}}`, `{"event_type":"step.delta","step":{"call_id":"c1"},"delta":{"type":"arguments_delta","arguments":"{}"}}`, `{"event_type":"step.start","step":{"type":"model_output","id":"i9"}}`, `{"event_type":"step.stop","step":{"id":"i9"}}`, `{"event_type":"step.stop","step":{"id":"i1","index":0}}`, done}},
		"sse-shaped":      {plain, false, []string{"event: interaction.created\ndata: " + created, "data: " + start(0, `{"type":"model_output"}`), ": ping", "data:", stop(0)}},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		c := cases[name]
		f := streamCase("interactions-responses/"+name, "gemini-2.5-pro", c.lines...)
		f.Original = c.original
		f.Finalize = c.finalize
		out = append(out, f)
	}
	ag := streamCase("interactions-responses/antigravity", "gemini-antigravity-1", created, start(0, `{"type":"function_call","name":"external_read_file","id":"r0"}`), stop(0), start(1, `{"type":"function_call","name":"external_other","id":"r1"}`), stop(1), done)
	ag.Original = `{"tools":[{"type":"function","name":"read_file"}]}`
	out = append(out, ag)
	steps := `"steps":[{"type":"model_output","id":"mo","content":"str <b>"},{"type":"model_output","step_id":"st","content":[{"type":"text","text":"t"},{"text":"untyped"},{"type":"image","data":"AA","mime_type":"image/png"},{"type":"image","url":"https://i"},{"type":"audio","mime_type":"audio/wav"},{"type":"audio"},{"type":"audio","mime_type":"weird"},{"type":"video","data":"VV"},{"type":"document","url":"https://d","filename":"d.pdf"},{"type":"other"}]},{"type":"thought","signature":"bogus","content":[{"text":"r1","signature":"` + geminiSig + `"},{"content":{"text":"r2"}}]},{"type":"thought","encrypted_content":"` + gptSignature() + `","content":"plain"},{"type":"function_call","name":"mcp__list","call_id":"c1","arguments":{"a":"<b>"}},{"type":"function_call","name":"run","id":"c2","arguments":"{\"input\":\"ls\"}"},{"type":"function_call","name":"zzz"},{"type":"function_result","name":"f"}]`
	for i, c := range []struct{ original, body string }{
		{plain, `{"id":"i1","model":"m1",` + steps + `,"status":"incomplete","interaction":{"environment":{"id":"env-n"}},"usage":{"input_tokens":1,"output_tokens":2}}`},
		{plain, `{"interaction":{"id":"i2","model":"","steps":[{"type":"model_output","content":"x"}],"status":"completed","finish_reason":"content_filter","environment_id":"e"},"metadata":{"usage":{"total_tokens":7}}}`},
		{"", `{"steps":[],"finish_reason":"max_tokens"}`},
		{original, `{"id":"p1","steps":[{"type":"function_call","name":"apply_patch","call_id":"c","arguments":"{\"input\":\"*** Begin Patch\\n*** End Patch\"}"}]}`},
		{original, `{"id":"p2","steps":[{"type":"function_call","name":"apply_patch","arguments":{"input":5}}]}`},
		{original, `{"id":"p3","steps":[{"type":"function_call","arguments":{}}]}`},
		{original, `{"id":"p4","status":"failed","steps":[]}`},
		{original, `{"id":"p5","error":null,"steps":[{"type":"model_output","content":"ok"}]}`},
		{original, `{"id":"p6","interaction":{"error":{"message":"x"}},"steps":[]}`},
		{original, `{"id":"p7","steps":[{"type":"function_call","name":"apply_patch","arguments":{"input":"x"}}]`},
		{`{"request":{"tools":[{"type":"custom","name":"apply_patch"}]}}`, `{"steps":[{"type":"function_call","name":"apply_patch","arguments":"{\"input\":\"y\"}"}]}`},
		{plain, `not json`},
	} {
		n := nonStream(fmt.Sprintf("interactions-responses/non-stream/%d", i), "gemini-2.5-pro", c.body)
		n.Original = c.original
		out = append(out, n)
	}
	return out
}

// interactionsResponsesRequests exercise ConvertInteractionsRequestToOpenAIResponses:
// system text, every input step type, content parts per role, tools with snake
// declarations, tool choices, thinking settings and the antigravity variant.
func interactionsResponsesRequests(model string) []fixture {
	steps := `"input":[{"type":"user_input","content":"plain"},{"type":"user_input","content":[{"type":"text","text":"a"},{"text":"b"},{"type":"image","data":"AA"},{"type":"image","mime_type":"image/png","data":"BB"},{"type":"image","file_data":"data:x"},{"type":"audio","mime_type":"audio/x-wav"},{"type":"audio","mime_type":"audio/"},{"type":"video","data":"VV","mime_type":"video/mp4"},{"type":"document","url":"https://d","filename":"d.pdf"},{"type":"document"},{"type":"thought"}]},{"type":"user_input","content":{"type":"text","text":"obj values"}},{"type":"model_output","content":[{"type":"text","text":"m"},{"type":"image","url":"u"},{"type":"video","data":"W"}]},{"type":"model_output","content":"answer"},{"type":"model_output"},{"type":"thought","content":[{"text":"t1"},{"content":{"text":"t2"}},{"text":""}]},{"type":"thought","content":"str"},{"type":"thought"},{"type":"function_call","name":"external_read_file","call_id":"c1","arguments":{"p":"<x>"}},{"type":"function_call","name":"g","id":"c2","arguments":"{\"s\":\"<y>\"}"},{"type":"function_call","name":"h"},{"type":"function_result","call_id":"c1","name":"external_read_file","result":{"ok":true}},{"type":"function_result","id":"c2","output":"done"},{"type":"function_result"},"bare",5,{"type":"unknown"}]`
	inputs := map[string]string{
		"steps":             `{` + steps + `}`,
		"antigravity/steps": `{` + steps + `,"tools":[{"name":"external_write_file"},{"function_declarations":[{"name":"external_execute_code","description":"d"}],"functionDeclarations":[{"name":"ignored"}]}]}`,
		"input/object":      `{"input":{"type":"model_output","content":"obj"}}`,
		"input/string":      `{"input":"hello","stream":true}`,
		"system/string":     `{"system_instruction":"s <x>","input":"x"}`,
		"system/text":       `{"system_instruction":{"text":5},"input":"x"}`,
		"system/parts":      `{"system_instruction":{"parts":[{"text":"a"},{"text":""},{"text":"b"}]},"input":"x"}`,
		"system/other":      `{"system_instruction":{"content":"x"},"input":"x"}`,
		"tools":             `{"tools":[{"name":"a","description":"d <x>","parameters":{"type":"object"}},{"function":{"name":"b","description":"fd","parameters":{"p":1}}},{"name":"c","parametersJsonSchema":{"j":1}},{"function_declarations":[{"name":"d"},{"description":"none"}]},{"name":"  "}],"input":"x"}`,
		"choice/gen":        `{"generation_config":{"tool_choice":{"type":"auto"},"thinking_level":" HIGH ","thinking_summaries":"auto"},"tool_choice":"none","input":"x"}`,
		"choice/top":        `{"tool_choice":"required","generation_config":{"thinkingConfig":{"thinkingLevel":"Low"},"thinking_summaries":5},"input":"x"}`,
		"effort/snake":      `{"generation_config":{"thinking_config":{"thinking_level":"minimal"},"thinking_level":7},"input":"x"}`,
		"effort/empty":      `{"generation_config":{"thinking_level":"  ","thinkingConfig":{"thinkingLevel":"high"}},"input":"x"}`,
		"top-level":         `{"response_modalities":["TEXT"],"service_tier":"flex","response_format":{"type":"json_object"},"previous_interaction_id":" ","previous_response_id":"p","environment":{"id":"e"},"agent_config":{"a":1},"input":"x"}`,
		"top-level/2":       `{"service_tier":3,"previous_interaction_id":"pi","environment_id":"ei","stream":1,"input":"x"}`,
		"model":             `{"model":"body","input":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		m := model
		if strings.HasPrefix(name, "antigravity/") {
			m = "antigravity-agent"
		}
		if name == "model" {
			m = ""
		}
		out = append(out, req("interactions-responses/"+name, m, inputs[name], i%2 == 1))
	}
	return out
}

// responsesToInteractions exercise ConvertOpenAIResponsesResponseToInteractions(NonStream):
// Responses events to Interactions steps, with text and argument dedupe keys, message
// fallbacks on item and response completion, and antigravity names.
func responsesToInteractions() []fixture {
	ev := func(body string) string { return "data: " + body }
	cases := map[string][]string{
		"text": {ev(`{"type":"response.created","response":{"id":"resp_1","model":"gpt-x"}}`), ev(`{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1"}}`), ev(`{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"Hi <b>"}`), ev(`{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":" é"}`),
			ev(`{"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"Hi <b> é"},{"type":"output_text","text":"second part"},{"type":"refusal","text":"no"},{"type":"text","text":""}]}}`),
			ev(`{"type":"response.output_text.delta","delta":"unkeyed"}`), ev(`{"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_2","content":[{"type":"output_text","text":"skipped by unkeyed"}]}}`),
			ev(`{"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[{"type":"message","id":"msg_1","content":[{"type":"output_text","text":"dup"},{"type":"output_text","text":"dup2"}]},{"type":"message","id":"msg_3","content":[{"type":"output_text","text":"late text"}]},{"type":"reasoning"}],"usage":{"input_tokens":3,"output_tokens":4,"total_tokens":7,"input_tokens_details":{"cached_tokens":1},"output_tokens_details":{"reasoning_tokens":2}}}}`), "data: [DONE]"},
		"tools": {ev(`{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}`), ev(`{"type":"response.reasoning_summary_text.delta","delta":"think"}`), ev(`{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","summary":[{"text":"s1"},{"text":""},{"text":"s2"}]}}`),
			ev(`{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}`), ev(`{"type":"response.function_call_arguments.delta","item_id":"fc_1","output_index":1,"delta":"{\"p\":\"<x>\"}"}`), ev(`{"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"p\":\"<x>\"}"}}`),
			ev(`{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_2","name":"other","arguments":"{}"}}`), ev(`{"type":"response.function_call_arguments.delta","call_id":"call_3","delta":"{}"}`), ev(`{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_3","arguments":""}}`),
			ev(`{"type":"response.output_item.added","item":{"type":"other"}}`), ev(`{"type":"response.incomplete","response":{"status":"incomplete","model":"gpt-y"}}`), ev(`{"type":"response.output_text.delta","delta":"after"}`)},
		"framing":   {"", ": keepalive", "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"multi\"}", `{"type":"response.output_text.delta","delta":"bare"}`, "data: not json", "data: [DONE]", "data: [DONE]"},
		"done-only": {"data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("responses-interactions/"+name, "gemini-2.5-pro", cases[name]...))
		if name == "tools" {
			out = append(out, streamCase("responses-interactions/antigravity-"+name, "antigravity-x", cases[name]...))
		}
	}
	for i, body := range []string{
		`{"id":"resp_9","model":"gpt-x","status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"a <b>"},{"type":"input_image","image_url":"data:image/png;base64,AA"},{"text":"untyped"},{"type":"refusal"}]},{"type":"function_call","call_id":"c1","name":"read_file","arguments":"{\"x\":1}"},{"type":"function_call","name":"f","namespace":"mcp","arguments":"not json"},{"type":"reasoning","summary":[{"text":"s"},{"text":""}]},{"type":"custom_tool_call","name":"run"}],"usage":{"input_tokens":1,"output_tokens":"2"}}`,
		`{"output":{"a":{"type":"message","content":[]}}}`, `{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("responses-interactions/non-stream/%d", i), "gemini-2.5-pro", body))
		if i == 0 {
			out = append(out, nonStream("responses-interactions/non-stream/antigravity", "antigravity-x", body))
		}
	}
	return out
}

// interactionsClaudeRequests exercise ConvertInteractionsRequestToClaude: generation
// config and thinking levels, tool choices, message accumulation (role merges, tool_use
// ordering), content parts, tool results and declarations.
func interactionsClaudeRequests(model string) []fixture {
	inputs := map[string]string{
		"merge":          `{"input":[{"type":"user_input","content":"u1"},{"type":"user_input","content":[{"type":"text","text":"u2"},{"text":"untyped"},"bare",{"type":"thinking","text":"user thinking dropped"},{"type":"image","mime_type":"image/png","data":"AA"},{"type":"image","source":{"media_type":"image/gif","data":"BB"}},{"type":"image","data":"CC"},{"type":"file","mimeType":"application/pdf","file_data":"DD"},{"type":"audio","data":"EE"},{"type":"video","file_data":"FF"},{"type":"other"},{"type":"x","content":{"text":"nested"}},{"type":"y","parts":[{"text":"p1"},{"text":""},{"thinking":"p2"}]}]},{"type":"model_output","content":[{"type":"text","text":"m1"},{"type":"thinking","thinking":"t"},{"type":"reasoning","content":"r"}]},{"type":"function_call","name":"a.b c","id":"c1","arguments":{"x":1}},{"type":"model_output","content":"after call"},{"type":"function_call","name":"g","call_id":"c 2","args":"str"},{"type":"function_call"},{"type":"function_result","call_id":"c1","result":"ok <x>"},{"type":"function_result","id":"c 2","output":[{"type":"text","text":"r1"},{"type":"image","mime_type":"image/png","data":"AA"},{"type":"blob"}],"is_error":true},{"type":"function_result","name":"n"},{"type":"function_result","tool_use_id":"t9","result":{"k":"v"},"is_error":"yes"},{"type":"thought","content":[{"type":"thinking","thinking":"th"}],"role":"user"},{"type":"model_output","text":"via text"},{"type":"model_output","content":{"text":"object ignored"}},{"type":"user_input","role":"assistant","content":"role override"}]}`,
		"nested":         `{"input":[{"role":"model","steps":[{"type":"user_input","content":"as assistant"},{"type":"function_call","name":"x"}]},{"role":"user","parts":[{"text":"p"}]},{"role":"assistant","parts":[{"text":"q"}]},{"steps":"notarray","content":"s"}]}`,
		"object":         `{"input":{"type":"model_output","content":"obj"},"stream":true}`,
		"string":         `{"input":"hello <b>","systemInstruction":{"parts":[{"text":"a"},{"text":""},{"content":{"text":"b"}}]}}`,
		"number":         `{"input":5,"system_instruction":{"thinking":"sys thinking"}}`,
		"config/snake":   `{"generation_config":{"max_output_tokens":100,"top_p":0.9,"temperature":0.1,"stop_sequences":["<s>"],"thinking_level":"HIGH","tool_choice":"any"},"input":"x"}`,
		"config/camel":   `{"generationConfig":{"maxOutputTokens":200,"topP":0.8,"stopSequences":"x","thinkingLevel":"minimal","toolChoice":{"type":"tool","name":"a.b"}},"input":"x"}`,
		"config/both":    `{"generation_config":{"max_output_tokens":1,"maxOutputTokens":2,"reasoning":{"effort":"medium"}},"generationConfig":{"temperature":9},"input":"x"}`,
		"thinking/none":  `{"generation_config":{"thinking_level":" Off "},"input":"x"}`,
		"thinking/auto":  `{"generation_config":{"thinkingLevel":"adaptive"},"reasoning":{"effort":"xhigh"},"input":"x"}`,
		"thinking/max":   `{"reasoning":{"effort":"max"},"input":"x"}`,
		"thinking/other": `{"reasoning":{"thinking_level":"turbo"},"input":"x"}`,
		"thinking/empty": `{"reasoning":{"effort":"  "},"generation_config":{"thinking_level":7},"input":"x"}`,
		"thinking/swap":  `{"generation_config":{"thinking_level":"none"},"reasoning":{"effort":"low"},"input":"x"}`,
		"choice/strings": `{"tool_choice":" AUTO ","toolChoice":"required","input":"x"}`,
		"choice/objects": `{"tool_choice":{"type":"function","function":{"name":"f g"}},"generation_config":{"toolChoice":{"type":"auto"}},"input":"x"}`,
		"choice/other":   `{"tool_choice":{"type":"none"},"toolChoice":5,"input":"x"}`,
		"choice/noname":  `{"tool_choice":{"type":"tool"},"input":"x"}`,
		"tools":          `{"tools":[{"function_declarations":[{"name":"a.b","description":"d <x>","parameters":{"type":"object","properties":{"q":{"type":"string"}}}},{"description":"no name"}]},{"functionDeclarations":[{"name":"c","parametersJsonSchema":{"type":"object","additionalProperties":false}}]},{"name":"direct","function":{"description":"fd"},"input_schema":{"type":"object"}},{"function":{"name":"fn","description":"fd2"},"parameters_json_schema":"str"},{"type":"google_search"},{"function_declarations":"str","name":"fallback"}],"input":"x"}`,
		"tools/none":     `{"tools":[{"type":"code_execution"}],"input":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("interactions-claude/"+name, model, inputs[name], i%2 == 0))
	}
	return out
}

// claudeToInteractions exercise ConvertClaudeResponseToInteractions(NonStream): block
// lifecycles with step index reuse, deltas for unknown blocks, usage merging, errors
// and buffered streams.
func claudeToInteractions() []fixture {
	ev := func(body string) string { return "data: " + body }
	cases := map[string][]string{
		"blocks": {ev(`{"type":"message_start","message":{"id":"msg_1","model":"claude-x","usage":{"input_tokens":5,"cache_read_input_tokens":2}}}`), "event: content_block_start", ev(`{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}`), ev(`{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"plan <x>"}}`), ev(`{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"s"}}`), ev(`{"type":"content_block_stop","index":0}`),
			ev(`{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}`), ev(`{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hi é"}}`), ev(`{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu_1","name":"lookup","input":{"pre":1}}}`), ev(`{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"late text"}}`), ev(`{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":"}}`), ev(`{"type":"content_block_stop","index":2}`), ev(`{"type":"content_block_stop","index":1}`),
			ev(`{"type":"content_block_delta","index":5,"delta":{"type":"input_json_delta","partial_json":"{}"}}`), ev(`{"type":"content_block_delta","index":6,"delta":{"type":"thinking_delta","thinking":"t6"}}`), ev(`{"type":"content_block_delta","index":7,"delta":{"type":"other"}}`),
			ev(`{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7,"thinking_tokens":3}}`), ev(`{"type":"message_stop"}`), "data: [DONE]", "[DONE]"},
		"error":  {ev(`{"type":"error","error":{"type":"overloaded_error","message":"busy"},"usage":{"input_tokens":1,"output_tokens":2}}`), ev(`{"type":"message_stop"}`)},
		"stop":   {ev(`{"type":"message_start","message":{"id":"","model":""}}`), ev(`{"type":"message_stop","usage":{"input_tokens":4}}`)},
		"frames": {"", "event: ping", `{"type":"message_start"}`, "data: not json", "data:", ev(`{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking"}}`), ev(`{"type":"message_delta","usage":{"cache_creation_input_tokens":-3,"cache_read_input_tokens":3}}`)},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("claude-interactions/"+name, "claude-opus-4-6", cases[name]...))
	}
	for i, body := range []string{
		`{"id":"msg_9","model":"claude-y","content":[{"type":"thinking","thinking":"t <x>"},{"type":"text","text":"answer"},{"type":"tool_use","id":"tu","name":"f","input":{"a":1}},{"type":"tool_use","name":"g","input":"str"},{"type":"redacted_thinking"}],"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":1,"thinking_tokens":2}}`,
		`{"content":[],"usage":{"output_tokens":"5"}}`,
		"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b\",\"model\":\"m\",\"usage\":{\"input_tokens\":2}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"f\",\"input\":{\"z\":1}}}\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\" {\\\"a\\\":2} \"}}\ndata: {\"type\":\"content_block_stop\",\"index\":0}\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"th\"}}\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"thinking\"}}\ndata: {\"type\":\"content_block_stop\",\"index\":1}\ndata: {\"type\":\"content_block_stop\",\"index\":9}\r\ndata: [DONE]\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":6}}",
		"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"input\":{}}}\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"not json\"}}\ndata: {\"type\":\"content_block_stop\",\"index\":0}",
		`not json`, ``,
	} {
		out = append(out, nonStream(fmt.Sprintf("claude-interactions/non-stream/%d", i), "claude-opus-4-6", body))
	}
	return out
}

// claudeInteractionsRequests exercise ConvertClaudeRequestToInteractions: system
// reminders deferred around tool results, tool result alignment and contents, thinking
// and tool choices, media and tools.
func claudeInteractionsRequests(model string) []fixture {
	inputs := map[string]string{
		"reminders":       `{"messages":[{"role":"user","content":"q"},{"role":"system","content":"plain reminder"},{"role":"assistant","content":[{"type":"text","text":"calling"},{"type":"tool_use","id":"t1","name":"a","input":{"x":1}},{"type":"tool_use","id":"t2","name":"b","input":"str"}]},{"role":"system","content":[{"type":"text","text":"deferred <r>"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"r2"},{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"pure"},{"type":"text","text":"extra","citations":[]},{"type":"image","source":{"media_type":"image/png","data":"AA"}},{"type":"image","source":{}},{"type":"document","data":"BB","mime_type":"application/pdf"},{"type":"search_result"},"str"],"is_error":true},{"type":"text","text":"after results"}]},{"role":"system","content":""},{"role":"assistant","content":"done"}]}`,
		"reminder-tail":   `{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"a"}]},{"role":"system","content":"tail reminder"}]}`,
		"reminder-media":  `{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"a"}]},{"role":"system","content":"r"},{"role":"user","content":[{"type":"image","source":{"media_type":"image/png","data":"AA"}},{"type":"text","text":"t"}]}]}`,
		"reminder-string": `{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"a"}]},{"role":"system","content":"r"},{"role":"user","content":"plain"}]}`,
		"parts":           `{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":""},{"type":"thinking","thinking":"t"},{"type":"text","text":""},{"type":"redacted_thinking"},{"type":"tool_use","name":"noid"}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":{"k":1}},{"type":"tool_result","name":"named","content":5},{"type":"tool_result","tool_use_id":"x2"}]},{"role":"other","content":"o"},{"role":"user","content":{"type":"text"}}]}`,
		"thinking":        `{"thinking":{"type":"Enabled","budget_tokens":"2048"},"output_config":{"effort":" MAX "},"max_tokens":9,"temperature":0,"top_p":1,"stop_sequences":["x"],"messages":[]}`,
		"thinking/2":      `{"thinking":{"type":"enabled"},"messages":[]}`,
		"thinking/3":      `{"thinking":{"type":"disabled"},"output_config":{"effort":5},"messages":[]}`,
		"thinking/4":      `{"thinking":{"type":"adaptive"},"messages":[]}`,
		"choice/any":      `{"tool_choice":{"type":"any"},"messages":[]}`,
		"choice/tool":     `{"tool_choice":{"type":"tool","name":" t "},"messages":[]}`,
		"choice/string":   `{"tool_choice":"Required","messages":[]}`,
		"choice/none":     `{"tool_choice":{"type":"none"},"messages":[]}`,
		"tools":           `{"tools":[{"name":" a ","description":"d <x>","input_schema":{"type":"object"}},{"name":"b","input_schema":"str"},{"name":" "},{"type":"web_search_20250305","name":"web_search"}],"messages":[]}`,
		"system":          `{"system":[{"type":"text","text":"s1"},{"text":""},"s2",{"type":"image"}],"messages":"x","stream":false}`,
		"model":           `{"model":"body","system":{"text":"obj"},"messages":[]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		m := model
		if name == "model" {
			m = " "
		}
		out = append(out, req("claude-interactions/"+name, m, inputs[name], i%2 == 1))
		if name == "parts" || name == "reminders" {
			f := req("compat/claude-interactions/"+name, m, inputs[name], false)
			f.Path = "request_compat"
			out = append(out, f)
		}
	}
	return out
}

// interactionsToClaude exercise ConvertInteractionsResponseToClaude(NonStream): block
// switching, tool blocks from deltas, signatures, stop reasons, usage with cache
// splits and errors.
func interactionsToClaude() []fixture {
	cases := map[string][]string{
		"blocks": {`{"event_type":"interaction.created","interaction":{"id":"int_c","model":"gemini-x"}}`, `{"event_type":"step.start","index":0,"step":{"type":"thought"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary","content":{"text":"plan"}}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_signature","signature":"sig"}}`, `{"event_type":"step.delta","index":0,"delta":{"type":"thought_summary"}}`, `{"event_type":"step.stop","index":0}`, `{"event_type":"step.delta","index":1,"delta":{"type":"thought_signature","signature":"late"}}`,
			`{"event_type":"step.start","index":1,"step":{"type":"model_output"}}`, `{"event_type":"step.delta","index":1,"delta":{"type":"text","text":"Hi <b>"}}`, `{"event_type":"step.delta","index":1,"delta":{"type":"text"}}`, `{"event_type":"step.delta","index":1,"delta":{"type":"thought_summary","text":"switch"}}`,
			`{"event_type":"step.start","index":2,"step":{"type":"function_call","name":"f","call_id":"c2","thoughtSignature":"fs"}}`, `{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta","arguments":"{\"a\":"}}`, `{"event_type":"step.delta","index":2,"delta":{"type":"arguments_delta"}}`, `{"event_type":"step.stop","index":2}`,
			`{"event_type":"step.delta","index":4,"step":{"name":"from_delta"},"delta":{"type":"arguments_delta","arguments":"{}"}}`, `{"event_type":"step.delta","index":5,"delta":{"type":"other","text":"x"}}`,
			`{"event_type":"interaction.completed","interaction":{"status":"incomplete","usage":{"total_input_tokens":10,"total_output_tokens":3,"cached_tokens":4,"cache_creation_tokens":2}}}`, `{"event_type":"done"}`, `{"event_type":"step.start","index":9,"step":{"type":"model_output"}}`},
		"error":   {`{"event_type":"step.start","index":0,"step":{"type":"model_output"}}`, `{"event_type":"interaction.failed","interaction":{"error":{"message":"m","type":"invalid_request_error"}}}`, `{"event_type":"response.failed"}`, "[DONE]", "data: [DONE]"},
		"usage":   {`{"event_type":"finish","finish_reason":"max_tokens","usage":{"input_tokens":5,"output_tokens":1,"cache_read_input_tokens":-1}}`, `{"event_type":"interaction.completed","usage":{"prompt_tokens":1}}`, "data: [DONE]"},
		"usage-2": {`{"event_type":"interaction.completed","metadata":{"usage":{"total_input_tokens":2,"cached_tokens":5}}}`},
		"framing": {"", ": c", "event: x\ndata: {\"event_type\":\"step.delta\",\"index\":0,\"delta\":{\"type\":\"text\",\"text\":\"framed\"}}", "data: not json", `{"event_type":"step.stop","index":0}`},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("interactions-claude/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		`{"interaction":{"id":"i1","model":"m1","status":"incomplete","steps":[{"type":"thought","content":[{"text":"t1"},{"content":{"text":"t2"}}],"signature":"s"},{"type":"thought","content":"single","thought_signature":"ts"},{"type":"model_output","content":[{"text":"a <b>"},{"text":""}]},{"type":"function_call","name":"f","call_id":"c","arguments":{"x":1},"extra_content":{"google":{"thought_signature":"g"}}},{"type":"function_call","name":"g","args":"str"},{"type":"other","content":"x"}],"usage":{"input_tokens":1,"output_tokens":2,"cache_read_tokens":3}}}`,
		`{"id":"r","steps":[{"type":"function_call","tool_use_id":"tu"}],"finish_reason":"length","usage":{"total_input_tokens":1,"cached_tokens":5}}`,
		`{}`, `not json`,
	} {
		out = append(out, nonStream(fmt.Sprintf("interactions-claude/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// geminiClaudeRequests exercise ConvertGeminiRequestToClaude: system text, tool-use ID
// pairing (given, generated, out of order), media parts, tool declarations (schema
// normalization, type lowercasing, re-marshaling), tool configs and thinking levels.
func geminiClaudeRequests(model string) []fixture {
	inputs := map[string]string{
		"system":       `{"system_instruction":{"parts":[{"text":""},{"text":"s1 <x>"},{"text":"t","thought":true},{"inline":1},{"text":""},{"text":"s2"}]},"systemInstruction":{"parts":[{"text":"camel ignored"}]},"contents":[{"role":"user","parts":[{"text":"q"}]}]}`,
		"system-empty": `{"system_instruction":{"parts":[{"text":""},{"text":""}]},"contents":[]}`,
		"tool-ids":     `{"contents":[{"role":"model","parts":[{"functionCall":{"name":"a.b","args":{"x":1}}},{"functionCall":{"name":"c","id":" given ","args":"str"}},{"functionCall":{"call_id":"cid","name":"d"}},{"text":"between"}]},{"role":"function","parts":[{"functionResponse":{"name":"c","id":"given","response":{"result":"rc"}}},{"functionResponse":{"name":"a.b","response":{"result":{"k":"<v>"}}}},{"functionResponse":{"name":"x","response":{"other":1}}},{"functionResponse":{"name":"y"}},{"functionResponse":{"name":"z","call_id":"cid"}}]},{"role":"user","parts":[{"functionCall":{"name":"user call ignored"}},{"functionResponse":{"name":"late"}}]},{"role":"tool","parts":[{"text":"tool role"}]}]}`,
		"media":        `{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"IMAGE/PNG","data":"iVBO"}},{"inline_data":{"mime_type":"application/pdf","data":"JVBE"}},{"inlineData":{"mimeType":"text/csv","data":"YQ"}},{"inlineData":{"mimeType":"audio/wav","data":"UklG"}},{"inlineData":{"mimeType":"image/png"}},{"inlineData":{"data":"x"}},{"fileData":{"mimeType":"image/jpeg","fileUri":"gs://i.jpg"}},{"file_data":{"mime_type":"Application/JSON","file_uri":"gs://d"}},{"fileData":{"fileUri":"gs://nomime"}},{"fileData":{"mimeType":"video/mp4","fileUri":"gs://v <x>"}},{"fileData":{"mimeType":"text/plain"}},{"executableCode":{"code":"x"}}]},{"role":"bogus","parts":[{"text":"dropped role"}]},{"parts":[{"text":"no role"}]}]}`,
		"tools":        `{"tools":[{"functionDeclarations":[{"name":"a.b c","description":"d <x> &","parameters":{"type":"OBJECT","properties":{"q":{"type":"STRING","description":"<q>"},"n":{"type":["INTEGER","NULL"],"enum":[1e3,2.50]},"o":{"type":"Object","properties":{"t":{"type":"string"}}}},"additionalProperties":true,"$schema":"x"}},{"name":"b","parametersJsonSchema":{"type":"object","additionalProperties":false,"$schema":"http://json-schema.org/draft-07/schema#","properties":{"type":{"type":"STRING"}}}},{"name":"c"},{"description":"no name","parameters":"str"},{"name":"d","parameters":{"type":5,"z":1,"a":2,"z":3}}]},{"function_declarations":[{"name":"snake ignored"}]},{"googleSearch":{}}],"contents":[]}`,
		"tools/empty":  `{"tools":[{"googleSearch":{}}],"contents":[]}`,
		"config/any1":  `{"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a.b"]}},"contents":[]}`,
		"config/any2":  `{"tool_config":{"function_calling_config":{"mode":"ANY","allowed_function_names":["a","b"]}},"toolConfig":{"functionCallingConfig":{"mode":"NONE"}},"contents":[]}`,
		"config/any3":  `{"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":"a"}},"contents":[]}`,
		"config/modes": `{"toolConfig":{"functionCallingConfig":{"mode":"NONE"}},"contents":[]}`,
		"config/auto":  `{"tool_config":{"function_calling_config":{"mode":"AUTO"}},"contents":[]}`,
		"config/other": `{"tool_config":{"function_calling_config":{"mode":"any"}},"contents":[]}`,
		"generation":   `{"generationConfig":{"maxOutputTokens":"77","topP":"0.25","temperature":0.3,"stopSequences":["<a>","b",5],"candidateCount":2},"service_tier":"flex","contents":[]}`,
		"generation/2": `{"generationConfig":{"stopSequences":[]},"service_tier":5,"contents":[]}`,
		"user-id":      `{"contents":[{"role":"user","parts":[{"text":"seed text"}]}],"metadata":{"user_id":"ignored"}}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for i, name := range names {
		out = append(out, req("gemini-claude/"+name, model, inputs[name], i%2 == 0))
	}
	for i, cfg := range []string{`{"thinkingLevel":"none"}`, `{"thinkingLevel":" XHIGH "}`, `{"thinkingLevel":"auto"}`, `{"thinkingLevel":"turbo"}`, `{"thinkingLevel":""}`, `{"thinking_level":"max"}`, `{"thinkingBudget":0}`, `{"thinkingBudget":-1}`, `{"thinkingBudget":-5}`, `{"thinkingBudget":1000}`, `{"thinking_budget":"30000"}`, `{"thinkingBudget":1e3,"thinkingLevel":"low"}`, `"str"`} {
		for _, m := range []string{"claude-opus-4-6", "claude-sonnet-4-5-20250929", "unknown-model"} {
			out = append(out, req(fmt.Sprintf("gemini-claude/thinking/%d/%s", i, m), m, `{"generationConfig":{"thinkingConfig":`+cfg+`},"contents":[]}`, false))
		}
	}
	return out
}

// claudeToGemini exercise ConvertClaudeResponseToGemini(NonStream): signature replay,
// tool calls assembled at block stop, finish reasons and usage, errors, line framing
// and part consolidation.
func claudeToGemini() []fixture {
	ev := func(body string) string { return "data: " + body }
	cases := map[string][]string{
		"tools": {ev(`{"type":"message_start","message":{"id":"msg_1","model":"claude-x"}}`), ev(`{"type":"content_block_start","index":0,"content_block":{"type":"thinking","signature":"` + geminiSig + `"}}`), ev(`{"type":"content_block_start","index":1,"content_block":{"type":"thinking","signature":"claude-sig"}}`), ev(`{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"plan"}}`), ev(`{"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"` + geminiSig + `"}}`), ev(`{"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":""}}`),
			ev(`{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"tu_1","name":"lookup"}}`), ev(`{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":" {\"q\":"}}`), ev(`{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"<x>\"} "}}`), ev(`{"type":"content_block_stop","index":2}`), ev(`{"type":"content_block_stop","index":1}`),
			ev(`{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","name":""}}`), ev(`{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta"}}`), ev(`{"type":"content_block_stop","index":3}`), ev(`{"type":"content_block_delta","index":4,"delta":{"type":"input_json_delta","partial_json":"[1]"}}`), ev(`{"type":"content_block_stop","index":4}`),
			ev(`{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":2,"thinking_tokens":1}}`), ev(`{"type":"message_stop"}`)},
		"text":    {ev(`{"type":"content_block_start","index":0,"content_block":{"type":"text"}}`), ev(`{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi <b> é"}}`), ev(`{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":""}}`), ev(`{"type":"content_block_delta","index":0}`), ev(`{"type":"message_start","message":{"id":"late","model":""}}`), ev(`{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta"}}`), ev(`{"type":"message_delta","usage":{"cache_creation_input_tokens":5}}`), ev(`{"type":"message_delta"}`)},
		"framing": {" data: {\"type\":\"error\"}", "event: x", "data:{\"type\":\"error\",\"error\":{\"message\":\"overloaded <x>\"}}", ev(`{"type":"error"}`), "data: not json", "data: [DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		out = append(out, streamCase("claude-gemini/"+name, "gemini-2.5-pro", cases[name]...))
	}
	for i, body := range []string{
		strings.Join(cases["tools"], "\n"),
		"data: {\"type\":\"message_start\",\"message\":{\"id\":\"m2\"}}\r\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"a\"}}\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"b\"}}\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"t1\"}}\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s1\"}}\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"thinking\",\"signature\":\"s2\"}}\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"c\"}}\n data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"text_delta\",\"text\":\"skipped\"}}\ndata:\ndata: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":1}}",
		`{"id":"msg","content":[{"type":"text","text":"not sse"}]}`, ``,
	} {
		out = append(out, nonStream(fmt.Sprintf("claude-gemini/non-stream/%d", i), "gemini-2.5-pro", body))
	}
	return out
}

// claudeNativeSig is a strict Claude thinking signature (protobuf channel block naming a
// Claude model), as the antigravity/gemini tests build it.
func claudeNativeSig() string {
	channel := append([]byte{0x08, 12, 0x10, 2, 0x32, byte(len("claude-sonnet-4-6"))}, "claude-sonnet-4-6"...)
	container := append([]byte{0x0a, byte(len(channel))}, channel...)
	payload := append([]byte{0x12, byte(len(container))}, container...)
	payload = append(payload, 0x18, 0x01)
	return base64.StdEncoding.EncodeToString(payload)
}

// geminiAntigravityRequests exercise ConvertGeminiRequestToAntigravity beyond the shared
// Gemini matrix: function-response grouping with sibling images, malformed responses,
// declaration renames and dedupe, empty tools, name rewrites (batched and per path),
// allowed names, schema and system renames, and Claude signature sanitizing.
func geminiAntigravityRequests(model string) []fixture {
	nativeSig := claudeNativeSig()
	normalized, ok := signature.CompatibleAntigravityClaudeThinkingSignature(nativeSig)
	if !ok {
		panic("claude signature not compatible")
	}
	tools := `[{"functionDeclarations":[{"name":"a b","description":"<d>","parameters":{"type":"object"}},{"name":"a_b"},{"name":"a b","description":"dup"},{"name":5},{"description":"no name"},{"name":"keep","parametersJsonSchema":{}}]},{"function_declarations":[{"name":"x/y","parameters":{"type":"object","properties":{}}},{"name":"keep"}]},{"functionDeclarations":[]},{},{"googleSearch":{}},"str"]`
	inputs := map[string]string{
		"group/images":       `{"contents":[{"role":"user","parts":[{"text":"q"}]},{"role":"model","parts":[{"functionCall":{"name":"f1","args":{}}},{"functionCall":{"name":"f2"}}]},{"role":"user","parts":[{"inlineData":{"mimeType":"image/jpeg","data":"AAA"}},{"functionResponse":{"name":"","response":{"r":"<1>"}}},{"inline_data":{"data":"BBB"}},{"inlineData":{"data":""}},{"inline_data":{"mime_type":"image/webp","data":"CCC"}},{"text":"dropped text"}]},{"role":"user","parts":[{"functionResponse":{"name":"f2","response":{}}}]},{"role":"user","parts":[{"text":"after"}]}]}`,
		"group/unmatched":    `{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"early","response":{}}}]},{"role":"model","parts":[{"functionCall":{"name":"a"}}]},{"role":"model","parts":[{"functionCall":{"name":"b"}},{"functionCall":{"name":"c"}}]},{"role":"user","parts":[{"functionResponse":{"name":"","response":{"x":1}}}]},{"role":"model","parts":[{"functionCall":{"name":"d"}}]},"skip",[1],{"role":"user","parts":[{"functionResponse":{"name":"  ","response":{}}},{"functionResponse":{"response":{}}},{"functionResponse":{"name":"z"}}]}]}`,
		"group/leftover":     `{"contents":[{"role":"model","parts":[{"functionCall":{"name":"a"}},{"functionCall":{"name":"b"}},{"functionCall":{"name":"c"}}]},{"role":"model","parts":[{"functionCall":{"name":"d"}}]},{"role":"user","parts":[{"functionResponse":{"name":"r1"}}]},{"role":"model","parts":[{"text":"no calls"}]},{"role":"user","parts":[{"functionResponse":{"name":""}}]}]}`,
		"group/odd-response": `{"contents":[{"role":"model","parts":[{"functionCall":{"name":"s"}},{"functionCall":{"name":"t"}}]},{"role":"user","parts":[{"functionResponse":"str"},{"functionResponse":{"name":"","response":"plain","id":"id1"}}]}],"generationConfig":{}}`,
		"group/object":       `{"contents":{"m":{"role":"model","parts":[{"functionCall":{"name":"o"}}]},"u":{"role":"user","parts":[{"functionResponse":{"name":""}}]},"n":null}}`,
		"group/no-contents":  `{"systemInstruction":{"parts":[{"text":"s"}]},"tools":[]}`,
		"rename/system":      `{"model":"drop-me","system_instruction":{"parts":[{"text":"snake <s>"}]},"systemInstruction":{"parts":[{"text":"camel"}]},"generation_config":{"response_json_schema":{"type":"object"},"responseJsonSchema":{"type":"string"}},"generationConfig":{"responseSchema":{"keep":true},"responseJsonSchema":{"drop":true}},"contents":[{"role":"user","parts":[{"text":"x"}]}],"safetySettings":[{"category":"c","threshold":"t"}]}`,
		"rename/system-only": `{"system_instruction":{"parts":[{"text":"snake"}]},"generation_config":{"responseJsonSchema":{"type":"object"}},"contents":[{"parts":[{"text":"x"}]},{"role":"function","parts":[{"function_response":{"name":"f"}}]},{"role":"tool","parts":[{"text":"t"}]},{"role":"MODEL","parts":[{"text":"m"}]}]}`,
		"tools/names":        `{"tools":` + tools + `,"toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["a b","keep",7,"x/y","<a>"]}},"tool_config":{"function_calling_config":{"allowed_function_names":["keep"]}},"contents":[{"role":"model","parts":[{"functionCall":{"name":"a b","args":{}}},{"functionCall":{"name":"x/y"}},{"function_call":{"name":"9lives"}},{"functionCall":{"name":7}},{"text":"t"}]},{"role":"user","parts":[{"functionResponse":{"name":"a b","response":{}}},{"function_response":{"name":"keep"}},{"functionResponse":{"name":"x/y","response":{}}}]}]}`,
		"tools/unchanged":    `{"tools":[{"functionDeclarations":[{"name":"plain","parametersJsonSchema":{"type":"object"}}]}],"toolConfig":{"functionCallingConfig":{"allowedFunctionNames":["plain"]}},"contents":[{"role":"model","parts":[{"functionCall":{"name":"plain"}}]}]}`,
		"tools/empty":        `{"tools":[{"functionDeclarations":[]},{},{"function_declarations":[]}],"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		"tools/empty-array":  `{"tools":[],"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		"tools/mixed-empty":  `{"tools":[{"functionDeclarations":[],"googleSearch":{}},{"codeExecution":{}}],"contents":[{"role":"user","parts":[{"text":"x"}]}]}`,
		"tools/object":       `{"tools":{"functionDeclarations":[{"name":"a b"}]},"contents":[{"role":"model","parts":[{"functionCall":{"name":"a b"}}]}]}`,
		"rewrite/per-path":   `{"tools":[{"functionDeclarations":[{"name":"a b"}]}],"toolConfig":{"functionCallingConfig":{"allowedFunctionNames":"a b"}},"tool_config":{"function_calling_config":{"allowed_function_names":7}},"contents":[{"role":"model","parts":{"functionCall":{"name":"a b"}}},{"role":"model","parts":[{"functionCall":{"name":"a b"}},{"function_response":{"name":"c/d"}}]},{"role":"user","parts":"x"}]}`,
		"rewrite/nested":     `{"tools":[{"tools":[{"function":{"name":"n m"}},{"name":"o p"}]},{"functionDeclarations":[{"name":"n m"}]}],"contents":[{"role":"model","parts":[{"functionCall":{"name":"n m"}},{"functionCall":{"name":"o p"}}]}]}`,
		"malformed/response": "{\"contents\":[{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"f\"}}]},{\"role\":\"user\",\"parts\":[{\"functionResponse\":{\"name\":\"\",\"id\":\"i\",\"response\":{\"a\":1,}}}]}]}",
		"malformed/bytes":    "{\"tools\":[{\"functionDeclarations\":[{\"name\":\"b\xffad\"}]}],\"contents\":[{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"name\":\"b\xffad\"}}]},{\"role\":\"user\",\"parts\":[{\"functionResponse\":{\"name\":\"\",\"response\":{\"t\":\"\xfe\"}}}]}]}",
	}
	claudeInputs := map[string]string{
		"claude/parts":     `{"contents":[{"role":"model","parts":[{"text":"reason <x> & \u2028","thought":true,"thoughtSignature":"` + nativeSig + `","z":1e3,"a":[1.50]},{"text":"bad sig","thought":true,"thoughtSignature":"EgEC"},{"text":"  ","thought":true,"thoughtSignature":"` + nativeSig + `"},{"text":"no sig","thought":true},{"text":"answer","thoughtSignature":"` + nativeSig + `"},{"functionCall":{"name":"f","args":{"k":"<v>"},"thought_signature":"x"}},{"text":"deep","meta":{"inner":[{"thought_signature":"z"}]}},{"text":"extra","extra_content":{"google":{"thought_signature":"g","other":1}}},{"text":"plain"},"str",null]},{"role":"user","parts":[{"text":"u","thought_signature":"s"},{"text":"clean"}]},{"role":"model","parts":[{"text":"","thought":true}]},{"role":"model","parts":[]},{"role":"model","parts":"notarray"},{"role":"function","parts":[{"functionResponse":{"name":"f","response":{"n":1.0}},"thoughtSignature":"s"}]}]}`,
		"claude/stable":    `{"contents":[{"role":"model","parts":[{"thought":true,"text":"kept raw","thoughtSignature":"` + normalized + `","b":2}]},{"role":"user","parts":[{"text":"q"}]}]}`,
		"claude/stable-2":  `{"contents":[{"role":"model","parts":[{"thought":true,"text":"re-marshaled","thoughtSignature":"` + normalized + `","b":2},{"text":"t","thoughtSignature":"x"}]}]}`,
		"claude/thought-1": `{"contents":[{"role":"model","parts":[{"thought":1,"text":"num thought","thoughtSignature":"` + nativeSig + `"},{"thought":"true","text":"str thought"}]}]}`,
		"claude/claude#":   `{"contents":[{"role":"model","parts":[{"thought":true,"text":"prefixed","thoughtSignature":"claude#` + nativeSig + `"},{"thought":true,"text":"snake","thought_signature":"` + nativeSig + `"},{"thought":true,"text":"in call","functionCall":{"name":"f","thoughtSignature":"` + nativeSig + `"}}]}]}`,
	}
	var out []fixture
	keys := func(m map[string]string) []string {
		var names []string
		for name := range m {
			names = append(names, name)
		}
		sort.Strings(names)
		return names
	}
	for i, name := range keys(inputs) {
		out = append(out, req("gemini-antigravity/"+name, model, inputs[name], i%2 == 0))
	}
	for _, name := range keys(claudeInputs) {
		for _, m := range []string{"claude-sonnet-4-6", "Claude-Opus-4-6-Thinking", "gemini-3-pro-preview"} {
			out = append(out, req("gemini-antigravity/"+name+"/"+m, m, claudeInputs[name], false))
		}
	}
	return out
}

// antigravityUpstreamResponses are Antigravity envelopes (`{"response":...}`) as the
// executor passes them: cpaUsageMetadata on non-terminal chunks, sanitized function
// names, missing finish reasons closed at [DONE], bare and malformed payloads.
func antigravityUpstreamResponses() []fixture {
	env := func(body string) string { return `{"response":` + body + `}` }
	cand := func(parts, extra string) string {
		return `{"content":{"role":"model","parts":` + parts + `}` + extra + `}`
	}
	cases := map[string][]string{
		"text": {env(`{"candidates":[` + cand(`[{"text":"Hi <b> é"}]`, ``) + `],"cpaUsageMetadata":{"promptTokenCount":3},"modelVersion":"gemini-3-flash","responseId":"r1"}`),
			env(`{"candidates":[` + cand(`[{"text":" more"}]`, ``) + `],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":2},"cpaUsageMetadata":{"stale":true},"modelVersion":"other"}`), "[DONE]", "[DONE]"},
		"finished": {"data: " + env(`{"candidates":[`+cand(`[{"text":"done"}]`, `,"finishReason":"STOP"`)+`],"usageMetadata":{"totalTokenCount":9}}`), "data: [DONE]"},
		"tools": {env(`{"candidates":[` + cand(`[{"functionCall":{"name":"a_b","args":{"q":"<x>"}}},{"functionCall":{"name":"x_y"}},{"function_call":{"name":5}},{"functionResponse":{"name":"a_b"}},{"functionCall":{"name":"unknown"}}]`, ``) + `,` + cand(`[{"functionCall":{"name":"x_y"}}]`, `,"index":1`) + `],"responseId":7}`),
			env(`{"candidates":{"content":{"parts":[{"functionCall":{"name":"a_b"}}]},"finishReason":"STOP"}}`), "[DONE]"},
		"empty":      {env(`{}`), `{}`, env(`{"candidates":[]}`), env(`{"usageMetadata":{}}`), `{"response":null}`, "[DONE]"},
		"usage-only": {env(`{"cpaUsageMetadata":{"totalTokenCount":5},"modelVersion":5}`), "[DONE]"},
		"bare":       {`{"candidates":[` + cand(`[{"text":"bare"}]`, ``) + `],"modelVersion":"bare-model","cpaUsageMetadata":{"x":1}}`, env(`{"candidates":[` + cand(`[{"text":"wrapped"}]`, ``) + `]}`), "data:  [DONE] "},
		"framing":    {"", ": keepalive", "data: not json", "data: " + env("{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"bad\xff\"}]}}]}"), `data: {"error":{"code":429,"message":"quota <x>"}}`, "[DONE]"},
		"rich": {env(`{"candidates":[` + cand(`[{"text":"plan","thought":true,"thoughtSignature":"sig"},{"thoughtSignature":"only-sig"},{"text":"Hello <b>"},{"inlineData":{"mimeType":"image/jpeg","data":"/9j/"}},{"inline_data":{"data":"iVBO"}},{"inlineData":{"data":""}},{"functionCall":{"name":"a_b","args":{"q":"<x>"}},"thoughtSignature":"s2"},{"functionCall":{"name":"plain"}},{"executableCode":{"code":"x"}}]`, ``) + `],"createTime":"2025-01-02T03:04:05.678Z","modelVersion":"gemini-3-flash","responseId":"r9","cpaUsageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"thoughtsTokenCount":1,"cachedContentTokenCount":3,"totalTokenCount":7}}`),
			env(`{"candidates":[` + cand(`[{"text":""}]`, `,"finishReason":"max_tokens"`) + `],"createTime":"bad","usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":"5","thoughtsTokenCount":2,"totalTokenCount":11}}`), "[DONE]"},
		"late-usage":    {env(`{"candidates":[` + cand(`[{"text":"a"}]`, `,"finishReason":"STOP"`) + `],"responseId":"r2"}`), env(`{"candidates":[` + cand(`[{"text":"b"}]`, ``) + `]}`), env(`{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}`), "[DONE]"},
		"pending-usage": {env(`{"candidates":[` + cand(`[{"functionCall":{"name":"x_y","args":{}}}]`, ``) + `],"cpaUsageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":1},"createTime":"2025-03-04T05:06:07+02:00"}`), env(`{"candidates":[` + cand(`[{"text":"t"}]`, `,"finishReason":"SAFETY"`) + `],"cpaUsageMetadata":{"promptTokenCount":3}}`), "[DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		f := streamCase("antigravity-up/"+name, "gemini-3-flash", cases[name]...)
		f.Original = `{"model":"client-model","tools":[{"functionDeclarations":[{"name":"a b"},{"name":"x/y"},{"name":"plain"}]}]}`
		out = append(out, f)
	}
	for i, body := range []string{
		env(`{"candidates":[` + cand(`[{"text":"Hello <b>"},{"functionCall":{"name":"a_b","args":{}}}]`, ``) + `,` + cand(`[{"text":"c1"}]`, `,"finishReason":"MAX_TOKENS","index":1`) + `,` + cand(`[]`, `,"finishReason":""`) + `],"cpaUsageMetadata":{"promptTokenCount":3},"usageMetadata":{"old":1},"modelVersion":"m"}`),
		env(`{"candidates":{"content":{"parts":[{"functionCall":{"name":"x_y"}}]}}}`),
		`{"candidates":[` + cand(`[{"functionCall":{"name":"a_b"}}]`, ``) + `],"cpaUsageMetadata":{"kept":true}}`,
		env(`null`), env(`[]`), `{}`, ``, `not json`, "{\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"bad\xfe\"}]}}]}}",
	} {
		n := nonStream(fmt.Sprintf("antigravity-up/non-stream/%d", i), "gemini-3-flash", body)
		n.Original = `{"model":"client-model","tools":[{"functionDeclarations":[{"name":"a b"},{"name":"x/y"}]}]}`
		out = append(out, n)
	}
	return out
}

// openAIAntigravityRequests exercise ConvertOpenAIRequestToAntigravity beyond the shared
// OpenAI Chat -> Gemini matrix: thinking-config normalization of a passed-through
// generationConfig, call-ID dedupe, argument and tool-result coercions, declaration
// renames and dedupe, tool choices and Claude signature sanitizing.
func openAIAntigravityRequests(model string) []fixture {
	var out []fixture
	for _, f := range openAIGeminiRequests(model) {
		f.Name = strings.Replace(f.Name, "openai-gemini/", "openai-antigravity/gemini-matrix/", 1)
		out = append(out, f)
	}
	user := `"messages":[{"role":"user","content":"x"}]`
	inputs := map[string]string{
		"thinking/snake":      `{` + user + `,"generationConfig":{"thinking_config":{"include_thoughts":true,"thinking_level":"high","thinkingBudget":512},"thinkingConfig":{"includeThoughts":"yes","thinking_budget":100},"includeThoughts":false,"include_thoughts":true,"temperature":1}}`,
		"thinking/camel":      `{` + user + `,"generation_config":{"thinkingConfig":{"includeThoughts":true,"thinkingLevel":"low","thinking_level":"high","include_thoughts":false}},"reasoning_effort":"medium"}`,
		"thinking/effort":     `{` + user + `,"generationConfig":{"thinkingConfig":{"thinkingBudget":2048}},"reasoning_effort":"auto"}`,
		"thinking/none":       `{` + user + `,"generationConfig":{"includeThoughts":1,"thinking_config":"str"},"reasoning_effort":"none"}`,
		"tool-ids":            `{"messages":[{"role":"user","content":"q"},{"role":"assistant","tool_calls":[{"id":"","type":"function","function":{"name":"A","arguments":"{}"}},{"id":"dup","type":"function","function":{"name":"B","arguments":{"o":1}}},{"id":"dup","type":"function","function":{"name":"C","arguments":5}},{"id":"bad id!","type":"function","function":{"name":"D","arguments":"not json"}},{"id":"dup_1","type":"function","function":{"name":"E"}},{"id":"skip","type":"function","function":{"name":""}},{"id":"skip","type":"function","function":{"name":"F","arguments":"[1]"}}]},{"role":"tool","tool_call_id":"dup","content":{"obj":"<v>"}},{"role":"tool","tool_call_id":"bad id!","content":[{"type":"text","text":"t"}]},{"role":"tool","tool_call_id":"dup_1","content":7},{"role":"tool","tool_call_id":"skip","content":""},{"role":"user","content":"after"},{"role":"tool","tool_call_id":"","content":"no id"}]}`,
		"tool-names":          `{"messages":[{"role":"assistant","content":"c","tool_calls":[{"id":"1","type":"function","function":{"name":"a b","arguments":"{}"}},{"id":"2","type":"function","function":{"name":"a_b","arguments":"{}"}},{"id":"3","type":"function","function":{"name":"x/y","arguments":"{}"}}]}],"tools":[{"type":"function","function":{"name":"a b","parameters":{"type":"object"},"strict":true}},{"type":"function","function":{"name":"a_b","strict":false}},{"type":"function","function":{"name":"x/y","parameters":"str"}},{"type":"function","function":{"name":"x/y","description":"dup"}},{"type":"function","function":{"description":"nameless"}},{"type":"function","function":{"description":"nameless 2"}}],"tool_choice":{"type":"function","function":{"name":"x/y"}}}`,
		"tool-choice/none":    `{` + user + `,"tools":[{"type":"function","function":{"name":"f"}},{"google_search":{}}],"tool_choice":" NONE "}`,
		"tool-choice/obj":     `{` + user + `,"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"Function","function":{"name":"  "}}}`,
		"tool-choice/objnone": `{` + user + `,"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":{"type":"none"}}`,
		"tool-choice/any":     `{` + user + `,"tools":[{"type":"function","function":{"name":"f"}}],"tool_choice":"any"}`,
		"tool-choice/other":   `{` + user + `,"tool_choice":{"type":"allowed_tools"}}`,
		"tool-choice/name":    `{` + user + `,"tool_choice":{"type":"function","function":{"name":" g h "}}}`,
		"format":              `{` + user + `,"generationConfig":{"responseSchema":{"a":1},"responseJsonSchema":{"b":2},"response_schema":{},"response_json_schema":{}},"response_format":{"type":"json_object"}}`,
		"format/schema":       `{` + user + `,"generation_config":{"responseJsonSchema":{"b":2}},"response_format":{"type":"json_schema","json_schema":{"schema":{"type":"object"}}}}`,
		"format/text":         `{` + user + `,"generationConfig":{"responseSchema":{"a":1}},"response_format":{"type":"text"}}`,
		"trailing-model":      `{"messages":[{"role":"user","content":"x"},{"role":"assistant","content":"kept as last turn"}]}`,
		"no-messages":         `{"messages":"str","tools":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	for i, name := range names {
		out = append(out, req("openai-antigravity/"+name, model, inputs[name], i%2 == 0))
	}
	claude := `{"messages":[{"role":"user","content":"q"},{"role":"assistant","content":"answer","reasoning_content":"thinking <x>","tool_calls":[{"id":"t1","type":"function","function":{"name":"Read","arguments":"{\"n\":1e3}"}}]},{"role":"tool","tool_call_id":"t1","content":"ok"}],"tools":[{"type":"function","function":{"name":"Read","parameters":{"type":"object"}}}]}`
	for _, m := range []string{"claude-sonnet-4-6", "gemini-3-pro-preview", "gemini-2.5-flash", "Claude-Opus-4-6-Thinking"} {
		out = append(out, req("openai-antigravity/claude-turns/"+m, m, claude, false))
		out = append(out, req("openai-antigravity/thinking-models/"+m, m, `{`+user+`,"reasoning_effort":"high","generationConfig":{"thinking_config":{"include_thoughts":false}}}`, true))
	}
	return out
}

// responsesAntigravityRequests exercise ConvertOpenAIResponsesRequest(Envelope)ToAntigravity:
// the shared Responses -> Gemini matrix through the envelope, Claude reasoning
// signatures (re-marshaled through Go's float64 decoding), Google Search stripping,
// default thinking summaries and the native web-search request.
func responsesAntigravityRequests(model string) []fixture {
	var out []fixture
	for _, f := range responsesGeminiRequests(model) {
		f.Name = strings.Replace(f.Name, "responses-gemini/", "responses-antigravity/gemini-matrix/", 1)
		out = append(out, f)
	}
	nativeSig := claudeNativeSig()
	normalized, _ := signature.CompatibleAntigravityClaudeThinkingSignature(nativeSig)
	reasoning := func(enc, text string) string {
		item := `{"type":"reasoning","summary":[`
		if text != "" {
			item += `{"type":"summary_text","text":"` + text + `"}`
		}
		item += `]`
		if enc != "-" {
			item += `,"encrypted_content":"` + enc + `"`
		}
		return item + `}`
	}
	assistant := `{"role":"assistant","content":[{"type":"output_text","text":"visible <a>"}]}`
	user := `{"role":"user","content":[{"type":"input_text","text":"continue"}]}`
	inputs := map[string]string{
		"claude/mixed":      `{"input":[` + reasoning(nativeSig, "first <r>") + `,` + assistant + `,` + reasoning(gptSignature(), "gpt reasoning") + `,{"type":"function_call","call_id":"c1","name":"f","arguments":"{\"n\":1e3,\"big\":12345678901234567890}"},{"type":"function_call_output","call_id":"c1","output":"ok"},` + reasoning(normalized, "") + `,` + assistant + `,` + reasoning("-", "no sig") + `,` + reasoning(normalized, "kept normalized") + `,` + user + `],"tools":[{"type":"function","name":"f"}]}`,
		"claude/unchanged":  `{"input":[` + reasoning(normalized, "already") + `,` + assistant + `,` + user + `]}`,
		"claude/no-thought": `{"input":[{"type":"reasoning","encrypted_content":"` + nativeSig + `"},{"role":"user","content":"x"}]}`,
		"claude/overflow":   `{"input":[` + reasoning(gptSignature(), "dropped") + `,{"role":"user","content":[{"type":"input_text","text":"n"}]},{"type":"function_call","call_id":"c","name":"f","arguments":"{\"n\":1e400}"}],"tools":[{"type":"function","name":"f"}]}`,
		"claude/role-items": `{"input":[{"role":"assistant","type":"","content":"r"},` + reasoning(nativeSig, "t") + `,{"type":"message","role":"user","content":"u"}]}`,
		"search/mixed":      `{"input":"find","tools":[{"type":"web_search","filters":{"allowed_domains":["a.com"]}},{"type":"function","name":"f"}]}`,
		"search/only":       `{"input":[{"role":"user","content":"find <x>"}],"instructions":"be brief","tools":[{"type":"web_search","filters":{"allowed_domains":[" a.com ","","b<c>.com"]}},{"type":"web_search_preview"}],"tool_choice":"auto","reasoning":{"effort":"low"}}`,
		"search/no-domains": `{"input":"find","tools":[{"type":"web_search_2025_08_26"}],"tool_choice":{"type":"web_search"}}`,
		"search/choice":     `{"input":"find","tools":[{"type":"web_search"}],"tool_choice":"none"}`,
		"summary/effort":    `{"input":"x","reasoning":{"effort":" High "}}`,
		"summary/explicit":  `{"input":"x","reasoning":{"effort":"medium","summary":"detailed"}}`,
		"summary/off":       `{"input":"x","reasoning":{"effort":"high","summary":"none"}}`,
		"summary/none":      `{"input":"x","reasoning":{"effort":"none"}}`,
		"summary/number":    `{"input":"x","reasoning":{"effort":5}}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	for i, name := range names {
		for _, m := range []string{"claude-sonnet-4-6", "gemini-3-flash", "gemini-3-pro-preview", "Claude-Opus-4-6-Thinking"} {
			out = append(out, req("responses-antigravity/"+name+"@"+m, m, inputs[name], i%2 == 0))
			if strings.HasPrefix(name, "search/") || name == "claude/mixed" {
				f := req("responses-antigravity/envelope/"+name+"@"+m, m, inputs[name], i%2 == 1)
				f.Path = "request_envelope"
				out = append(out, f)
			}
		}
	}
	return out
}

// antigravityToResponses run the Gemini Responses response matrix through Antigravity
// envelopes, plus an envelope-wrapped client request in the non-stream path.
func antigravityToResponses() []fixture {
	wrap := func(s string) string {
		t := strings.TrimSpace(s)
		if strings.HasPrefix(t, "data:") {
			t = strings.TrimSpace(t[5:])
		}
		if strings.HasPrefix(t, "{") {
			return `{"response":` + t + `}`
		}
		return t
	}
	var out []fixture
	for _, f := range geminiToResponses() {
		if f.Path != "stream" && f.Path != "non_stream" {
			continue
		}
		f.Name = strings.Replace(f.Name, "gemini-responses/", "antigravity-responses/", 1)
		if f.Path == "stream" {
			for i := range f.Lines {
				f.Lines[i] = wrap(f.Lines[i])
			}
		} else {
			f.Input = wrap(f.Input)
			f.Translated = `{"project":"","request":{"contents":[]},"model":"m"}`
		}
		out = append(out, f)
	}
	n := nonStream("antigravity-responses/non-stream/request-wrapped", "gemini-3-flash", `{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a_b","args":{}}}]},"finishReason":"STOP"}],"responseId":"rw"}}`)
	n.Original = `{"request":{"model":"inner","tools":[{"type":"function","name":"a.b"}]},"tools":[{"type":"function","name":"ignored"}]}`
	n.Translated = `{"project":"","request":{"tools":[]},"model":"m"}`
	out = append(out, n)
	return out
}

// interactionsAntigravityRequests exercise ConvertInteractionsRequestToAntigravity: the
// shared Interactions -> Gemini inputs through the envelope, plus reasoning, stream
// flags, untrimmed allowed names, declaration renames and dedupe, built-in tools and
// name rewrites.
func interactionsAntigravityRequests(model string) []fixture {
	var out []fixture
	for _, f := range interactionsGeminiRequests(model) {
		f.Name = strings.Replace(f.Name, "interactions-gemini/", "interactions-antigravity/gemini-matrix/", 1)
		out = append(out, f)
	}
	inputs := map[string]string{
		"reasoning/effort":     `{"reasoning":{"effort":" HIGH ","summary":"auto"},"generation_config":{"thinking_level":"low"},"input":"x"}`,
		"reasoning/auto":       `{"reasoning":{"effort":"auto","summary":"none"},"input":"x"}`,
		"reasoning/level":      `{"reasoning":{"thinking_level":"Medium","summary":"detailed"},"input":"x"}`,
		"reasoning/blank":      `{"reasoning":{"effort":"  ","summary":5},"generationConfig":{"thinkingConfig":{"includeThoughts":true}},"input":"x"}`,
		"reasoning/null":       `{"reasoning":null,"input":"x"}`,
		"stream/flag":          `{"stream":"true","input":"x"}`,
		"stream/false":         `{"stream":false,"input":"x"}`,
		"choice/untrimmed":     `{"tool_choice":{"type":"function","function":{"name":" a b "}},"tools":[{"name":" a b "},{"name":"a b"}],"input":"x"}`,
		"choice/none":          `{"tool_choice":"none","tools":[{"name":"f"}],"input":"x"}`,
		"choice/tool":          `{"tool_choice":{"type":"tool","name":"x/y"},"tools":[{"type":"function","function":{"name":"x/y","description":"nested <d>","parameters":{"type":"object"}}}],"input":"x"}`,
		"tools/declarations":   `{"tools":[{"functionDeclarations":[{"name":"a b","description":5,"parametersJsonSchema":{"type":"object"},"parameters":{"ignored":true},"response":{"type":"string"},"responseJsonSchema":{"r":1}},{"name":"  "},{"name":"a_b"},{"name":"a b","description":"dup"}]},{"function_declarations":[{"name":"c/d"}],"functionDeclarations":"str"},{"type":"function","name":"9lives"},{"name":"keep","function":"notobj"}],"input":"x"}`,
		"tools/builtins":       `{"tools":[{"type":"url_context","urlContext":{"u":1}},{"type":"url_context","url_context":"s"},{"type":"code_execution","codeExecution":{"c":1}},{"type":"google_search","googleSearch":{"g":1}},{"type":"web_search","google_search":{"w":1}},{"type":"web_search"},{"url_context":{"a":1},"code_execution":{},"google_search":{"x":1},"web_search":{"y":2}},{"type":"other","url_context":{}},"str",5,null],"input":"x"}`,
		"tools/scalar":         `{"tools":"str","input":"x"}`,
		"tools/empty":          `{"tools":[],"input":"x"}`,
		"rewrite/batch":        `{"tools":[{"name":"a b"},{"name":"x/y"}],"tool_choice":{"type":"function","function":{"name":"a b"}},"input":[{"type":"function_call","name":"a b","call_id":"c1","arguments":{}},{"type":"function_result","name":"a b","call_id":"c1","result":"r"},{"type":"function_call","name":"x/y"},{"type":"function_call","name":"plain"}]}`,
		"rewrite/native-parts": `{"tools":[{"name":"a b"}],"input":[{"type":"user_input","role":"model","parts":[{"functionCall":{"name":"a b"}},{"functionResponse":{"name":5}}]}]}`,
		"rewrite/allowed":      `{"tools":[{"name":"a b"}],"tool_choice":{"type":"tool","name":"a b"},"input":"x"}`,
		"safety":               `{"safetySettings":[{"category":"c"}],"input":"x"}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	for i, name := range names {
		out = append(out, req("interactions-antigravity/"+name, model, inputs[name], i%2 == 0))
	}
	return out
}

// antigravityToInteractions run the Gemini -> Interactions response matrix through
// Antigravity envelopes, plus array payloads, usage fallbacks and name restores.
func antigravityToInteractions() []fixture {
	wrap := func(s string) string {
		t := strings.TrimSpace(s)
		if strings.HasPrefix(t, "{") {
			return `{"response":` + t + `}`
		}
		return s
	}
	var out []fixture
	for _, f := range geminiToInteractions() {
		f.Name = strings.Replace(f.Name, "gemini-interactions/", "antigravity-interactions/", 1)
		f.Model = "gemini-3-flash"
		if f.Path == "stream" {
			for i := range f.Lines {
				f.Lines[i] = wrap(f.Lines[i])
			}
		} else {
			f.Input = wrap(f.Input)
		}
		out = append(out, f)
	}
	original := `{"tools":[{"name":"a b"},{"name":"x/y"}]}`
	cases := map[string][]string{
		"array":      {`[{"response":{"candidates":[{"content":{"parts":[{"text":"a"}]}}]}},{"candidates":[{"content":{"parts":[{"text":"b"}]}}]},5,{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"a_b","args":{}}}]},"finishReason":"STOP"}],"cpaUsageMetadata":{"promptTokenCount":1}}}]`, `[]`, "[DONE]"},
		"cpa-usage":  {`{"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"x_y"}},{"functionResponse":{"name":"a_b","response":{"r":1}}}]},"finishReason":"STOP"}],"cpaUsageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":0,"cached_content_token_count":5}}}`, "[DONE]"},
		"both-usage": {`{"response":{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":3},"cpaUsageMetadata":{"totalTokenCount":9}}}`},
		"data-lines": {`data: {"response":{"candidates":[{"content":{"parts":[{"text":"p"}]}}]}}`, `data: [DONE]`, "  data:  {\"response\":{}}  "},
		"null":       {`{"response":null}`, `null`, `{}`, "[DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	for _, name := range names {
		f := streamCase("antigravity-interactions/"+name, "gemini-3-flash", cases[name]...)
		f.Original = original
		out = append(out, f)
	}
	for i, body := range []string{
		`{"response":{"responseId":"r9","candidates":[{"content":{"parts":[{"functionCall":{"name":"a_b","args":{"q":1}}},{"functionResponse":{"name":"x_y"}},{"inlineData":{"mimeType":"image/png","data":""}},{"inline_data":{"mime_type":"Video/MP4","data":"VV"}},{"inlineData":{"data":"no-mime"},"thoughtSignature":"s"},{"inlineData":{"data":""},"inline_data":{"mime_type":"image/gif","data":"GG"}}]}}],"cpaUsageMetadata":{"promptTokenCount":4,"thoughts_token_count":1}}}`,
		`{"candidates":[{"content":{"parts":[{"text":"bare"}]}}],"usage_metadata":{"cached_content_token_count":3}}`,
		`{"response":"str"}`,
	} {
		n := nonStream(fmt.Sprintf("antigravity-interactions/non-stream/%d", i), "gemini-3-flash", body)
		n.Original = original
		out = append(out, n)
	}
	return out
}

// gcarrier wraps a signature in the Claude-facing Gemini carrier envelope.
func gcarrier(direction, target, sig string) string {
	return "cpa-gemini-carrier-v1:" + direction + ":" + target + ":" + base64.RawStdEncoding.EncodeToString([]byte(sig))
}

// claudeAntigravityRequests exercise ConvertClaudeRequestToAntigravity: signature
// resolution per target provider (cache mode, cache recovery after a streamed
// signature), Gemini carriers in every direction, tool results with images, tool
// declarations, tool choices, thinking configs and content merging.
func claudeAntigravityRequests(model string) []fixture {
	nativeSig := claudeNativeSig()
	normalized, _ := signature.CompatibleAntigravityClaudeThinkingSignature(nativeSig)
	th := func(text, sig string) string {
		return `{"type":"thinking","thinking":"` + text + `","signature":"` + sig + `"}`
	}
	asst := func(blocks ...string) string {
		return `{"role":"assistant","content":[` + strings.Join(blocks, ",") + `]}`
	}
	text := func(t string) string { return `{"type":"text","text":"` + t + `"}` }
	tool := func(id, name string) string {
		return `{"type":"tool_use","id":"` + id + `","name":"` + name + `","input":{"a":1}}`
	}
	user := `{"role":"user","content":"q"}`
	msgs := func(items ...string) string { return `{"messages":[` + strings.Join(items, ",") + `]}` }
	inputs := map[string]string{
		"sig/native":            msgs(user, asst(th("plan <p>", nativeSig), text("answer"), tool("t1", "Read")), `{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}`),
		"sig/normalized":        msgs(user, asst(th("plan", normalized), text("a"))),
		"sig/gpt":               msgs(user, asst(th("gpt plan", gptSignature()), text("a"), `{"type":"tool_use","id":"t2","name":"f","input":{},"signature":"`+nativeSig+`"}`)),
		"sig/unsigned":          msgs(user, asst(`{"type":"thinking","thinking":"no sig"}`, text("a")), user),
		"sig/cache-hit":         msgs(user, asst(`{"type":"thinking","thinking":"cached plan <c>"}`, text("a"))),
		"sig/empty-text":        msgs(user, asst(th("", nativeSig), th("  ", nativeSig), text("a"))),
		"sig/user-think":        msgs(`{"role":"user","content":[` + th("user thinking", nativeSig) + `,{"type":"text","text":"u"}]}`),
		"carrier/standalone":    msgs(user, asst(th("t1", gcarrier("standalone", "text", geminiSig)), text("x"), th("", gcarrier("standalone", "any", geminiSig)))),
		"carrier/next-text":     msgs(user, asst(th("think", gcarrier("next", "text", geminiSig)), text("visible"), th("", gcarrier("next", "function", geminiSig)), tool("c1", "f"))),
		"carrier/next-miss":     msgs(user, asst(th("", gcarrier("next", "function", geminiSig)), text("t"), th("", gcarrier("next", "any", geminiSig)))),
		"carrier/previous":      msgs(user, asst(text("before"), th("", gcarrier("previous", "text", geminiSig)), tool("c2", "g"), th("", gcarrier("previous", "function", geminiSig)), th("", gcarrier("previous", "text", geminiSig)))),
		"carrier/unmarked":      msgs(user, asst(text("a"), th("", geminiSig), th("", geminiSig), text("b"), th("t", geminiSig), tool("c3", "h"), th("", geminiSig))),
		"carrier/invalid":       msgs(user, asst(th("", "cpa-gemini-carrier-v1:sideways:text:QQ"), th("", "cpa-gemini-carrier-v1:next:text:!!!"), th("x", "cpa-gemini-carrier-v1:next"), th("y", gcarrier("next", "text", "skip_thought_signature_validator")), text("z"))),
		"carrier/pending":       msgs(user, asst(th("", gcarrier("next", "function", geminiSig)), text("text wants text"), th("", gcarrier("next", "text", geminiSig)), tool("c4", "f")), user),
		"carrier/skip-empty":    msgs(user, asst(th("t", gcarrier("next", "text", geminiSig)), text(""), tool("c6", "f"))),
		"carrier/skip-no-args":  msgs(user, asst(th("t", gcarrier("next", "function", geminiSig)), `{"type":"tool_use","id":"c7","name":"f"}`, text("after"))),
		"carrier/invalid-tail":  msgs(user, asst(text("a"), th("", "cpa-gemini-carrier-v1:sideways:text:QQ"))),
		"carrier/pending-think": msgs(user, asst(th("", gcarrier("next", "function", geminiSig)), `{"type":"tool_use","id":"c8","name":"f"}`, th("unsigned thought", ""), text("x"))),
		"carrier/reorder":       msgs(user, asst(text("first"), th("later thought", geminiSig), tool("c5", "f"), th("", gcarrier("previous", "function", geminiSig)), text("after call"))),
		"tool-results":          `{"messages":[{"role":"user","content":"q"},` + asst(tool("call-a", "a b"), tool("get_weather-call-123", "x"), tool("t3", "Read")) + `,{"role":"user","content":[{"type":"tool_result","tool_use_id":"t3","content":[{"type":"text","text":"r1"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBO"}},{"type":"image","source":{"type":"url","url":"u"}}]},{"type":"tool_result","tool_use_id":"call-a","content":{"type":"image","source":{"type":"base64","media_type":"image/jpeg","data":"/9j"}}},{"type":"tool_result","tool_use_id":"get_weather-call-123","content":{"k":"<v>"}},{"type":"tool_result","tool_use_id":"unknown-x-y-z","content":5},{"type":"tool_result","tool_use_id":"ab","content":[{"type":"image","source":{"type":"base64","data":"only"}}]},{"type":"tool_result","tool_use_id":"nocontent"},{"type":"tool_result","tool_use_id":"","content":"x"},{"type":"text","text":"after results"}]}],"tools":[{"name":"a b","input_schema":{"type":"object"}}]}`,
		"tool-args":             msgs(user, asst(`{"type":"tool_use","id":"s1","name":"f","input":"{\"x\":1} trailing"}`, `{"type":"tool_use","id":"s2","name":"f","input":"plain"}`, `{"type":"tool_use","id":"s3","name":"f","input":null}`, `{"type":"tool_use","id":"s4","name":"f","input":[1]}`, `{"type":"tool_use","id":"s5","name":"f"}`, `{"type":"tool_use","name":"noid","input":{}}`)),
		"tools":                 `{"tools":[{"name":"a b","description":"d <x>","input_schema":{"type":"object","properties":{"q":{"type":"string","format":"uri"}},"additionalProperties":false},"cache_control":{"type":"ephemeral"},"type":"custom","x":1},{"name":"a_b","input_schema":{"type":"object"}},{"name":"a b","input_schema":{"type":"object"}},{"type":"web_search_20250305","name":"web_search","input_schema":{}},{"name":"noschema"},{"name":"str","input_schema":"x"},{"name":5,"input_schema":{"type":"object"}}],"tool_choice":{"type":"tool","name":"a b"},"messages":[{"role":"user","content":"x"}]}`,
		"tools/none":            `{"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":" None ","messages":[{"role":"user","content":"x"}]}`,
		"tools/any":             `{"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":{"type":"any"},"messages":[{"role":"user","content":"x"}]}`,
		"tools/auto-str":        `{"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":"auto","messages":[{"role":"user","content":"x"}]}`,
		"tools/search-only":     `{"tools":[{"type":"web_search_20250305","name":"web_search","max_uses":3}],"messages":[{"role":"user","content":"find"}]}`,
		"thinking/enabled":      `{"thinking":{"type":"enabled","budget_tokens":4096},"tools":[{"name":"f","input_schema":{"type":"object"}}],"messages":[{"role":"user","content":"x"}],"temperature":0.5,"top_p":"0.9","top_k":40,"max_tokens":1e3}`,
		"thinking/adaptive":     `{"thinking":{"type":"adaptive"},"output_config":{"effort":" MAX "},"messages":[{"role":"user","content":"x"}]}`,
		"thinking/auto":         `{"thinking":{"type":"auto"},"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":"none","messages":[{"role":"user","content":"x"}]}`,
		"thinking/budget-str":   `{"thinking":{"type":"enabled","budget_tokens":"512"},"messages":[{"role":"user","content":"x"}]}`,
		"thinking/dropped":      `{"thinking":{"type":"enabled","budget_tokens":2048},"messages":[` + user + `,` + asst(`{"type":"thinking","thinking":"unsigned"}`, text("a")) + `]}`,
		"system/array":          `{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=1"},{"type":"text","text":""},{"type":"text","text":"sys <s>"},{"type":"image"},{"type":5,"text":"num type"}],"messages":[{"role":"system","content":"mid reminder"},{"role":"user","content":"u"},{"role":"developer","content":[{"type":"text","text":"dev"}]},{"role":"user","content":""},{"role":5,"content":"bad role"},{"role":"user","content":7}]}`,
		"system/string":         `{"system":"plain system","messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}},{"type":"image","source":{"type":"base64"}},{"type":"text","text":""},{"type":"text","text":"t"}]},{"role":"user","content":"merged"}]}`,
		"merge/tool-turns":      `{"messages":[{"role":"user","content":"q"},` + asst(tool("m1", "f"), tool("m2", "g")) + `,{"role":"system","content":"reminder between"},{"role":"user","content":[{"type":"text","text":"note"},{"type":"tool_result","tool_use_id":"m2","content":"r2"},{"type":"tool_result","tool_use_id":"m1","content":"r1"}]},{"role":"user","content":"more"}]}`,
	}
	var names []string
	for name := range inputs {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	// A streamed Claude signature for "cached plan <c>" lands in the signature cache before
	// the request that sends that thinking text back unsigned.
	seed := streamCase("claude-antigravity/seed-cache", "claude-sonnet-4-6",
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"cached plan <c>","thought":true}]}}]}}`,
		`{"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"","thought":true,"thoughtSignature":"`+normalized+`"}]}}]}}`)
	seed.Translated = `{"model":"claude-sonnet-4-6"}`
	out = append(out, seed)
	for i, name := range names {
		for _, m := range []string{"claude-sonnet-4-6", "gemini-3-flash", "claude-opus-4-6-thinking", "unknown-model"} {
			out = append(out, req("claude-antigravity/"+name+"@"+m, m, inputs[name], i%2 == 0))
		}
	}
	return out
}

// antigravityToClaude exercise ConvertAntigravityResponseToClaude(NonStream) per target
// model group: signature decoding and carriers, signature-only parts, tool calls with
// signatures and stable IDs, finish and usage handling, and web-search grounding.
func antigravityToClaude() []fixture {
	nativeSig := claudeNativeSig()
	normalized, _ := signature.CompatibleAntigravityClaudeThinkingSignature(nativeSig)
	env := func(parts, extra string) string {
		return `{"response":{"candidates":[{"content":{"role":"model","parts":` + parts + `}` + extra + `}],"modelVersion":"mv","responseId":"rid"}}`
	}
	usage := `,"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"thoughtsTokenCount":2,"cachedContentTokenCount":3,"totalTokenCount":17}`
	cases := map[string][]string{
		"thinking":    {`{"response":{"candidates":[{"content":{"parts":[{"text":"plan","thought":true}]}}],"cpaUsageMetadata":{"promptTokenCount":4,"candidatesTokenCount":1}}}`, env(`[{"text":" more","thought":true},{"text":"","thought":true,"thoughtSignature":"`+normalized+`"}]`, ``), env(`[{"text":"next thought","thought":true,"thoughtSignature":"`+geminiSig+`"},{"text":"answer <a>"}]`, `,"finishReason":"STOP"`), `{"response":{"usageMetadata":{"promptTokenCount":10,"totalTokenCount":30}}}`, "[DONE]"},
		"sig-parts":   {env(`[{"thoughtSignature":"`+geminiSig+`"},{"text":"visible","thoughtSignature":"`+geminiSig+`"},{"text":"more"},{"thought_signature":"`+geminiSig+`","text":""}]`, ``), env(`[{"functionCall":{"name":"a_b","args":{"q":1}},"thoughtSignature":"`+geminiSig+`"},{"thoughtSignature":"`+geminiSig+`"},{"functionCall":{"id":"fc-1","name":"plain","args":{"z":2.50,"a":"<x>"}}},{"functionCall":{"name":"noargs"}}]`, `,"finishReason":"MAX_TOKENS"`+usage), "[DONE]"},
		"text-only":   {env(`[{"text":"hi"}]`, ``), env(`[{"text":""}]`, ``), env(`[{"text":" there"},{"inlineData":{"data":"x"}}]`, `,"finishReason":"STOP"`), "[DONE]"},
		"empty":       {`{"response":{"candidates":[]}}`, "[DONE]", "[DONE]"},
		"no-response": {"[DONE]"},
		"usage-total": {env(`[{"text":"x"}]`, `,"finishReason":"STOP","usageMetadata":{"promptTokenCount":3,"totalTokenCount":9}`), env(`[{"text":"late"}]`, ``), "[DONE]"},
		"tools-twice": {env(`[{"functionCall":{"name":"f","args":{}}},{"functionCall":{"name":"g","args":{}}},{"text":"after"},{"text":"t","thought":true}]`, `,"finishReason":"STOP"`), `{"response":{"usageMetadata":{}}}`, "[DONE]"},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	original := `{"model":"client","tools":[{"name":"a b","input_schema":{"type":"object"}}]}`
	var out []fixture
	for _, m := range []string{"claude-sonnet-4-6", "gemini-3-flash", ""} {
		for _, name := range names {
			f := streamCase("antigravity-claude/"+name+"@"+m, m, cases[name]...)
			f.Original = original
			if m != "" {
				f.Translated = `{"model":"` + m + `","request":{}}`
			}
			out = append(out, f)
		}
		for i, body := range []string{
			env(`[{"text":"t1","thought":true},{"text":"t2","thought":true,"thoughtSignature":"`+normalized+`"},{"text":"answer"},{"thoughtSignature":"`+geminiSig+`"},{"functionCall":{"name":"a_b","args":{"q":1}},"thoughtSignature":"`+geminiSig+`"},{"functionCall":{"id":"x","name":"plain","args":[1]}},{"text":"tail","thoughtSignature":"`+geminiSig+`"}]`, `,"finishReason":"STOP"`+usage),
			env(`[{"text":"think","thought":true},{"functionCall":{"name":"f","args":{}},"thoughtSignature":"`+geminiSig+`"},{"text":"","thought":true,"thoughtSignature":"`+geminiSig+`"},{"thoughtSignature":"`+geminiSig+`"}]`, `,"finishReason":"MAX_TOKENS"`),
			env(`[{"thoughtSignature":"`+geminiSig+`"},{"text":"x","thought":true},{"text":"y","thoughtSignature":"`+geminiSig+`"}]`, `,"finishReason":"OTHER","usageMetadata":{"totalTokenCount":4,"promptTokenCount":9}`),
			`{"response":{}}`, `{}`, `not json`,
		} {
			n := nonStream(fmt.Sprintf("antigravity-claude/non-stream/%d@%s", i, m), m, body)
			n.Original = original
			if m != "" {
				n.Translated = `{"model":"` + m + `"}`
			}
			out = append(out, n)
		}
	}
	// Web search grounding: a typed search tool in the client request and Google Search
	// in the upstream request.
	searchOrig := `{"model":"client","tools":[{"type":"web_search_20250305","name":"web_search"}],"messages":[{"role":"user","content":"find"}]}`
	searchReq := `{"model":"gemini-3-flash","requestType":"web_search","request":{"tools":[{"googleSearch":{}}]}}`
	gm := `,"groundingMetadata":{"webSearchQueries":["q <1>"],"groundingChunks":[{"web":{"uri":"https://a","title":"A"}},{"web":{"uri":" https://a "}},{"web":{"uri":"https://b","title":"B <b>"}},{"other":1}],"groundingSupports":[{"segment":{"startIndex":0,"endIndex":6,"text":"Answer"},"groundingChunkIndices":[0,2]},{"segment":{"startIndex":3,"endIndex":4}},{"segment":{"startIndex":7,"endIndex":12},"groundingChunkIndices":[9,-1,2]},{"segment":{"startIndex":20,"endIndex":99},"groundingChunkIndices":[1]},{"noSegment":true}]}`
	long := strings.Repeat("é", 60)
	search := map[string][]string{
		"grounded":   {env(`[{"text":"Answer é is "},{"text":"plan","thought":true}]`, ``), env(`[{"text":"cited."}]`, gm+`,"finishReason":"STOP"`+usage), "[DONE]"},
		"buffered":   {env(`[{"text":"no grounding "}]`, ``), env(`[{"text":"`+long+`"}]`, `,"finishReason":"STOP"`), `{"response":{"usageMetadata":{"promptTokenCount":1}}}`, "[DONE]"},
		"no-support": {env(`[{"text":"plain answer"}]`, `,"groundingMetadata":{"groundingChunks":[{"web":{"uri":"https://c"}}]},"finishReason":"STOP"`), "[DONE]"},
	}
	var searchNames []string
	for name := range search {
		searchNames = append(searchNames, name)
	}
	sort.Strings(searchNames)
	for _, name := range searchNames {
		f := streamCase("antigravity-claude/search/"+name, "gemini-3-flash", search[name]...)
		f.Original = searchOrig
		f.Translated = searchReq
		out = append(out, f)
	}
	for i, body := range []string{
		env(`[{"text":"Answer é is cited. Trailing text"}]`, gm+`,"finishReason":"STOP"`+usage),
		env(`[{"text":"no metadata"}]`, `,"finishReason":"STOP"`),
		`{"candidates":[{"content":{"parts":[{"text":"bare"}]},"groundingMetadata":{"webSearchQueries":[]}}]}`,
	} {
		n := nonStream(fmt.Sprintf("antigravity-claude/search/non-stream/%d", i), "gemini-3-flash", body)
		n.Original = searchOrig
		n.Translated = searchReq
		out = append(out, n)
	}
	return out
}

// claudeApplyPatchToResponses mirror the apply_patch bridge tests of
// claude_openai-responses_response_test.go (built there with helpers the miner cannot
// read): decoded previews, late and separate identities, interleaved calls, invalid and
// truncated input, Unicode fragments, identity conflicts, snapshots, EOF, non-stream.
func claudeApplyPatchToResponses() []fixture {
	start := func(index int, id, name string) string {
		return fmt.Sprintf(`data: {"type":"content_block_start","index":%d,"content_block":{"type":"tool_use","id":%q,"name":%q,"input":{}}}`, index, id, name)
	}
	snap := func(index int, id, name, input string) string {
		return fmt.Sprintf(`data: {"type":"content_block_start","index":%d,"content_block":{"type":"tool_use","id":%q,"name":%q,"input":%s}}`, index, id, name, input)
	}
	frag := func(index int, fragment string) string {
		return fmt.Sprintf(`data: {"type":"content_block_delta","index":%d,"delta":{"type":"input_json_delta","partial_json":%q}}`, index, fragment)
	}
	stop := func(index int) string { return fmt.Sprintf(`data: {"type":"content_block_stop","index":%d}`, index) }
	msgStart := `data: {"type":"message_start","message":{"id":"msg_ap","usage":{"input_tokens":1}}}`
	end := []string{`data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}`, `data: {"type":"message_stop"}`}
	text := []string{`data: {"type":"content_block_start","index":9,"content_block":{"type":"text","text":""}}`, `data: {"type":"content_block_delta","index":9,"delta":{"type":"text_delta","text":"done"}}`, stop(9)}
	request := `{"tools":[{"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"start: patch"}}]}`
	namespaced := `{"tools":[{"type":"namespace","name":"ns","tools":[{"type":"custom","name":"apply_patch"}]},{"type":"function","name":"lookup"}]}`
	shadowed := `{"tools":[{"type":"function","name":"apply_patch"},{"type":"custom","name":"apply_patch"}],"input":[{"type":"additional_tools","tools":[{"type":"custom","name":"apply_patch"}]}]}`
	mixed := `{"tools":[{"type":"custom","name":"apply_patch"},{"type":"custom","name":"freeform"},{"type":"function","name":"lookup"}]}`
	patch := "*** Begin Patch\n*** Add File: a.txt\n+hello\n*** End Patch"
	type sc struct {
		original string
		finalize bool
		lines    []string
	}
	join := func(parts ...[]string) []string {
		var out []string
		for _, p := range parts {
			out = append(out, p...)
		}
		return out
	}
	cases := map[string]sc{
		"preview":          {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"*** Begin Patch\n*** Add File: a.txt\n+hello\n`), frag(0, `*** End Patch"}`), stop(0)}, end)},
		"late-identity":    {request, false, join([]string{msgStart, start(0, "", ""), frag(0, `{"input":"first\n`), start(1, "c2", "apply_patch"), frag(1, `{"input":"second\n"}`), stop(1), start(0, "c1", "apply_patch"), frag(0, `more"}`), stop(0)}, text, end)},
		"invalid":          {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":5}`), frag(0, `x`), stop(0)}, end)},
		"invalid-key":      {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"patch":"x"}`), stop(0)}, end)},
		"truncated":        {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"*** Begin Patch\n`), stop(0)}, end)},
		"unicode":          {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"caf\u00`), frag(0, `e9 \ud83d`), frag(0, `\ude00 <b> é"}`), stop(0)}, end)},
		"separate-ids":     {request, false, join([]string{msgStart, start(0, "", "apply_patch"), frag(0, `{"input":"x"}`), start(0, "c1", ""), stop(0)}, end)},
		"conflict-id":      {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), start(0, "c2", "apply_patch"), frag(0, `{"input":"x"}`)}, end)},
		"conflict-name":    {mixed, false, join([]string{msgStart, start(0, "c1", "freeform"), start(0, "c1", "apply_patch")}, end)},
		"pending-id":       {request, false, join([]string{msgStart, start(0, "c1", ""), start(0, "c2", ""), start(0, "", "apply_patch")}, end)},
		"missing-id":       {request, false, join([]string{msgStart, start(0, "", "apply_patch"), frag(0, `{"input":"`+strings.ReplaceAll(patch, "\n", `\n`)+`"}`), stop(0)}, end)},
		"nameless":         {request, false, join([]string{msgStart, start(0, "", ""), frag(0, `{"input":"anon"}`), stop(0)}, end)},
		"snapshot-ok":      {request, false, join([]string{msgStart, snap(0, "c1", "apply_patch", `{"input":"snap"}`), snap(0, "c1", "apply_patch", `{ "input" : "snap" }`), stop(0)}, end)},
		"snapshot-diff":    {request, false, join([]string{msgStart, snap(0, "c1", "apply_patch", `{"input":"one"}`), snap(0, "c1", "apply_patch", `{"input":"two"}`)}, end)},
		"snapshot-bad":     {request, false, join([]string{msgStart, snap(0, "c1", "", `{"input":7}`), start(0, "", "apply_patch")}, end)},
		"snapshot-args":    {request, false, join([]string{msgStart, snap(0, "c1", "apply_patch", `{"input":"full"}`), frag(0, `{"input":"fu`), stop(0)}, end)},
		"snapshot-extend":  {request, false, join([]string{msgStart, snap(0, "c1", "apply_patch", `{"input":"ab"}`), frag(0, `{"input":"a"}`), stop(0)}, end)},
		"snapshot-badargs": {request, false, join([]string{msgStart, snap(0, "c1", "apply_patch", `{"input":"x"}`), frag(0, `{"input":5`), stop(0)}, end)},
		"snapshot-late":    {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"a"}`), stop(0), start(1, "c2", "lookup"), snap(0, "c1", "apply_patch", `{"input":"b"}`)}, end)},
		"snapshot-same":    {request, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"a"}`), stop(0), start(1, "c2", "lookup"), stop(1), snap(0, "c1", "apply_patch", `{"input":"a"}`)}, end)},
		"namespace":        {namespaced, false, join([]string{msgStart, start(0, "c1", "ns__apply_patch"), frag(0, `{"input":"n"}`), stop(0), start(1, "c2", "lookup"), frag(1, `{}`), stop(1)}, end)},
		"shadowed":         {shadowed, false, join([]string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"s"}`), stop(0)}, end)},
		"eof":              {request, true, []string{msgStart, start(0, "c1", "apply_patch"), frag(0, `{"input":"x"}`)}},
		"eof-plain":        {`{"tools":[{"type":"function","name":"lookup"}]}`, true, []string{msgStart, start(0, "c1", "lookup")}},
		"eof-completed":    {request, true, join([]string{msgStart}, end)},
		"after-complete":   {request, false, join([]string{msgStart}, end, []string{start(0, "c9", "apply_patch")})},
	}
	var names []string
	for name := range cases {
		names = append(names, name)
	}
	sort.Strings(names)
	var out []fixture
	for _, name := range names {
		c := cases[name]
		f := streamCase("claude-apply-patch/"+name, "claude-sonnet-4-6", c.lines...)
		f.Original = c.original
		f.Finalize = c.finalize
		out = append(out, f)
		if !c.finalize {
			n := nonStream("claude-apply-patch/non-stream/"+name, "claude-sonnet-4-6", strings.Join(c.lines, "\n"))
			n.Original = c.original
			out = append(out, n)
		}
	}
	return out
}
