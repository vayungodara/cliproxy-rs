# Compatibility and parity with CLIProxyAPI

cliproxy-rs aims to behave like CLIProxyAPI at commit [`6fecc6e`](https://github.com/router-for-me/CLIProxyAPI/tree/6fecc6e5567912661654a4eaf9b8f5436facd1c2) (v8.0.10): the same `config.yaml`, the same credential files, the same routes and the same answers. To track how close it is, every route, translation, config setting, plugin method and Go test file in that commit is listed as one item, 1,687 in all, and each item is audited against this repository.

## What works today

- Client APIs: `POST /v1/messages` and `/v1/messages/count_tokens` (Anthropic), `POST /v1/chat/completions`, `/v1/completions`, `/v1/responses` and `/v1/responses/compact` (OpenAI), `/v1beta/models/...` and `/v1beta/interactions` (Gemini), `GET /v1/models`, and the Codex paths under `/backend-api/codex/`. Streaming (SSE) and non-streaming, with format translation between the three protocols.
- Images and video: `POST /v1/images/generations` and `/v1/images/edits` through Codex accounts, xAI and OpenAI-compatible upstreams, and the `/v1/videos` and `/openai/v1/videos` routes for xAI video.
- WebSocket: the Responses WebSocket on `GET /v1/responses` and `GET /backend-api/codex/responses`, used by Codex clients, including response steering (`oauth.providers.codex.response-steering`).
- Realtime and live through a Codex account: `/v1/realtime` (WebSocket and WebRTC calls), `/v1/live`, call sidebands and local ephemeral keys (`/v1/realtime/client_secrets`). The WebRTC media relay is an optional build feature (`cargo build --release -p cliproxy --features cpa-server/media-relay`) and is not in the release binaries.
- Providers: Claude (OAuth and API keys), Codex (OAuth and API keys), Kimi, Meta, xAI, Devin, Gemini API keys and Gemini Interactions, Vertex AI (service accounts imported with `-vertex-import`, and API keys), AI Studio (through the `/v1/ws` browser relay), and OpenAI-compatible upstreams such as OpenRouter.
- Account sign-in from the command line or the dashboard: Claude, Codex (browser or device code), Kimi, Meta, xAI and Devin.
- Routing: round-robin, weighted and fill-first selection, retries, cooldowns, session affinity, model aliases and exclusions, payload rules, per-credential and global proxies.
- The v8 Management API for configuration, credentials, OAuth sign-in, quota checks (`/requests/api-call`), usage counters, logs and model catalogs, plus the dashboard at `/management.html`.
- HTTPS on the main port (`server.tls`), logging in Go's format to stdout or a rotating `main.log` with Go's per-request access log lines, request log files with client and upstream sections, LAN discovery (`-discover` and the `server.discovery` advertisement), `.env` loading, and remote model catalog updates as in Go (`--local-model` turns them off).
- Plugins (`plugins`, Linux and macOS): loading and configuration, plugin-defined routes, the Management API routes to list, enable, configure and delete plugins, and the plugin quota routes.
- Home mode (`-home-jwt`): bootstrap, config updates, request dispatch, usage, process and request logs, in-flight reporting and shared KV state. Home-managed plugin sync, tasks and status reporting remain unavailable.
- The `PGSTORE_*`, `OBJECTSTORE_*` and `GITSTORE_*` storage backends, and the Redis-protocol usage subscriber on the main port.
- Config and credential files are watched and reloaded without a restart. A plaintext management key is hashed on first start, as Go does.

The dashboard says when the server lacks an endpoint instead of failing: actions it cannot do are disabled and named, and pages it cannot load say which route is missing. What is still missing is listed under [Upcoming features](../README.md#upcoming-features), and the deliberate differences from Go are in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md).

## Where it stands

<!-- parity-summary:start -->
Audit of 2026-10-05.

| Milestone | Items | Covered | Partial | Missing |
|---|---:|---:|---:|---:|
| M1 | 118 | 53 | 65 | 0 |
| M2 | 158 | 111 | 40 | 7 |
| M3 | 350 | 190 | 112 | 48 |
| M4 | 510 | 232 | 258 | 20 |
| M5 | 303 | 180 | 102 | 21 |
| M6 | 248 | 69 | 130 | 49 |
| All | 1687 | 835 | 707 | 145 |
<!-- parity-summary:end -->

An item is covered when it is implemented and a Rust test or a fixture recorded from the Go server checks it. Partial means it works but not every case is pinned by a test, or only part of it is implemented; most partial items are Go test files whose behaviour other tests cover without porting each case. Missing means it is not implemented. A deliberate difference from Go, listed in [DIFFERENCES-FROM-GO.md](DIFFERENCES-FROM-GO.md), counts as covered.

## What each milestone covers

| Milestone | Scope |
|---|---|
| M1 | Claude: OAuth sign-in, Messages passthrough, request shaping, model catalogs and client keys |
| M2 | Translation between the OpenAI Chat, Completions and Responses formats, Gemini and Interactions, and Claude |
| M3 | The other providers: Codex, Antigravity, AI Studio, Vertex, Gemini keys, Kimi, xAI, Meta, Devin and OpenAI-compatible upstreams |
| M4 | Routing and operations: account selection, cooldowns, retries, payload rules, aliases, proxies, logging and the shared config |
| M5 | The Management API, WebSockets, realtime and live voice, and access control |
| M6 | Plugins, the terminal UI, LAN discovery and Home (cluster) mode |

Most missing items are in M6 (plugin runtime integration and parts of Home mode) and M3 (Antigravity). The plugin store and terminal UI exist, though some Go cases remain unported. The README's [Upcoming features](../README.md#upcoming-features) list summarises the feature gaps.

The audit combines a source scan with saved route probes and manual judgments. Rerunning it reuses those saved probes and judgments. Live-provider compatibility needs separate testing. Counts can lag runtime wiring; a method implemented in a host library may still be unused by the server. The feature descriptions above follow the server's actual call paths.

## The audit files

Everything behind the numbers is in [`docs/parity-audit/`](parity-audit):

- [`checklist.md`](parity-audit/checklist.md) lists every item with a link to the Go source it comes from.
- [`status.md`](parity-audit/status.md) gives each item its status, the evidence (the Rust code and tests that cover it) and a note.
- [`manual.tsv`](parity-audit/manual.tsv) holds the judgments made by reading code, with their evidence.
- [`audit.py`](parity-audit/audit.py) rebuilds `status.md` and the table above. [`probe.py`](parity-audit/probe.py) starts the binary and requests every route in the checklist without credentials, to see which ones are routed; its results are in `routes.json`.

Run `python3 docs/parity-audit/audit.py` from the repository root after a change to refresh both.
