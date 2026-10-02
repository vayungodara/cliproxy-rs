# Local Claude differential findings, 2026-10-02

The baseline is not wire-compatible with the Go reference. All 33 scripted cases
completed with no capture or downstream transport failures, but every case had
observable differences. The run recorded 2,980 differing fields, including repeated
raw/parsed representations of the same difference. This is not a count of bugs.

Rust production source is unchanged from master `574c426`. The harness was run at
its first commit, `fa2db4f`. The unmodified Go oracle is pinned to
[`6fecc6e`](https://github.com/router-for-me/CLIProxyAPI/commit/6fecc6e5567912661654a4eaf9b8f5436facd1c2).
Measurements were prepared with Amp and verified against local captures. No real
provider account, credential or endpoint was contacted.

## Reproduction and evidence

From the repository root, with the prerequisites in [the harness README](../../harness/README.md):

```sh
./harness/run
```

The command builds both implementations, runs collector regression tests, and
runs the proxies in a new Linux user/network/mount/PID namespace. Only loopback
exists, and an explicit egress probe must return `ENETUNREACH`. Failure stops the
run; there is no unrestricted fallback. Build-time GitHub/registry downloads occur
before isolation. The ephemeral test CA remains verified. Private namespace hosts
and Rust dial overrides preserve logical first-party URLs, Host, SNI and port 443.
Go OAuth refresh uses `platform.claude.com`, not `claude.ai`.

The standalone Rust driver uses the production config/credential loaders, runtime,
router and executor. `ClaudeExecutor::with_client` supplies test trust/routing.
It does not cover the production binary's command-line or listener initialization.
Each proxy receives identical request bytes, synthetic credentials and config
settings, apart from bind port and writable auth path. Environments are whitelisted.

Live output is in `harness/.cache/latest`. The committed
[compressed evidence](../../harness/evidence/2026-10-02.json.gz) retains all 33
fixtures, both observations, every diff and the summary from the final run. It
contains no private test keys, certificates or process logs. Inspect it without
running either proxy:

```sh
gzip -dc harness/evidence/2026-10-02.json.gz | jq '.summary'
gzip -dc harness/evidence/2026-10-02.json.gz |
  jq '.cases[] | select(.name == "buffered") | .go.upstream'
```

Captures retain raw ClientHello records, ordered/cased HTTP/1.1 headers, exact
upstream bodies, downstream status/headers/body and raw transfer framing. The
comparator ignores only downstream Date values and raw TLS cryptographic random
bytes; parsed TLS ordering, lengths, extension data and session reuse remain
significant. JSON bodies, billing signatures, generated request IDs and chunk
boundaries are not normalized. `--strict` therefore fails on all captured
differences, including legitimate nondeterminism. Normal exit 0 means successful
collection, not parity.

Severity: **P1** blocks meaningful provider or client compatibility; **P2** is a
specific response/wire mismatch that still needs correction. These are findings
about the captured baseline, not predictions about concurrent workstreams.

## TLS and HTTP profile

### P1: native inference TLS profile and resumption differ

**PARITY: M1-0017. Cases: `buffered`, `count-tokens`, `tls-reconnect-two-turns`.**

The Go cold ClientHello payload is 508 bytes; Rust is 1,509 bytes. Including the
handshake and record headers, the captures are 517 and 1,518 bytes respectively.
Go offers 17 ciphers and HTTP/1.1-only ALPN; Rust offers 28 ciphers and `h2,http/1.1`.
Rust adds supported groups 4588/65074 and a 1,258-byte key-share extension, versus
Go's 38-byte X25519 extension. Extension order and padding also differ.

| Cold fingerprint | Go | Rust |
| --- | --- | --- |
| JA3 MD5 | `d871d02cecbde59abbf8f4806134addf` | `6aac88aefeb1c3f8b576782dbe085b6e` |
| JA4 | `t13d1714h1_5b57614c22b0_43ade6aba3df` | `t13d2811h2_257f3020b3a2_78e6aca7449b` |

When the fake upstream forces reconnect, Go resumes the session on its second
handshake. Its final extension is `pre_shared_key`, with a 224-byte identity and
48-byte binder. The server confirms `session_reused=true`. Rust performs another
cold handshake with no PSK and `session_reused=false`. Fix the native TLS profile
and cache behavior together; matching User-Agent alone does not address this.

### P1: ordered software headers and beta assembly are absent

**PARITY: M1-0016, M1-0022, M1-0023, M1-0026. Cases: `buffered`, `count-tokens`, `cloak-tools-beta`.**

Go sends 22 Messages headers in the documented order, including the CLI identity,
session ID, full Stainless tuple, timeout, dangerous-browser flag, `x-app`, request
ID and Connection. Rust sends seven lowercase baseline headers. Go advertises
`gzip, deflate, br, zstd`; Rust advertises `identity`.

Go count_tokens omits `X-Stainless-Timeout` and includes
`token-counting-2024-11-01`. Rust uses the same two fixed beta tokens for both
routes. The unknown caller beta in `cloak-tools-beta` survives only in Go.
Port ordered profiles and conditional beta construction, not a fixed superset.

## Request rewriting and tool restoration

### P1: cloaking, native billing and identity behavior differ

**PARITY: M1-0013, M1-0014, M1-0015. Cases: `buffered`, `cloak-tools-beta`, `native-signals`.**

The same 101-byte generic Messages input becomes a 1,118-byte Go upstream body,
while Rust forwards 101 bytes. Go adds signed billing, official CLI identity,
caller-prompt reminders, cache-control/diagnostics and structured `metadata.user_id`.
Rust leaves these unsigned and absent. In the verified native fixture, Go preserves
the caller system text but adds a signed billing block; Rust forwards the original
body unchanged. Keep native detection, cloak policy and first-party signing
separate when fixing this; native passthrough is not unconditional byte passthrough.

### P1: MCP names are not rewritten or restored

**PARITY: M1-0019. Cases: `tool-roundtrip`, `tool-stream-roundtrip`, `cloak-tools-beta`.**

Go rewrites the custom tool `fixture_lookup` to
`mcp__poem_real__leisure_fixture_lookup` in declarations, tool-use history and
tool_choice. The already-native MCP declaration retains its name. Rust preserves
the custom name upstream. Both proxies receive the same scripted alias response:
Go restores `fixture_lookup` in buffered JSON and SSE; Rust leaks the alias to the
caller. The fixture alias was taken from an independent clean Go capture, not
derived from Rust output. Port forward and inverse mappings as one contract.

## OAuth lifecycle

### P1: expired credentials never refresh in Rust

**PARITY: M1-0011, M1-0012, M1-0018, M1-0024, M1-0025, M4-0027. Case: `refresh-expired`.**

With expiry set to 2000, Go calls the local token endpoint, then the local profile
endpoint, persists rotated access/refresh tokens and sends inference with the
rotated access token. The unknown flattened credential field survives persistence.
Rust makes no OAuth request, leaves the expired credential unchanged and sends
inference with the original access token. The fake upstream accepts both tokens,
so equal downstream 200 statuses do not establish refresh parity.

Go OAuth has a distinct compact ClientHello, JA3
`203503b7023848ab87b9836c336b8e81`, and ordered Axios token/profile header profiles.
The token/profile captures have seven/eight headers respectively. Fix refresh,
persistence and the acquisition transport together. This case does not establish
singleflight, transient backoff, roles inspection or deletion-race parity.

## Response decoding and streaming

### P1: compressed success responses become errors

**PARITY: M1-0004. Cases: `gzip-json`, `gzip-unlabelled`, `gzip-sse`, `deflate-json`.**

Go returns 200 with decoded JSON/SSE in all four cases. Rust returns 502 with
compression-not-supported errors. This includes gzip identified from magic bytes
without Content-Encoding, and compressed streaming bodies. Add the Go-compatible
decoder behavior without relying solely on automatic client decompression.

### P2: terminal events, line endings and truncated-stream errors differ

**PARITY: M1-0004, M1-0020, M1-0084. Cases: `stream-crlf`, `stream-after-stop`, `stream-truncated`.**

Go normalizes CRLF SSE lines to LF; Rust preserves CRLF. Go stops after
`message_stop`; Rust forwards the deliberately appended ping. After a truncated
chunked body, both emit an SSE error, but Go reports `unexpected EOF` while Rust
reports `upstream request failed: error decoding response body: error reading a
body from connection`. Port terminal recognition and public error text. Chunk
framing differences are retained separately from SSE payload differences.

### P2: empty upstream error fallback differs

**PARITY: M1-0004. Case: `empty-500`.**

Both return 500 and an Anthropic error envelope. Go's message is `status 500`;
Rust's is `Internal Server Error`. Ordinary nonempty scripted 400/401/429/500/503
response statuses and bodies match.

## Routes, catalogs and middleware

### P2: model inventory and negotiation differ

**PARITY: M1-0003. Cases: `models-openai`, `models-anthropic`.**

Go returns 18 registered models; Rust returns seven fixed models. The default Go
catalog includes creation metadata missing in Rust. An Anthropic-Version header
selects Go's Anthropic catalog with display names, token limits and
`first_id/last_id/has_more`; Rust still returns the OpenAI-shaped catalog. Match
both registered inventory and header-based envelope negotiation.

### P2: no eligible credential has a different failure contract

**PARITY: M1-0003, M1-0004, M4-0027. Case: `disabled-credential`.**

With the only credential disabled, Go has no registered provider model and returns
400 `unknown provider for model claude-sonnet-4-6`. Rust returns 503
`auth_not_found`. Neither sends upstream traffic. This is not evidence that Rust
uses disabled credentials; it is a registration/error-contract mismatch.

### P2: common response headers differ

**PARITY: M1-0003, M1-0004, M1-0010. Cases: all downstream routes.**

Go's common CORS headers, exposed-header list and inference trace header are absent
from Rust's buffered responses. Rust SSE includes allow-origin but not the same
complete middleware profile. Header casing/order, Content-Type parameters,
Connection and transfer framing also differ. Reuse the recorded headers when
porting middleware; generated trace values are not themselves parity defects.

## Behavior that matched within this slice

- M1-0004: ordinary uncompressed buffered JSON, fragmented LF SSE, mid-stream
  provider error SSE, bootstrap 429, count_tokens response and nonempty
  400/401/429/500/503 response bodies/statuses match. This excludes headers and
  transport framing.
- M1-0010: missing/wrong keys and query-key precedence return identical status
  and body; a percent-encoded valid query key succeeds in both.
- M1-0021: the scripted Retry-After and Anthropic quota headers are captured;
  neither forwards those raw headers or the fixture's private response header
  downstream. No claim is made about internal quota classification/cooldowns.
- M1-0084: after downstream disconnect following the first SSE event, both fake
  upstream writers encounter `SSLEOFError` before proxy shutdown. This establishes
  observable upstream closure, not exactly-once lease completion or a deadline.
- The two-turn fixtures retain both requests. Generic Go cloaking derives changing
  wire session IDs for these inputs. They do not establish stable native session
  continuity or previous-message-ID behavior.

## Test hooks and known gaps

No additional production hook was needed for this baseline. No production files
or PARITY entries were changed. Two integration constraints matter for follow-up:

1. The injected Rust client uses baseline wreq defaults today. If a future native
   profile is built only inside `ClaudeExecutor::new`, this driver would bypass
   that code. The production client construction needs a test trust/dial override
   that preserves its native profile, or a profile builder usable by this driver.
   Future OAuth refresh must use injectable trust/routing too; namespace safety
   must not be weakened to make refresh pass.
2. Rust already exposes runtime store statistics, but this driver does not export
   them and there is no matched Go hook for internal lease/cooldown outcomes.
   No shared deterministic clock or request-ID source is injected. Those are
   limitations on internal lifecycle assertions, not missing baseline wire capture.

This is a tested Claude-first subset, not full drop-in parity. Management v8,
other providers/translators, OAuth login/code exchange, concurrent refresh/deletion,
multi-account selection/retries/quota cooldowns, proxies, HTTP/2 upstream behavior,
chunked upstream request bodies, Brotli/zstd, TLS 1.2 and timing/performance remain
untested. The fake upstream negotiates HTTP/1.1, while retaining all offered ALPN.
The strict comparator includes generated HTTP IDs and chunk scheduling differences;
it should not be presented as a deterministic semantic-only gate.

## Verification

- `./harness/run`: 33 cases, zero capture/transport failures, exit 0. The run used
  Go 1.26.0, Rust 1.99.0 and the default cloned/pinned Go checkout.
- `./harness/run --case buffered --strict`: observed differences, exit 1.
- Direct non-namespace execution: refused before proxy startup with
  `namespace must contain only loopback`.
- `python3 -m unittest discover -s harness -p 'test_*.py'`: five passed, including
  independent Go JA3/JA4 vectors, GREASE/order, fragmented TLS records, PSK lengths,
  raw transfer framing, lossless diff behavior and fail-closed startup.
- `cargo test --workspace`: 23 passed; zero failures.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed without warnings.
- `cargo fmt --all --check`: passed.
- Driver-specific Clippy with `-D warnings` and fmt check: passed. Its separate
  workspace keeps Go/network setup outside the default workspace tests.
