package cliproxy

import (
	"encoding/json"
	"fmt"
	"os"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
)

// Records the provider key Go v8.0.35 registers an OpenAI-compatible credential's
// models under (registerModelsForAuth), for credentials that did not come from
// config synthesis, such as Home-dispatched and plugin auths.
func TestZZProviderKey(t *testing.T) {
	yaml := "openai-compatibility:\n  - name: Acme\n    base-url: http://127.0.0.1:1\n    api-key-entries: [{api-key: k}]\n    models: [{name: m-acme}]\n"
	cfg, err := config.ParseConfigBytes([]byte(yaml))
	if err != nil {
		t.Fatal(err)
	}
	type record struct {
		Name       string            `json:"name"`
		Provider   string            `json:"provider"`
		Label      string            `json:"label"`
		Attributes map[string]string `json:"attributes"`
		Registered []string          `json:"registered"`
	}
	cases := []record{
		{Name: "compat-name", Provider: "openai-compatibility", Attributes: map[string]string{"compat_name": "Acme"}},
		{Name: "compat-name-other-provider", Provider: "zed", Attributes: map[string]string{"compat_name": " Acme "}},
		{Name: "compat-name-home-provider", Provider: "openai-compatible-acme", Attributes: map[string]string{"compat_name": "acme"}},
		{Name: "unprefixed-provider-key", Provider: "openai-compatibility", Attributes: map[string]string{"compat_name": "Acme", "provider_key": "Acme"}},
		{Name: "prefixed-provider-key", Provider: "openai-compatibility", Attributes: map[string]string{"compat_name": "Acme", "provider_key": "openai-compatible-ACME"}},
		{Name: "label", Provider: "openai-compatibility", Label: "Acme", Attributes: map[string]string{}},
		{Name: "label-other-case", Provider: "OpenAI-Compatibility", Label: "ACME", Attributes: map[string]string{}},
		{Name: "compatibility-provider-key", Provider: "openai-compatibility", Attributes: map[string]string{"compat_name": " Acme ", "provider_key": "OpenAI-Compatibility"}},
		{Name: "compatibility-provider-key-other-provider", Provider: "zed", Attributes: map[string]string{"compat_name": "ACME", "provider_key": "openai-compatibility"}},
		{Name: "compatibility-provider-key-label", Provider: "openai-compatibility", Label: "Acme", Attributes: map[string]string{"provider_key": "OpenAI-Compatibility"}},
		{Name: "unnamed-provider", Provider: "Acme", Attributes: map[string]string{}},
		{Name: "unnamed-provider-provider-key", Provider: "Acme", Attributes: map[string]string{"provider_key": "Foo"}},
		{Name: "unmatched-provider-provider-key", Provider: "zed", Attributes: map[string]string{"provider_key": "Foo"}},
	}
	for i := range cases {
		c := &cases[i]
		attrs := map[string]string{"api_key": "x"}
		for k, v := range c.Attributes {
			attrs[k] = v
		}
		a := &coreauth.Auth{ID: fmt.Sprintf("zz-%d", i), Provider: c.Provider, Label: c.Label, Attributes: attrs, Status: coreauth.StatusActive}
		(&Service{cfg: cfg}).registerModelsForAuth(t.Context(), a)
		c.Registered = registry.GetGlobalRegistry().GetModelProviders("m-acme")
		GlobalModelRegistry().UnregisterClient(a.ID)
	}
	data, _ := json.MarshalIndent(map[string]any{"yaml": yaml, "cases": cases}, "", "  ")
	if err := os.WriteFile(os.Getenv("CPA_FIXTURE_OUT"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
