# cliproxy-rs

cliproxy-rs is a Rust rewrite of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI). It runs one local server that accepts OpenAI, Anthropic and Gemini API requests and serves them with the accounts and API keys you connect: Claude and ChatGPT (Codex) subscriptions signed in through OAuth, Kimi, Meta, xAI and Devin accounts, Gemini API keys, and any OpenAI-compatible upstream. Coding tools such as Claude Code and Amp, and clients built on the OpenAI, Anthropic or Gemini SDKs, point at it as if it were the provider.

It reads the same `config.yaml` and the same credential files as CLIProxyAPI v8, and serves the same HTTP routes and v8 Management API. A Go user can stop the Go binary, start this one on the same directory, and keep their accounts. It ships as a single binary with the management dashboard built in.

![The dashboard overview, light theme](docs/img/dashboard-overview-light.png)

## Status

cliproxy-rs follows CLIProxyAPI at commit `6fecc6e` (v8.0.10). It is new and does not cover everything the Go server does yet. [docs/PARITY-STATUS.md](docs/PARITY-STATUS.md) tracks every route, setting and test suite item by item.

Works today:

- Client APIs: `POST /v1/messages` and `/v1/messages/count_tokens` (Anthropic), `POST /v1/chat/completions`, `/v1/completions`, `/v1/responses` and `/v1/responses/compact` (OpenAI), `/v1beta/models/...` and `/v1beta/interactions` (Gemini), `GET /v1/models`, and the Codex paths under `/backend-api/codex/`. Streaming (SSE) and non-streaming, with format translation between the three protocols.
- WebSocket: the Responses WebSocket on `GET /v1/responses` and `GET /backend-api/codex/responses`, used by Codex clients.
- Realtime and live through a Codex account: `/v1/realtime` (WebSocket and WebRTC calls), `/v1/live`, call sidebands and local ephemeral keys (`/v1/realtime/client_secrets`).
- Providers: Claude (OAuth and API keys), Codex (OAuth and API keys), Kimi, Meta, xAI, Devin, Gemini API keys and Gemini Interactions, and OpenAI-compatible upstreams such as OpenRouter.
- Account sign-in from the command line or the dashboard: Claude, Codex (browser or device code), Kimi, Meta, xAI and Devin.
- Routing: round-robin, weighted and fill-first selection, retries, cooldowns, session affinity, model aliases and exclusions, payload rules, per-credential and global proxies.
- The v8 Management API for configuration, credentials, OAuth sign-in, quota checks (`/requests/api-call`), usage counters, logs and model catalogs, plus the dashboard at `/management.html`.
- HTTPS on the main port (`server.tls`), logging in Go's format to stdout or a rotating `main.log`, LAN discovery (`-discover` and the `server.discovery` advertisement), `.env` loading, and remote model catalog updates as in Go (`-local-model` turns them off).
- Config and credential files are watched and reloaded without a restart. A plaintext management key is hashed on first start, as Go does.

Not yet supported (at the time of writing):

- Providers: Antigravity, AI Studio, Vertex.
- Image and video endpoints, and Responses WebSocket steering.
- The WebRTC media relay for live calls is an optional build feature and is not in the release binaries.
- Request log files, an access log, plugins, the terminal UI (`-tui`) and the Home control plane (`-home-jwt`).

The dashboard says when the server lacks an endpoint instead of failing: actions it cannot do are disabled and named, and pages it cannot load say which route is missing.

## Install

