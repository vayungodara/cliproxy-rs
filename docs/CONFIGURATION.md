# Configuration

cliproxy-rs reads one YAML file, `config.yaml`. Pass its path with `--config`; without it, the server reads `config.yaml` in the current directory. It accepts CLIProxyAPI's v8 layout (shown here) and the older flat layout, and every setting in CLIProxyAPI's [`config.example.yaml`](https://github.com/router-for-me/CLIProxyAPI/blob/6fecc6e5567912661654a4eaf9b8f5436facd1c2/config.example.yaml) is accepted. Settings for features that are not ported yet are kept in the file but have no effect.

The server watches the file and applies most changes within a second, without a restart. `server.host`, `server.port`, `server.tls` and `server.trusted-proxies` are read at start, so restart after changing them. When the dashboard or the Management API changes a setting, only that key is rewritten; the rest of the file, comments included, stays as it was. The first such save of a file in an older layout moves its settings to the current layout, with their comments, and writes Windows (CRLF) line endings as LF. Saves through the older `/v0/management` setting endpoints are the exception: as in CLIProxyAPI, they keep a file that has no current-layout sections in its older layout. When the file uses YAML that cannot be moved line by line, such as anchors, that save writes the file again from its values: the comments at the top are kept, and other comments may be lost.

## A complete small example

```yaml
config-version: 8
server:
  host: "127.0.0.1"
  port: 8317
access:
  api-keys:
    - "your-client-key"
management:
  secret-key: "your-management-key"
oauth:
  auth-dir: "~/.cliproxy-rs/auth"
routing:
  strategy: round-robin
  session-affinity: true
  retry:
    request-retry: 2
observability:
  logs:
    logging-to-file: true
```

## server

| Setting | What it does |
|---|---|
| `host` | The address to listen on. `"127.0.0.1"` accepts only this computer; `""` listens on every interface. |
| `port` | The port, `8317` by default. |
| `tls.enable`, `tls.cert`, `tls.key` | Serve HTTPS on the same port with this certificate and key file. |
| `max-request-body-bytes` | The largest request body accepted, in bytes; `0` (the default) or a negative value sets no limit. A larger body gets `413` before it is processed: on the client routes, the Management API and plugin routes. As in Go, a streamed body whose JSON value or multipart form ends before the limit is handled from the bytes before it. Changes apply to the next request. The client routes also stop at 64 MiB whatever this says. The older top-level `max-request-body-bytes` also works; the one under `server` wins. |
| `github-token` | A GitHub token. CLIProxyAPI sends it to the GitHub API ahead of the `GITHUB_TOKEN` environment variable: on its release checks, its panel downloads and plugin store requests that no `plugins.store-auth` rule covers. cliproxy-rs keeps the setting but sends it nowhere yet: its release check sends no token, its dashboard is built in, and the plugin store use is upcoming. Management API config reads show it, as in CLIProxyAPI. The older top-level `github-token` also works; the one under `server` wins. |
| `trusted-proxies` | Addresses of reverse proxies or tunnels whose `X-Forwarded-For` header is trusted. Set it when a tunnel on the same machine forwards requests, for example `[127.0.0.1, "::1"]`. See [running it safely](GETTING-STARTED.md#running-it-safely). |

## access

| Setting | What it does |
|---|---|
| `api-keys` | The client keys your tools send, as `Authorization: Bearer <key>`, `x-api-key`, `x-goog-api-key` or `?key=`. Keep at least one; with an empty list anyone who can reach the server can use it. |

## management

| Setting | What it does |
|---|---|
| `secret-key` | The dashboard and Management API password. A plaintext value is replaced by its bcrypt hash on first start. The `MANAGEMENT_PASSWORD` environment variable works too. When neither is set, the management API is off. |
| `allow-remote` | `false` (the default) accepts management requests from this computer only. |
| `disable-control-panel` | `true` stops serving the dashboard at `/management.html`; the Management API keeps working. |

## oauth

| Setting | What it does |
|---|---|
| `auth-dir` | The folder for account sign-in files, `~/.cli-proxy-api` by default. `~` is expanded. The server watches it, so a file added, edited or removed there takes effect without a restart. |
| `providers.codex.signin-with-chatgpt-identifier` | The agent name sent when binding Sign in with ChatGPT tokens to a Codex credential (`POST /v8/management/oauth/siwc/bind`), `CLIProxyAPI` when empty. Legacy spelling: `codex.signin-with-chatgpt-identifier`. Go also uses it as the SIWC request User-Agent prefix; cliproxy-rs does not send SIWC requests yet. |
| `providers.codex.chatgpt-keep-alive` | A cliproxy-rs addition, `false` by default. `true` keeps up to 2 idle connections to `chatgpt.com` per proxy for 90 seconds, so Codex HTTP requests skip the TCP and TLS handshake. Off, every Codex request opens its own connection, as CLIProxyAPI does. A change applies from the next request, without a restart. Go starts with the key in the file and ignores it. A config edit through Go's v8 Management API moves the key into a comment, so the setting is off again when you return to cliproxy-rs; edits through Go's v0 endpoints keep it. Setting the key through Go's v8 API is refused, as for any key Go does not know. |

## upstream

Provider settings shared by account sign-ins and provider API keys. Settings left under `oauth.providers`, such as `oauth.providers.codex.header-defaults`, apply to sign-ins only.

| Setting | What it does |
|---|---|
| `codex.disable-codex-cloaking`, `codex.stream-bootstrap-buffering`, `codex.stream-bootstrap-timeout`, `codex.orphan-delegation-compatibility`, `codex.model-level-cooling`, `codex.response-steering` | Codex request shaping, stream start-up, cooldown scope and full-duplex WebSocket steering. |
| `claude.model-level-cooling`, `claude.disable-cloaking-model-list`, `claude.disable-claude-cloak-mode`, `claude.header-defaults.*` | Claude cooldown scope, cloaking and the fallback request headers (`user-agent`, `package-version`, `runtime-version`, `os`, `arch`, `timeout`, `timezone`, `stabilize-device-profile`). |
| `claude.tool-search` | `false` by default. `true` turns Codex client `tool_search` on Responses requests to Claude into Anthropic native tool search (`defer_loading` tools and `tool_reference` results) on credentials that support it: Anthropic's own API, or a Claude API key with `native-tool-search: true`. Other credentials and models that predate tool search get the request without `tool_search`. The Codex model catalog then advertises the search tool for a Claude model only when every credential serving it supports native tool search. |
| `xai.inject-x-search` | Adds the `x_search` tool to xAI requests. |

Older files spell these as `oauth.providers.codex.*`, `oauth.providers.claude.*` (with `claude-code.disable-cloaking-model-list`) and `oauth.providers.xai.*`, or as the flat `codex`, `claude`, `claude-code`, `disable-claude-cloak-mode`, `claude-header-defaults` and `xai` keys. They load the same way; when a setting appears more than once, `upstream` wins, then `oauth.providers`, then the flat key. The next save through the dashboard or the v8 Management API writes it under `upstream`. The Management API reads and writes these at `/v8/management/config/upstream/...`. It also accepts the `oauth/providers/...` paths, as CLIProxyAPI does: a read there shows the current value, and a write or delete there changes it under `upstream`. The same goes for `client.codex.optimize-multi-agent-v2` at its older `oauth/providers/codex`, `providers/codex` and `codex` paths.

## api-keys

Provider API keys, as opposed to account sign-ins, go here, grouped by provider: `claude`, `codex`, `gemini`, `vertex`, `xai` and `openai-compatibility` for any service that speaks the OpenAI API. The dashboard's Provider keys page edits them. An OpenAI-compatible service, for example:

```yaml
api-keys:
  openai-compatibility:
    - name: openrouter
      base-url: "https://openrouter.ai/api/v1"
      keys:
        - api-key: "sk-or-..."
      models:
        - name: "moonshotai/kimi-k2"
          alias: "kimi-k2"
```

## routing

How the server picks an account for each request. [MULTI-ACCOUNT.md](MULTI-ACCOUNT.md) explains these in detail.

| Setting | What it does |
|---|---|
| `strategy` | `round-robin` (the default), `fill-first`, `weighted-round-robin`, or `soonest-reset` (experimental and opt-in: select the account whose weekly window resets soonest). |
| `session-affinity` | `true` keeps a conversation on the account that served its first request. The installer writes `true` for new configs. If omitted, the server default is `false`, as in Go. |
| `session-affinity-ttl` | How long a conversation stays bound to its account, `"1h"` by default. |
| `retry.request-retry` | Extra rounds over the accounts after a failed attempt, `0` by default. |
| `retry.max-retry-credentials` | At most this many accounts tried per round; `0` means all of them. |
| `retry.max-retry-interval` | The longest wait, in seconds, for an account to come out of cooldown before retrying; `0` means do not wait. |
| `cooldown.disable-cooling` | `true` keeps failing accounts in rotation instead of resting them. |
| `cooldown.transient-error-cooldown-seconds` | How long to rest an account after a temporary upstream error; `0` means 60 seconds, a negative value turns it off. |
| `cooldown.save-cooldown-status` | `true` keeps cooldowns in `.cds` files in `auth-dir`, so a restart does not forget them. |
| `cooldown.max-trusted-cooldown` | A cliproxy-rs addition. When an account hits its usage limit, the provider says when it resets, sometimes days ahead, and the account rests until then. Providers often reset early, so cliproxy-rs trusts the stated time for at most this long (`"1h"` by default; a Go duration such as `"90m"`, or a bare number meaning seconds; at least 10 seconds), then lets the next request through to check. If the account is still limited, the next rest is twice as long, and never longer than the stated reset. `0` trusts the stated reset, as CLIProxyAPI does. An unreadable value is logged and the default is used. See [differences from Go](DIFFERENCES-FROM-GO.md#deliberate-differences). |

### Optional preset: fail over only on 429

This commented preset keeps 429 cooldown and failover for Claude and Codex OAuth accounts. It stops ordinary failover after a failed attempt for the listed non-429 statuses, without adding a cooldown. Credential-preparation failures and an enabled `requests.streaming.bootstrap-retries` can still cause another attempt. Unlisted statuses keep normal handling. The preset applies only to OAuth accounts and leaves plan limits and provider checks unchanged. A credential's own `request_scoped_errors` rules take precedence.

Uncomment the block to use it. `oauth-request-scoped-errors` is the legacy spelling of `oauth.request-scoped-errors`; use only one. `(?s).*` matches any body, including an empty one. Rules need a body matcher; status alone does not match.

```yaml
# oauth-request-scoped-errors:
#   claude: &plan-limits-only
#     - {status: 429, match-regexr: ["(?s).*"], action: continue-and-cooldown}
#     - {status: 400, match-regexr: ["(?s).*"], action: stop}
#     - {status: 401, match-regexr: ["(?s).*"], action: stop}
#     - {status: 402, match-regexr: ["(?s).*"], action: stop}
#     - {status: 403, match-regexr: ["(?s).*"], action: stop}
#     - {status: 404, match-regexr: ["(?s).*"], action: stop}
#     - {status: 408, match-regexr: ["(?s).*"], action: stop}
#     - {status: 500, match-regexr: ["(?s).*"], action: stop}
#     - {status: 502, match-regexr: ["(?s).*"], action: stop}
#     - {status: 503, match-regexr: ["(?s).*"], action: stop}
#     - {status: 504, match-regexr: ["(?s).*"], action: stop}
#   codex: *plan-limits-only
```

## requests

| Setting | What it does |
|---|---|
| `proxy-url` | An outbound proxy for every upstream request, such as `socks5://127.0.0.1:1080` or `http://proxy:3128`. A single account can use its own proxy instead; see [per-account proxies](MULTI-ACCOUNT.md#per-account-proxies-and-request-shaping). |
| `tls-handshake-timeout-seconds` | Seconds a new connection to `api.anthropic.com` (Claude) or `chatgpt.com` (Codex) may take to be ready, so a server that accepts the connection but never answers the TLS handshake fails the attempt and request retry can engage. The default `0` waits indefinitely. It never limits a request once connected. In cliproxy-rs it also covers the DNS lookup, the TCP connect and a proxy's CONNECT, which Go leaves unbounded. |

Claude accounts ignore `HTTPS_PROXY` and `HTTP_PROXY`. Without an explicit proxy, they connect directly. Set `requests.proxy-url` or the account's `proxy_url` when you need one. Claude API keys pointed at a custom, non-Anthropic base URL use the standard transport, which can inherit environment proxies.

## observability

| Setting | What it does |
|---|---|
| `logs.logging-to-file` | `true` writes the log to `main.log` in a `logs` folder, rotated, instead of standard output. |
| `logs.logs-max-total-size-mb` | The most disk space the log files may use. |
| `logs.request-log` | `true` writes one file per request with client and upstream request/response sections. Recognised sensitive headers and URL fields are masked, but bodies can contain credentials, including Realtime client secrets, as well as private prompts and output. Keep the logs private. |
| `usage.claude-cache-telemetry` | A cliproxy-rs addition, `false` by default. With `usage.usage-statistics-enabled: true` and management enabled, adds content-free final-wire Claude cache observations to the existing usage queue and RESP channel. No tokenizer or maintenance requests run. Hashes rotate on restart; additional records without returned usage have `usage_observed: false`. Large payloads add parsing and hashing work before upstream send. See [Claude cache telemetry](CLAUDE-CACHE-TELEMETRY.md). Go ignores the setting at startup, preserves it on v0 saves, and archives it on v8 edits. |
| `pprof.enable` | `false` by default. `true` starts a separate plain-HTTP debug listener, without API keys, management authentication or TLS. Keep it on loopback: its command-line route can expose arguments, and profiling consumes CPU. Changes apply without a restart. |
| `pprof.addr` | The debug listener address, `"127.0.0.1:8316"` by default, also used for an empty or whitespace-only value. Changes move the listener without a restart. A bind failure is logged without stopping the main server; another config publication retries it. |

The legacy top-level `pprof` mapping works too. `/debug/pprof/` lists the routes;
`cmdline` returns NUL-separated arguments and `symbol` accepts program counters in
the raw query or a POST body, separated by `+`. Go runtime profiles return plain-text
`501` answers. CPU `profile?seconds=30` requires a Unix build with
`cargo build --release -p cliproxy --features cpa-server/pprof` and returns a pprof
protobuf. Default builds, published release binaries and Docker images exclude the
profiler and symbol resolver. Their `symbol` route consumes POST bodies but returns
only `num_symbols: 1`, without resolving any names, including shared-library names.
With the feature, an unstripped build gives more symbol names. Do not use another
SIGPROF profiler in the same process. Listener changes allow five seconds for active
requests to finish. On timeout, the error is logged, active requests continue, and
no replacement starts until another config publication. Temporary accept errors
keep the listener open and back off from 5 ms to one second; config changes and
shutdown interrupt the wait. Addresses require numeric ports; unlike Go, `:` and
service-name ports such as `:http` are not accepted.

## models

| Setting | What it does |
|---|---|
| `catalog`, `codex-catalog`, `devin-catalog` | Where the model catalog (`models.json`), the Codex client catalog and the Devin catalog come from: an absolute local path or an `http`/`https` URL. Empty keeps the default. Any other value, such as a relative path, `~/models.json` or a `file://` URL, refuses the config, as in CLIProxyAPI v8.0.35. cliproxy-rs checks and keeps these settings but does not use them yet; see [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md). |

## worker-threads

`worker-threads: 4` at the top level sets how many threads serve requests. Without it, cliproxy-rs uses the smaller of the CPU count and two, which is plenty for one person: a coding-agent request with a 300 KB prompt costs about 20 to 30 ms of CPU (a small chat request about 1 ms), and most of a request's time is spent waiting on the provider. Each extra thread can keep its own pool of freed memory, so more threads mean a larger resident size. The `TOKIO_WORKER_THREADS` environment variable overrides it.

The key is read once at start from the `-config` file (`./config.yaml` by default), before a remote store (`PGSTORE_*`, `OBJECTSTORE_*`, `GITSTORE_*`) or Home supplies a config. It has no effect in a store's `config.yaml` or in the config Home supplies (the local `-config` file still applies); set `TOKIO_WORKER_THREADS` there. A value there that differs from the running thread count logs a warning.

This setting exists only in cliproxy-rs. CLIProxyAPI starts with it in the file and keeps it through v0 Management API saves, but any v8 configuration write comments it out, and uploading a whole `config.yaml` that contains it is refused. With a config shared between the two servers, use `TOKIO_WORKER_THREADS`. See [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Environment

- `MANAGEMENT_PASSWORD`: the management key, instead of `management.secret-key`.
- `TOKIO_WORKER_THREADS`: the number of request threads, instead of `worker-threads`.
- `RUST_LOG`: overrides the log level, for example `RUST_LOG=debug`.
- A `.env` file in the working directory is loaded at start, as in CLIProxyAPI.
- `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` keep the config and the account files in PostgreSQL, an S3-compatible bucket or a git repository, as in CLIProxyAPI. When the store has no config yet, the local one (or `config.example.yaml`) seeds it after a plaintext management key is replaced with its bcrypt hash and legacy settings that a v8 setting overrides are removed. A file in which the key's value would still appear as another setting's value, for example through a YAML anchor, is refused and the server does not start; comments and keys are not checked.
