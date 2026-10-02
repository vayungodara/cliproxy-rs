package executor

import (
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor/helps"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/thinking"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
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
		Path  string `json:"path,omitempty"`
		Value string `json:"value,omitempty"`
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
	for _, tc := range []struct{ fn, in, path, value string }{
		{"sjson_delete", `{"a": 1, "b": 2, "c": 3}`, "a", ""},
		{"sjson_delete", `{"a": 1, "b": 2, "c": 3}`, "b", ""},
		{"sjson_delete", `{"a": 1, "b": 2, "c": 3}`, "c", ""},
		{"sjson_delete", `{"only":true}`, "only", ""},
		{"sjson_delete", `[1,2]`, "-1", ""},
		{"sjson_delete", `{"\"x":1,"y":2}`, `\"x`, ""},
		{"sjson_delete", `{"t":{"type":"x","effort":"y"}}`, "t.effort", ""},
		{"sjson_delete", `{"m":[ {"a":1} , {"b":2} ]}`, "m.0", ""},
		{"sjson_set_str", `{"a":1}`, "model", "k3"},
		{"sjson_set_str", ` { "a": 1 } `, "b", "x"},
		{"sjson_set_str", `{}`, "thinking.type", "enabled"},
		{"sjson_set_str", `{"a":1}`, "b", "é<\"&"},
		{"sjson_set_str", `{"a":1}`, "b", "plain <tag> & co"},
		{"sjson_set_str", `[1]`, "3", "x"},
		{"sjson_set_str", `"scalar"`, "a.b", "x"},
		{"sjson_set_raw", `{"m":[{"a":1},{"b":2}]}`, "m.1.c", "true"},
		{"sjson_set_raw", `  `, "a.0", "1"},
		{"sjson_set_raw", `{"stream_options":[]}`, "stream_options.include_usage", "true"},
		{"sjson_set_raw", `{"a":[]}`, "a.-1", "5"},
		{"sjson_set_raw", `{"a":{}}`, "a.x.2", "5"},
	} {
		var got []byte
		var err error
		switch tc.fn {
		case "sjson_delete":
			got, err = sjson.DeleteBytes([]byte(tc.in), tc.path)
		case "sjson_set_str":
			got, err = sjson.SetBytes([]byte(tc.in), tc.path, tc.value)
		default:
			got, err = sjson.SetRawBytes([]byte(tc.in), tc.path, []byte(tc.value))
		}
		v := vector{Fn: tc.fn, In: tc.in, Path: tc.path, Value: tc.value, Out: string(got)}
		if err != nil {
			v.Err = err.Error()
		}
		vectors = append(vectors, v)
	}
	for _, in := range []string{`{"n":1e2}`, `{"n":-12}`, `{"n":0.1}`, `{"n":1.50}`, `{"n":-0}`, `{"n":12345678901234567890}`, `{"n":null}`, `{"n":true}`} {
		vectors = append(vectors, vector{Fn: "gjson_string", In: in, Out: gjson.Get(in, "n").String()})
	}
	raw, _ := json.MarshalIndent(vectors, "", "  ")
	dir := filepath.Join(out, "kimi")
	_ = os.MkdirAll(dir, 0o755)
	if err := os.WriteFile(filepath.Join(dir, "vectors.json"), append(raw, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
