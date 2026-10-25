package main

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"

	clauderesponses "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/responses"
	"github.com/tidwall/sjson"
)

// toolSearchFixtures re-records every openai-response:claude pair fixture with
// upstream.claude.tool-search on and writes ../claude_tool_search.json: the fixtures
// whose output then differs from the recorded (switch off) one, plus every fixture
// mined from claude_openai-responses_tool_search_test.go. The rest are identical in
// both modes, and the Rust test replays them from the pair file with the switch on.
func toolSearchFixtures(root, pairs string) {
	var r *registration
	for _, reg := range registrations(root) {
		if reg.client == "openai-response" && reg.upstream == "claude" {
			reg := reg
			r = &reg
		}
	}
	src, err := os.ReadFile(filepath.Join(root, "internal/translator/claude/openai/responses/claude_openai-responses_tool_search_test.go"))
	if err != nil {
		panic(err)
	}
	tests := map[string]bool{}
	for _, m := range regexp.MustCompile(`(?m)^func (Test\w+)`).FindAllStringSubmatch(string(src), -1) {
		tests[m[1]] = true
	}
	raw, err := os.ReadFile(filepath.Join(pairs, "openai-response-claude.json"))
	if err != nil {
		panic(err)
	}
	var doc struct {
		Fixtures []fixture `json:"fixtures"`
	}
	if err := json.Unmarshal(raw, &doc); err != nil {
		panic(err)
	}
	clauderesponses.SetClientToolSearchEnabled(true)
	var out []fixture
	differ := 0
	for _, off := range doc.Fixtures {
		if off.Bytes {
			// Byte-encoded inputs carry no tool_search items; recording them needs a decode.
			continue
		}
		f := off
		f.Outputs, f.Dynamic, f.Variants, f.StreamVariants, f.ToolError, f.RequestError = nil, nil, nil, nil, false, ""
		on := record(*r, f)
		// Mask the dynamic values of both recordings: a Go map-order value that one
		// recording happened not to flag is not a tool_search difference.
		dyn := append(append([]dynamic{}, on.Dynamic...), off.Dynamic...)
		changed := on.ToolError != off.ToolError || on.RequestError != off.RequestError ||
			len(on.Variants) != len(off.Variants) || masked(on.Outputs, dyn) != masked(off.Outputs, dyn)
		if changed {
			differ++
		}
		if changed || tests[strings.SplitN(off.Name, ":", 2)[0]] {
			out = append(out, encode(on))
		}
	}
	result, err := json.MarshalIndent(map[string]any{"client": r.client, "upstream": r.upstream, "tool_search": true, "fixtures": out}, "", " ")
	if err != nil {
		panic(err)
	}
	if err := os.WriteFile(filepath.Join(pairs, "..", "claude_tool_search.json"), append(result, '\n'), 0o644); err != nil {
		panic(err)
	}
	fmt.Printf("tool_search: %d of %d fixtures differ with the switch on, %d written\n", differ, len(doc.Fixtures), len(out))
}

// masked flattens outputs with every dynamic value replaced by a placeholder.
func masked(outputs [][]string, dyn []dynamic) string {
	var flat []string
	for _, o := range outputs {
		flat = append(flat, o...)
	}
	for _, d := range dyn {
		if d.Out >= len(flat) {
			return "missing output"
		}
		if d.Data < 0 {
			flat[d.Out], _ = sjson.Set(flat[d.Out], d.Path, "<dyn>")
			continue
		}
		lines := strings.Split(flat[d.Out], "\n")
		if d.Data < len(lines) && strings.HasPrefix(lines[d.Data], "data:") {
			payload, _ := sjson.Set(strings.TrimSpace(lines[d.Data][5:]), d.Path, "<dyn>")
			lines[d.Data] = "data: " + payload
		}
		flat[d.Out] = strings.Join(lines, "\n")
	}
	raw, _ := json.Marshal(flat)
	return string(raw)
}
