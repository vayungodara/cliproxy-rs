# Foundation review (oracle, 2026-10-02, skeleton commit 99a3067)

Go reference: `/home/vayun/projects/.amp/in/CLIProxyAPI` at `6fecc6e`. This file is the spec for the contracts rewrite. Go paths below are relative to that clone.

## (a) Seam changes to make before fan-out

### 1. Generic runtime credential with provider-specific views

Replace `AppState`'s `Vec<ClaudeCredential>` with a shared credential store holding, per credential:

- Stable ID and an explicit source: file path versus config entry.
- Provider and common routing controls, including disabled state.
- Config-derived attributes.
- The complete mutable JSON metadata, preserving unknown fields.
- A revision/generation for refresh and management updates.

Keep a borrowed, validated Claude view inside `cpa-exec`; the five-field `ClaudeCredential` must not be the stored source of truth. Do not transliterate every field of Go's `Auth` either.

- Go distinguishes config-derived attributes from mutable credential metadata: `sdk/cliproxy/auth/types.go` L47-108.
- Claude execution needs metadata beyond tokens (account identity, device pool, fingerprint settings, source). Request-time preparation mutates that metadata: `internal/runtime/executor/claude_executor_auth.go` L83-149.

Refresh returns a metadata patch; it does not rewrite a private credential copy. One store serializes refresh, management edits and reloads, preserves unrelated fields and rejects stale results (revision check). Singleflight coordination wraps the store; provider code owns only the OAuth exchange. File publication is atomic (temp file + rename) with restrictive permissions (0600), and memory is updated consistently after persistence.

### 2. Execution envelope instead of `send(token, endpoint, body)`

Transport-neutral contract types in `cpa-core` (the `http` crate is fine there), roughly:

```text
ExecutionRequest:
  operation
  source_format, response_format
  requested_model, resolved_model
  original_body, working_body
  downstream_stream
  inbound headers/query and request/session context
  authenticated caller identity

ExecutionResponse:
  headers
  buffered bytes OR fallible byte/event stream

ExecError:
  status, upstream body/headers, retry-after
  request/model/credential failure scope
```

`wreq` stays entirely inside `cpa-exec`.

- Caller identity is not the upstream credential. Go derives the MCP tool-alias secret from the authenticated downstream API key so aliases stay stable across failover and refresh (`internal/runtime/executor/claude_executor_request.go` L1653-1663). Keep the matched principal and its source as private request context. Never forward it as a header.
- Downstream non-streaming does not imply upstream non-streaming. Go requests Claude SSE when translating a non-streaming response into another format (`internal/runtime/executor/claude_executor_execute.go` L56-80).

### 3. Translation is a library used by executors, not route middleware

Dependency direction: `cpa-server (runtime) -> cpa-exec -> cpa-translate -> cpa-core`.

`cpa-translate` owns format-pair registration and request / non-stream / stream / count transforms. Executors decide where transforms run relative to provider preparation. Claude ordering:

```text
select credential
-> request translation
-> provider transformations/cloaking
-> upstream request
-> decompression + provider response restoration
-> response translation
```

Provider tool-alias reversal happens before generic response translation, with its reverse map private to the execution. Translation needs request-local state, the original request, and the translated request before wire aliases (`claude_executor_execute.go` L390-436).

Streaming transform contract: framed SSE input (never raw TCP chunks), zero or more outputs per input, terminal and error handling, request-local state. No shared `Any` state in the public API.

### 4. Selection behind one runtime coordinator

Create an axum-independent `cpa-server::runtime` module. It owns the store, selection, refresh coordination and execution outcomes. `AppState` holds handles, not credential vectors and counters.

- Selection input: provider candidates, model, session/affinity context, excluded attempts.
- Selection output: an owned credential snapshot plus an attempt/lease identity.
- Completion input: success, scoped failure, retry-after, or cancellation.

The lease lives until a streaming response completes or is dropped. Returning headers is not completion (`sdk/cliproxy/auth/conductor_stream.go` L91-154).

Replace the copied `Config` in `AppState` with a shared snapshot handle. The config stream later needs an editable YAML document and a resolved runtime config as separate representations; management must not round-trip the runtime struct for persistence.

### Ownership

| Owner | Write targets |
|---|---|
| Contracts (main) | Shared core contracts, runtime/store interfaces, `AppState`, route wiring, workspace manifests |
| Stream 1 | Claude executor, provider auth exchanges, provider-private response/cloak state, behind the agreed refresh/store APIs |
| Stream 2 | `cpa-translate` and its fixtures only |
| Stream 3 | Config document/schema/store and management handlers using shared runtime APIs |
| Stream 4 | Harness, captures and fixtures; no production-interface redesign |

## (b) Correctness and security problems in the skeleton

Fix now:

