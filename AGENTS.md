# cliproxy-rs

If you are an agent installing cliproxy-rs for a user rather than developing it, stop here and follow [docs/AI-SETUP.md](docs/AI-SETUP.md).

A Rust rewrite of CLIProxyAPI (https://github.com/router-for-me/CLIProxyAPI) targeting full parity and drop-in compatibility: same `config.yaml`, same auth JSON files in `auth-dir`, same HTTP routes and response shapes, same v8 Management API.

## Reference

- The reference is CLIProxyAPI at commit `6fecc6e`: clone https://github.com/router-for-me/CLIProxyAPI, check out that commit and treat the clone as read-only. Cite Go file paths in comments only where behaviour is non-obvious.
- When Go behaviour and documentation disagree, the Go code wins. Port behaviour, not structure: idiomatic Rust over transliterated Go.
- A deliberate difference from Go is listed in `docs/DIFFERENCES-FROM-GO.md`, and tests that compare against Go goldens encode it explicitly.

## Layout

- `crates/cpa-core`: config and credential file formats. No networking.
- `crates/cpa-common`: provider-neutral, Go-exact helpers shared by translators and executors.
- `crates/cpa-translate`: format translation between the Anthropic, OpenAI and Gemini APIs.
- `crates/cpa-exec`: one module per upstream provider (wire format, auth headers, HTTP client profile).
- `crates/cpa-server`: axum routes, client auth, credential selection, management API.
- `crates/cpa-plugin`, `crates/cpa-store`, `crates/cpa-home`: the plugin host, the remote config and auth stores, and Home mode.
- `crates/cliproxy`: the binary. Go-style single-dash flags are accepted.
- `ui/` is the dashboard, `harness/` sends the same requests to Go and Rust and compares them.

## Rules

- The full test suite is slow and memory-heavy (the BoringSSL build and the release link). While iterating, run the affected crate or a filter, for example `cargo test -p cpa-exec kimi`; CI runs fmt, `clippy -D warnings` and the whole suite. On a shared or small machine, add `CARGO_BUILD_JOBS=2 nice -n 19`.
- The Go and PostgreSQL comparison tests in `cargo test` run fully only in CI, inside a network-denied namespace. For a local Go/Rust differential run on Linux, use `./harness/run` (see `harness/README.md`).
- Request bodies are forwarded byte for byte unless a ported rule rewrites them. Never re-serialize JSON just to pass it through.
- Upstream credentials and client keys must never be logged or forwarded to the wrong side.
- Upstream request and response capture goes through the shared sink in `crates/cpa-server/src/request_logging.rs`. It redacts `Cookie` and `Set-Cookie` values, credentials in URLs and short keys, and spools large sections to disk. Don't format or log captured bytes anywhere else.
- Keep request preparation linear in the body size. Coding agents send bodies from 100 KB to several MB, so walk a JSON body once and edit by path (see `crates/cpa-exec/src/claude/signals.rs`) instead of searching it again from the start for each element.
- Performance and memory claims come from `docs/BENCHMARKS.md`; `bench/run.sh` and `bench/messages.sh` reproduce them.
- Tests use local mock upstreams and never send traffic to real provider accounts. Give every test a private `auth-dir`: without one, credential loading falls back to `~/.cli-proxy-api`, where real logins live. Build test runtimes with `cpa_server::testing::runtime`.
- Mark deliberate simplifications with a `ponytail:` comment naming the ceiling and the upgrade path.

## Review guidelines

Flag, in order: behaviour that differs from Go at `6fecc6e` without an entry in `docs/DIFFERENCES-FROM-GO.md`; request or response bytes changed on passthrough; credentials or client keys reaching logs or the wrong side; tests that can touch the network or a real `auth-dir`; work on a request body that grows faster than linearly with its size. Skip style nits that `cargo fmt` and clippy already cover.
