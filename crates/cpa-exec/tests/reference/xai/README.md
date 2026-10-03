# xAI executor goldens

`../../fixtures/xai_go.json` holds outputs of the unmodified Go `XAIExecutor`
(CLIProxyAPI `6fecc6e`). For every scenario the generator builds the credential
(explicit attributes and metadata, or the real config synthesizer for `xai-api-key`
entries), applies `ForAPIKey` for API-key credentials as the conductor does, binds a
configured model when the scenario names one, and runs `Execute`, `ExecuteStream` or
`CountTokens`. A context round tripper (`cliproxy.roundtripper`) sends every upstream
request to a one-shot raw TCP capture server and records the URL the executor chose
(`api.x.ai`, `cli-chat-proxy.grok.com` or a configured base URL). The scenario records
that URL, the exact upstream request text, what the executor returned (payload, stream
chunks, or status, message, retry-after and credential scope) and the usage record Go's
reporter published. Nothing contacts a provider; keys are fake.

Scenarios run in order in one process, so Go's reasoning replay cache carries from one
turn to the next; the Rust test runs them in the same order on one executor.
`GROKENC1..3` in payloads become structurally valid Grok encrypted-content blobs.
Normalization is limited to the capture server address (`UPSTREAM`). Scenarios tagged
`needs` depend on a shared helper that has not landed in Rust yet (the apply_patch
Responses bridge); the Rust test skips them by name and fails if the list changes.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate"/tests/reference/xai/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/xai_go.json"
)
rm -rf "$tmp"
```
