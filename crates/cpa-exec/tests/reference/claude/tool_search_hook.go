package main

import (
	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	responsesclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/responses"
)

// The server publishes upstream.claude.tool-search to the translator on every config
// load (internal/api/server_reload.go applyClaudeToolSearchConfig).
func init() {
	afterLoad = func(cfg *config.Config) {
		responsesclaude.SetClientToolSearchEnabled(cfg != nil && cfg.Claude.ToolSearch)
	}
}
