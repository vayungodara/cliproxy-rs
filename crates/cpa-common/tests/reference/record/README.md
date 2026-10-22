# Recorded Go calls for `thinking` and `signature`

The fixtures in `../../fixtures/go_*.jsonl.gz` hold real CLIProxyAPI outputs. Each line
is one call into `internal/thinking`, a thinking provider applier, `internal/signature`
or `helps.translatedRequestSummaryConfig`, with its inputs, Go's output (or error text)
and every model-registry lookup Go made during that call. The Rust tests replay each
line and compare outputs exactly.

`go_calls.jsonl.gz` and `go_signature_matrix.jsonl.gz` are recorded at v8.0.27
(`bf9341629ef2300ac9d846a2e80fe2756f63a671`) for its Antigravity Claude 5.5 CAQS
signatures (Go 9b9c2bdf and 174248ca). Between `6fecc6e` and v8.0.27, `internal/signature`
changed only by those two commits, and `internal/thinking` and
`helps.translatedRequestSummaryConfig` did not change, so every other recorded function
behaves as at `6fecc6e`. `go_thinking_matrix.jsonl.gz` is recorded at v8.0.35
(`ae3ee3be303fb07fb8173279923413005bf32177`) for the Claude `DisableType` change (Go
ad9e456d): it adds claude-sonnet-5-5 (`between_tools`), the adaptive-only Claude models
and a body carrying `thinking.block_binding`, and records each model's `disable_type`.

- `go_calls.jsonl.gz`: every call made while Go's own suites ran:
  `./internal/thinking/...`, `./internal/signature/...`,
  `./internal/runtime/executor/helps/`, `./internal/translator/...` and the
  `Thinking|Summary|Signature` tests in `./test/` (the end-to-end thinking matrix).
- `go_thinking_matrix.jsonl.gz`: `matrix_test.go.txt`, every applier over modes,
  capabilities and body shapes plus the resolved pipeline over source, target,
  capability and suffix.
- `go_signature_matrix.jsonl.gz`: `signature_matrix_test.go.txt`, every signature in
  `signature_corpus.json` (all raw signatures from Go's signature tests at `6fecc6e`,
  plus the `Q`-form ones the v8.0.27 suites pass to a signature function) through every
  target, block kind and validator option.

Recording runs on a throwaway copy, never on the reference checkout. No network access
is needed beyond the Go module cache.

```sh
reference=/absolute/path/to/CLIProxyAPI      # at v8.0.27 (v8.0.35 for the thinking matrix)
here=$(pwd)                                   # this directory
copy=$(mktemp -d)
tar -C "$reference" --exclude=.git -cf - . | tar -C "$copy" -xf -
chmod -R u+w "$copy"                          # a read-only reference stays read-only in the copy
mkdir -p "$copy/internal/recorder"
cp recorder.go.txt "$copy/internal/recorder/recorder.go"
python3 patch.py "$copy"
cp matrix_test.go.txt "$copy/test/zz_matrix_test.go"
cp signature_matrix_test.go.txt "$copy/internal/signature/zz_matrix_test.go"
cd "$copy"
out=$(mktemp -d); matrix=$(mktemp -d); sig=$(mktemp -d)
CPA_RECORD_DIR=$out go test -count=1 -p 1 -parallel 1 -skip TestRecord ./internal/thinking/... \
  ./internal/signature/... ./internal/runtime/executor/helps/ ./internal/translator/...
CPA_RECORD_DIR=$out go test -count=1 -p 1 -parallel 1 -run 'Thinking|Summary|Signature' -skip TestRecord ./test/
CPA_RECORD_DIR=$matrix go test -count=1 -run TestRecordThinkingMatrix ./test/
CPA_SIG_CORPUS=$here/signature_corpus.json CPA_RECORD_DIR=$sig \
  go test -count=1 -run TestRecordSignatureMatrix ./internal/signature/
cd "$here"
python3 merge.py "$out" calls.jsonl && gzip -9nc calls.jsonl > ../../fixtures/go_calls.jsonl.gz
python3 merge.py "$matrix" m.jsonl && gzip -9nc m.jsonl > ../../fixtures/go_thinking_matrix.jsonl.gz
python3 merge.py "$sig" s.jsonl --by-signature && gzip -9nc s.jsonl > ../../fixtures/go_signature_matrix.jsonl.gz
rm calls.jsonl m.jsonl s.jsonl
```

Two Go tests fail under recording, `TestConvertGeminiRequestToAntigravityBoundsLargePayloadCopies`
and `TestConvertGeminiRequestToGeminiReusesLargeNormalizedPayload`: they assert allocation
budgets, and the recorder serializes their 20 MiB payloads. Their behaviour is unaffected.

Strings that are not valid UTF-8 are recorded as `{"b64": ...}` and replayed as raw bytes,
because Go strings are bytes and the Rust API takes `&[u8]`.

`merge.py` lists every record it drops. At v8.0.27 that is 19 lines over 64 KiB:

- 17 calls with a 32 MiB signature (`normalize_claude`, `valid_claude`, `inspect_cais`,
  `inspect_antigravity_caqs` and others), or a 64 MiB `split_prefix` input: the length
  caps, covered by `length_caps_reject_before_decoding`.
- Two `sanitize_gemini` calls with large inline media that Go returns unchanged, rebuilt
  byte for byte in `large_inline_data_passes_through_like_go`.

Replay normalizes protobuf-go's `proto:` separator inside error strings only: the library
picks U+0020 or U+00A0 per binary so nothing depends on it.

`../sjson/main.go` generates `../../fixtures/sjson_go.json` (7,266 sjson edits and 24
gjson reads) for `src/json.rs`, checked by `src/json_go_vectors.rs`. Run
`go run . > ../../fixtures/sjson_go.json` in a module requiring
`github.com/tidwall/sjson v1.2.5` and `github.com/tidwall/gjson v1.18.0`. `../unicode/main.go`
generates `crates/cpa-core/src/gostr_tables.rs` from Go 1.26's `unicode` and `strconv` tables.
