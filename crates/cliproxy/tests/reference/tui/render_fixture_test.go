// Records what Go's TUI models render for fixed inputs, for the Rust TUI golden test.
// Overlaid as internal/tui/zz_render_fixture_test.go in CLIProxyAPI 6fecc6e (see
// README.md). Messages are fed straight into the models: no command runs, so nothing
// is fetched. lipgloss renders without colour, so the output is the text layout.
package tui

import (
	"encoding/json"
	"errors"
	"os"
	"strings"
	"testing"

	tea "github.com/charmbracelet/bubbletea"
	"github.com/charmbracelet/lipgloss"
	"github.com/muesli/termenv"
)

type renderCase struct {
	Name   string         `json:"name"`
	Tab    string         `json:"tab"`
	Width  int            `json:"width"`
	Locale string         `json:"locale"`
	Input  map[string]any `json:"input"`
	Lines  []string       `json:"lines"`
}

func keyMsg(s string) tea.KeyMsg {
	switch s {
	case "down":
		return tea.KeyMsg{Type: tea.KeyDown}
	case "enter":
		return tea.KeyMsg{Type: tea.KeyEnter}
	}
	return tea.KeyMsg{Type: tea.KeyRunes, Runes: []rune(s)}
}

func lines(s string) []string {
	out := strings.Split(s, "\n")
	for i := range out {
		out[i] = strings.TrimRight(out[i], " ")
	}
	for len(out) > 0 && out[len(out)-1] == "" {
		out = out[:len(out)-1]
	}
	return out
}

func list(v any) []map[string]any {
	raw, _ := json.Marshal(v)
	var out []map[string]any
	_ = json.Unmarshal(raw, &out)
	return out
}

func strs(v any) []string {
	raw, _ := json.Marshal(v)
	var out []string
	_ = json.Unmarshal(raw, &out)
	return out
}

func num(input map[string]any, key string) int {
	f, _ := input[key].(float64)
	return int(f)
}

