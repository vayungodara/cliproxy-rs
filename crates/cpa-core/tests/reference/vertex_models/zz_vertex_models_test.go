package cliproxy

import (
	"encoding/json"
	"os"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

// Records the models Go v8.0.35 registers for Vertex API-key entries
// (buildVertexCompatConfigModels after the config load's sanitizer), and the upstream
// model each request model executes as (executionModelCandidatesWithAlias, through
// zz_vertex_models_export.go) for credentials synthesized from config.
func TestZZVertexConfigModels(t *testing.T) {
	yaml := "api-keys:\n  vertex:\n    - base-url: https://v.example.invalid\n      models: [{name: v-only}, {name: v1, alias: va}, {name: \" v2 \", alias: \"  \"}, {name: \" \", alias: ghost}, {name: V-ONLY, alias: \"\"}, {name: v3, alias: v-only}]\n      keys: [{api-key: vk}]\n"
	cfg, err := config.ParseConfigBytes([]byte(yaml))
	if err != nil {
		t.Fatal(err)
	}
	type model struct {
		ID              string `json:"id"`
		MetadataModelID string `json:"metadata_model_id"`
		DisplayName     string `json:"display_name"`
		OwnedBy         string `json:"owned_by"`
		Type            string `json:"type"`
	}
	var out []model
	for _, m := range buildVertexCompatConfigModels(&cfg.VertexCompatAPIKey[0]) {
		out = append(out, model{m.ID, m.MetadataModelID, m.DisplayName, m.OwnedBy, m.Type})
	}
	data, _ := json.MarshalIndent(map[string]any{"yaml": yaml, "models": out, "execution": recordVertexExecution(t)}, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

func recordVertexExecution(t *testing.T) map[string]any {
	// The Kelvin sign in "\u212a1" folds to "k1" under both EqualFold and ToLower.
	yaml := "api-keys:\n  vertex:\n    - base-url: https://v.example.invalid\n      models: [{name: v-only}, {name: v1, alias: va}, {name: \" v2 \", alias: \"  \"}, {name: \" \", alias: ghost}, {name: V-ONLY, alias: \"\"}, {name: v3, alias: v-only}, {name: k-real, alias: k1}, {name: \"pro(high)\", alias: pro}]\n      keys: [{api-key: vk}]\n    - base-url: https://v.example.invalid\n      models: [{name: t1, alias: va}]\n      keys: [{api-key: tk, prefix: team}]\n"
	requests := map[string][]string{
		"vk": {"va", "VA", "v1", "va(high)", "v-only", "V-ONLY", "v2", "V2(8192)", "ghost", "missing", " va ", " missing ", "\u212a1", "pro", "pro(low)", "team/va"},
		"tk": {"team/va", "va", "team/missing", "TEAM/va", "team/v1"},
	}
	cfg, err := config.ParseConfigBytes([]byte(yaml))
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
	for _, auth := range auths {
		if _, err := manager.Register(t.Context(), auth); err != nil {
			t.Fatal(err)
		}
	}
	manager.RefreshAPIKeyModelAlias()
	type execCase struct {
		APIKey   string   `json:"api_key"`
		Request  string   `json:"request"`
		Upstream []string `json:"upstream"`
	}
	var cases []execCase
	for _, auth := range auths {
		key := auth.Attributes["api_key"]
		for _, request := range requests[key] {
			cases = append(cases, execCase{key, request, manager.ZZExecutionModels(auth, request)})
		}
	}
	return map[string]any{"yaml": yaml, "cases": cases}
}
