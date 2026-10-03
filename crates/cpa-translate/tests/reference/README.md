# Translator goldens

`../fixtures/pairs/<client>-<upstream>.json` holds outputs produced by CLIProxyAPI at
`6fecc6e`, built with Go 1.26 (the toolchain upstream releases use; Go 1.27 changes how
encoding/json writes invalid UTF-8). The generator calls only translator code through
`sdk/translator`'s registry, the same entry points executors use, and never contacts a
provider.

For each requested pair it reads the pair's `init.go` registration, mines converter calls
and JSON/SSE literals from that pair's Go tests, and adds the shared edge matrices in
`matrix.go`: model capabilities across every static catalog, conditional string escaping,
gjson number and array coercions, malformed bytes and JSON, Claude user-ID seeds, and tool
call/result states. Every case runs twice. JSON leaves that differ between the runs
(random IDs) or hold the current time are recorded as `dynamic`; the Rust test checks
their shape (digit-free prefix, timestamp window) and compares them as numbered
placeholders, so a repeated ID must stay consistent. All other bytes are compared
exactly, chunk by chunk for streams. Fixtures with invalid UTF-8 set `bytes` and store
each byte as the character with the same value.

Paths:

- `request`: `sdk/translator.TranslateRequest` (pair plus summary pipeline), compared
  with `cpa_translate::translate_request`.
- `request_compat`: the `...WithCompat` request converters (OpenAI -> Claude, Claude ->
  OpenAI, Gemini, Codex and Interactions), compared with the `*_with_compat` exports.
- `request_envelope`: `TranslateRequestEnvelope` with a `ModelInfo` whose native web
  search is on, compared with `cpa_translate::translate_request_envelope`.
- `non_stream`, `token_count`: `TranslateNonStream`, `TranslateTokenCount`. A non-stream
  fixture with `tool_error` and empty output is Go's nil apply_patch result.
- `stream`: `TranslateStream` per input line, compared with the pair's `go_stream`
  (`finalize` adds `FinalizeToolInput` at transport end). Event splitting and client
  framing are covered by unit tests in `src/stream.rs`.

`sdk` (in place of the pair list) writes `../fixtures/sdk_registry.json` from `sdk.go`:
which of the 49 format pairs Go registers (request, stream, non-stream, TokenCount), and
`TranslateRequest`/`TranslateTokenCount` results for pairs without a translator (the
model-rewrite fallback). `tests/sdk_translator.rs` replays it.

`apply_patch_responses` (in place of the pair list) writes
`../fixtures/apply_patch_responses.json` from `apply_patch.go`: scenarios for
translator/common's `NormalizeApplyPatchResponsesRequest` and `ApplyPatchResponsesBridge`,
helps' `NormalizeApplyPatchResponsesRequest` and `ApplyPatchResponsesState`, and the
openai-response:codex pair with an executor-owned bridge. Scenarios ported from Go's tests
carry the Go test's name; the rest are edge cases. Every op's payloads and error text are
recorded, and `tests/apply_patch_responses.rs` replays them.

Regenerate with a temporary module whose import path sits inside the reference module's
internal-package boundary. The reference checkout stays unchanged:

```sh
reference=/absolute/path/to/CLIProxyAPI
crate=/absolute/path/to/cliproxy-rs/crates/cpa-translate
tmp=$(mktemp -d)
cp "$crate"/tests/reference/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -go=1.26.0 -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  pairs=$(ls "$crate"/tests/fixtures/pairs | sed 's/\.json$//' | python3 -c 'import sys
names = ["openai-response", "openai", "claude", "gemini", "codex", "antigravity", "interactions"]
for line in sys.stdin:
    s = line.strip()
    c = next(n for n in names if s.startswith(n + "-"))
    print(c + ":" + s[len(c) + 1:])')
  go run . "$reference" "$crate/tests/fixtures/pairs" $pairs
  go run . "$reference" "$crate/tests/fixtures/pairs" sdk
  go run . "$reference" "$crate/tests/fixtures/pairs" apply_patch_responses
)
rm -rf "$tmp"
```

The pair list (`client:upstream`, Go format names) is every registered pair; the generator
panics on a pair Go does not register.

Known gaps: the extraction does not interpret dynamic table expressions, registry mocks
or assertions about Go slice backing addresses, and plugin hooks (M6) are not exercised.
The dynamic model registry is empty during generation, so capabilities come from the
static catalogs only.
