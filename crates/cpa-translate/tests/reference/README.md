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
- `request_compat`: `ConvertOpenAIRequestToClaudeWithCompat`.
- `non_stream`, `token_count`: `TranslateNonStream`, `TranslateTokenCount`.
- `stream`: `TranslateStream` per input line, compared with the pair's `go_stream`.
  Event splitting and client framing are covered by unit tests in `src/stream.rs`.

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
  go run . "$reference" "$crate/tests/fixtures/pairs" openai:claude openai:openai openai-response:codex openai-response:claude
)
rm -rf "$tmp"
```

List every ported pair (`client:upstream`, Go format names) on the command line.

Known gaps: the extraction does not interpret dynamic table expressions, registry mocks
or assertions about Go slice backing addresses, and plugin hooks (M6) are not exercised.
The dynamic model registry is empty during generation, so capabilities come from the
static catalogs only.
