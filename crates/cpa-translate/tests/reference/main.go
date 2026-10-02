// Generates byte-level goldens by calling the pinned Go translators, never providers.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"

	claude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/chat-completions"
	openai "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/openai/openai/chat-completions"
	"github.com/tidwall/gjson"
	"github.com/tidwall/sjson"
)

type fixture struct {
	Name         string     `json:"name"`
	Client       string     `json:"client"`
	Upstream     string     `json:"upstream"`
	Path         string     `json:"path"`
	Model        string     `json:"model"`
	Stream       bool       `json:"stream"`
	Input        string     `json:"input,omitempty"`
	Events       []string   `json:"events,omitempty"`
	Output       string     `json:"output,omitempty"`
	Chunks       [][]string `json:"chunks,omitempty"`
	GeneratedIDs []string   `json:"generated_ids,omitempty"`
}

func normalize(raw []byte) string {
	if gjson.GetBytes(raw, "created").Exists() {
		raw, _ = sjson.SetBytes(raw, "created", 0)
	}
	return string(raw)
}

func eval(expr ast.Expr, env map[string]any) any {
	switch e := expr.(type) {
	case *ast.BasicLit:
		if e.Kind == token.STRING {
			s, _ := strconv.Unquote(e.Value)
			return s
		}
	case *ast.Ident:
		if e.Name == "true" {
			return true
		}
		if e.Name == "false" {
			return false
		}
		return env[e.Name]
	case *ast.ParenExpr:
		return eval(e.X, env)
	case *ast.CallExpr:
		if len(e.Args) == 1 {
			return eval(e.Args[0], env)
		}
	case *ast.BinaryExpr:
		if e.Op == token.ADD {
			a, okA := eval(e.X, env).(string)
			b, okB := eval(e.Y, env).(string)
			if okA && okB {
				return a + b
			}
		}
	case *ast.CompositeLit:
		values := []string{}
		for _, v := range e.Elts {
			s, ok := eval(v, env).(string)
			if !ok {
				return nil
			}
			values = append(values, s)
		}
		return values
	}
	return nil
}

