package cliproxy

import (
	"encoding/json"
	"os"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

// Records the models Go v8.0.35 registers for config entries whose model names and
// aliases are plain YAML scalars that are not strings (numbers, booleans): yaml.v3
// reads each into the string field as its text. Aliases, merges and the legacy
// layout included. The upstream model comes through ../vertex_models's export overlay.
func TestZZScalarModelNames(t *testing.T) {
	yaml := `api-keys:
  claude:
    - models: [{name: 123, alias: 4.0}, {name: 0x1F, alias: c1}, {name: c2, alias: 007}, {name: &n 1_000, alias: c3}, {name: *n, alias: c4}]
      keys: [{api-key: ck}]
  vertex:
    - base-url: https://v.example.invalid
      models: [{name: 1.50, alias: true}, {name: 1e3}, {name: ~, alias: v1}, {name: .inf, alias: v2}, &m {name: -0, alias: v3}, {<<: *m, alias: v4}]
      keys: [{api-key: vk}]
  openai-compatibility:
    - name: acme
      base-url: http://127.0.0.1:1
      keys: [{api-key: ok}]
      models: [{name: .5, alias: +12}, {name: 0o17}, {name: yes, alias: o1}, {name: 2001-12-14, alias: o2}]
`
	legacy := "codex-api-key:\n  - api-key: lk\n    base-url: http://127.0.0.1:2\n    models: [{name: 2.50, alias: False}]\n"
	// Exclusions, OAuth model aliases and tagged scalars, which yaml.v3 also reads as
	// their text; an anchor in a header value aliased as a model name, anchors outside
	// any such field aliased as exclusions, and OAuth channels named like payload keys.
	lists := `x: &l [2.50]
api-keys:
  claude:
    - headers: {x-v: &h 0x2A}
      models: [{name: claude-x, alias: 2.50}, {name: claude-y, alias: y}, {name: !!float 1.50, alias: !!int 0x1F}, {name: !!bool True, alias: t1}, {name: *h, alias: h1}]
      excluded-models: &ex [2.50, 1e3]
      keys: [{api-key: ek}]
    - models: [{name: claude-q, alias: 1e3}, {name: claude-r, alias: r}]
      excluded-models: *ex
      keys: [{api-key: ek2}]
    - models: [{name: claude-s, alias: 2.50}, {name: claude-t, alias: t}]
      excluded-models: *l
      keys: [{api-key: ek3}]
oauth:
  model-alias:
    claude: [{name: 1.50, alias: 007}, {name: claude-z, alias: !!bool true}]
    codex: [{name: c, alias: &a 0x10}, {name: d, alias: *a}]
    params: [{name: 1.50, alias: 007}]
  excluded-models:
    claude: [2.50, !!int 0x1F, false]
    match: [2.50]
`
	legacyLists := `claude-api-key:
  - api-key: lk2
    models: [{name: a, alias: 2.50}, {name: b, alias: bb}]
    excluded-models: [2.50]
oauth-model-alias:
  codex: [{name: 1.50, alias: 1e3}]
m: &m {codex: [0x1F]}
oauth-excluded-models: *m
`
	out := map[string]any{}
	for name, text := range map[string]string{"yaml": yaml, "legacy": legacy, "lists": lists, "legacy-lists": legacyLists} {
		cfg, err := config.ParseConfigBytes([]byte(text))
		if err != nil {
			t.Fatal(err)
		}
		auths, err := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
			Config: cfg, Now: time.Unix(1, 0), IDGenerator: synthesizer.NewStableIDGenerator(),
		})
		if err != nil {
			t.Fatal(err)
		}
		manager := coreauth.NewManager(nil, nil, nil)
		manager.SetConfig(cfg)
		service := &Service{cfg: cfg, coreManager: manager}
		// Each registered model and the upstream model it executes as.
		models := map[string][][2]string{}
		for _, auth := range auths {
			if _, err := manager.Register(t.Context(), auth); err != nil {
				t.Fatal(err)
			}
			service.registerModelsForAuth(t.Context(), auth)
			for _, m := range registry.GetGlobalRegistry().GetModelsForClient(auth.ID) {
				key := auth.Attributes["api_key"]
				models[key] = append(models[key], [2]string{m.ID, manager.ZZExecutionModels(auth, m.ID)[0]})
			}
			GlobalModelRegistry().UnregisterClient(auth.ID)
		}
		section := map[string]any{"yaml": text, "models": models}
		if len(cfg.OAuthExcludedModels) > 0 {
			section["oauth-excluded-models"] = cfg.OAuthExcludedModels
		}
		if len(cfg.OAuthModelAlias) > 0 {
			aliases := map[string][][2]string{}
			for channel, entries := range cfg.OAuthModelAlias {
				for _, e := range entries {
					aliases[channel] = append(aliases[channel], [2]string{e.Name, e.Alias})
				}
			}
			section["oauth-model-alias"] = aliases
		}
		out[name] = section
	}
	data, _ := json.MarshalIndent(out, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
