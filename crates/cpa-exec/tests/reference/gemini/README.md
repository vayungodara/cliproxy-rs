# Gemini and Interactions executor goldens

`../../fixtures/gemini_go.json` holds outputs of the Go `GeminiExecutor`
(`NewGeminiExecutor` and `NewGeminiInteractionsExecutor`) at CLIProxyAPI `6fecc6e`, with
the Gemini to Claude response commits the Rust translator follows (see
[below](#translator-commits-in-the-recorded-tree)). For
every scenario the generator parses the scenario config with `config.ParseConfigBytes`,
synthesizes credentials with the real config synthesizer (or uses explicit attributes),
binds the model info the conductor would bind for configured models
(`cliproxy.resolved_api_key_model_info`, after `rewriteModelForAuth` strips the
credential prefix), binds the attempt's canonical session the way the conductor does
(`session.Enrich`, `ensureCanonicalSessionMetadata`, `syncMetadataSessionToContext`; read
by `$CPA-SESSION-ID`, explicit or derived) and records it as `session` for the Rust
`ExecRequest.session`, and runs `Execute`, `ExecuteStream` or
`CountTokens` against a one-shot raw TCP capture server. It records the exact upstream
request text and what the executor returned: the payload, the stream chunks, or the
status and message of the error. Nothing contacts Google; every key is fake.

Normalization is limited to the capture server address (`UPSTREAM`). Gemini answers carry
a fixed `createTime` so translators that would otherwise read the clock stay
deterministic. The scenario config includes `requests.payload` rules for two models, so
payload-rule scenarios exercise `cpa_common::payload` through the executor.

`needs` lists the translator registrations a scenario depends on whose Go result is not
the identity: `pair:<client>-><upstream>` for a request or response translation, and
`token_count:<client>-><upstream>` for a count shape that differs from the upstream
body. A same-format request counts only when Go's normalizer changes the body (compared
with the registry fallback, which only sets a differing `model`). The Rust test runs a
scenario as soon as every listed registration exists in `cpa_translate`, and prints the
ones still waiting.

`vectors` records Go answers for the exported helpers the executor ports:
`helps.FilterSSEUsageMetadata` (in call order: it remembers trace IDs process-wide),
`helps.JSONPayload`, `helps.EnsureGeminiLeadingUserContent` /
`EnsureGeminiTrailingUserContent`, and the Claude `message_start` input-token estimate
(`helps.TranslateStreamWithClaudeInputTokens` with an upstream format that has no stream
transform, so chunks reach the estimator unchanged).

Regenerate from the patched tree described in
[Translator commits in the recorded tree](#translator-commits-in-the-recorded-tree), not
from plain `6fecc6e`, which would record stale Gemini to Claude expectations. Use a
temporary module inside the reference module's internal-package boundary (the reference
checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # the patched tree below (tree cc013917), not plain 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate"/tests/reference/gemini/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/gemini_go.json"
)
```

## Translator commits in the recorded tree

The Gemini to Claude response translator follows v8.0.35 without `7f2f17d7`, whose
executor half (a 502 for `MALFORMED_FUNCTION_CALL`) is not ported yet. The recorded tree is
`6fecc6e` with `a0d31924`, `68ea171d`, `58257007` and `202b8b8c` cherry-picked, which
apply cleanly. One Go test in that package calls the later two-value
`ConvertClaudeRequestToGemini`; [`6fecc6e-gemini-claude-test.patch`](6fecc6e-gemini-claude-test.patch)
adapts it, and `go test ./internal/translator/gemini/claude/ ./internal/util/` passes.
Re-recording changed only `usage_claude_stream_non_terminal_usage` (`[DONE]` now closes
the open block and sends `message_delta` before `message_stop`, `a0d31924`) and values
the test already normalizes.

```sh
git checkout 6fecc6e && git cherry-pick a0d31924 68ea171d 58257007 202b8b8c \
  && git am /absolute/path/to/cliproxy-rs/crates/cpa-exec/tests/reference/gemini/6fecc6e-gemini-claude-test.patch
git rev-parse 'HEAD^{tree}'   # cc01391700b600815d1a342bc4b8190a206b7e94
```

Go stream chunks are what the executor hands the route handler. The Rust executor
returns the bytes the handler writes, so the test frames each Go chunk the way Go's
handler does: `data: <chunk>\n\n` for Gemini clients without `alt` and for OpenAI
clients, the bare chunk for Gemini clients with `alt` and for Claude clients, a closing
blank line for Responses clients, and the Interactions handler's rules for Interactions
clients.

## White images

`fixGeminiImageAspectRatio` inserts a white PNG produced by Go's `image/png` encoder.
Rust's deflate does not reproduce Go's bytes, so the ten PNGs are embedded as
`crates/cpa-exec/src/gemini_white_png.bin.gz` (records of big-endian u16 width, u16
height, u32 length, PNG bytes). Regenerate it with `white_png.go.txt`: copy it to an
empty directory as `main.go`, run `go mod init pngs && go run .`, then
`gzip -9nc white.bin > gemini_white_png.bin.gz`.
