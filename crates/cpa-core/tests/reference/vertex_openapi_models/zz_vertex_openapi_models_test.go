package cliproxy

import (
	"encoding/json"
	"os"
	"sort"
	"testing"
	"time"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

// Records, for Go v8.0.35 (d9451cb6), what auth files register: the file synthesizer's
// model_aliases (now also from "models") and openapi_models attributes, the models
// registerModelsForAuth lists (Vertex files also get their partner models), the
// upstream model each request executes as, and AuthKind / AccountInfo for metadata
// shapes (service_account, siwc).
func TestZZVertexOpenAPIModels(t *testing.T) {
	yaml := "oauth-model-alias:\n  vertex:\n    - {name: deepseek-ai/deepseek-v4.1-flash-maas, alias: deepseek-flash}\n    - {name: gemini-2.5-pro, alias: g-pro}\n  claude:\n    - {name: claude-sonnet-4-5, alias: global-sonnet}\n"
	cfg, err := config.ParseConfigBytes([]byte(yaml))
	if err != nil {
		t.Fatal(err)
	}
	files := []struct {
		Name     string   `json:"name"`
		Body     string   `json:"body"`
		Requests []string `json:"requests"`
	}{
		{"openapi_list", `{"type":"vertex","project_id":"p","location":"global","openapi_models":["qwen/qwen3-coder-480b-a35b-instruct-maas"]}`, []string{"deepseek-flash", "qwen/qwen3-coder-480b-a35b-instruct-maas", "g-pro"}},
		{"models_aliases", `{"type":"vertex","project_id":"p","models":[{"name":"deepseek-ai/deepseek-v4.1-flash-maas","alias":"ds-maas"},{"name":"xai/grok-4-maas"}],"service_account":{"client_email":"sa@test.iam.gserviceaccount.com"}}`, []string{"ds-maas", "xai/grok-4-maas", "deepseek-flash"}},
		{"openapi_string_dedup", `{"type":"vertex","project_id":"p","openapi-models":" meta/llama-4 , Meta/Llama-4,,deepseek-ai/x ","models":["mistralai/mistral-small-2503",{"name":"META/LLAMA-4"},{"alias":"no-name"},7]}`, []string{"meta/llama-4", "Meta/Llama-4", "mistralai/mistral-small-2503"}},
		{"static_and_thinking", `{"type":"vertex","project_id":"p","openapi_models":["gemini-2.5-pro","GEMINI-2.5-FLASH","claude-sonnet-4-5","gemini-2.5-flash(high)","publishers/meta/models/llama-4"]}`, []string{"claude-sonnet-4-5", "gemini-2.5-flash(high)"}},
		{"excluded", `{"type":"vertex","project_id":"p","openapi_models":"meta/llama-4,meta/llama-3","excluded_models":["meta/llama-3*"]}`, []string{"meta/llama-4", "meta/llama-3"}},
		{"both_alias_keys", `{"type":"vertex","project_id":"p","model_aliases":[{"name":"meta/llama-4","alias":"ll"}],"models":[{"name":"meta/llama-3","alias":"LL"},{"name":"qwen/q","alias":"qq","fork":true}]}`, []string{"ll", "LL", "qq", "qwen/q", "meta/llama-3"}},
		{"bad_models_array", `{"type":"vertex","project_id":"p","models":[{"name":"meta/a","alias":"aa"},{"name":5,"alias":"bb"}]}`, []string{"aa", "meta/a"}},
		{"case_insensitive_keys", `{"type":"vertex","project_id":"p","models":[{"Name":"meta/a","ALIAS":"x1"},null,{"name":"meta/b","alias":"x2","Alias":"x3","fork":null},{"name":"meta/c","alias":"x4","Display-Name":" C ","FORCE-MAPPING":true}]}`, []string{"x1", "x2", "x3", "x4"}},
		{"claude_file_models", `{"type":"claude","access_token":"t","email":"c@x.test","models":[{"name":"claude-sonnet-4-5","alias":"sonnet"}],"openapi_models":["x/y"]}`, []string{"sonnet", "global-sonnet", "x/y"}},
	}
	type model struct {
		ID              string   `json:"id"`
		MetadataModelID string   `json:"metadata_model_id"`
		DisplayName     string   `json:"display_name"`
		OwnedBy         string   `json:"owned_by"`
		Type            string   `json:"type"`
		UserDefined     bool     `json:"user_defined"`
		Levels          []string `json:"levels,omitempty"`
		Min             int      `json:"min,omitempty"`
		Max             int      `json:"max,omitempty"`
		Zero            bool     `json:"zero,omitempty"`
		Dynamic         bool     `json:"dynamic,omitempty"`
		Thinking        bool     `json:"thinking"`
	}
	type exec struct {
		Request  string `json:"request"`
		Upstream string `json:"upstream"`
		Supports bool   `json:"supports"`
	}
	type fileOut struct {
		Name       string            `json:"name"`
		Body       string            `json:"body"`
		Attributes map[string]string `json:"attributes"`
		Models     []model           `json:"models"`
		Exec       []exec            `json:"exec"`
	}
	var outFiles []fileOut
	for _, f := range files {
		auths, err := synthesizer.SynthesizeAuthFile(&synthesizer.SynthesisContext{Config: cfg, Now: time.Unix(1, 0)}, "/auths/"+f.Name+".json", []byte(f.Body))
		if err != nil || len(auths) != 1 {
			t.Fatalf("%s: %v %d", f.Name, err, len(auths))
		}
		manager := coreauth.NewManager(nil, nil, nil)
		manager.SetConfig(cfg)
		manager.SetOAuthModelAlias(cfg.OAuthModelAlias)
		service := &Service{cfg: cfg, coreManager: manager}
		registered, err := manager.Register(t.Context(), auths[0])
		if err != nil {
			t.Fatal(err)
		}
		service.registerModelsForAuth(t.Context(), registered)
		attrs := map[string]string{}
		for _, k := range []string{"model_aliases", "openapi_models", "auth_kind", "excluded_models"} {
			if v, ok := registered.Attributes[k]; ok {
				attrs[k] = v
			}
		}
		var models []model
		for _, m := range GlobalModelRegistry().GetModelsForClient(registered.ID) {
			mm := model{ID: m.ID, MetadataModelID: m.MetadataModelID, DisplayName: m.DisplayName, OwnedBy: m.OwnedBy, Type: m.Type, UserDefined: m.UserDefined}
			if m.Thinking != nil {
				mm.Thinking = true
				mm.Levels, mm.Min, mm.Max, mm.Zero, mm.Dynamic = m.Thinking.Levels, m.Thinking.Min, m.Thinking.Max, m.Thinking.ZeroAllowed, m.Thinking.DynamicAllowed
			}
			models = append(models, mm)
		}
		sort.Slice(models, func(i, j int) bool { return models[i].ID < models[j].ID })
		var execs []exec
		for _, r := range f.Requests {
			execs = append(execs, exec{r, manager.ResolveExecutionModel(registered, r), GlobalModelRegistry().ClientSupportsModel(registered.ID, r)})
		}
		GlobalModelRegistry().UnregisterClient(registered.ID)
		outFiles = append(outFiles, fileOut{f.Name, f.Body, attrs, models, execs})
	}
	// AuthKind and AccountInfo on bare metadata (no auth_kind attribute).
	type account struct {
		Metadata string `json:"metadata"`
		Kind     string `json:"kind"`
		Type     string `json:"type"`
		Value    string `json:"value"`
	}
	var accounts []account
	for _, raw := range []string{
		`{"service_account":{"client_email":" sa@p.iam.gserviceaccount.com "}}`,
		`{"service_account":{"client_email":7}}`,
		`{"service_account":{}}`,
		`{"service_account":"x"}`,
		`{"email":" ","service_account":{"client_email":"sa2@p"}}`,
		`{"email":"e@x","service_account":{"client_email":"sa3@p"}}`,
		`{"siwc":{"refresh_token":"r"}}`,
		`{"siwc":{}}`,
		`{"project_id":"p"}`,
	} {
		var meta map[string]any
		if err := json.Unmarshal([]byte(raw), &meta); err != nil {
			t.Fatal(err)
		}
		a := &coreauth.Auth{ID: "x", Provider: "vertex", Metadata: meta}
		kind, value := a.AccountInfo()
		accounts = append(accounts, account{raw, a.AuthKind(), kind, value})
	}
	data, _ := json.MarshalIndent(map[string]any{"yaml": yaml, "files": outFiles, "accounts": accounts}, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
