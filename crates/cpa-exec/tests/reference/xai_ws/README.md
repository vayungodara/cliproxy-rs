# xAI upstream WebSocket goldens

`../../fixtures/xai_ws_go.json` holds what the unmodified Go `XAIAutoExecutor`
(CLIProxyAPI `6fecc6e`, constructed as production registers it) did on downstream
WebSocket turns (`WithDownstreamWebsocket`, plus `WithRequiredUpstreamWebsocket` for a
continuation). Each scenario runs its turns in order on one execution session against a
scripted local upstream: a gorilla WebSocket endpoint for `/v1/responses` (upgrade
rejections, replies per received frame, binary frames, close codes) and plain HTTP for
`/v1/responses/compact`. For every turn the generator records the downstream chunks or
error, the upgrade requests (path and headers, without the random key), the frames the
upstream received and the compaction requests. Credentials are fake API keys whose
`base_url` is the local upstream; nothing contacts a provider. Normalization is limited
to the upstream address (`UPSTREAM`).

`xai_ws_tests.rs` replays the scenarios against an axum upstream with the same script.
It compares the `Authorization`, `Content-Type`, `X-Grok-Conv-Id` and custom headers of
each upgrade; the rest of the handshake belongs to the shared Go-standard dialer.

Regenerate with a temporary module inside the reference module's internal-package
boundary, as for `../xai` (run it without network access):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate"/tests/reference/xai_ws/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/xai_ws_go.json"
)
rm -rf "$tmp"
```
