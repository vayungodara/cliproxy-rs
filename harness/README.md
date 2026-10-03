# Local differential harness

From the repository root, run:

```sh
./harness/run
```

The command clones and builds the **unmodified** Go reference at
`6fecc6e5567912661654a4eaf9b8f5436facd1c2`, builds the Rust test driver, runs collector
regression tests, and starts both proxies against one scripted local TLS upstream.
No production source or workspace manifest is changed. The Rust driver uses the
production config loader, credential loader, runtime, routes and Claude executor;
`ClaudeExecutor::with_hooks` injects test trust and dial routing only. It is not a
test of the production binary's command-line or listener setup.

## Prerequisites and safety

Linux, Go **1.26+**, Rust with Cargo, CMake, Clang, Perl, Python 3, OpenSSL, util-linux
(`unshare`, `mount`), and iproute2 (`ip`) are required. Rust builds BoringSSL.
Unprivileged user, network, mount and PID namespaces must be enabled. The command
does **not** install packages, change the machine's global hosts/trust files, or
fall back to an unrestricted run if isolation fails. A failed prerequisite or
namespace setup stops the run before either proxy starts.

Downloads from GitHub and language dependency registries happen during the build,
before isolation. Runtime has only loopback interfaces and routes; an explicit
egress check requires `ENETUNREACH`. The private `/etc/hosts` maps
`api.anthropic.com` and `platform.claude.com` to `127.0.0.2`; the Rust client's dial
override maps those logical hosts to `127.0.0.3`. One server listens on port 443
and labels captures by the local destination address. URL, Host, SNI and port all
remain first-party, including OAuth refresh. TLS verification stays enabled using
an ephemeral test CA, not an insecure TLS setting. No real credentials are read.
Each proxy gets a whitelist-only environment and its own generated working/auth
directory. The input credentials, caller key, device/account identity and request
bytes are identical. Config differs only in bind port and writable auth path.

The Go binary's unrelated background update attempts can appear as denied network
errors in its log. They cannot leave the network namespace. `-local-model` and
disabled management panel updates suppress catalog/panel downloads. All test
credentials contain `FAKE` or use `example.invalid`. Successful runs delete their
generated private test keys; the public CA/certificate and captures remain.

An existing clean reference checkout can avoid cloning:

```sh
CPA_GO_REFERENCE=/absolute/path/to/CLIProxyAPI ./harness/run
```

The wrapper refuses a different revision, modifications or untracked files. It never
changes an existing reference checkout. Build output goes in `harness/.cache/`
and `harness/rust/target/`, both ignored by Git. The driver's separate workspace
and lockfile keep Go/network requirements out of `cargo test --workspace`.

## Results and exit codes

`harness/.cache/latest` points to the last completed run. Each fixture directory
contains `fixture.json`, `go.json`, `rust.json`, and `diff.json`, plus per-proxy
config, fake auth file and process log. `summary.json` lists statuses, difference
counts and capture/transport errors. Review all differences, not just statuses:

```sh
jq . harness/.cache/latest/summary.json
jq . harness/.cache/latest/buffered/diff.json
jq '.upstream.tls[].profile' harness/.cache/latest/buffered/go.json
jq '.upstream.http[].headers' harness/.cache/latest/count-tokens/go.json
```

Raw ClientHello TLS records, raw HTTP request heads, exact upstream body bytes,
downstream status/header/body bytes and chunked transfer framing are retained as
hex. Ordered/cased headers and parsed TLS fields are also readable JSON. JA3 and
JA4 are calculated from the observed ClientHello, not the advertised User-Agent.
The harness has 57 cases (`cases()` in `fixtures.py`). The committed
[evidence archive](evidence/2026-10-02.json.gz) is from the first baseline run, when
there were 33 cases, and holds their fixtures, observations, diffs and summary; use
`gzip -dc` and `jq` to inspect it offline.
The collector's Go-derived 508-byte fixture asserts JA3
`d871d02cecbde59abbf8f4806134addf`; JA4 hashing follows
[FoxIO's technical specification](https://github.com/FoxIO-LLC/ja4/blob/main/technical_details/JA4.md).
Forced reconnect captures both cold and resumed handshakes, including PSK identity
and binder lengths and server-confirmed session reuse.

Exit **0** means capture completed, **not** parity. Exit **1**, with `--strict`,
means at least one captured field differs. Exit **2** means a prerequisite,
capture or transport failure; an unexpected startup failure also exits nonzero.
Cases that should exercise inference require an observed upstream request, so a
startup/model-registration failure cannot masquerade as provider parity evidence.
The Go health endpoint becomes ready before registration, so readiness also waits
for its `full client load complete` log marker. Expired-token testing waits for
the actual Go token rotation to be persisted, rather than sleeping a guessed
refresh interval.

```sh
./harness/run --case buffered --case count-tokens
./harness/run --case tls-reconnect-two-turns --strict
python3 -m unittest discover -s harness -p 'test_*.py'
cargo clippy --manifest-path harness/rust/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path harness/rust/Cargo.toml --check
```

## Comparison boundaries

Only the downstream Date **value** is replaced with `<wall-clock>` when diffing;
presence, order and casing remain significant. Raw TLS records are retained but
not byte-compared: client randoms, session IDs, ephemeral key bytes, opaque PSK
identities and binders are intentionally nondeterministic. Their lengths, ordered
extensions, ciphers, groups, signatures, supported versions, ALPN and session reuse
are compared. HTTP request/response bodies are never JSON-normalized. Billing
blocks, tool names, metadata and request/trace IDs are not stripped. Consequently,
strict comparison also reports generated ID and chunk-boundary differences; it
is not a deterministic semantic-only CI gate.

Fixtures cover buffered Messages, verified native signals and cloaking with
custom/native MCP tools and an unknown beta, tool-use history/choice rewriting,
buffered/SSE inverse name restoration, fragmented/CRLF SSE, pre-stream rejection, mid-stream errors,
truncation, events after `message_stop`, disconnect after the first event,
first-party count_tokens, 400/401/429/500/503 and empty error bodies, Retry-After
and Anthropic quota headers, declared/unlabelled gzip, gzip SSE, deflate,
model catalog negotiation, auth failures/query precedence, disabled credentials,
expired-token refresh, two-turn continuity, and TLS reconnect/resumption.

This is a Claude-first tested slice, not full drop-in parity. It does not exercise
Management v8, other providers/translations, OAuth login/code exchange, concurrent
refresh, credential deletion races, multi-credential retries/cooldowns, HTTP/2
upstream requests, chunked upstream request bodies, proxy transport modes,
Brotli/zstd, TLS 1.2 or performance/timing parity. The upstream intentionally
negotiates HTTP/1.1 while recording the client's full offered ALPN. The disconnect
case observes upstream writes failing before process shutdown, not internal
lease completion or an exact cancellation deadline. The Rust transport-injection
and runtime-observation gaps are described in the differential review.
