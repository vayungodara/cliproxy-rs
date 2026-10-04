// Overlaid into internal/api/handlers/management by gen.sh (go run -overlay); the
// checkout is not modified.

package management

import "github.com/router-for-me/CLIProxyAPI/v8/internal/pluginstore"

// SetPluginStoreTestHooks sets the seams Go's own plugin store tests set
// (plugin_store_release_test.go): the store's HTTP client and its rate limiter.
func (h *Handler) SetPluginStoreTestHooks(doer pluginstore.HTTPDoer, limiter *pluginstore.GitHubRateLimiter) {
	h.pluginStoreHTTPClient = doer
	h.pluginStoreRateLimiter = limiter
}
