# Moving from CLIProxyAPI (Go) to cliproxy-rs

cliproxy-rs follows CLIProxyAPI at commit `6fecc6e` (v8.0.10). It reads the same files, so switching is a matter of stopping one binary and starting the other on the same config. This page lists what carries over, what does not, and how to switch back.

## What carries over

- `config.yaml`. The v8 layout and the older flat layout (top-level `port`, `auth-dir`, an `api-keys` list, `remote-management`) are both read. Every v8 setting is accepted, including settings for features cliproxy-rs does not have yet (listed below); those are kept in the file but have no effect.
- Credential files in `oauth.auth-dir` (default `~/.cli-proxy-api`). Claude, Codex, Antigravity, Kimi, Meta, xAI, Devin, Vertex and AI Studio files are used as they are.
- The hashed management key. Go and cliproxy-rs both hash a plaintext `management.secret-key` with bcrypt on start and write the hash back, and each accepts the other's hash.
- Client keys in `access.api-keys`, provider keys under `api-keys`, OpenAI-compatible upstreams, model aliases and exclusions, payload rules, proxies, routing strategy, retries and cooldowns.
- The client routes (`/v1/...`, `/v1beta/...`, `/backend-api/codex/...`) and the Responses WebSocket, so clients need no changes.
- The v8 Management API at `/v8/management`, apart from the gaps below, and the dashboard at `/management.html`.

When a setting is changed through the dashboard or the Management API, cliproxy-rs edits only that key and keeps the rest of the text and its comments. As in Go, that first write also converts a flat legacy config to the v8 layout and moves keys that are not v8 settings into comments instead of deleting them. The dashboard's raw editor saves the text you submit.

The way back works too. In our tests against fake sign-in servers, Go 6fecc6e listed the Claude, Codex, Kimi and Meta credential files written by cliproxy-rs as active, and accepted a management key hashed by cliproxy-rs.

## Switching

1. Stop the Go server.
2. Back up `config.yaml` and the credential directory. They hold OAuth refresh tokens, so keep the backup private.
3. Check your command line against the flag list below. A few Go modes are not available yet, and cliproxy-rs exits with an error instead of starting without them.
4. Start cliproxy-rs with the same config: `cliproxy --config /path/to/config.yaml`.
5. Open `/management.html` and check that your credentials are listed and healthy, then send a test request with one of your client keys.