Download a binary for Linux, macOS or Windows from the [releases page](https://github.com/vayungodara/cliproxy-rs/releases), or build from source. See [docs/INSTALL.md](docs/INSTALL.md) for both, plus Docker and running as a service.

```sh
cargo build --release -p cliproxy     # needs Rust, cmake, clang and perl (BoringSSL)
./target/release/cliproxy --config config.yaml
```

## Quick start

A minimal `config.yaml`:

```yaml
server:
  host: "127.0.0.1"   # listen on this machine only
  port: 8317
access:
  api-keys:
    - "sk-change-me-client-key"      # what your tools send to the proxy
management:
  secret-key: "change-me-management" # for the dashboard; hashed on first start
oauth:
  auth-dir: "~/.cli-proxy-api"       # where credential files live
```

Sign in to an account and start the server:

```sh
cliproxy --config config.yaml --claude-login    # or --codex-login, --codex-device-login, --kimi-login, --meta-login, --xai-login, --devin-login
cliproxy --config config.yaml
```

On a machine without a browser, add `--no-browser` and open the printed URL elsewhere. You can also connect accounts from the dashboard at `http://127.0.0.1:8317/management.html`.

Check it works:

```sh
curl http://127.0.0.1:8317/v1/messages \
  -H "Authorization: Bearer sk-change-me-client-key" \
  -H "anthropic-version: 2023-06-01" -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-5-20250929","max_tokens":32,"messages":[{"role":"user","content":"Say ok"}]}'
```

`GET /v1/models` with the same key lists the models your connected accounts can serve.

## Use with Claude Code

Point Claude Code at the proxy with its gateway variables. `ANTHROPIC_AUTH_TOKEN` is sent as `Authorization: Bearer` and `ANTHROPIC_API_KEY` as `x-api-key`; cliproxy-rs accepts either.

```sh
export ANTHROPIC_BASE_URL=http://127.0.0.1:8317
export ANTHROPIC_AUTH_TOKEN=sk-change-me-client-key
claude
```

To keep the setting, put both variables in the `env` block of `~/.claude/settings.json`. Run `/status` in Claude Code to confirm the base URL and credential it uses. The proxy picks a connected account for each request, so several Claude subscriptions can share the load. See Anthropic's [gateway guide](https://docs.claude.com/en/docs/claude-code/llm-gateway-connect) for other surfaces such as the VS Code extension.

## Use with Amp

Amp sends model requests from its own servers, so it needs a URL it can reach over the internet. Expose the proxy through a tunnel or reverse proxy with HTTPS (for example Tailscale Funnel or Cloudflare Tunnel), and read the security notes below first.

In Amp, open Settings, then Model Routing, add a Custom URL connection and set:

- Base URL: your public URL, for example `https://proxy.example.com` (Amp appends `/v1/messages`).
- API format: Anthropic Messages, for Claude models through your Claude accounts. Use the Chat Completions or Responses format for OpenAI-style models.
- API key: one of your `access.api-keys`. Amp sends it as `Authorization: Bearer`.
- Models: map Amp's model IDs to the IDs from your proxy's `/v1/models`, one per line, such as `anthropic/claude-sonnet-4-6 -> claude-sonnet-4-6`.

Make sure the connection is active and ordered before other connections that serve the same models. `amp config model-providers check-access --provider-model <model>` shows which connection served a test request. Amp's [model routing docs](https://ampcode.com/docs/customize/model-routing) describe every field.

CLIProxyAPI had a separate Amp integration module in older versions. It was removed upstream, and cliproxy-rs does not have one. The Custom URL connection replaces it.

## Use with other clients

Any OpenAI-compatible client works with base URL `http://127.0.0.1:8317/v1` and one of your client keys as the API key. Gemini clients use `http://127.0.0.1:8317` with the key in `x-goog-api-key` or `?key=`.

Codex CLI can use the proxy as a custom model provider in `~/.codex/config.toml`:

```toml
model_provider = "cliproxy"

[model_providers.cliproxy]
name = "cliproxy-rs"
base_url = "http://127.0.0.1:8317/v1"
env_key = "CLIPROXY_API_KEY"   # export CLIPROXY_API_KEY=sk-change-me-client-key
wire_api = "responses"
supports_websockets = true
```

cliproxy-rs serves the Responses API, the Responses WebSocket and the Codex client model catalog (`/v1/models?client_version=...`) that Codex uses, but this setup has not been tested end to end with Codex CLI.

## The dashboard

The management dashboard is built into the binary at `/management.html`. It shows traffic per credential for the last 200 minutes, credential health and cooldowns, and lets you connect accounts, edit provider keys, client keys, model aliases, payload rules and the whole configuration with a reviewed diff before saving. It needs `management.secret-key` to be set. The key you type stays in the browser tab's memory and is never stored.

The same dashboard also works with the Go server as a drop-in `management.html`. See [ui/PANEL.md](ui/PANEL.md).

## Security

- Set `access.api-keys`. With an empty list the proxy accepts requests from anyone who can reach it and spends your accounts' quota for them. The server logs a warning at start when the list is empty.
- Keep `server.host` on `127.0.0.1` unless other machines need access. The default empty host listens on every interface.
- The management API is off until `management.secret-key` (or the `MANAGEMENT_PASSWORD` environment variable) is set. With `management.allow-remote: false`, only requests from `127.0.0.1` or `::1` are accepted. Five wrong keys from one address block it for 30 minutes.
- Behind a tunnel or reverse proxy on the same machine (cloudflared, Tailscale Funnel, Caddy, nginx), every request arrives from `127.0.0.1`, so the server treats every internet client as local. Then `allow-remote: false` no longer keeps them out of the management API, a local-only `--password` is accepted from the internet, and five wrong keys from anyone ban the tunnel's address, which locks everyone out. Set `server.trusted-proxies` to the proxy's address, for example `[127.0.0.1, "::1"]` for a local cloudflared, and restart. The server then takes the client address from `X-Forwarded-For` and similar headers sent by that proxy only. This applies to the Go server in the same way. cliproxy-rs logs a warning the first time a forwarded management request arrives without the setting. If you do not need remote management, also consider leaving `management.secret-key` empty on an exposed server.
- Do not send client keys or the management key over plain HTTP across a network. Terminate TLS in the tunnel or reverse proxy, or serve HTTPS directly with `server.tls` (`enable: true` plus `cert` and `key` file paths).
- Credential files in `auth-dir` hold OAuth refresh tokens. Anyone who can read them can use the accounts. Keep the directory private (the server writes them with mode 0600) and treat downloaded credential files and config backups the same way.

## Account risk

cliproxy-rs uses subscription accounts (Claude Pro and Max, ChatGPT Plus and Pro, Kimi, Meta) outside the providers' own apps, and it shapes requests to look like the official command-line clients. Providers' terms of service may not allow this. A provider can rate-limit, suspend or close an account it believes is used this way, and you are responsible for how you use your accounts. Sharing one subscription between several people, or serving other users through a public endpoint, makes that more likely. API keys from the providers' developer platforms are billed per request and are the supported way to use their models from other software.

This project is not affiliated with Anthropic, OpenAI, Google, Moonshot AI, Meta or any other provider.

## Compatibility with CLIProxyAPI

The same files work for both servers. [docs/MIGRATING-FROM-GO.md](docs/MIGRATING-FROM-GO.md) lists what carries over, which command-line flags and settings differ, the [deliberate differences from Go CLIProxyAPI](docs/MIGRATING-FROM-GO.md#differences-from-go-cliproxyapi), and how to switch back.

## Performance

On a 2-vCPU test machine with a local fake upstream, cliproxy-rs used 14 MB of memory at idle against Go's 45 MB, and 22 to 42 MB under load against 57 to 104 MB. Go handled about 20% more non-streaming requests per second, and more fast streams. The release binary is 29.6 MB against Go's 69.1 MB. [docs/BENCHMARKS.md](docs/BENCHMARKS.md) has the method, every number and the caveats.

## Development

The workspace is `crates/cpa-core` (config and credential formats), `crates/cpa-exec` (one module per upstream provider), `crates/cpa-translate` (protocol translation), `crates/cpa-server` (routes, selection, management API), `crates/cliproxy` (the binary) and `ui/` (the dashboard, see [ui/README.md](ui/README.md)). Tests run against local mock upstreams only:

```sh
cargo test --workspace
```

This project was written with AI coding agents in [Amp](https://ampcode.com), working in parallel threads against the Go source as the reference, under the maintainer's direction.

## License

MIT. See [LICENSE](LICENSE). CLIProxyAPI, which this project follows closely, is also MIT licensed.
