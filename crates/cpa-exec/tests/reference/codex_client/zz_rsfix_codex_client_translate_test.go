package helps

// Vectors for cpa_exec::codex_client. Copy into internal/runtime/executor/helps/ of
// CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>; it writes
// codex_client_translate_go.json. Pure functions only: no network.

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	_ "github.com/router-for-me/CLIProxyAPI/v8/internal/translator"
	sdktranslator "github.com/router-for-me/CLIProxyAPI/v8/sdk/translator"
)

type rsfixClientVector struct {
	Fn       string            `json:"fn"`
	From     string            `json:"from,omitempty"`
	To       string            `json:"to,omitempty"`
	Target   string            `json:"target,omitempty"`
	Compat   bool              `json:"compat,omitempty"`
	Optimize bool              `json:"optimize,omitempty"`
	Orphan   bool              `json:"orphan,omitempty"`
	Headers  map[string]string `json:"headers,omitempty"`
	In       string            `json:"in"`
	Out      string            `json:"out"`
	Bool     bool              `json:"bool,omitempty"`
}

func rsfixClientHeaders(m map[string]string) http.Header {
	h := http.Header{}
	for k, v := range m {
		h.Set(k, v)
	}
	return h
}

func TestRSFixCodexClientTranslate(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	ctx := context.Background()
	var out []rsfixClientVector

	respIn := `{"model":"gpt-5.5","instructions":"sys","input":[` +
		`{"type":"agent_message","author":"a","recipient":"b","content":[{"type":"encrypted_content","encrypted_content":"hello agent"}]},` +
		`{"type":"function_call_output","call_id":"orphan","name":"create_thread","namespace":"codex_app","output":"thread ok"},` +
		`{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},` +
		`{"type":"reasoning","summary":[],"content":[{"type":"reasoning_text","text":"thinking"}]}],` +
		`"tools":[{"type":"function","name":"f","parameters":{"type":"object","properties":{"n":{"type":"integer"}}}}],` +
		`"reasoning":{"effort":"high","summary":"auto"}}`
	claudeIn := `{"model":"claude-x","max_tokens":100,"messages":[{"role":"user","content":"hi"},` +
		`{"role":"assistant","content":[{"type":"thinking","thinking":"plan","signature":""},{"type":"text","text":"ok"}]},` +
		`{"role":"user","content":"go"}],"thinking":{"type":"enabled","budget_tokens":1024},` +
		`"tools":[{"name":"t","input_schema":{"type":"object","properties":{"n":{"type":"integer"}}}}]}`
	openaiIn := `{"model":"m","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"ok","reasoning_content":"plan"},{"role":"user","content":"go"}],` +
		`"tools":[{"type":"function","function":{"name":"t","parameters":{"type":"object","properties":{"n":{"type":"integer"}}}}}]}`

	codexHeaders := map[string]string{"User-Agent": "codex_cli_rs/0.150.0 (Linux; x86_64)", "X-Openai-Subagent": "collab_spawn"}
	plainHeaders := map[string]string{"User-Agent": "curl/8"}
	cfg := &config.Config{}
	cfg.Client.Codex.OptimizeMultiAgentV2 = true
	cfg.Codex.OrphanDelegationCompatibility = true

	cases := []struct {
		from sdktranslator.Format
		in   string
		to   []sdktranslator.Format
	}{
		{sdktranslator.FormatOpenAIResponse, respIn, []sdktranslator.Format{sdktranslator.FormatClaude, sdktranslator.FormatGemini, sdktranslator.FormatCodex, sdktranslator.FormatOpenAI}},
		{sdktranslator.FormatClaude, claudeIn, []sdktranslator.Format{sdktranslator.FormatCodex, sdktranslator.FormatGemini, sdktranslator.FormatInteractions, sdktranslator.FormatOpenAI}},
		{sdktranslator.FormatOpenAI, openaiIn, []sdktranslator.Format{sdktranslator.FormatClaude, sdktranslator.FormatCodex}},
	}
	for _, c := range cases {
		for _, to := range c.to {
			for _, compat := range []bool{false, true} {
				for _, target := range []string{"", "codex"} {
					for _, headers := range []map[string]string{codexHeaders, plainHeaders} {
						got := TranslateRequestWithAPIKeyModelCompatibilityForExecutor(ctx, rsfixClientHeaders(headers), cfg, target, c.from, to, "test-model", []byte(c.in), true, compat)
						out = append(out, rsfixClientVector{Fn: "translate", From: c.from.String(), To: to.String(), Target: target, Compat: compat, Optimize: true, Orphan: true, Headers: headers, In: c.in, Out: string(got)})
					}
				}
			}
		}
	}

	optimizeIn := `{"model":"gpt-5.5","input":[` +
		`{"type":"agent_message","author":"a","content":[{"type":"encrypted_content","encrypted_content":"hello agent"}]},` +
		`{"type":"function_call_output","call_id":"orphan","name":"send_message_to_thread","namespace":"codex_app","output":{"ok":true}}],` +
		`"tools":[{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"type":"string","encrypted":true}}}}]}]}`
	for _, optimize := range []bool{false, true} {
		for _, orphan := range []bool{false, true} {
			for _, compat := range []bool{false, true} {
				c := &config.Config{}
				c.Client.Codex.OptimizeMultiAgentV2 = optimize
				c.Codex.OrphanDelegationCompatibility = orphan
				got, renamed := OptimizeCodexMultiAgentV2RequestForAuth(ctx, rsfixClientHeaders(codexHeaders), []byte(optimizeIn), c, nil, compat)
				out = append(out, rsfixClientVector{Fn: "optimize_auth", Compat: compat, Optimize: optimize, Orphan: orphan, Headers: codexHeaders, In: optimizeIn, Out: string(got), Bool: renamed})
			}
		}
	}

	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_client_translate_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
