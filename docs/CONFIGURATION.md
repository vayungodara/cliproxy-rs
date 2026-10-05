# Configuration

cliproxy-rs reads one YAML file, `config.yaml`. Pass its path with `--config`; without it, the server reads `config.yaml` in the current directory. It accepts CLIProxyAPI's v8 layout (shown here) and the older flat layout, and every setting in CLIProxyAPI's [`config.example.yaml`](https://github.com/router-for-me/CLIProxyAPI/blob/6fecc6e5567912661654a4eaf9b8f5436facd1c2/config.example.yaml) is accepted. Settings for features that are not ported yet are kept in the file but have no effect.

The server watches the file and applies most changes within a second, without a restart. `server.host`, `server.port`, `server.tls` and `server.trusted-proxies` are read at start, so restart after changing them. When the dashboard or the Management API changes a setting, only that key is rewritten; the rest of the file, comments included, stays as it was.

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
| `strategy` | `round-robin` (the default), `fill-first`, `weighted-round-robin`, or `soonest-reset` (experimental, a cliproxy-rs addition: spend the account whose weekly window resets soonest first). |
| `session-affinity` | `true` keeps a conversation on the account that served its first request. Off by default. |
| `session-affinity-ttl` | How long a conversation stays bound to its account, `"1h"` by default. |
| `retry.request-retry` | Extra rounds over the accounts after a failed attempt, `0` by default. |
| `retry.max-retry-credentials` | At most this many accounts tried per round; `0` means all of them. |
| `retry.max-retry-interval` | The longest wait, in seconds, for an account to come out of cooldown before retrying; `0` means do not wait. |
| `cooldown.disable-cooling` | `true` keeps failing accounts in rotation instead of resting them. |
| `cooldown.transient-error-cooldown-seconds` | How long to rest an account after a temporary upstream error; `0` means 60 seconds, a negative value turns it off. |
| `cooldown.save-cooldown-status` | `true` keeps cooldowns in `.cds` files in `auth-dir`, so a restart does not forget them. |
| `cooldown.max-trusted-cooldown` | A cliproxy-rs addition. When an account hits its usage limit, the provider says when it resets, sometimes days ahead, and the account rests until then. Providers often reset early, so cliproxy-rs trusts the stated time for at most this long (`"1h"` by default, also written as seconds), then lets the next request through to check. If the account is still limited, the next rest is twice as long, and never longer than the stated reset. `0` trusts the stated reset, as CLIProxyAPI does. See [differences from Go](DIFFERENCES-FROM-GO.md#deliberate-differences). |

## requests

| Setting | What it does |
|---|---|
| `proxy-url` | An outbound proxy for every upstream request, such as `socks5://127.0.0.1:1080` or `http://proxy:3128`. A single account can use its own proxy instead; see [per-account proxies](MULTI-ACCOUNT.md#per-account-proxies-and-request-shaping). |

## observability

| Setting | What it does |
|---|---|
| `logs.logging-to-file` | `true` writes the log to `main.log` in a `logs` folder, rotated, instead of standard output. |
| `logs.logs-max-total-size-mb` | The most disk space the log files may use. |
| `logs.request-log` | `true` writes one file per request with the request and the response, for debugging. Request bodies are written as they are, so these files can hold prompts. |

## Environment

- `MANAGEMENT_PASSWORD`: the management key, instead of `management.secret-key`.
- `RUST_LOG`: overrides the log level, for example `RUST_LOG=debug`.
- A `.env` file in the working directory is loaded at start, as in CLIProxyAPI.
- `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` keep the config and the account files in PostgreSQL, an S3-compatible bucket or a git repository, as in CLIProxyAPI.
