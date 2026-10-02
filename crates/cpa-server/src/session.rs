//! Session identity for affinity (sdk/cliproxy/session/identity.go).

use axum::http::HeaderMap;

/// The session key a request binds to, if any.
// ponytail: explicit Claude Code header only; the full Go precedence (other client
// headers, prompt_cache_key, conversation IDs, hashes, parent/root) lands with M4-0021.
pub fn resolve(headers: &HeaderMap, _body: &[u8]) -> Option<String> {
    headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}
