package cliproxy

// The Codex client catalog for one config, through Go's own config synthesis, model
// registration and handler (/v1/models?client_version=). Copy into sdk/cliproxy/ of
// CLIProxyAPI v8.0.35 (ae3ee3be) and run with RSFIX_OUT=<dir>; it writes codex_models_go.json.
// Nothing is sent upstream: catalogs are built locally.

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/gin-gonic/gin"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/runtime/executor"
	responsesclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/responses"
	"github.com/router-for-me/CLIProxyAPI/v8/internal/watcher/synthesizer"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/api/handlers/openai"
	coreauth "github.com/router-for-me/CLIProxyAPI/v8/sdk/cliproxy/auth"
	"github.com/router-for-me/CLIProxyAPI/v8/sdk/config"
)

const rsfixCodexModelsConfig = `codex-api-key:
  - api-key: sk-FAKE-codex
    base-url: http://127.0.0.1:1
    models:
      - name: gpt-5.5
claude-api-key:
  - api-key: sk-FAKE-claude
    base-url: http://127.0.0.1:1
    models:
      - name: claude-sonnet-4-6
        alias: sonnet-long
        max-context-length: 150000
      - name: claude-opus-4-6
        alias: opus-think
        thinking:
          levels: [low, high]
openai-compatibility:
  - name: acme
    base-url: http://127.0.0.1:1
    api-key-entries:
      - api-key: sk-FAKE-acme
    models:
      - name: acme-vision
        input-modalities: [TEXT, image, audio]
        thinking:
          levels: [none, low, ultra]
      - name: acme-plain
        input-modalities: [audio]
      - name: gpt-6-sol
        alias: acme-sol
        thinking:
          levels: [low, high]
      - name: gpt-5.5
        input-modalities: [text]
        thinking:
          levels: [low, xhigh]
client:
  codex:
    optimize-multi-agent-v2: true
`

func TestRSFixCodexModels(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	gin.SetMode(gin.TestMode)
	cfg, err := config.ParseConfigBytes([]byte(rsfixCodexModelsConfig))
	if err != nil {
		t.Fatal(err)
	}
	auths, err := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
		Config: cfg, Now: time.Unix(1, 0), IDGenerator: synthesizer.NewStableIDGenerator(),
	})
	if err != nil {
		t.Fatal(err)
	}
	service := &Service{cfg: cfg}
	registry := GlobalModelRegistry()
	manager := coreauth.NewManager(nil, nil, nil)
	manager.SetConfig(cfg)
	for _, auth := range auths {
		if _, err := manager.Register(t.Context(), auth); err != nil {
			t.Fatal(err)
		}
		service.registerModelsForAuth(t.Context(), auth)
		defer registry.UnregisterClient(auth.ID)
	}
	h := openai.NewOpenAIAPIHandler(handlers.NewBaseAPIHandlers(&cfg.SDKConfig, manager))
	out := map[string]string{"config": rsfixCodexModelsConfig}
	for _, version := range []string{"0.150.0", "cpa", "0.143.0"} {
		rec := httptest.NewRecorder()
		c, _ := gin.CreateTestContext(rec)
		c.Request = httptest.NewRequest(http.MethodGet, "/v1/models?client_version="+version, nil)
		h.OpenAIModels(c)
		out[version] = rec.Body.String()
	}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_models_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// A Codex API key without configured models registers the Pro catalog with the
// gpt-image-* built-ins (WithCodexBuiltins). Records Go's plain /v1/models list.
const rsfixCodexBuiltinsConfig = `codex-api-key:
  - api-key: sk-FAKE-codex-builtins
    base-url: http://127.0.0.1:1
`

