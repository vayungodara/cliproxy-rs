# Moving from CLIProxyAPI (Go) to cliproxy-rs

cliproxy-rs follows CLIProxyAPI at commit `6fecc6e` (v8.0.10). It reads the same files, so switching is a matter of stopping one binary and starting the other on the same config. This page lists what carries over, what does not, and how to switch back.

## What carries over

- `config.yaml`. The v8 layout and the older flat layout (top-level `port`, `auth-dir`, an `api-keys` list, `remote-management`) are both read. Every v8 setting is accepted, including settings for features cliproxy-rs does not have yet (listed below); those are kept in the file but have no effect.
- Credential files in `oauth.auth-dir` (default `~/.cli-proxy-api`). Claude, Codex, Kimi, Meta, xAI, Devin, Vertex and AI Studio files are used as they are. Antigravity files, a provider cliproxy-rs does not serve yet, are listed in the dashboard but not used for requests.
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

If you run Go as a systemd service or in Docker, replace the binary or image and keep the same paths. See [INSTALL.md](INSTALL.md) for both.

Do not run Go and cliproxy-rs against the same credential directory at the same time. Both refresh OAuth tokens in the background and write the new tokens to the files. Some providers rotate the refresh token on every refresh, so a refresh by one server can invalidate the token the other is holding, and the account then needs a new sign-in. To try cliproxy-rs next to Go, give it a copy of the directory and a different port, or connect a separate test account.

## Command-line flags

cliproxy-rs accepts every CLIProxyAPI flag, in Go's single-dash spelling (`-config`) or with two dashes. These work as in Go: `-config`, `-claude-login`, `-codex-login`, `-codex-device-login`, `-kimi-login`, `-kimi-ai-login`, `-xai-login`, `-meta-login`, `-devin-login`, `-vertex-import` (with `-vertex-import-prefix`), `-no-browser`, `-oauth-callback-port`, `-password`, `-local-model`, `-home-jwt` (or `HOME_JWT`), and LAN discovery with `-discover` (or the `discover` subcommand) and its `-discover-*` options.

`-antigravity-login` exits with a "not supported by cliproxy-rs yet" error and status 1. `-tui` and `-tui -standalone` open the terminal UI as in Go, with the differences listed in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Not available yet

These settings are accepted in `config.yaml` and kept on save, but cliproxy-rs does not act on them yet, or only in part:

| Go setting or feature | In cliproxy-rs |
| --- | --- |
| `observability.logs.request-log` and error request logs | Request log files and per-request error logs are written as in Go, with the client's request (headers masked) and the response it received, but without Go's `=== API REQUEST ===` and `=== API RESPONSE ===` sections: the provider executors do not report the upstream exchange to the log yet. |
| `pprof` | No profiling endpoint. |
| `gpt-image` models through Codex accounts | `/v1/images/generations` and `/v1/images/edits` reach xAI and OpenAI-compatible upstreams only. A request that routes to a Codex credential fails; Go serves it through the ChatGPT backend. |
| Plugins on the request path | Frontend auth, model routers and the plugin executors they route to, request, response and stream-chunk interceptors, the request lifecycle and usage plugins run as in Go, and so do the `host.http.*`, `host.auth.*` and `host.affinity.lookup` callbacks. Providers owned by a plugin are not served yet: an auth file a plugin parses (including one saved by a plugin sign-in) is not loaded as a credential, and plugin models and executors are only reached through a model router. Plugin schedulers, request and response translators, thinking appliers, the `host.model.*` callbacks and the WebSocket response observer are not called. |
| Home reporting and storage | With `-home-jwt`, bootstrap, config updates and dispatch through Home work, but usage, logs and in-flight requests are not reported back to Home, Home's KV storage is not used, and Home's plugin sync, plugin tasks and plugin status reports are not implemented. Plugins load from the local configuration only. |
| `management.panel-github-repository`, `management.disable-auto-update-panel`, `MANAGEMENT_STATIC_PATH` | The dashboard is built into the binary and never downloaded or read from disk. `management.disable-control-panel` is honoured. |
| Config reload log summaries | The config is reloaded, but the changes are not summarised in the log. |

`.env` in the working directory is loaded as in Go, and the `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` storage backends and `WRITABLE_PATH` work as in Go. `RUST_LOG`, when set, overrides the log level from `debug`.

The one provider that is not available yet, Antigravity, is listed in the README under [Upcoming features](../README.md#upcoming-features).

## Management API differences

The v8 routes for config, credentials, OAuth sign-in, `requests/api-call`, cooldown reset, usage, logs, model definitions and plugins behave as in Go, and so do the older `/v0/management` routes, apart from those listed below. OAuth sign-in through the API works for Claude, Codex, Kimi, Meta, xAI and Devin. Antigravity sign-in and the Vertex import (`oauth/import`) return `404` with `provider_not_found`.

Not available on cliproxy-rs:

- `server/latest-version` answers `502` with "no release repository is configured" until cliproxy-rs publishes releases. Go asks GitHub for the latest CLIProxyAPI release.

The bundled dashboard checks which server it is talking to and marks these features as not available instead of failing.

## Differences from Go

A few behaviours differ from Go on purpose, for example config writes keep the rest of `config.yaml` byte for byte and the dashboard is never downloaded. [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md) lists all of them.

### Additions

Features Go does not have. Each is opt-in; the defaults behave as Go does.

- Reset-aware routing: `routing.strategy: soonest-reset` (alias `reset-first`). Among the accounts available right now, cliproxy-rs sends requests to the one whose weekly usage window resets soonest, and keeps using it until it cools down or one of its usage windows (Claude's 5-hour or 7-day, Codex's primary or secondary) is used up. Then it moves to the account with the next soonest reset. This spends quota that would otherwise expire at the reset, instead of spreading requests evenly. Reset times come from the rate-limit headers of each account's latest response (`anthropic-ratelimit-unified-*` for Claude, `x-codex-*` for Codex). An account without a known reset (not used yet, or its reset has passed) first gets one request, so its reset is learned, and then ranks by it. An account whose responses carry no reset comes after the accounts with a known reset. Accounts that rank equally take turns. Session affinity still wins: a conversation bound to an account stays on it. With several providers for one model, the first provider is used, as with `fill-first`. The default stays `round-robin`. Go reads `soonest-reset` as `round-robin`.

## Switching back

Stop cliproxy-rs and start Go on the same config and credential directory. Credentials that cliproxy-rs connected or refreshed stay valid for Go, and the management key keeps working. The config needs no changes: if it uses `routing.strategy: soonest-reset`, Go treats that as `round-robin`.

## Details

[PARITY.md](PARITY.md) sums up what works and links the item-by-item audit of every route, setting, flag and Go test suite.
