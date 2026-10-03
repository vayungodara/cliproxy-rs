# cliproxy-rs: product facts

Plain facts about cliproxy-rs for anyone writing about it. Each number says where it comes from. Nothing here is a claim about future work.

## What it is

cliproxy-rs is a Rust rewrite of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI), the Go proxy by router-for-me. It follows CLIProxyAPI v8.0.10 (commit `6fecc6e`).

It is one local server. Tools send it OpenAI, Anthropic or Gemini API requests, and it serves them with the accounts and keys the user has connected: Claude and ChatGPT (Codex) subscriptions signed in through OAuth, Kimi, Meta, xAI and Devin accounts, Claude, Codex and Gemini API keys, and OpenAI-compatible upstreams such as OpenRouter. It translates between the three API formats, spreads requests across accounts, and moves on to another account when one is rate-limited.

It started from a post on X by @maria_rcks on 2 October 2026: "why hasnt anyone built cliproxyapi but: rust, nice ui, support stuff like websockets".

## Who it is for

- People who use coding agents such as Claude Code, Amp or Codex CLI and want them to use their own subscriptions and keys through one endpoint.
- Current CLIProxyAPI users. cliproxy-rs reads the same `config.yaml`, the same credential files and the same management key, and serves the same routes and v8 Management API. Switching is stopping one binary and starting the other on the same files, and switching back works the same way.

## What it has

- One binary with the dashboard built in. Release builds for Linux (x86_64, arm64), macOS (Apple silicon, Intel) and Windows (x86_64), plus a Dockerfile.
- The client routes for chat completions, completions, Responses, Messages, token counting, Gemini `generateContent` and Interactions, model lists and the Codex paths, streaming and non-streaming.
- WebSocket: the Responses WebSocket that Codex clients use, and the Realtime WebSocket.
- Realtime and live voice through a Codex account (`/v1/realtime`, `/v1/live`, WebRTC call setup).
- Account sign-in from the command line for Claude, Codex (browser or device code), Kimi, Meta, xAI and Devin; from the dashboard for Claude, Codex, Kimi and Meta.
- Routing: round-robin, weighted and fill-first selection, retries, cooldowns, session affinity, model aliases and exclusions, payload rules, per-account proxies.
- The v8 Management API, HTTPS on the main port, config and credential hot reload, remote model catalog updates, LAN discovery, logging in Go's format.

## The dashboard

- A new design, built for this project in Svelte 5. It shows traffic per account for the last 200 minutes, account health and cooldowns, and covers connecting accounts, provider keys, client keys, models, payload rules, quotas, the full configuration (with a reviewed diff before saving), logs, usage, plugins and system information.
- Small: 42.3 KB of gzipped JavaScript and 4.5 KB of gzipped CSS. The build fails if it grows past 42,642 B and 6,853 B, the size of the dashboard it replaced.
- No external requests: no CDN, web fonts or analytics. The management key stays in the tab's memory and is never written to browser storage.
- When the server lacks a feature, the dashboard says so instead of showing an error or hiding the screen.
- It also ships as a single `management.html` (169 KB, 66 KB gzipped) that Go CLIProxyAPI servers can use in place of their own panel. It was tested against an unmodified Go 6fecc6e server.

## Measured numbers

From [BENCHMARKS.md](BENCHMARKS.md): commit `4abce40` against the Go v8.0.10 release binary, same config, a 2-vCPU virtual machine, a local fake upstream, median of three runs.

- Memory at idle: 13.8 MB (Go: 44.6 MB). Under load: 22 to 23 MB (Go: 57 to 79 MB). With 256 slow streams open: 42 MB (Go: 104 MB).
- Startup to first answered request: 15 ms (Go: 82 ms).
- Binary: 29.6 MB, 12.6 MB as a release archive (Go: 69.1 MB and 22.9 MB).
- Throughput: Go handled about 20% more non-streaming requests per second (1,677 against 1,402) and more fast streams (989 against 627 per second). cliproxy-rs handled slightly more translated Anthropic-format streams (640 against 607) with lower CPU per request. Do not describe cliproxy-rs as faster than Go overall.

## Gaps today

- Providers not supported yet: Antigravity, AI Studio, Vertex. xAI and Devin can be signed in from the command line only.
- No image or video endpoints, no Responses WebSocket steering.
- The WebRTC media relay for live calls is an optional build feature, not in the release binaries.
- No request log files, no access log lines, no plugins, no terminal UI, no Home control plane, no pprof.
- Codex CLI asks `/v1/models` for its own catalog format; cliproxy-rs answers with the OpenAI list, so Codex may not show every model. Codex CLI has not been tested end to end.
- The Windows build comes from the release workflow and has not been run by hand. The macOS builds are not signed or notarized.
- [MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md) and [PARITY-STATUS.md](PARITY-STATUS.md) list every difference.

## Risks users should know

Using subscription accounts outside the providers' own apps may break their terms of service, and providers can limit or close accounts. API keys are the supported way to use their models from other software. The README says this plainly, and the project is not affiliated with any provider.

## How it was made

cliproxy-rs was written with AI coding agents in [Amp](https://ampcode.com), working in parallel threads against the Go source as the reference, under the maintainer's direction. Behaviour is checked against Go with recorded Go outputs and a harness that sends the same requests to both servers.

## License and status

MIT licensed, as is CLIProxyAPI. The repository is https://github.com/vayungodara/cliproxy-rs. No release has been published yet.
