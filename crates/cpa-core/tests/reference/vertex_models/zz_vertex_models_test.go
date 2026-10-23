package cliproxy

import (
	"encoding/json"
	"os"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
)

// Records the models Go v8.0.35 registers for Vertex API-key entries
// (buildVertexCompatConfigModels after the config load's sanitizer).
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
	data, _ := json.MarshalIndent(map[string]any{"yaml": yaml, "models": out}, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
