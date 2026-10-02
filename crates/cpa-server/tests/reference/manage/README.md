# Management and config goldens

`../../fixtures/manage_go.json` holds outputs of CLIProxyAPI at `6fecc6e`, produced
in-process by `main.go`. It runs real Go code only: gin `ClientIP`, the management
middleware, `config.LoadConfig`, the config/file credential synthesizers and a
complete `api.NewServer` driven through `httptest`. It opens no sockets and calls no
provider, GitHub or OAuth endpoint. Secrets are `fake-*` strings; bcrypt hashes are
regenerated on every run, so tests hash the same plaintext themselves.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-server
tmp=$(mktemp -d)
cp "$crate/tests/reference/manage/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/manage_go.json"
)
rm -rf "$tmp"
```

Absolute auth-file paths are rewritten to `/fixture-root/...` and their `auth_index`
recomputed for that path, so the fixture does not depend on the temp directory.
`httptest` requests can carry header values a real HTTP parser would trim (for
example `Authorization: "Bearer "`); the Rust replay skips those steps and covers
the parsing rule with a unit test. Go reports `X-CPA-SUPPORT-PLUGIN: 1` because the
reference is a cgo build; cliproxy-rs cannot load native plugins and reports `0`.