For a systemd service, replace the binary and keep your config and credential paths. The default Docker image expects `/data/config.yaml` and runs as UID/GID 10001. Update mounts and permissions, or override `--config` and set an accessible `oauth.auth-dir` to keep existing paths. [INSTALL.md](INSTALL.md#docker) covers volume ownership, listen address and management access.

Do not run Go and cliproxy-rs with credentials for the same account at the same time, even from separate copies of the auth directory. Both refresh OAuth tokens. A provider that rotates refresh tokens can invalidate the token held by the other server and force a new sign-in. A copied auth directory is not an isolated test. To try both servers side by side, use a separate test account and a different port. Keep the backup private, and do not assume its refresh tokens remain usable after either server refreshes them.

## Command-line flags

cliproxy-rs accepts every CLIProxyAPI flag, in Go's single-dash spelling (`-config`) or with two dashes. These work as in Go: `-config`, `-claude-login`, `-codex-login`, `-codex-device-login`, `-antigravity-login`, `-kimi-login`, `-kimi-ai-login`, `-xai-login`, `-meta-login`, `-devin-login`, `-vertex-import` (with `-vertex-import-prefix`), `-no-browser`, `-oauth-callback-port`, `-password`, `--local-model`, `-home-jwt` (or `HOME_JWT`), and LAN discovery with `-discover` (or the `discover` subcommand) and its `-discover-*` options.

`-tui` and `-tui -standalone` open the terminal UI as in Go, with the differences listed in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Not available yet

These settings are accepted in `config.yaml` and kept on save, but cliproxy-rs does not act on them yet, or only in part:

| Go setting or feature | In cliproxy-rs |
| --- | --- |
| `pprof` | No profiling endpoint. |
| Plugins on the request path | Frontend auth, model routers, plugin catalogs and executors (with or without a router), plugin schedulers, request and response translators and normalizers, thinking appliers, request, response and stream-chunk interceptors, the WebSocket response observer, the request lifecycle and usage plugins run as in Go, and so do the `host.http.*`, `host.auth.*`, `host.model.*` and `host.affinity.lookup` callbacks. Auth files go to the plugin auth provider owning their type before the built-in loader (`auth.parse`), as in Go; plugins are trusted code, and an auth provider sees every auth file with no `type`. Not called yet: per-credential model discovery (`model.for_auth`), and the plugin response hooks on a Claude client's buffered Claude answer. |
| Home-managed plugins | With `-home-jwt`, bootstrap, config updates, dispatch, usage, process and request logs, in-flight reporting and shared KV state work. Plugin binaries must already be installed locally; Home-supplied configuration can configure them, but Home-managed plugin sync, tasks and status reports are unavailable. |
| `management.panel-github-repository`, `management.disable-auto-update-panel`, `MANAGEMENT_STATIC_PATH` | The dashboard is built into the binary and never downloaded or read from disk. `management.disable-control-panel` is honoured. |
| Config reload log summaries | The config is reloaded, but the changes are not summarised in the log. |

`.env` in the working directory is loaded as in Go, and the `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` storage backends and `WRITABLE_PATH` work as in Go. `RUST_LOG`, when set, overrides the log level from `debug`.

## Management API differences

The v8 routes for config, credentials, OAuth sign-in, `requests/api-call`, cooldown reset, usage, logs, model definitions and plugins behave as in Go, and so do the older `/v0/management` routes, apart from those listed below. OAuth sign-in through the API works for Claude, Codex, Antigravity, Kimi, Meta, xAI and Devin, and `oauth/import?provider=vertex` (or `/v0/management/vertex/import`) imports a Vertex service account as in Go.

Not available on cliproxy-rs:

- `server/latest-version` answers `502` with "no release repository is configured" because cliproxy-rs doesn't check for its own releases yet. Go asks GitHub for the latest CLIProxyAPI release.

The bundled dashboard checks which server it is talking to and marks these features as not available instead of failing.

## Differences from Go

A few behaviours differ from Go on purpose, for example config writes keep the rest of `config.yaml` byte for byte and the dashboard is never downloaded. [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md) lists all of them.

### Additions

Features Go does not have. Reset-first routing is the default; the others are opt-in.

- Reset-first routing: `routing.strategy: soonest-reset` (alias `reset-first`), the default when `routing.strategy` is unset. Go reads an unset strategy as `round-robin`, so a Go config without the key behaves differently here; add `routing.strategy: round-robin` to keep Go's rotation. Among ready accounts, it selects the one whose weekly window resets soonest. It stays there until the account cools down or reaches a usage limit, then moves to the next. Reset times come from each account's latest response headers (`anthropic-ratelimit-unified-*` for Claude, `x-codex-*` for Codex). An account without a known future reset gets one request to learn it. Accounts without reset headers come after those with a known reset; equally ranked accounts take turns, so API keys rotate as with `round-robin`. Session affinity takes precedence. With several providers for one model, the first provider is selected, as with `fill-first`. Go reads `soonest-reset` as `round-robin`.
- Codex connection reuse: `oauth.providers.codex.chatgpt-keep-alive: true` keeps up to 2 idle connections to `chatgpt.com` per proxy for 90 seconds instead of opening one per request. Off by default.

## Switching back

Stop cliproxy-rs and start Go on the same config and credential directory. Credentials that cliproxy-rs connected or refreshed stay valid for Go, and the management key keeps working. The config needs no changes: Go treats `routing.strategy: soonest-reset`, and an unset strategy, as `round-robin`. Go also starts with `oauth.providers.codex.chatgpt-keep-alive` set and ignores it; a config edit through Go's v8 Management API moves the key into a comment, so keep-alive is off again when you come back to cliproxy-rs.

## Details

[PARITY.md](PARITY.md) sums up what works and links the item-by-item audit of every route, setting, flag and Go test suite.
