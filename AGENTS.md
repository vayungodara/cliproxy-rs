# cliproxy-rs

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI) targeting full parity and drop-in compatibility: same `config.yaml`, same auth JSON files in `auth-dir`, same HTTP routes and response shapes, same v8 Management API.

## Reference

- The reference is CLIProxyAPI at commit `6fecc6e`: clone https://github.com/router-for-me/CLIProxyAPI, check out that commit and treat the clone as read-only. Cite Go file paths in comments only where behaviour is non-obvious.
- When Go behaviour and documentation disagree, the Go code wins. Port behaviour, not structure: idiomatic Rust over transliterated Go.

## Layout

- `crates/cpa-core`: config and credential file formats. No networking.
- `crates/cpa-exec`: one module per upstream provider (wire format, auth headers, HTTP client profile).
- `crates/cpa-server`: axum routes, client auth, credential selection, management API.
- `crates/cliproxy`: the binary. Go-style single-dash flags are accepted.

## Rules

- Build and test with `cargo test --workspace`. On a shared or small machine, run it at low priority and limit parallel jobs, for example `CARGO_BUILD_JOBS=2 nice -n 19 cargo test --workspace`; the BoringSSL build and the release link use a lot of memory.
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Tests use local mock upstreams. Never send test traffic to real provider accounts.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.
