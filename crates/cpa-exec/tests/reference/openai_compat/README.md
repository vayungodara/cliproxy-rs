# OpenAI-compatible executor goldens

`../../fixtures/openai_compat_go.json` holds outputs of the unmodified Go
`OpenAICompatExecutor` (CLIProxyAPI `6fecc6e`). For every scenario the generator
parses the scenario config with `config.ParseConfigBytes`, synthesizes credentials
with the real config synthesizer (or uses explicit attributes), binds the model info
the conductor would bind for config models, and runs `Execute`, `ExecuteStream` or
`CountTokens` against a one-shot raw TCP capture server. It records the exact
upstream request text and what the executor returned (payload, stream chunks, or
status, message and retry-after). Nothing contacts a provider; keys are `sk-fake-*`.

Normalization is limited to the capture server address (`UPSTREAM`) and Go's random
60-hex-digit multipart boundary (`BOUNDARY`). Scenarios tagged `needs` depend on a
shared helper that has not landed in Rust yet (thinking, signature validation, or a
translator pair); the Rust test skips them by name and fails if the list changes.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate"/tests/reference/openai_compat/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/openai_compat_go.json"
)
rm -rf "$tmp"
```

Go stream chunks are bare `data:` lines and include `data: [DONE]`. The Rust
translator contract emits complete `data: ...\n\n` events and leaves `[DONE]` to the
route, so the test compares each Go chunk plus `\n\n` and drops `data: [DONE]`.
A Go error without a status code (status 0) is answered 500 by Go's handler.
