# Server-core goldens

`../../fixtures/server_go.json` holds outputs of CLIProxyAPI at `6fecc6e`, produced
in-process by `main.go`. It runs real Go code only and opens no sockets:

- `sanitize`: `auth.SanitizeUpstreamErrorSummary`.
- `extract`: `auth.ExtractUpstreamErrorSummary`.
- `availability`: `registry.RegisterClient`, `ApplyClientModelProjections` and
  `GetAvailableModels` on the real model registry (Go `modelRegistrationAvailability`).
- `cooldown`: `auth.Manager.MarkResult` sequences on fresh credentials, reported as
  whole seconds until each model's `NextRetryAfter`.

Replayed by `sanitize::tests`, `registry::tests` and `scheduler::tests`.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-server
tmp=$(mktemp -d)
cp "$crate/tests/reference/server/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/server_go.json"
)
```

Inputs that carry secrets use `fake` values only.
