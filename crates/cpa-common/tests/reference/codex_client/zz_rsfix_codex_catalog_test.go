package models

// Vectors for cpa_common::codex_catalog. Copy into internal/client/codex/models/ of
// CLIProxyAPI 6fecc6e and run with RSFIX_OUT=<dir>; it writes codex_catalog_go.json.
// The registry facts the builder reads are recorded with each vector so the Rust
// replay looks up exactly what Go saw. Pure functions only: no network.

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/registry"
)

type rsfixFacts struct {
	ID                      string                    `json:"id"`
	Type                    string                    `json:"type"`
	OwnedBy                 string                    `json:"owned_by"`
	DisplayName             string                    `json:"display_name"`
	Description             string                    `json:"description"`
	ContextLength           int                       `json:"context_length"`
	MetadataModelID         string                    `json:"metadata_model_id"`
	Thinking                *registry.ThinkingSupport `json:"thinking"`
	ExplicitThinking        bool                      `json:"explicit_thinking"`
	InputModalities         []string                  `json:"input_modalities"`
	ExplicitInputModalities bool                      `json:"explicit_input_modalities"`
}

func rsfixFactsOf(info *registry.ModelInfo) *rsfixFacts {
	if info == nil {
		return nil
	}
	return &rsfixFacts{ID: info.ID, Type: info.Type, OwnedBy: info.OwnedBy, DisplayName: info.DisplayName, Description: info.Description,
		ContextLength: info.ContextLength, MetadataModelID: info.MetadataModelID, Thinking: info.Thinking, ExplicitThinking: info.ExplicitThinking,
		InputModalities: info.SupportedInputModalities, ExplicitInputModalities: info.ExplicitInputModalities}
}

