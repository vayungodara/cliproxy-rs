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

## Harvested inputs

Static mining cannot evaluate table-driven tests or inputs built by helpers, so
`harvest/main.go` records them at run time. It instruments a scratch copy of the reference
(never the checkout itself): every converter registered in `internal/translator/**/init.go`,
the exported `...WithCompat` request converters and sdk/translator's
`TranslateRequest`/`TranslateStream`/`TranslateNonStream` get a same-signature wrapper that
logs calls made directly from a `_test.go` file, with the enclosing Test function and call
line. With `HARVEST_JSONL` set, the generator appends one fixture per recorded request or
non-stream call, and one per stream (calls sharing a `param` pointer within a test), named
`Test:line`, unless an existing fixture already has the same input and test name. Inputs
over 256 KiB are skipped, and a test contributes at most 24 fixtures per path. Outputs
still come from `run()` through the registry, so harvested fixtures differ from mined
ones only in where their input came from.

```sh
scratch=$(mktemp -d)
(cd "$reference" && tar --exclude=.git -cf - .) | tar -xf - -C "$scratch"
tool=$(mktemp -d)
cp "$crate"/tests/reference/harvest/main.go "$tool/"
(cd "$tool" && go mod init harvesttool && go run . "$scratch")
(cd "$scratch" && go mod download && HARVEST_OUT="$scratch/calls.jsonl" \
  go test -count=1 ./internal/translator/... ./sdk/translator/... ./test/... \
    ./internal/util/... ./internal/client/codex/apply-patch/...)
# Then run the pair generation above with HARVEST_JSONL="$scratch/calls.jsonl".
```

The same run records helper calls with their results (value mode): the helpers listed in
`values` in `harvest/main.go` (apply-patch tool, translator/common and util helpers, and
package-private helpers of some pairs) log arguments before the call and results after,
when a test calls them directly. `go run . "$reference" "$crate/tests/fixtures/pairs"
helpers` with `HARVEST_JSONL` set writes `../fixtures/go_helpers.json`, one record per
test and distinct arguments, and `src/go_helper_tests.rs` replays every record against the
Rust port of that helper. A helper joins value mode only together with a replay arm.

Two allocation-bound Go tests (`...BoundsLargePayloadCopies`,
`...ReusesLargeNormalizedPayload`) fail in the instrumented copy because the recorder
copies their multi-MiB payloads; that is expected. Run the tests with external network
denied.

Known gaps: the extraction does not interpret registry mocks or assertions about Go slice
backing addresses, tests that call unlisted internal helpers rather than converters are not
harvested, and plugin hooks (M6) are not exercised.
The dynamic model registry is empty during generation, so capabilities come from the
static catalogs only.
