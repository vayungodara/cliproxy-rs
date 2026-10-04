// Overlaid into internal/api by gen.sh (go run -overlay); the checkout is not modified.

package api

import "github.com/router-for-me/CLIProxyAPI/v8/internal/api/handlers/management"

// ManagementHandler exposes the server's management handler to the golden generator.
func (s *Server) ManagementHandler() *management.Handler { return s.mgmt }