func render(c renderCase) []string {
	SetLocale(c.Locale)
	defer SetLocale("en")
	client := NewClientWithBaseURL("http://127.0.0.1:8317", "")
	in := c.Input
	switch c.Tab {
	case "dashboard":
		m := newDashboardModel(client)
		m.SetSize(c.Width, 1000)
		cfg, _ := in["config"].(map[string]any)
		m, _ = m.Update(dashboardDataMsg{config: cfg, authFiles: list(in["files"]), apiKeys: strs(in["keys"])})
		if e, ok := in["error"].(string); ok {
			m, _ = m.Update(dashboardDataMsg{err: errors.New(e)})
		}
		return lines(m.content)
	case "config":
		m := newConfigTabModel(client)
		m.SetSize(c.Width, 1000)
		cfg, _ := in["config"].(map[string]any)
		m, _ = m.Update(configDataMsg{config: cfg})
		for i := 0; i < num(in, "down"); i++ {
			m, _ = m.handleNormalKey(keyMsg("down"))
		}
		if v, ok := in["update_error"].(string); ok {
			m, _ = m.Update(configUpdateMsg{path: "debug", err: errors.New(v)})
		}
		if _, ok := in["update_ok"]; ok {
			m, _ = m.Update(configUpdateMsg{path: "debug", value: true})
		}
		if v, ok := in["edit"].(string); ok {
			m, _ = m.handleNormalKey(keyMsg("enter"))
			m.textInput.SetValue(v)
		}
		return lines(m.renderContent())
	case "auth":
		m := newAuthTabModel(client)
		m.SetSize(c.Width, 1000)
		m, _ = m.Update(authFilesMsg{files: list(in["files"])})
		for i := 0; i < num(in, "down"); i++ {
			m, _ = m.handleNormalInput(keyMsg("down"))
		}
		if _, ok := in["expand"]; ok {
			m, _ = m.handleNormalInput(keyMsg("enter"))
		}
		if _, ok := in["confirm"]; ok {
			m, _ = m.handleNormalInput(keyMsg("d"))
		}
		if v, ok := in["action"].(string); ok {
			m, _ = m.Update(authActionMsg{action: v})
		}
		if v, ok := in["edit"].(string); ok {
			m, _ = m.handleNormalInput(keyMsg(v))
		}
		return lines(m.renderContent())
	case "keys":
		m := newKeysTabModel(client)
		m.SetSize(c.Width, 1000)
		m, _ = m.Update(keysDataMsg{
			apiKeys: strs(in["keys"]), gemini: list(in["gemini-api-key"]), claude: list(in["claude-api-key"]),
			vertex: list(in["vertex-api-key"]), openai: list(in["openai-compatibility"]),
		})
		for i := 0; i < num(in, "down"); i++ {
			m, _ = m.Update(keyMsg("down"))
		}
		if _, ok := in["confirm"]; ok {
			m, _ = m.Update(keyMsg("d"))
		}
		if _, ok := in["add"]; ok {
			m, _ = m.Update(keyMsg("a"))
		}
		if v, ok := in["error"].(string); ok {
			m, _ = m.Update(keyActionMsg{err: errors.New(v)})
		}
		return lines(m.renderContent())
	case "oauth":
		m := newOAuthTabModel(client)
		m.SetSize(c.Width, 1000)
		for i := 0; i < num(in, "down"); i++ {
			m, _ = m.Update(keyMsg("down"))
		}
		if v, ok := in["start"].(map[string]any); ok {
			m.pollGeneration = 1
			m, _ = m.Update(oauthStartMsg{
				url: v["url"].(string), state: "st", providerName: v["provider"].(string),
				userCode: v["user_code"].(string), deviceFlow: v["device"].(bool),
				expiresIn: num(v, "expires_in"), generation: 1,
			})
		}
		if v, ok := in["poll_error"].(string); ok {
			m, _ = m.Update(oauthPollMsg{state: "st", generation: 1, err: errors.New(v)})
		}
		return lines(m.renderContent())
	case "logs":
		m := newLogsTabModel(client, nil)
		m.SetSize(c.Width, 1000)
		m, _ = m.Update(logsPollMsg{lines: strs(in["lines"]), latest: 5})
		if v, ok := in["filter"].(string); ok {
			m, _ = m.Update(keyMsg(v))
		}
		if _, ok := in["pause"]; ok {
			m, _ = m.Update(keyMsg("a"))
		}
		return lines(m.renderLogs())
	case "gate":
		app := NewAppWithBaseURL("http://127.0.0.1:8317", in["password"].(string), nil)
		if v, ok := in["error"].(string); ok {
			app.authError = v
		}
		return lines(app.renderAuthView())
	case "bars":
		app := NewAppWithBaseURL("http://127.0.0.1:8317", "", NewLogHook(1))
		app.width = c.Width
		app.activeTab = num(in, "active")
		if v, ok := in["logs"].(bool); ok && !v {
			app.logsEnabled = false
			app.refreshTabs()
		}
		return []string{strings.TrimRight(app.renderTabBar(), " "), strings.TrimRight(app.renderStatusBar(), " ")}
	}
	panic("unknown tab " + c.Tab)
}

func TestZZRenderFixture(t *testing.T) {
	out := os.Getenv("CPA_FIXTURE_OUT")
	if out == "" {
		t.Skip("CPA_FIXTURE_OUT not set")
	}
	lipgloss.SetColorProfile(termenv.Ascii)
	var cases []renderCase
	data, err := os.ReadFile(os.Getenv("CPA_FIXTURE_IN"))
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(data, &cases); err != nil {
		t.Fatal(err)
	}
	for i := range cases {
		cases[i].Lines = render(cases[i])
	}
	data, _ = json.MarshalIndent(map[string]any{"cases": cases}, "", " ")
	if err := os.WriteFile(out, append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
