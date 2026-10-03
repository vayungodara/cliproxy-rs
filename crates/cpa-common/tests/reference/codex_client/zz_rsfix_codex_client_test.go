package multiagentv2

// Vectors for cpa_common::codex_client. Copy into
// internal/client/codex/optimize-multi-agent-v2/ of CLIProxyAPI 6fecc6e and run with
// RSFIX_OUT=<dir>; it writes codex_client_go.json. Pure functions only: no network.

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
)

type rsfixVector struct {
	Fn       string            `json:"fn"`
	Headers  map[string]string `json:"headers,omitempty"`
	In       string            `json:"in,omitempty"`
	Flag     bool              `json:"flag,omitempty"`
	Compat   bool              `json:"compat,omitempty"`
	Models   []map[string]any  `json:"models,omitempty"`
	Markdown string            `json:"markdown,omitempty"`
	Out      string            `json:"out"`
	Bool     bool              `json:"bool,omitempty"`
}

func rsfixHeaders(m map[string]string) http.Header {
	h := http.Header{}
	for k, v := range m {
		h.Set(k, v)
	}
	return h
}

func rsfixLookup(id string) *registry.ModelInfo { return registry.LookupModelInfo(id) }

func TestRSFixCodexClient(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	ctx := context.Background()
	var out []rsfixVector
	codexUA := map[string]string{"User-Agent": "codex_cli_rs/0.150.0 (Linux; x86_64)"}
	spawn := map[string]string{"User-Agent": "codex-tui/0.154.0", "X-Openai-Subagent": "collab_spawn"}

	// Orphan delegation.
	orphanIn := `{"input":[` +
		`{"type":"function_call","call_id":"c1","name":"create_thread","namespace":"codex_app","arguments":"{}"},` +
		`{"type":"function_call_output","call_id":"c1","name":"create_thread","namespace":"codex_app","output":"paired"},` +
		`{"type":"function_call_output","call_id":"c1","name":"create_thread","namespace":"codex_app","output":"second <one>"},` +
		`{"type":"function_call_output","call_id":"c2","name":"send_message_to_thread","namespace":"codex_app","output":{"ok":true, "n": 1.50}},` +
		`{"type":"function_call_output","call_id":"c3","name":"create_thread","namespace":"other","output":"kept"},` +
		`{"type":"function_call_output","name":"send_message_to_thread","namespace":"codex_app"},` +
		`{"type":"function_call_output","call_id":"c4","name":"unknown","namespace":"codex_app","output":"kept"}]}`
	for _, c := range []struct {
		headers map[string]string
		in      string
		enabled bool
	}{
		{spawn, orphanIn, true},
		{spawn, orphanIn, false},
		{codexUA, orphanIn, true},
		{map[string]string{"x-openai-subagent": "COLLAB_SPAWN"}, orphanIn, true},
		{spawn, `{"input":"text"}`, true},
		{spawn, ``, true},
	} {
		got := RewriteCodexOrphanDelegationInput(ctx, rsfixHeaders(c.headers), []byte(c.in), c.enabled)
		out = append(out, rsfixVector{Fn: "orphan", Headers: c.headers, In: c.in, Flag: c.enabled, Out: string(got)})
	}

	// Multi-agent v2 input rewrite (non-Codex targets).
	agentIn := `{"input":[` +
		`{"type":"agent_message","author":"a","recipient":"b","internal_chat_message_metadata_passthrough":{"x":1},"content":[{"type":"encrypted_content","encrypted_content":"secret <x>"},{"type":"input_text","text":"plain"},{"type":"encrypted_content","encrypted_content":5}]},` +
		`{"type":"message","role":"user","author":"u","content":"hi"},` +
		`{"type":"agent_message","content":"not array"}]}`
	for _, c := range []struct {
		headers  map[string]string
		optimize bool
		compat   bool
	}{
		{codexUA, true, false},
		{codexUA, false, false},
		{codexUA, false, true},
		{codexUA, true, true},
		{map[string]string{"User-Agent": "curl/8"}, true, false},
		{map[string]string{"User-Agent": "Codex Desktop/1.0"}, true, false},
	} {
		cfg := &config.Config{}
		cfg.Client.Codex.OptimizeMultiAgentV2 = c.optimize
		got := RewriteCodexMultiAgentV2Input(ctx, rsfixHeaders(c.headers), []byte(agentIn), cfg, c.compat)
		out = append(out, rsfixVector{Fn: "agent_input", Headers: c.headers, In: agentIn, Flag: c.optimize, Compat: c.compat, Out: string(got)})
	}

	// Spawn-agent model list from the embedded catalog and a fixed available set.
	templates, defaultTemplate, _, errLoad := loadCodexCatalogTemplates()
	if errLoad != nil {
		t.Fatal(errLoad)
	}
	available := []map[string]any{
		{"id": "gpt-5.5", "display_name": "ignored"},
		{"id": "claude-sonnet-4-6", "display_name": "Claude Sonnet"},
		{"id": "gemini-2.5-pro"},
		{"id": "custom-model", "display_name": "zz Custom", "description": "From config"},
		{"id": "gpt-5.5"},
		{"id": "Alpha", "display_name": "alpha"},
		{"id": "with`tick"},
	}
	for id := range templates {
		if id == "gpt-6-sol" || id == "gpt-5.4" {
			available = append(available, map[string]any{"id": id})
		}
	}
	models := codexSpawnAgentModelsFromTemplates(available, templates, defaultTemplate, rsfixLookup)
	markdown := formatCodexSpawnAgentModels(models)
	out = append(out, rsfixVector{Fn: "spawn_models", Models: available, Out: markdown})

	// Description rewrite.
	heading := codexSpawnAgentModelsHeading
	for _, desc := range []string{
		"Spawns an agent to work on a task.\nMore text.",
		"Intro line\n  Spawns an agent here.",
		"No marker at all",
		"No marker, trailing newline\n",
		"",
		"Pre\n    " + heading + "\n    - `old`: Old.\n    - `old2`: Old2.\nSpawns an agent now.",
		heading + "\n- `a`: A.\nTail",
	} {
		out = append(out, rsfixVector{Fn: "replace_models", In: desc, Markdown: markdown, Out: replaceCodexSpawnAgentModels(desc, markdown)})
	}
	out = append(out, rsfixVector{Fn: "replace_models", In: "unchanged", Markdown: "", Out: replaceCodexSpawnAgentModels("unchanged", "")})

	// Tool preparation and namespace optimization with a registered model set.
	registry.GetGlobalRegistry().RegisterClient("rsfix-codex-client", "codex", []*registry.ModelInfo{
		{ID: "gpt-5.5", OwnedBy: "openai"},
		{ID: "rsfix-extra", OwnedBy: "openai", DisplayName: "RSFix Extra", Description: "A test model"},
	})
	defer registry.GetGlobalRegistry().UnregisterClient("rsfix-codex-client")
	availableNow := registry.GetGlobalRegistry().GetAvailableModels("openai")
	preparedMarkdown := formatCodexSpawnAgentModels(codexSpawnAgentModelsFromTemplates(availableNow, templates, defaultTemplate, rsfixLookup))
	toolsIn := `{"tools":[` +
		`{"type":"function","name":"spawn_agent","description":"Spawns an agent for subtasks.","parameters":{"type":"object","properties":{"message":{"type":"string","encrypted":true}}}},` +
		`{"type":"namespace","name":"collaboration","tools":[{"type":"function","name":"spawn_agent","description":"Nested. Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}},{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":true,"type":"string"}}}}]},` +
		`{"type":"function","name":"followup_task","parameters":{"properties":{"message":{"type":"string"}}}}],` +
		`"input":[{"type":"additional_tools","tools":[{"type":"function","name":"send_message","parameters":{"properties":{"message":{"encrypted":false}}}}]},` +
		`{"type":"agent_message","content":[{"type":"encrypted_content","encrypted_content":"hidden"}]}]}`
	conflictIn := `{"tools":[{"type":"namespace","name":"collaboration-optimize","tools":[]},{"type":"function","name":"spawn_agent","description":"Spawns an agent.","parameters":{"properties":{"message":{"encrypted":true}}}}]}`
	conflictInput := `{"tools":[{"type":"function","name":"spawn_agent","description":"d"}],"input":[{"type":"additional_tools","tools":[{"type":"namespace","name":"x","tools":[{"type":"function","name":"collaboration-optimize__spawn"}]}]}]}`
	for _, c := range []struct {
		headers map[string]string
		in      string
		enabled bool
	}{
		{codexUA, toolsIn, true},
		{codexUA, toolsIn, false},
		{map[string]string{"User-Agent": "curl/8"}, toolsIn, true},
		{codexUA, conflictIn, true},
		{codexUA, `{"tools":[{"type":"function","name":"other"}]}`, true},
	} {
		got, prepared := PrepareCodexMultiAgentV2Tools(ctx, rsfixHeaders(c.headers), []byte(c.in), c.enabled, false)
		out = append(out, rsfixVector{Fn: "prepare_tools", Headers: c.headers, In: c.in, Flag: c.enabled, Markdown: preparedMarkdown, Out: string(got), Bool: prepared})

		cfg := &config.Config{}
		cfg.Client.Codex.OptimizeMultiAgentV2 = c.enabled
		optimized, renamed := OptimizeCodexMultiAgentV2Request(ctx, rsfixHeaders(c.headers), []byte(c.in), cfg)
		out = append(out, rsfixVector{Fn: "optimize", Headers: c.headers, In: c.in, Flag: c.enabled, Markdown: preparedMarkdown, Out: string(optimized), Bool: renamed})
	}
	for _, in := range []string{toolsIn, conflictIn, conflictInput, `{"tools":[{"type":"function","name":"collaboration-optimize.x"}]}`, `{}`} {
		out = append(out, rsfixVector{Fn: "conflict", In: in, Bool: HasCodexMultiAgentV2NamespaceConflict([]byte(in))})
	}

	// Response restoration.
	for _, in := range []string{
		`{"type":"response.output_item.done","item":{"type":"function_call","namespace":"collaboration-optimize","name":"spawn_agent","arguments":"{\"namespace\":\"collaboration-optimize\"}","call_id":"c"}}`,
		`{"type":"response.completed","response":{"output":[{"type":"function_call","name":"collaboration-optimize.send_message","arguments":"{}"},{"type":"custom_tool_call","name":"collaboration-optimize__followup","input":"x"},{"type":"namespace","name":"collaboration-optimize"},{"type":"function_call_output","output":{"name":"collaboration-optimize.x"}}],"n":1.50e2,"big":12345678901234567890,"html":"<a>&"}}`,
		`{"type":"function_call","name":"collaboration-optimize.","namespace":"other"}`,
		`{"type":"message","name":"collaboration-optimize"}`,
		`[1,2]`,
		`not json`,
	} {
		out = append(out, rsfixVector{Fn: "restore", In: in, Flag: true, Out: string(RestoreCodexMultiAgentV2Response([]byte(in), true))})
	}
	out = append(out, rsfixVector{Fn: "restore", In: `{"type":"function_call","namespace":"collaboration-optimize"}`, Flag: false, Out: string(RestoreCodexMultiAgentV2Response([]byte(`{"type":"function_call","namespace":"collaboration-optimize"}`), false))})

	for _, ua := range []string{"codex_cli_rs", "codex_cli_rs/1", "codex_cli_rsx", "codex_exec/2", "Codex Desktop/3", "codex-tui/4", " codex-tui/5 ", "Codex-TUI/6", ""} {
		out = append(out, rsfixVector{Fn: "client_ua", In: ua, Bool: IsCodexClientUserAgent(ua)})
	}

	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_client_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
