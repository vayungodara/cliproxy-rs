# AI Studio executor and `/v1/ws` relay goldens

`../../fixtures/aistudio_go.json` holds outputs of the unmodified Go `AIStudioExecutor`
(`NewAIStudioExecutor`, CLIProxyAPI `6fecc6e`) running over a real `wsrelay.Manager`.
The generator serves the manager's handler on a local `httptest` server and connects a
gorilla websocket client that plays the AI Studio browser: for each scenario it records
the `http_request` frame it receives (message ID as `ID`, `sent_at` as `SENT_AT`) and
answers with the scenario's scripted replies (`http_response`, `stream_start`,
`stream_chunk`, `stream_end`, `error`). It records what the executor returned: the
payload, the stream chunks, or the status and message of the error, and the usage
record Go's reporter published. Nothing contacts Google; no credential exists.

Two more sections pin the relay protocol: `decode` is what gorilla's `ReadJSON`
(`json.NewDecoder(r).Decode`) makes of frames, and `encode` is gorilla's `WriteJSON`
(`json.NewEncoder(w).Encode`) of messages.

Regenerate offline (dependencies downloaded beforehand) from an empty directory holding
these `.go` files and a `go.mod` for module
`github.com/router-for-me/CLIProxyAPI/v8/fixture` that requires
`github.com/router-for-me/CLIProxyAPI/v8` with a `replace` to the Go checkout:

```
GOFLAGS=-mod=mod GOPROXY=off go run . <repo>/crates/cpa-exec/tests/fixtures/aistudio_go.json
```

## v8.0.35 recordings

`../../fixtures/aistudio_go_v8.0.35.json` holds scenarios recorded from v8.0.35
(`ae3ee3be`, clean checkout) with the same generator; the Rust test replaces the
`6fecc6e` recordings of the same names with them and adds the new ones (the Claude
`MALFORMED_FUNCTION_CALL` error of `7f2f17d7`, and `5ec31442`'s 2xx first status,
late non-2xx `http_response` and `[DONE]` at the stream end). Every case still runs;
`RSFIX_ONLY` (a Go regexp on the name) selects the ones written:

```
RSFIX_ONLY='^(stream_claude_bare|stream_codex_no_pair|stream_first_status_201|stream_http_response_error_after_start|stream_responses_http_response|execute_claude_malformed|stream_claude_malformed|stream_claude_malformed_then_relay_error)$' \
  GOFLAGS=-mod=mod GOPROXY=off go run . <repo>/crates/cpa-exec/tests/fixtures/aistudio_go_v8.0.35.json
```
