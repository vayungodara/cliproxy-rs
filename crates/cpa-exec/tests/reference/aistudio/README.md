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