func TestRSFixCodexBuiltins(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	gin.SetMode(gin.TestMode)
	cfg, err := config.ParseConfigBytes([]byte(rsfixCodexBuiltinsConfig))
	if err != nil {
		t.Fatal(err)
	}
	auths, err := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
		Config: cfg, Now: time.Unix(1, 0), IDGenerator: synthesizer.NewStableIDGenerator(),
	})
	if err != nil {
		t.Fatal(err)
	}
	service := &Service{cfg: cfg}
	registry := GlobalModelRegistry()
	manager := coreauth.NewManager(nil, nil, nil)
	manager.SetConfig(cfg)
	for _, auth := range auths {
		if _, err := manager.Register(t.Context(), auth); err != nil {
			t.Fatal(err)
		}
		service.registerModelsForAuth(t.Context(), auth)
		defer registry.UnregisterClient(auth.ID)
	}
	h := openai.NewOpenAIAPIHandler(handlers.NewBaseAPIHandlers(&cfg.SDKConfig, manager))
	rec := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(rec)
	c.Request = httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	h.OpenAIModels(c)
	out := map[string]string{"config": rsfixCodexBuiltinsConfig, "models": rec.Body.String()}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_builtins_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

// upstream.claude.tool-search on (here through its legacy spelling): supports_search_tool
// stays for a pure Claude model only when every credential serving it supports native
// tool search for its execution model. Writes codex_tool_search_go.json.
const rsfixCodexToolSearchConfig = `claude:
  tool-search: true
claude-api-key:
  - api-key: sk-FAKE-claude-origin
    models:
      - name: claude-sonnet-4-6
        alias: sonnet-native
      - name: claude-sonnet-4-0
        alias: sonnet-legacy
  - api-key: sk-FAKE-claude-trusted
    base-url: http://127.0.0.1:1
    native-tool-search: true
    models:
      - name: claude-opus-4-6
        alias: opus-trusted
      - name: claude-opus-4-6
        alias: opus-mixed
  - api-key: sk-FAKE-claude-gateway
    base-url: http://127.0.0.1:1
    models:
      - name: claude-haiku-4-5
        alias: haiku-gateway
      - name: claude-opus-4-6
        alias: opus-mixed
`

func TestRSFixCodexToolSearch(t *testing.T) {
	dir := os.Getenv("RSFIX_OUT")
	if dir == "" {
		t.Skip("RSFIX_OUT not set")
	}
	gin.SetMode(gin.TestMode)
	cfg, err := config.ParseConfigBytes([]byte(rsfixCodexToolSearchConfig))
	if err != nil {
		t.Fatal(err)
	}
	responsesclaude.SetClientToolSearchEnabled(cfg.Claude.ToolSearch)
	defer responsesclaude.SetClientToolSearchEnabled(false)
	auths, err := synthesizer.NewConfigSynthesizer().Synthesize(&synthesizer.SynthesisContext{
		Config: cfg, Now: time.Unix(1, 0), IDGenerator: synthesizer.NewStableIDGenerator(),
	})
	if err != nil {
		t.Fatal(err)
	}
	service := &Service{cfg: cfg}
	registry := GlobalModelRegistry()
	manager := coreauth.NewManager(nil, nil, nil)
	manager.SetConfig(cfg)
	manager.RegisterExecutor(executor.NewClaudeExecutor(cfg))
	for _, auth := range auths {
		if _, err := manager.Register(t.Context(), auth); err != nil {
			t.Fatal(err)
		}
		service.registerModelsForAuth(t.Context(), auth)
		defer registry.UnregisterClient(auth.ID)
	}
	h := openai.NewOpenAIAPIHandler(handlers.NewBaseAPIHandlers(&cfg.SDKConfig, manager))
	rec := httptest.NewRecorder()
	c, _ := gin.CreateTestContext(rec)
	c.Request = httptest.NewRequest(http.MethodGet, "/v1/models?client_version=0.150.0", nil)
	h.OpenAIModels(c)
	out := map[string]string{"config": rsfixCodexToolSearchConfig, "0.150.0": rec.Body.String()}
	data, err := json.MarshalIndent(out, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, "codex_tool_search_go.json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}