func TestRSFixCodexCatalog(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	reg := registry.GetGlobalRegistry()
	clients := map[string][2]any{
		"rsfix-codex": {"codex", []*registry.ModelInfo{
			{ID: "gpt-5.5", OwnedBy: "openai", Type: "openai", Created: 1},
			{ID: "gpt-6-sol", OwnedBy: "openai", Type: "openai", Created: 1, MaxContextLength: 128000},
			{ID: "my-alias", MetadataModelID: "gpt-5.4", OwnedBy: "openai", Type: "openai", Created: 1, DisplayName: "My Alias"},
			{ID: "team/gpt-6-sol", MetadataModelID: "gpt-6-sol", OwnedBy: "openai", Type: "openai", Created: 1},
			{ID: "gpt-image-2", OwnedBy: "openai", Type: "openai", Created: 1},
		}},
		"rsfix-claude": {"claude", []*registry.ModelInfo{
			{ID: "claude-sonnet-4-6", OwnedBy: "anthropic", Type: "claude", Created: 1, DisplayName: "Claude Sonnet 4.6", Description: "Balanced <model> & more", ContextLength: 200000, MaxCompletionTokens: 64000, MaxContextLength: 150000,
				Thinking: &registry.ThinkingSupport{Min: 1024, Max: 64000, Levels: []string{"low", "medium", "high", "max"}}},
			{ID: "gpt-5.4", OwnedBy: "openai", Type: "claude", Created: 1},
		}},
		"rsfix-compat": {"openai-compatibility", []*registry.ModelInfo{
			{ID: "compat-vision", OwnedBy: "acme", Type: "openai-compatibility", Created: 1, DisplayName: "zz Vision", ExplicitInputModalities: true, SupportedInputModalities: []string{"TEXT", "image", "audio", "text"},
				ExplicitThinking: true, Thinking: &registry.ThinkingSupport{Levels: []string{"none", "low", "ultra"}}},
			{ID: "compat-plain", OwnedBy: "acme", Type: "openai-compatibility", Created: 1, DisplayName: "Alpha Plain", ExplicitInputModalities: true, SupportedInputModalities: []string{"audio"}},
			{ID: "img-model", OwnedBy: "acme", Type: "openai-image", Created: 1},
			{ID: "my-alias", MetadataModelID: "gpt-5.4", OwnedBy: "acme", Type: "openai-compatibility", Created: 1, ExplicitThinking: true, Thinking: &registry.ThinkingSupport{Levels: []string{"low", "high"}}},
		}},
		"rsfix-devin": {"devin", []*registry.ModelInfo{
			{ID: "devin/swe-1", OwnedBy: "cognition", Type: "devin", Created: 1, DisplayName: "SWE 1 (devin)"},
		}},
	}
	for id, c := range clients {
		reg.RegisterClient(id, c[0].(string), c[1].([]*registry.ModelInfo))
		defer reg.UnregisterClient(id)
	}

	available := reg.GetAvailableModels("openai")
	sort.SliceStable(available, func(i, j int) bool { return available[i]["id"].(string) < available[j]["id"].(string) })

	providers := map[string][]string{}
	lookups := map[string]*rsfixFacts{}
	record := func(id string) {
		ids := []string{id}
		if idx := strings.Index(id, "/"); idx != -1 {
			ids = append(ids, strings.TrimSpace(id[idx+1:]))
		}
		for _, candidate := range ids {
			providers[candidate] = reg.GetModelProviders(candidate)
			lookups[candidate] = rsfixFactsOf(registry.LookupModelInfo(candidate))
		}
		for _, candidate := range ids {
			for _, other := range ids {
				for _, p := range providers[other] {
					lookups[candidate+"|"+p] = rsfixFactsOf(registry.LookupModelInfo(candidate, p))
				}
			}
		}
	}
	for _, model := range available {
		record(model["id"].(string))
	}
	webSearch := map[string]bool{"gpt-5.5": true, "claude-sonnet-4-6": false}
	webSearchFn := func(id string) *bool {
		if v, ok := webSearch[id]; ok {
			return &v
		}
		return nil
	}
	applyPatch := map[string]bool{"gpt-5.5": true, "gpt-6-sol": true, "compat-vision": true, "compat-plain": true, "devin/swe-1": true, "gpt-image-2": true, "img-model": true}
	applyPatchFn := func(id string) bool { return applyPatch[id] }

	type vector struct {
		Optimize   bool   `json:"optimize"`
		Version    string `json:"version"`
		ApplyPatch bool   `json:"apply_patch"`
		Out        string `json:"out"`
	}
	var vectors []vector
	for _, c := range []struct {
		optimize, patch bool
		version         string
	}{
		{false, false, ""}, {true, true, "0.150.0"}, {false, true, "0.143.9"}, {true, false, "cpa"}, {false, false, "v0.144.0-beta+1"},
	} {
		var patchFn ApplyPatchCapabilityForModelFunc
		if c.patch {
			patchFn = applyPatchFn
		}
		out, err := MarshalCompact(BuildResponseForClientWithToolCapabilities(available, reg.GetModelProviders, webSearchFn, patchFn, c.optimize, c.version))
		if err != nil {
			t.Fatal(err)
		}
		vectors = append(vectors, vector{Optimize: c.optimize, Version: c.version, ApplyPatch: c.patch, Out: string(out)})
	}

	validation := []map[string]string{}
	for _, raw := range []string{
		`{}`, `{"models":[]}`, `not json`,
		`{"models":[{"slug":"x"}]}`,
		`{"models":[{"slug":"gpt-5.5","display_name":"a","description":"b","base_instructions":"c","minimal_client_version":"0","visibility":"list","default_reasoning_level":"low","context_window":10,"max_context_window":5,"priority":1,"supported_reasoning_levels":[{"effort":"low"}]}]}`,
		`{"models":[{"slug":"gpt-5.5","display_name":"a","description":"b","base_instructions":"c","minimal_client_version":"0","visibility":"list","default_reasoning_level":"high","context_window":1,"max_context_window":5,"priority":1,"supported_reasoning_levels":[{"effort":"low"}]}]}`,
		`{"models":[{"slug":"other","display_name":"a","description":"b","base_instructions":"c","minimal_client_version":"0","visibility":"list","default_reasoning_level":"low","context_window":1,"max_context_window":5,"priority":1.5,"supported_reasoning_levels":[{"effort":"low"}]}]}`,
		`{"models":[{"slug":"gpt-5.5","display_name":"a","description":"b","base_instructions":"c","minimal_client_version":"0","visibility":"list","default_reasoning_level":"low","context_window":1,"max_context_window":5,"priority":0,"supported_reasoning_levels":[{"effort":"low"}]}]}`,
		`{"models":[{"slug":"gpt-5.5","display_name":"a","description":"b","base_instructions":"c","minimal_client_version":"0","visibility":"list","default_reasoning_level":"low","context_window":1,"max_context_window":5,"priority":0,"supported_reasoning_levels":[{"effort":"low"}]},{"slug":"gpt-5.5"}]}`,
	} {
		msg := ""
		if err := registry.ValidateCodexClientModelsJSON([]byte(raw)); err != nil {
			msg = err.Error()
		}
		validation = append(validation, map[string]string{"in": raw, "err": msg})
	}

	data, err := json.MarshalIndent(map[string]any{
		"available": available, "providers": providers, "lookups": lookups,
		"web_search": webSearch, "apply_patch": applyPatch, "vectors": vectors, "validation": validation,
	}, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_catalog_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
