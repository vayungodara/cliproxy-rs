# Payload and custom header goldens

`../../fixtures/payload_go.json` holds outputs of CLIProxyAPI at `6fecc6e`, produced
in-process by `main.go` from real Go code only:

- `payload`: `helps.ApplyPayloadConfigWithTrackedPathsForExecutor` over the rule set in
  `config` (parsed with `config.ParseConfigBytes`), including the tracked paths it
  reports.
- `headers`: `util.ApplyCustomHeadersFromAttrs` with a session ID in the request
  context.

Each rule holds one param path so Go's random map iteration cannot change the output.
Replayed by `tests/payload.rs`.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-common
tmp=$(mktemp -d)
cp "$crate/tests/reference/payload/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/payloadfixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/payload_go.json"
)
```