func main() {
	if len(os.Args) != 3 {
		panic("usage: generate REFERENCE_ROOT OUTPUT.json")
	}
	var fixtures []fixture
	addRequest := func(name, upstream, model, input string, stream bool, compat ...bool) {
		var output []byte
		path := "request"
		if upstream == "claude" && len(compat) > 0 && compat[0] {
			output = claude.ConvertOpenAIRequestToClaudeWithCompat(model, []byte(input), stream)
			path = "request_compat"
		} else if upstream == "claude" {
			output = claude.ConvertOpenAIRequestToClaude(model, []byte(input), stream)
		} else {
			output = openai.ConvertOpenAIRequestToOpenAI(model, []byte(input), stream)
		}
		f := fixture{Name: name, Client: "openai", Upstream: upstream, Path: path, Model: model, Stream: stream, Input: input, Output: string(output)}
		known := map[string]bool{}
		sanitize := regexp.MustCompile(`[^a-zA-Z0-9_-]`)
		for _, message := range gjson.Get(input, "messages").Array() {
			id := message.Get("tool_call_id").String()
			if id != "" {
				known[sanitize.ReplaceAllString(id, "_")] = true
			}
			for _, call := range message.Get("tool_calls").Array() {
				id := call.Get("id").String()
				if id != "" {
					known[sanitize.ReplaceAllString(id, "_")] = true
				}
			}
		}
		for i, message := range gjson.Get(f.Output, "messages").Array() {
			for j, part := range message.Get("content").Array() {
				key := "id"
				if part.Get("type").String() == "tool_result" {
					key = "tool_use_id"
				}
				id := part.Get(key).String()
				if strings.HasPrefix(id, "toolu_") && !known[id] {
					path := fmt.Sprintf("messages.%d.content.%d.%s", i, j, key)
					f.GeneratedIDs = append(f.GeneratedIDs, path)
					f.Output, _ = sjson.Set(f.Output, path, "generated")
				}
			}
		}
		fixtures = append(fixtures, f)
	}
	addResponse := func(name, upstream, model string, events []string) {
		f := fixture{Name: name, Client: "openai", Upstream: upstream, Path: "stream", Model: model, Events: events, Chunks: [][]string{}}
		var state any
		for _, event := range events {
			var chunks [][]byte
			if upstream == "claude" {
				chunks = claude.ConvertClaudeResponseToOpenAI(context.Background(), model, nil, nil, []byte(event), &state)
			} else {
				chunks = openai.ConvertOpenAIResponseToOpenAI(context.Background(), model, nil, nil, []byte(event), &state)
			}
			outputs := []string{}
			for _, chunk := range chunks {
				if upstream == "claude" {
					outputs = append(outputs, normalize(chunk))
				} else {
					outputs = append(outputs, string(chunk))
				}
			}
			f.Chunks = append(f.Chunks, outputs)
		}
		fixtures = append(fixtures, f)
		if upstream == "claude" {
			raw := strings.Join(events, "\n") + "\n"
			output := claude.ConvertClaudeResponseToOpenAINonStream(context.Background(), model, nil, nil, []byte(raw), nil)
			fixtures = append(fixtures, fixture{Name: name + "/buffered", Client: "openai", Upstream: upstream, Path: "non_stream", Model: model, Input: raw, Output: normalize(output)})
		}
	}
	for _, directory := range []string{"internal/translator/claude/openai/chat-completions", "internal/translator/openai/openai/chat-completions"} {
		upstream := "claude"
		if strings.Contains(directory, "openai/openai") {
			upstream = "openai"
		}
		files, _ := filepath.Glob(filepath.Join(os.Args[1], directory, "*_test.go"))
		for _, file := range files {
			fset := token.NewFileSet()
			parsed, err := parser.ParseFile(fset, file, nil, 0)
			if err != nil {
				panic(err)
			}
			for _, declaration := range parsed.Decls {
				function, ok := declaration.(*ast.FuncDecl)
				if !ok || !strings.HasPrefix(function.Name.Name, "Test") {
					continue
				}
				env := map[string]any{}
				requestModel := "claude-opus-5-5"
				ast.Inspect(function.Body, func(node ast.Node) bool {
					if call, ok := node.(*ast.CallExpr); ok && len(call.Args) == 3 {
						if id, ok := call.Fun.(*ast.Ident); ok && strings.HasPrefix(id.Name, "ConvertOpenAIRequest") {
							if model, ok := eval(call.Args[0], env).(string); ok {
								requestModel = model
							}
						}
					}
					return true
				})
				var streamEvents []string
				ast.Inspect(function.Body, func(node ast.Node) bool {
					if call, ok := node.(*ast.CallExpr); ok && len(call.Args) == 6 {
						if id, ok := call.Fun.(*ast.Ident); ok && (id.Name == "ConvertClaudeResponseToOpenAI" || id.Name == "ConvertOpenAIResponseToOpenAI") {
							if event, ok := eval(call.Args[4], env).(string); ok {
								streamEvents = append(streamEvents, event)
							}
						}
					}
					assignment, ok := node.(*ast.AssignStmt)
					if ok {
						for i, right := range assignment.Rhs {
							if i < len(assignment.Lhs) {
								if id, ok := assignment.Lhs[i].(*ast.Ident); ok {
									env[id.Name] = eval(right, env)
								}
							}
						}
						for _, right := range assignment.Rhs {
							call, ok := right.(*ast.CallExpr)
							if !ok {
								continue
							}
							id, ok := call.Fun.(*ast.Ident)
							if !ok {
								continue
							}
							if id.Name == "ConvertOpenAIRequestToClaude" || id.Name == "ConvertOpenAIRequestToClaudeWithCompat" || id.Name == "ConvertOpenAIRequestToOpenAI" {
								model, okM := eval(call.Args[0], env).(string)
								input, okI := eval(call.Args[1], env).(string)
								stream, okS := eval(call.Args[2], env).(bool)
								if okM && okI && okS {
									addRequest(function.Name.Name+fmt.Sprintf(":%d", fset.Position(call.Pos()).Line), upstream, model, input, stream, id.Name == "ConvertOpenAIRequestToClaudeWithCompat")
								}
							}
							if id.Name == "ConvertClaudeResponseToOpenAINonStream" {
								input, okI := eval(call.Args[4], env).(string)
								if okI {
									output := claude.ConvertClaudeResponseToOpenAINonStream(context.Background(), "", nil, nil, []byte(input), nil)
									fixtures = append(fixtures, fixture{Name: function.Name.Name, Client: "openai", Upstream: upstream, Path: "non_stream", Input: input, Output: normalize(output)})
								}
							}
						}
						if events, ok := env["events"].([]string); ok && len(events) > 0 {
							addResponse(function.Name.Name, upstream, "claude-opus-4-6", events)
							delete(env, "events")
						}
					}
					return true
				})
				if len(streamEvents) > 0 {
					addResponse(function.Name.Name, upstream, "claude-opus-4-6", streamEvents)
				}
				// Request table inputs are literals inside keyed composite fields.
				ast.Inspect(function.Body, func(node ast.Node) bool {
					field, ok := node.(*ast.KeyValueExpr)
					if !ok {
						return true
					}
					key, ok := field.Key.(*ast.Ident)
					if !ok || !strings.Contains(function.Name.Name, "Request") || (key.Name != "input" && key.Name != "inputJSON" && key.Name != "rawJSON" && key.Name != "body") {
						return true
					}
					input, ok := eval(field.Value, env).(string)
					if ok && gjson.Valid(input) {
						addRequest(function.Name.Name+fmt.Sprintf(":%d", fset.Position(field.Pos()).Line), upstream, requestModel, input, false)
					}
					return true
				})
			}
		}
	}
	for _, effort := range []string{"none", "auto", "minimal", "low", "medium", "high", "xhigh", "max", "invalid"} {
		for _, model := range []string{"claude-opus-4-6", "claude-sonnet-4-5-20250929", "unknown"} {
			addRequest("effort/"+model+"/"+effort, "claude", model, `{"reasoning_effort":"`+effort+`","include_reasoning":true,"messages":[{"role":"user","content":"hi"}]}`, false)
		}
	}
	for i, input := range []string{
		`{ "model" : "old", "number":1e+09, "model":"duplicate", "opaque": {"z":9007199254740993,"a":"\u0061"} }`,
		"{ \"model\":\"gpt-test\", \"opaque\":1e+09 }\n",
		`{ "opaque":1e+09 }`, `{ }`, `[]`, `invalid`, `null`, `{"model":null}`, `{"model":9}`, `{"model":"escaped\u0020model"}`,
		`{`, `{"opaque":1`, `{"model":`, `{"model":"old"`, `{"model":"old",}`, ` { } trailing`,
		` {"nested":{"braces":"}\\\"["},"array":[1,{"n":1e+09}]} trailing {}`,
	} {
		addRequest(fmt.Sprintf("normalization/raw/%d", i), "openai", "gpt-test", input, true)
	}
	for _, number := range []string{"0", "-0", "0.75", "1e-7", "1e-6", "1e21", "1.2345678901234567e-9"} {
		addRequest("top-p/"+number, "claude", "claude-test", `{"top_p":`+number+`,"messages":[{"role":"user","content":"hi"}]}`, false)
	}
	for i, parameters := range []string{
		`{"allOf":[{"type":"\u006fbject","properties":{"n":{"default":9007199254740993}}}]}`,
		`{"required":["z","a"],"allOf":[{"properties":{"a":{}},"required":"invalid"}]}`,
		`{"required":["z","a"],"allOf":[{"properties":{"a":{}}}]}`,
		`{"required":["z","a"],"allOf":[{"required":null}]}`,
		`{"allOf":[null,{"type":null,"properties":{"ignored":{}}},{"type":["string","object"],"properties":{"a":{"description":"<&>\u2028"}}}]}`,
	} {
		addRequest(fmt.Sprintf("schema/edge/%d", i), "claude", "claude-test", `{"tools":[{"type":"function","function":{"name":"f","parameters":`+parameters+`}}]}`, false)
	}
	for i, input := range []string{` {"id":"opaque","created":123,"usage":{"n":1e+09}} `, `invalid`} {
		output := openai.ConvertOpenAIResponseToOpenAINonStream(context.Background(), "m", nil, nil, []byte(input), nil)
		fixtures = append(fixtures, fixture{Name: fmt.Sprintf("normalization/non-stream/%d", i), Client: "openai", Upstream: "openai", Path: "non_stream", Model: "m", Input: input, Output: string(output)})
	}
	for _, choice := range []string{`"auto"`, `"none"`, `"required"`, `"any"`, `{"type":"any"}`, `{"type":"function","function":{"name":"a.b"}}`,
		`{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"a.b"}}]}}`,
		`{"type":"allowed_tools","tools":[{"name":"missing"}]}`} {
		addRequest("choice/"+choice, "claude", "claude-test", `{"tool_choice":`+choice+`,"parallel_tool_calls":false,"tools":[{"type":"function","function":{"name":"a.b","parameters":{"allOf":[{"properties":{"z":{"default":1e+09,"description":"<&>"}},"required":["z"]},{"properties":{"a":{"type":"string"}},"required":["a","z"]}]}}}],"messages":[{"role":"user","content":"test"}]}`, true)
	}
	addResponse("normalization/done", "openai", "m", []string{`data: {"id":"x","choices":[]}`, "data: [DONE]", `data: {"choices":[],"cost":"0"}`})
	addResponse("normalization/bare", "openai", "m", []string{`{"id":"y"}`})
	addResponse("normalization/bare-done", "openai", "m", []string{"[DONE]", `{"id":"after"}`})
	addResponse("normalization/timestamp", "openai", "m", []string{`data: {"created":123,"n":1e+09}`, ` {"created":456,"n":9007199254740993} `})
	addRequest("fallback/random-and-counter", "claude", "claude-test", `{"messages":[{"role":"assistant","tool_calls":[{"type":"function","function":{"name":"a","arguments":"{}"}},{"type":"function","function":{"name":"b","arguments":"[]"}}]},{"role":"tool","content":null},{"role":"tool","content":false}]}`, false)
	addResponse("parallel/out-of-order", "claude", "requested-model", []string{
		`data: {"type":"message_start","message":{"id":"msg-x","model":"upstream-model","usage":{"input_tokens":13,"output_tokens":1,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}}`,
		`data: {"type":"content_block_start","index":7,"content_block":{"type":"tool_use","id":"call-a","name":"first","input":{"ignored":true}}}`,
		`data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"call-b","name":"second"}}`,
		`data: {"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"{\"n\": "}}`,
		`data: {"type":"content_block_delta","index":7,"delta":{"type":"input_json_delta","partial_json":"1e+09}"}}`,
		`data: {"type":"content_block_stop","index":2}`,
		`data: {"type":"content_block_stop","index":7}`,
		`data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4,"cache_read_input_tokens":0}}`,
		`data: {"type":"message_stop"}`, `data: {"type":"message_stop"}`,
	})
	for _, reason := range []string{"end_turn", "max_tokens", "refusal", "sensitive", "stop_sequence", "unknown"} {
		addResponse("finish/"+reason, "claude", "m", []string{`data: {"type":"message_delta","delta":{"stop_reason":"` + reason + `"},"usage":{"input_tokens":13,"output_tokens":4,"cache_read_input_tokens":22000,"cache_creation_input_tokens":31}}`, `data: {"type":"message_stop"}`})
	}
	for _, reason := range []string{"missing", "end_turn", "stop_sequence", "max_tokens", "refusal", "sensitive"} {
		delta := `{}`
		if reason != "missing" {
			delta = `{"stop_reason":"` + reason + `"}`
		}
		input := `data: {"type":"message_delta","delta":` + delta + `}`
		output := claude.ConvertClaudeResponseToOpenAINonStream(context.Background(), "", nil, nil, []byte(input), nil)
		fixtures = append(fixtures, fixture{Name: "TestConvertClaudeResponseToOpenAINonStreamFinishReasons/" + reason, Client: "openai", Upstream: "claude", Path: "non_stream", Input: input, Output: normalize(output)})
	}
	addResponse("error-and-empty", "claude", "m", []string{`event: ping`, `data: {"type":"ping"}`, `data: {"type":"error","error":{"type":"overloaded_error","message":"busy"}}`, `data: {"type":"content_block_delta","delta":{"type":"text_delta","text":""}}`})
	raw, err := json.MarshalIndent(fixtures, "", "  ")
	if err != nil {
		panic(err)
	}
	if err = os.WriteFile(os.Args[2], append(raw, '\n'), 0644); err != nil {
		panic(err)
	}
	fmt.Printf("wrote %d reference fixtures\n", len(fixtures))
}
