package main

import (
	"context"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type pair struct {
	In  string `json:"in"`
	Out string `json:"out"`
}

type claudeTokens struct {
	Original string   `json:"original"`
	Chunks   []string `json:"chunks"`
	Out      []string `json:"out"`
}

// vectors records Go answers for the exported helpers the executor ports. Inputs that
// carry a traceId run in order: FilterSSEUsageMetadata keeps process-wide state.
func vectors() map[string]any {
	filterInputs := []string{
		``,
		`data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"totalTokenCount":1}}`,
		`data:{"candidates":[{"finishReason":"STOP"}],"usageMetadata":{"totalTokenCount":2}}`,
		`data: {"candidates":[{"finishReason":"  "}],"usageMetadata":{"totalTokenCount":2}}`,
		`  data: {"response":{"usageMetadata":{"a":1},"candidates":[{}]},"usageMetadata":{"b":2}}`,
		`data: {"response":{"candidates":[{"finishReason":"MAX_TOKENS"}],"usageMetadata":{"a":1}}}`,
		`{"usageMetadata":{"x":1},"candidates":[]}`,
		`  {"usageMetadata":{"x":1}}  `,
		`{"candidates":[]}`,
		`event: x`,
		`data: [DONE]`,
		`data: not json`,
		"data: {\"usageMetadata\":{\"x\":1}}\ndata: {\"y\":2}\nid: 3",
		"event: e\ndata: {\"usageMetadata\":{\"x\":1}}",
		`data: {"traceId":"vec-a","candidates":[{"finishReason":"STOP"}]}`,
		`data: {"traceId":"vec-a","usageMetadata":{"total":5}}`,
		`data: {"traceId":"vec-a","usageMetadata":{"total":6}}`,
		`data: {"traceId":"vec-b","response":{"candidates":[{"finishReason":"STOP"}]}}`,
		`data: {"traceId":"vec-b","response":{"usageMetadata":{"total":5}}}`,
		`data: {"traceId":"","candidates":[{"finishReason":"STOP"}]}`,
		`data: {"usageMetadata":null}`,
		`data:   {"usageMetadata":{"x":1}}   `,
	}
	var filter []pair
	for _, in := range filterInputs {
		filter = append(filter, pair{In: in, Out: string(helps.FilterSSEUsageMetadata([]byte(in)))})
	}

	var payload []pair
	for _, in := range []string{``, `  `, `[DONE]`, ` data: [DONE]`, `event: x`, `data:{"a":1}`, `data:   {"a":1}  `, `{"a":1}`, `[1]`, `data: [1]`, `data:`, `: comment`, `id: 1`, `data: {broken`} {
		payload = append(payload, pair{In: in, Out: string(helps.JSONPayload([]byte(in)))})
	}

	turnInputs := []string{
		`{"contents":[]}`,
		`{"contents":[{"role":"model","parts":[]}]}`,
		`{"contents":[{"role":"user"},{"role":"model"}]}`,
		`{"contents":[{"role":"user"},{"role":"assistant","parts":[{"text":"x"}]}]}`,
		`{"contents":[{"role":"user"},{"role":"model","parts":[{"functionResponse":{}}]}]}`,
		`{"contents":[{"role":"user"},{"role":"model","parts":{"functionResponse":{}}}]}`,
		`{"contents":{"0":{"role":"model"}}}`,
		`{"contents":[ {"role" : "model"} , {"role":"model"} ]}`,
		`{"request":{"contents":[{"role":"model"}]}}`,
		`{"contents":"model"}`,
		`not json`,
	}
	var leading, trailing []pair
	for _, in := range turnInputs {
		leading = append(leading, pair{In: in, Out: string(helps.EnsureGeminiLeadingUserContent([]byte(in), "contents"))})
		trailing = append(trailing, pair{In: in, Out: string(helps.EnsureGeminiTrailingUserContent([]byte(in), "contents"))})
	}
	leading = append(leading, pair{In: turnInputs[8], Out: string(helps.EnsureGeminiLeadingUserContent([]byte(turnInputs[8]), "request.contents"))})

	original := `{"system":[{"type":"text","text":"Be kind."},"raw system"],"messages":[{"role":"user","content":[{"type":"text","text":"hello world"},{"type":"image","source":{}},{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"result text"}]}]},{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"lookup","input":{"q": "x" , "n":1}},{"type":"thinking","thinking":"hmm"}]}],"tools":[{"name":"lookup","description":"find things","input_schema":{"type":"object","properties":{"q":{"type":"string"}}}}],"tool_choice":{"type":"tool","name":"lookup"}}`
	cases := []claudeTokens{
		{Original: original, Chunks: []string{"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"output_tokens\":1}}}\n\n", "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}\n\n"}},
		{Original: original, Chunks: []string{"event: message_start\r\ndata:\t{\"type\":\"message_start\",\"message\":{}}  \r\n\r\n"}},
		{Original: original, Chunks: []string{"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12}}}\n\n"}},
		{Original: `{"messages":[]}`, Chunks: []string{"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}"}},
		{Original: `not json`, Chunks: []string{"data: {\"type\":\"message_start\"}"}},
		{Original: original, Chunks: []string{"data: {\"type\":\"ping\"}", "data: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}"}},
		{Original: `{"system":"  spaced  ","messages":[{"role":"user","content":"x"}],"tool_choice":"auto"}`, Chunks: []string{"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0}}}"}},
	}
	fake := sdktranslator.FromString("vector-upstream")
	claude := sdktranslator.FromString("claude")
	for i := range cases {
		state := helps.NewClaudeInputTokenState(claude, fake, claude, []byte(cases[i].Original))
		var param any
		for _, chunk := range cases[i].Chunks {
			outs := helps.TranslateStreamWithClaudeInputTokens(context.Background(), fake, claude, "m", []byte(cases[i].Original), nil, []byte(chunk), &param, state)
			for _, out := range outs {
				cases[i].Out = append(cases[i].Out, string(out))
			}
		}
	}

	return map[string]any{
		"filter_sse_usage":     filter,
		"json_payload":         payload,
		"leading_user_content": leading,
		"trailing_user":        trailing,
		"claude_input_tokens":  cases,
	}
}
