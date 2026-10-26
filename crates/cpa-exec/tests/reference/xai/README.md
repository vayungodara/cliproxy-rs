# xAI executor goldens

`../../fixtures/xai_go.json` holds outputs of the Go `XAIExecutor` at CLIProxyAPI
`6fecc6e` with the Claude translator commits listed in
`../claude/README.md` ("Translator commits in the recorded trees"); of those, only
`2c1dcc7c` changes these recordings (Claude clients with a web search tool ask for
`web_search_call.action.sources`). For every scenario the generator builds the credential
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
reference=/absolute/path/to/CLIProxyAPI   # 6fecc6e with the commits above
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

## v8.0.35 recordings

`../../fixtures/xai_go_v8.0.35.json` holds the scenarios whose answers changed when Go moved the payload
rules to the end of the pipeline (`868e137`), recorded from v8.0.35
(`ae3ee3be303fb07fb8173279923413005bf32177`, clean worktree) with the generator above;
the Rust test replaces the `6fecc6e` recordings of the same names with them. Today:
`payload_override_stop_dropped` (the configured `stop` now survives the built-in delete; the name is kept).
The file also holds v8.0.35-only scenarios: `web_search_unsupported_options` and
`web_search_context_size_only_stream` (bbfd7886: the web_search options xAI rejects are
dropped). Every case still runs; `RSFIX_ONLY` (a Go regexp on the name) selects the
ones written, which are then appended to the file:

```sh
RSFIX_ONLY='^payload_override_stop_dropped$' go run . "$crate/tests/fixtures/xai_go_v8.0.35.json"
RSFIX_ONLY='^web_search_(unsupported_options|context_size_only_stream)$' go run . new.json
```
