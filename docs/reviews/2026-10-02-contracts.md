# Contracts review (oracle, 2026-10-02, commit 9c9051f)

Follow-up to `2026-10-02-foundation.md`. Go reference at `6fecc6e`. Verdict: keep the commit, do one more shared-contract pass before freezing interfaces.

## A. Translation contract (cpa-translate)

- Add token-count translation. Go has a separate `ResponseTokenCountTransform`, used by Claude's local estimator and upstream count path.
- Permit same-format registrations. Go registers OpenAI -> OpenAI (`internal/translator/openai/openai/chat-completions/init.go` L9-18) with model and streaming normalization. Identity is the fallback when nothing is registered, not a mandatory rule.
- Document that `non_stream` may receive buffered SSE after provider restoration: Go's Claude -> OpenAI non-stream translator consumes SSE lines (`internal/translator/claude/openai/chat-completions/claude_openai_response.go` L340-351).
- A typed request context carrying model capabilities is needed for Antigravity Responses (`ModelInfo`); Claude-first work may defer it.

## B. Executor envelope and configuration seam

1. `ExecRequest.body` means the working request before executor translation. The executor owns translation; nothing upstream of it translates.
2. Carry route, query, alternate operation (`alt` / `$alt`, e.g. `responses/compact`, which the Claude executor rejects) and typed session context.
3. Executors get an immutable config snapshot (or a shared handle) per execution. Claude preparation and cloak depend on config and must see hot changes coherently.
4. `ExecError` keeps upstream headers and can request a direct error response. Go's first-party fast-error path (`internal/runtime/executor/claude_executor_fast_error.go` L80-153) returns the upstream status and body untouched instead of normal Claude error normalization; returning it as a success `ExecResponse` would wrongly record success.
5. Keep a scheduler retry hint separate from permission to emit a downstream `Retry-After`. Go's ordinary Claude error handler only emits `Retry-After` from `SafeResponseHeaders` (`sdk/cliproxy/auth/home_concurrency.go` L281-330), which recognizes concrete scheduler/cooldown errors, not every upstream error with a retry hint. The skeleton test that expects `Retry-After: 7` on a plain upstream 429 is not reference-derived.
6. Go's `statusErr` yields the text `status N` for an empty upstream error body, not the canonical reason phrase.

## C. Runtime ownership, publication, attempt lifetime

- Runtime needs: config publication; credential import/remove/reconcile with generations that stay monotonic across delete and re-create; an executor preparation/refresh contract that returns a `MetadataPatch`; a singleflight entry point. Runtime serializes acquisition, commits the patch, and executes with the committed snapshot (Go: `sdk/cliproxy/auth/conductor_execution.go` L1534-1595).
- Decide whether `type` is immutable (today a patch can change `metadata.type` while `Credential.provider` stays). Recompute `label` after an email change. Config-backed patches currently "succeed" without persistence; make that an explicit runtime-only operation or delegate to config-document persistence.
- `select` takes a selection context: provider candidates, route and resolved model, session/affinity, exclusions (first implementation may ignore some). Keep that context in the lease; a `FailureScope::Model` outcome must name the model.
- Completion keeps the structured failure, not just status/scope/duration.
- The lease is an owned, non-cloneable completion guard from selection onward. Today cancellation while awaiting `execute()` skips completion entirely.
- Errors are terminal: fuse the stream wrapper after `Err`. The route's `scan` also polls once more before seeing its `failed` flag; a future translator stream could stay pending forever there.
- Agree an executor-owned test transport hook with the harness: `ClaudeExecutor::new(base_url)` alone cannot configure test trust and dial routing that preserve the logical first-party URL, Host and SNI.

## Verdicts on claimed fixes

- Disabled credentials: correct.
- Empty host: works on default Linux but is not guaranteed dual-stack; Tokio inherits `IPV6_V6ONLY`. Configure the socket explicitly, and do not fall back to IPv4 on every IPv6 error (address-in-use must surface).
- Query keys: precedence, decoded names, malformed escapes, semicolons and candidate order are correct. Non-UTF-8 decoding is not Go-accurate (see defects).
- Caller identity: correct and kept separate. `ExecRequest`'s derived `Debug` prints raw auth headers; do not treat the envelope as safe to log.
- 64 MiB body limit: correct as a chosen policy; a deliberate difference from Go's uncapped handler.
- Header suppression: correct for successful responses; error headers not fully correct (B4, B5).
- Delayed SSE headers and terminal error event: correct. Claude stream fidelity still needs stopping at `message_stop` and Go's line-ending normalization; those belong in provider handling, not the generic framer.
- No-auth 503: Go trims the model; and a wrongly typed `stream` makes the serde peek discard a valid model. Extract fields independently.

## Defects

- `write_atomic` uses a predictable temp name with `create(true).truncate(true)`: an existing temp keeps its old permissions, a symlink is followed and its target truncated, and failures leave credential bytes behind. Create a uniquely named sibling with `create_new` and 0600, rename, clean up on failure. Run the blocking write/fsync off Tokio workers when wiring async callers. The global store lock itself is fine.
- Upstream error body: `res.bytes()` then `truncate` reads everything first. Read only a bounded prefix from the stream and drop the rest.
- SSE framer: unbounded buffer for an unterminated event and a rescan from byte zero on every push (quadratic). Add a size bound and an incremental scan position; make framing fallible before translators depend on it.
- Query decoding: `from_utf8_lossy` lets a key containing U+FFFD match `%FF`. Compare decoded bytes with configured key bytes and build `Caller` from the matched configured key. Do not skip invalid-UTF-8 pairs: `key=%FF&key=good` must still pick the first value.

## Deferrals

Safe during isolated porting once the interfaces above exist: scheduler/cooldown/retry, custom-origin token estimation, the passthrough-headers filter (default stays drop), the broader config precedence port.

Not safe as production stopgaps:
- Compression: `Accept-Encoding: identity` is a preference. Decode before framing/parsing, or reject declared or sniffed (gzip `1f 8b`, zstd `28 b5 2f fd`) compression instead of forwarding corrupt bodies.
- Structural config nulls: `access: null` becomes an empty key list, an open proxy; Go rejects it. Fix before exposing a listener.
- Real OAuth traffic before preparation/refresh coordination: revision checks alone do not stop duplicate rotating-token exchanges or inconsistent device identity creation.

Most valuable added checks: cancellation before response creation; exactly-once completion after EOF, error and drop; `Err` followed by a permanently pending stream; existing temp file and symlink persistence failures; malformed-byte query keys; bounded error reads; compressed responses despite `identity`.
