# Translator goldens

`../fixtures/go.json` contains outputs produced by CLIProxyAPI at `6fecc6e`.
The generator extracts constant request inputs and SSE sequences from the Go
translator tests, then adds asymmetric tool, cache, finish-reason and raw-JSON
cases. It calls only translator functions. It does not call provider endpoints.

Generate using a temporary module whose import path is inside the reference
module's internal-package boundary. The reference checkout remains unchanged:

```sh
reference=/absolute/path/to/CLIProxyAPI
crate=/absolute/path/to/cliproxy-rs/crates/cpa-translate
tmp=$(mktemp -d)
cp "$crate/tests/reference/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$reference" "$crate/tests/fixtures/go.json"
)
rm -rf "$tmp"
```

Go synthesizes wall-clock `created` values in Claude Chat conversions and fallback
tool IDs. Those top-level timestamps are normalized to zero; OpenAI passthrough
timestamps are never normalized. Exact generated-ID field paths are recorded in
each fixture and normalized after Rust independently checks their shape, clock
range and counter. All other bytes are preserved. SSE JSON payloads are
compared byte for byte; the Rust contract wraps them as complete `data:` events.
Go's route/executor layers perform this framing outside the translator.

Extraction does not interpret dynamic table expressions, registry mocks,
or assertions about Go slice backing addresses. Compatibility entry points
are tested separately from native Claude requests.
Fixture names retain the original Go test names and source line numbers.

The implemented subset is OpenAI Chat clients with Claude upstreams (request,
stream, buffered SSE non-stream) and OpenAI-to-OpenAI normalization (request,
stream, non-stream). Neither pair registers a token-count transform in Go.
Claude thinking capabilities use the pinned built-in model list, not runtime
registry overrides. JSON/SSE transforms require UTF-8 input; Go can preserve
malformed non-UTF-8 bytes. OpenAI non-stream passthrough remains byte-preserving.
The owned request result cannot
express Go's matching-model backing-slice reuse optimization.
