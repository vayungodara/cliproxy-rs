# cliproxy-rs: product facts

Plain facts about cliproxy-rs for anyone writing about it. Each number says where it comes from. Nothing here is a claim about future work.

## What it is

cliproxy-rs is a Rust rewrite of [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI), the Go proxy by router-for-me. It follows CLIProxyAPI v8.0.10 (commit `6fecc6e`).

It is one local server. Tools send it OpenAI, Anthropic or Gemini API requests, and it serves them with the accounts and keys the user has connected: Claude and ChatGPT (Codex) subscriptions signed in through OAuth, Kimi, Meta, xAI and Devin accounts, Vertex AI service accounts, Claude, Codex and Gemini API keys, and OpenAI-compatible upstreams such as OpenRouter. It translates between the three API formats, spreads requests across accounts, and moves on to another account when one is rate-limited.

It started from a post on X by @maria_rcks on 2 October 2026: "why hasnt anyone built cliproxyapi but: rust, nice ui, support stuff like websockets".

## Who it is for

- People who use coding agents such as Claude Code, Amp or Codex CLI and want them to use their own subscriptions and keys through one endpoint.
- Current CLIProxyAPI users. cliproxy-rs reads the same `config.yaml`, the same credential files and the same management key, and serves the same routes and v8 Management API. Switching is stopping one binary and starting the other on the same files, and switching back works the same way.

## What it has

- One binary with the dashboard built in. Release builds for Linux (x86_64, arm64), macOS (Apple silicon, Intel) and Windows (x86_64), plus a Dockerfile.
- The client routes for chat completions, completions, Responses, Messages, token counting, Gemini `generateContent` and Interactions, model lists and the Codex paths, streaming and non-streaming.
- Image generation and editing (xAI, OpenAI-compatible and `gpt-image` models) and xAI video.
- WebSocket: the Responses WebSocket that Codex clients use, with response steering, and the Realtime WebSocket.
- Realtime and live voice through a Codex account (`/v1/realtime`, `/v1/live`, WebRTC call setup).
- Account sign-in from the command line or the dashboard for Claude, Codex (browser or device code), Kimi, Meta, xAI and Devin.
- Routing: round-robin, weighted and fill-first selection, retries, cooldowns, session affinity, model aliases and exclusions, payload rules, per-account proxies.
- The v8 Management API, HTTPS on the main port, config and credential hot reload, remote model catalog updates, LAN discovery, logging in Go's format with per-request access log lines and request log files.
- Plugins on Linux and macOS, Home mode, and the Postgres, object-storage and git storage backends.

## The dashboard

- A new design, built for this project in Svelte 5. It shows traffic per account for the last 200 minutes, account health and cooldowns, and covers connecting accounts, provider keys, client keys, models, payload rules, quotas, the full configuration (with a reviewed diff before saving), logs, usage, plugins and system information.
- Small: 46.9 KB of gzipped JavaScript and 4.7 KB of gzipped CSS. The build fails past 47,200 B and 6,853 B.
- Made for first-time users without getting in the way of everyone else: a three-step start that shows only until an account and a client key exist, a Use with tools page that tests your key and gives ready-to-copy settings for Claude Code, Codex CLI, Cursor and the OpenAI and Anthropic SDKs, account limits on the overview when known, and sign-in errors that say what to change.
- No external requests: no CDN, web fonts or analytics. The management key stays in the tab's memory and is never written to browser storage.
- When the server lacks a feature, the dashboard says so instead of showing an error or hiding the screen.
- It also ships as a single `management.html` (184 KB, 71 KB gzipped) that Go CLIProxyAPI servers can use in place of their own panel. It was tested against an unmodified Go 6fecc6e server.

## Measured numbers

From [BENCHMARKS.md](BENCHMARKS.md): commit `d068002` against the Go v8.0.10 release binary, same config, a 2-vCPU virtual machine, a local fake upstream, median of three runs.

- Memory at idle: 14.8 MB (Go: 45.1 MB). Under load: 24 to 25 MB (Go: 57 to 79 MB). With 256 slow streams open: 43 MB (Go: 104 MB).
- Startup to first answered request: 13 ms (Go: 98 ms).
- Binary: 33.0 MB, 14.0 MB as a release archive (Go: 69.1 MB and 22.9 MB).
- Throughput: Go handled about 20% more non-streaming requests per second (1,696 against 1,403). Fast streams were level (956 against 940 per second). Translated Anthropic-format streams ran at 913 per second against Go's 613, with lower CPU per request. Do not describe cliproxy-rs as faster than Go overall.

## Gaps today

- Providers not supported yet: Antigravity and AI Studio.
- The WebRTC media relay for live calls is an optional build feature, not in the release binaries.
- Request log files lack the upstream request and response sections. No terminal UI, no pprof.
- Codex CLI has not been tested end to end against cliproxy-rs.
- The Windows build comes from the release workflow and has not been run by hand. The macOS builds are not signed or notarized.
- [MIGRATING-FROM-GO.md](MIGRATING-FROM-GO.md) and [PARITY-STATUS.md](PARITY-STATUS.md) list every difference.

## Risks users should know

Using subscription accounts outside the providers' own apps may break their terms of service, and providers can limit or close accounts. API keys are the supported way to use their models from other software. The README says this plainly, and the project is not affiliated with any provider.

## How it was made

Built with some help from AI. Behaviour is checked against Go with recorded Go outputs and a harness that sends the same requests to both servers.

## License and status

MIT licensed, as is CLIProxyAPI. The repository is https://github.com/vayungodara/cliproxy-rs. No release has been published yet.
