package executor

// Vectors for the Codex executor's CountTokens. Copy into internal/runtime/executor/ of
// CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>; it writes codex_tokens_go.json.
// CountTokens is local (tiktoken); nothing reaches the network.

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	cliproxyexecutor "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/executor"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type rsfixTokensVector struct {
	Model    string            `json:"model"`
	From     string            `json:"from"`
	Response string            `json:"response"`
	Headers  map[string]string `json:"headers,omitempty"`
	In       string            `json:"in"`
	Out      string            `json:"out"`
	Err      string            `json:"err,omitempty"`
}

func TestRSFixCodexTokens(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	responses := `{"model":"gpt-5.5","instructions":"  Be brief.  ","input":[` +
		`{"type":"message","role":"user","content":[{"type":"input_text","text":"Count these tokens, please."},{"type":"input_image","image_url":"data:x"}]},` +
		`{"type":"function_call","name":"lookup","arguments":"{\"q\":\"weather in Paris\"}","call_id":"c1"},` +
		`{"type":"function_call_output","call_id":"c1","output":"Sunny, 21C"},` +
		`{"type":"reasoning","text":"internal"}],` +
		`"tools":[{"type":"function","name":"lookup","description":"Look things up.","parameters":{"type":"object","properties":{"q":{"type":"string"}}}}],` +
		`"text":{"format":{"type":"json_schema","name":"answer","schema":{"type":"object"}}}}`
	claude := `{"model":"claude-x","max_tokens":64,"system":"You are terse.","messages":[{"role":"user","content":"Hello there, how many tokens?"},` +
		`{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"calc","input":{"x":1}}]},` +
		`{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"2"}]}],` +
		`"tools":[{"name":"calc","description":"Adds.","input_schema":{"type":"object","properties":{"x":{"type":"integer"}}}}]}`
	chat := `{"model":"m","messages":[{"role":"system","content":"sys prompt"},{"role":"user","content":"What is 2+2?"}],` +
		`"tools":[{"type":"function","function":{"name":"add","description":"Add numbers","parameters":{"type":"object","properties":{"a":{"type":"integer"}}}}}]}`

	codexUA := map[string]string{"User-Agent": "codex_cli_rs/0.150.0 (Linux; x86_64)"}
	var out []rsfixTokensVector
	for _, c := range []struct {
		from     sdktranslator.Format
		response sdktranslator.Format
		in       string
		headers  map[string]string
	}{
		{sdktranslator.FormatOpenAIResponse, sdktranslator.FormatOpenAIResponse, responses, nil},
		{sdktranslator.FormatOpenAIResponse, sdktranslator.FormatOpenAIResponse, responses, codexUA},
		{sdktranslator.FormatCodex, sdktranslator.FormatCodex, responses, nil},
		{sdktranslator.FormatClaude, sdktranslator.FormatClaude, claude, nil},
		{sdktranslator.FormatOpenAI, sdktranslator.FormatOpenAI, chat, nil},
		{sdktranslator.FormatOpenAIResponse, sdktranslator.FormatOpenAIResponse, `{"input":[]}`, nil},
	} {
		for _, model := range []string{"gpt-5.5", "gpt-5.5(high)", "gpt-4", "gpt-4o-mini", "o3"} {
			exec := NewCodexExecutor(&config.Config{})
			headers := http.Header{}
			for k, v := range c.headers {
				headers.Set(k, v)
			}
			resp, err := exec.CountTokens(context.Background(), nil, cliproxyexecutor.Request{Model: model, Payload: []byte(c.in)},
				cliproxyexecutor.Options{SourceFormat: c.from, ResponseFormat: c.response, Headers: headers, OriginalRequest: []byte(c.in)})
			v := rsfixTokensVector{Model: model, From: c.from.String(), Response: c.response.String(), Headers: c.headers, In: c.in, Out: string(resp.Payload)}
			if err != nil {
				v.Err = err.Error()
			}
			out = append(out, v)
		}
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_tokens_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
