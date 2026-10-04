# Codex goldens

`../../fixtures/codex_go.json` holds outputs produced by CLIProxyAPI at `6fecc6e`:

- `sjson`: tidwall/sjson set/delete results the Rust byte editor must reproduce.
- `oauth`: the Codex OAuth service, device login and executor refresh, run against an
  in-process mock. `http.DefaultTransport` is replaced so `auth.openai.com` resolves to the
  mock; nothing leaves the machine. Captured requests (path, body, headers), parsed tokens,
  the auth file bytes from `CodexTokenStorage.SaveTokenToFile`, credential file names, JWT
  claim parsing and pasted-callback parsing.
- `executor`: `CodexExecutor.Execute` / `ExecuteStream` cases against a loopback upstream,
  with the upstream request Go sent and the chunks or error it returned.
- `alpha_search`: verbatim copies of the unexported `sanitizeCodexAlphaSearchBody` and
  `rewriteCodexAlphaSearchModel`, so the fixture records `encoding/json` behaviour.
- `quota`: `ParseCodexQuotaEventHeaders` and `QuotaState.ObserveResponseHeadersForProvider`.
- `images`: the Images API path (`executeOpenAIImage`, `executeOpenAIImageStream`, source
  format `openai-image`, `request_path` metadata, client headers in a gin context) for direct
  gpt-image-* models and the Responses tool path, with base64 payloads so multipart bytes
  survive JSON.

All tokens are fake. Generate with a temporary module whose import path is inside the
reference module's internal-package boundary; the reference checkout stays unchanged:

```sh
reference=/absolute/path/to/CLIProxyAPI
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/codex/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/codex_go.json"
)
```

Go writes time-dependent values in two places; tests treat them as shapes: refresh
`expired`/`last_refresh` (recorded as `<time>`) and the random `session_id` Go adds when a
model's override headers carry a Mac OS User-Agent. Go streams one chunk per SSE line while
the Rust executor yields whole events, so stream outputs are compared event by event.

## WebSocket error frames

`../../fixtures/codex_ws_errors.json` holds `parseCodexWebsocketErrorWithCooling` results
(status, message, retry hint, credential scope, headers). The function is unexported, so
the generator is a test file copied into the reference tree:

```sh
cp zz_rsfix_ws_errors_test.go "$reference/internal/runtime/executor/"
RSFIX_OUT=/tmp/rsfix go test -count=1 -run TestRSFixCodexWebsocketErrors ./internal/runtime/executor/
cp /tmp/rsfix/codex_ws_errors.json ../../fixtures/
```
