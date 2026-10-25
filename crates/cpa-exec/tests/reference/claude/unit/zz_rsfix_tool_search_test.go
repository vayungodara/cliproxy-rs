package executor

import (
	"context"

	"github.com/router-for-me/CLIProxyAPI/v8/internal/config"
	responsesclaude "github.com/router-for-me/CLIProxyAPI/v8/internal/translator/claude/openai/responses"
)

// Go's tool_search tests set upstream.claude.tool-search process-wide; cliproxy-rs reads
// it from the config, so each call's config is recorded with the value the executor pins.
func init() {
	rsfixConfigHook = func(ctx context.Context, cfg *config.Config) *config.Config {
		copied := *cfg
		copied.Claude.ToolSearch = responsesclaude.ClientToolSearchEnabledFor(ctx)
		return &copied
	}
}