- **Disabled credentials are used.** A file with `"disabled": true` still joins round-robin. Go loads it as explicit disabled state (`internal/watcher/synthesizer/file.go` L188-217). Preserve disabled state and exclude it from selection.
- **Empty host cannot bind.** `":8317"` is not a valid Tokio socket address. Implement Go's empty-host wildcard (all interfaces, IPv4 and IPv6) with explicit socket addresses.

Replace during the contracts work:

- **Client-key query parsing.** Go uses the first decoded value for each name and decodes names too. Rust accepts any repeated value and does not decode names, so `?key=wrong&key=valid` passes only in Rust and `?%6bey=valid` passes only in Go. Match Go's `url.ParseQuery` including malformed-escape behaviour (`internal/access/config_access/provider.go` L57-105).
- **Inbound headers and context** never reach the executor. Add the envelope. Forward only provider-approved headers and rebuild upstream auth. The current code drops all inbound headers, so client keys do not leak; keep that separation.
- **Response headers.** Go disables upstream header passthrough by default; when enabled it filters hop-by-hop and `Connection`-nominated headers, cookies, encodings and reserved CPA headers. `Retry-After` has its own safe error path (`sdk/api/handlers/handlers.go` L193-203, `sdk/api/handlers/header_filter.go` L21-125). Implement the config-controlled policy, not a two-header allowlist.
- **Compression.** wreq rc.31 does not decompress with the enabled features. Rust drops `Content-Encoding` while forwarding compressed bytes. Go decodes declared encodings and sniffs unlabelled gzip/zstd (`claude_executor_request.go` L888-976). Decode before parsing or translation.
- **Errors and streaming.** Go formats Claude errors by status/body, delays SSE headers until the stream bootstraps, and emits `event: error` for terminal failures after streaming starts (`sdk/api/handlers/claude/code_handlers.go` L235-325 and L362-481). Carry structured errors across the executor/runtime boundary.
- **Request size.** axum's `Bytes` extractor silently caps bodies at 2 MiB with a plaintext 413. Choose the compatibility limit explicitly and map extraction failures through the route's error formatter.
- **Config precedence.** `Option` collapses explicit `null` into absence; Go uses YAML node presence, so a nested null replaces the legacy value. Go rejects null structural parents (`access: null`). Empty auth-dir resolves to the default in Go. Go's loader leaves an omitted port at 0; 8317 is not the loader default. See `internal/config/config_v8.go` L188-274, `internal/config/config_load.go` L63-84, `internal/util/util.go` L74-95. (Owned by the config stream.)

The skeleton's passthrough test enshrines wrong behaviour: unconditional `request-id`, raw `{"type":"error"}` on 429, and count-tokens always sent to a custom upstream (Go estimates locally for non-first-party origins, `claude_executor_tokens.go` L22-32). Replace those assertions with reference-derived fixtures.

## (c) Differential harness

1. **A localhost base URL does not exercise the first-party path.** Go applies its Anthropic transport profile only to exactly HTTPS `api.anthropic.com` on port 443 without userinfo (`internal/runtime/executor/helps/claude_upstream.go` L8-16, `helps/utls_client.go` L376-393). Use isolated DNS/dial routing or a local CONNECT proxy that preserves the logical URL, Host and SNI, terminating TLS at the capture server with test trust configured for both clients. Never modify the machine's global hosts or trust store. Fixture tokens must have real OAuth token shapes with complete fake identity metadata, and external egress must be denied so profile preparation cannot reach real services.
2. **Three separate comparisons.** HTTP behaviour (status, route/error shape, headers, body, SSE events). HTTP wire profile (raw ordered and cased headers, request target, body bytes; an axum `HeaderMap` cannot show order or case). TLS profile (parsed ClientHello: ordered ciphers and extensions, groups, signature algorithms, ALPN, padding, resumption). Never compare encrypted bytes. The Go profile advertises only `http/1.1`; plain wreq advertises `h2` first (`helps/utls_client.go` L193-245).
3. **Control state without normalizing away the cloak.** Same caller identity, credential identity, device profile, session inputs, timezone and starting continuity state; separate writable auth dirs. Normalize generated UUIDs and time fields narrowly and document each rule, or inject determinism. Do not normalize billing blocks, metadata or tool aliases. Compare across multiple turns and credential changes. Replay responses and compare downstream output too, including fragmented SSE, compressed responses, failure before first event, midstream failure and cancellation.
4. **wreq emulation is a feasibility gate.** At 6.0.0-rc.31, `TlsOptions` exposes cipher/group/signature lists, extension ordering, ALPN and key-share controls, but not uTLS-style raw ClientHello construction. Prototype cold and resumed handshakes first. If wreq cannot reproduce required fields, change the TLS backend rather than weakening the comparator.
