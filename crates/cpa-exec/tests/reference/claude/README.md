# Claude executor goldens

`../../../src/claude/testdata/go_executor.json` holds outputs of CLIProxyAPI at
`6fecc6e`, produced in-process by `main.go` from the inputs in `scenarios.json`.

For each scenario the generator writes the scenario's `config.yaml` and fake auth
file, loads them with Go's `config.LoadConfig` and watcher synthesizers, and runs the
real `ClaudeExecutor` (`Execute`, `ExecuteStream` or `CountTokens`) after
`session.Enrich`, as the conductor does. Upstream traffic goes to a capturing
`http.RoundTripper` passed through the executor's `cliproxy.roundtripper` context
hook, so nothing leaves the process. The captured upstream URL, header map (after
Go's wire-casing pass) and body are recorded with Go's downstream output.

Reply bodies may contain `{{ALIAS:name}}` (the MCP alias Go assigned to `name`) and
`{{DRIFT:name}}` (that alias with a different tool word); the substituted reply is
recorded so the Rust replay restores exactly what Go restored. The file also records
sjson `SetRawBytes`/`DeleteBytes`/`SetBytes` results used by `rawjson.rs`.

The Rust replay (`claude::tests::executor_scenarios_match_go`) feeds the same config
and auth files through `cpa_core::config::credentials::load`, runs the request
pipeline and compares upstream bytes and headers. Random values Go and Rust generate
independently (`x-client-request-id`, the device and session in non-CLI fake user IDs
and the CCH over them) are normalized; everything else is byte-exact. Wire header
order is not observable through a `RoundTripper`; `./harness/run` covers it.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/claude/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/reference/claude/scenarios.json" "$crate/src/claude/testdata/go_executor.json"
)
```

The date in the current-date reminder is the generation date and is recorded per
scenario; the replay pins its clock to it.
