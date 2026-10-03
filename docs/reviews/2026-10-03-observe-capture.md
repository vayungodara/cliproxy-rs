# Request capture contract and adoption

Reference: CLIProxyAPI `6fecc6e`. M4-0250 remains partial until provider owners
adopt this seam. M4-0254 remains the accepted partial: corrupt Brotli may withhold
partial decoded bytes and codec diagnostic wording differs. Normal compressed
bodies, raw fallback bytes, downstream capture, retention and commercial mode
are implemented. M4-0304 remains partial for Go recovery cases.

## Additive contracts

`cpa_core::exec::{CaptureSink, CaptureObserver, CaptureEvent, UpstreamRequest}`
are an optional, default no-op per-call observer. Use `request.capture().clone()`
before spawning stream workers. `CaptureSink::record(event)` performs no work
without an observer. `UsageSink::with_capture` carries this private adjunct without
adding a field to every existing `ExecRequest` literal. Usage tracker replacement
preserves it; usage accounting and wire logging are independent.

`Trace::with_request_id(Option<String>)`, `Trace::request_id()` and the additive
`Trace::with_capture(CaptureSink)` let connection tasks retain middleware identity
and capture explicitly. Ordinary HTTP handlers using `dispatch::serve` (OpenAI
Chat/Completions/Responses/Compact, Claude Messages/Count, Gemini, images and videos)
already adopt middleware identity and capture before keepalive deferral. Responses
WebSocket and Realtime own their connection lifetimes and must pass them explicitly.
Go's trace header is timestamp-auth-index-request-ID, in that order.

`request_logging::RequestLog` is in request extensions. `capture_sink()` exposes
the core seam. `request_id()` exposes the full middleware UUID. For downstream
Responses WebSocket, call `detach_websocket()` before returning 101; it returns
`None` when disabled. Its `WebsocketLog::append_part(&[u8])` accepts one complete
Go-formatted timeline part (`Timestamp: <RFC3339Nano>\nEvent:
websocket.<request|response|disconnect>\n<payload>\n`). Parts get a blank-line
separator. `close(self).await` waits for final delivery; dropping the sink schedules
delivery. Canceling a close waiter does not cancel the independently owned writer.

`RemoteDispatch::request_log(Vec<u8>)` is a default no-op addition. The binary's
Home dispatcher forwards Go's `{headers,request_id,request_log}` payload with
`Client::rpush_request_log`; full Home logs never fall back to local files. Headers
in this envelope are intentionally original, as in Go; text headers are masked.
Forced error logs with normal logging disabled stay local.

## Provider adoption table

Paths in the Go column are relative to `internal/runtime/executor`, except the
two explicitly named Live paths. Each line number identifies a `RecordAPIRequest`
call site in the pinned reference. These are 37 sites (35 executor sites plus
two Live sites). Several sites share one Rust send path; capture every physical
attempt, not once per logical request. Do not log refresh/login requests that
have no incoming call observer.

All HTTP sites use `request.capture().record(CaptureEvent::Request(UpstreamRequest
{ ... }))` just before sending. Follow with `ResponseMetadata(status, headers)`
after headers, `ResponseChunk(bytes)` at Go's whole-body/scanner-chunk sites, and
`ResponseError(text)` at Go's transport/read errors. Fill provider/auth fields from
the selected account exactly as the Go call site does. Pass header pairs in Go
HTTP-header spelling, retaining duplicate values. The sink sorts and masks them;
never pre-mask API keys twice. Bodies are intentionally unredacted.

| Owner | Pinned Go request sites | Rust send owner |
|---|---|---|
| Claude | `claude_executor_execute.go:317`, `claude_executor_stream.go:311`, `claude_executor_tokens.go:233` | `cpa-exec/src/claude.rs`: `generate_with`, `count_tokens`, `send`; raw stream reads in `claude/stream.rs` |
| Codex | `codex_executor_execute.go:93,264`, `codex_executor_stream.go:101` | `cpa-exec/src/codex.rs`: `send`, `buffered`, `compact`, `stream_with_session` |
| Codex images | `codex_openai_images.go:693` | `cpa-exec/src/codex.rs` HTTP path; `cpa-server/src/images.rs`: `Prepared::raw/normalized` routes |
| OpenAI/xAI | `openai_compat_executor.go:169,266,389,637` | `cpa-exec/src/openai_compat.rs`, `openai_compat_http.rs`: generate/stream/media sends and count paths |
| OpenAI/xAI | `xai_executor_request.go:184` | `cpa-exec/src/xai_request.rs` HTTP request path |
| Google | `gemini_executor.go:203,322,456,541,716` | `cpa-exec/src/gemini.rs`: `generate`, `count_tokens`, native image generation |
| Google | `gemini_vertex_executor.go:393,530,653,817,962,1056` | `cpa-exec/src/vertex.rs`: `generate`, `count_tokens`, Imagen; service-account and API-key branches |
| Google | `aistudio_executor.go:164,244,457` | AIStudio executor absent in this base; attach the same seam when it lands |
| Google | `antigravity_executor_request.go:143`, `antigravity_executor_tokens.go:109` | Antigravity executor absent in this base; attach the same seam when it lands |
| Device | `kimi_executor.go:180,319,470,602` | `cpa-exec/src/kimi.rs`, `kimi_http.rs`: generate/stream/count/compact sends |
| Device | `devin_executor.go:254,336` | `cpa-exec/src/devin.rs`, `devin_request.rs`: generate/count sends |
| Device | `meta_executor_execute.go:253` | `cpa-exec/src/meta.rs`, `meta_wire.rs`: execute HTTP send |
| Realtime | `internal/client/codex/live/live.go:309` | `cpa-server/src/realtime/http.rs`: `call`; `cpa-exec/src/codex_live.rs`: `live_post` |
| Realtime | `internal/client/codex/live/capabilities.go:155` | `cpa-server/src/realtime/http.rs`: `hangup` |

Additional non-executor call sites: `internal/api/server_routes.go:461` belongs
to the Codex alpha-search owner (`cpa-server/src/codex_alpha.rs`), and
`internal/pluginhost/http_bridge.go:195` belongs to the Plugins owner
(`cpa-plugin/src/executor.rs`, `Host::executor_http_request`). Management `/api-call`
is excluded from inbound capture, as in Go.

For upstream WebSockets use `WebsocketRequest`, `WebsocketHandshake`,
`WebsocketResponse`, and `WebsocketError { stage, error }`. A rejected upgrade is
an HTTP attempt using Request/ResponseMetadata/ResponseChunk. The URI must be the
HTTP upgrade URI (`ws` to `http`, `wss` to `https`), as Go's helper requires.

## Proof and review

Real Go harnesses are under `cpa-server/tests/reference/{capture,upstream_capture}`;
fixtures are generated with external networking denied. Tests cover bytes,
normal codecs, accepted malformed-codec ceiling, SSE spacing, retries, duplicate
metadata, missing requests, OAuth value omission, disabled capture, reload,
HEAD bodies, nested served composition, CORS, Home forwarding, cancel/drop,
bounded deferred bodies, sticky disk errors and the nonblocking stream queue.

Oracle findings fixed: Home forwarding; nested-router HEAD/header snapshots;
CORS headers missing from inner snapshots; interrupted WebSocket close losing
delivery; deferred requests incorrectly mixed into hot-enabled normal logs.
No finding was rejected beyond the integrator's settled malformed-codec ceiling.
