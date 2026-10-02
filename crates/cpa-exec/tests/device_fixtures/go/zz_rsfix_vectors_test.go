package executor

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
)

// Pure-function vectors: input -> Go output, for the Rust unit tests.
func TestRSFixKimiVectors(t *testing.T) {
	out := rsfixOut(t)
	type vector struct {
		Fn    string `json:"fn"`
		In    string `json:"in"`
		Model string `json:"model,omitempty"`
		From  string `json:"from,omitempty"`
		To    string `json:"to,omitempty"`
		Out   string `json:"out"`
		Err   string `json:"err,omitempty"`
	}
	var vectors []vector
	add := func(fn, in string, got []byte, err error) {
		v := vector{Fn: fn, In: in, Out: string(got)}
		if err != nil {
			v.Err = err.Error()
		}
		vectors = append(vectors, v)
	}
	for _, in := range []string{
		`{"properties":{"node":{"$ref":"#/$defs/Node"}},"$defs":{"Node":{"type":"object","description":"tree","properties":{"child":{"$ref":"#/$defs/Node"}}}}}`,
		`{"properties":{"a":{"$ref":"#/definitions/A","description":"override <b>"}},"definitions":{"A":{"type":"string","enum":["x","é"],"maximum":1.50}}}`,
		`{"type":"object","properties":{"list":{"type":"array","items":{"$ref":"#/$defs/Item"}}},"$defs":{"Item":{"$ref":"#/$defs/Leaf"},"Leaf":{"type":"integer"}}}`,
		`{"properties":{"x":{"$ref":"#/missing"}}}`,
		`{"$defs":{"a":1}, "properties":{}}`,
	} {
		add("schema", in, []byte(normalizeKimiParametersSchema(in)), nil)
	}
	for _, in := range []string{
		`{"messages":[{"role":"assistant","content":"","reasoning_content":"r","tool_calls":[{"id":"call_1","type":"function","function":{"name":"f","arguments":"{}"}}]},{"role":"tool","content":"x"}]}`,
		`{"messages":[{"role":"assistant","reasoning_content":"r","tool_calls":[{"id":"a"},{"id":"b"}]},{"role":"tool","content":"x"}]}`,
		`{"messages":[{"role":"assistant","content":"x","reasoning_content":"first"},{"role":"assistant","content":"y","tool_calls":[{"id":"c"}]}]}`,
		`{"messages":[{"role":"assistant","content":[{"type":"text","text":" a "},{"type":"text","text":"b"}],"tool_calls":[{"id":"c"}]}]}`,
		`{"messages":[{"role":"user","content":"u"},{"role":"assistant","content":[{"type":"text","text":" "}]},{"role":"assistant","function_call":{"name":"f"}},{"role":"assistant","content":null},{"role":"assistant","content":[{}]}]}`,
		`{"messages": [ {"role": "assistant", "content": "<tag> & \"q\"", "tool_calls": [{"id": "t1"}]}, {"role": "tool", "call_id": "t1", "content": "r"} ]}`,
		`{"messages":[{"role":"assistant","reasoning_content":"[reasoning unavailable]","content":"é","tool_calls":[{"id":"t"}]}]}`,
	} {
		got, err := normalizeKimiToolMessageLinks([]byte(in))
		add("links", in, got, err)
	}
	for _, in := range []string{
		`{"temperature":0.6,"thinking":{"type":"disabled"}}`,
		`{"temperature":0.6}`,
		`{"temperature":1}`,
		`{"temperature":"1.0","thinking":{"type":"enabled"}}`,
		`{ "a": 1, "temperature": 0.2 }`,
	} {
		add("temperature", in, normalizeKimiTemperature([]byte(in)), nil)
	}
	for _, in := range []string{
		`{"input":[{"type":"function_call","call_id":"a"},{"type":"message","role":"user"},{"type":"function_call_output","call_id":"a"}]}`,
		`{"input":[{"type":"function_call","call_id":"a"},{"type":"message","role":"user"}]}`,
		`{"input":[{"type":"custom_tool_call","call_id":"a"},{"type":"function_call","id":"fco_x"},{"type":"message"},{"type":"custom_tool_call_output","call_id":"a"},{"type":"function_call_output","call_id":"zzz"}]}`,
	} {
		got, err := helps.NormalizeKimiResponsesInput([]byte(in))
		add("responses_input", in, got, err)
	}
	for _, tc := range []struct{ in, model, from, to string }{
		{`{"model":"x","reasoning_effort":"low"}`, "kimi-k2.8(max)", "openai", "kimi"},
		{`{"reasoning_effort":"none"}`, "kimi-k2.7-code", "openai", "kimi"},
		{`{"reasoning_effort":"none"}`, "kimi-k2.5", "openai", "kimi"},
		{`{"reasoning_effort":"xhigh"}`, "kimi-k2.5", "openai", "kimi"},
		{`{"a":1,"reasoning_effort":"high"}`, "kimi-k2", "openai", "kimi"},
		{`{"reasoning_effort":"ultra"}`, "kimi-custom", "openai", "kimi"},
		{`{}`, "kimi-k2.5(9000)", "openai", "kimi"},
		{`{"thinking":{"type":"enabled","effort":"xhigh"}}`, "kimi-k2.5", "kimi", "kimi"},
		{`{"reasoning_effort":null}`, "kimi-k2.5", "openai", "kimi"},
		{`{}`, "kimi-k2.5(auto)", "openai", "kimi"},
		{`{}`, "kimi-k2.5(-5)", "openai", "kimi"},
		{`{"model":"k3","reasoning":{"effort":"low","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"max"}}]}`, "kimi-k3(high)", "openai-response", "codex"},
		{`{"model":"k3","reasoning":{"effort":"xhigh"},"input":"hi"}`, "kimi-k3", "openai-response", "codex"},
		{`{"model":"k3","reasoning":{"summary":"detailed"},"input":"hi"}`, "kimi-k3(none)", "openai-response", "codex"},
		{`{"model":"k3","reasoning":{"effort":"none","summary":"concise"}}`, "kimi-k3", "openai-response", "codex"},
		{`{"model":"k","reasoning":{"effort":"high"}}`, "kimi-k2", "openai-response", "codex"},
	} {
		got, err := helps.ApplyRequestThinking([]byte(tc.in), cliproxyexecutor.Request{Model: tc.model, Payload: []byte(tc.in)}, cliproxyexecutor.Options{OriginalRequest: []byte(tc.in)}, tc.from, tc.to, "kimi")
		v := vector{Fn: "thinking", In: tc.in, Model: tc.model, From: tc.from, To: tc.to, Out: string(got)}
		if err != nil {
			v.Err = err.Error()
		}
		vectors = append(vectors, v)
	}
	_ = thinking.ParseSuffix
	for _, tc := range []struct{ in, model string }{
		{`{"model":"k3","x":1}`, "kimi-k3"},
		{`data: {"type":"message_start","message":{"model":"k3"}}`, "kimi-k3(high)"},
		{`data:{"type":"message_delta","model":"k3","message":{"model":"k3"}}`, "kimi-k3"},
		{`data: {"type":"ping"}`, "kimi-k3"},
	} {
		e := NewKimiExecutor(nil)
		vectors = append(vectors, vector{Fn: "restore_model", In: tc.in, Model: tc.model, Out: string(e.restoreResponseModel([]byte(tc.in), tc.model))})
	}
	raw, _ := json.MarshalIndent(vectors, "", "  ")
	dir := filepath.Join(out, "kimi")
	_ = os.MkdirAll(dir, 0o755)
	if err := os.WriteFile(filepath.Join(dir, "vectors.json"), append(raw, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
