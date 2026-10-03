# Realtime goldens

Replayed by `tests/realtime.rs`. Both generators are test files copied into the
CLIProxyAPI `6fecc6e` tree. Run them with a dead-end proxy so nothing can reach OpenAI
even if a case escapes its local mock (loopback stays direct):

```sh
export HTTPS_PROXY=http://127.0.0.1:9 HTTP_PROXY=http://127.0.0.1:9
cp zz_rsfix_realtime_http_test.go "$reference/internal/api/"
cp zz_rsfix_live_ws_test.go "$reference/internal/client/codex/live/"
RSFIX_OUT=/tmp/rsfix go test -count=1 -run TestRSFixRealtimeHTTP ./internal/api/
RSFIX_OUT=/tmp/rsfix go test -count=1 -run TestRSFixLiveWebsockets ./internal/client/codex/live/
cp /tmp/rsfix/realtime_http_go.json /tmp/rsfix/codex_live_ws_go.json /tmp/rsfix/codex_live_ws_raw_go.json /tmp/rsfix/codex_live_ws_end_go.json ../../fixtures/
```

- `realtime_http_go.json`: the real Go server (routes, access manager with
  `api-keys: [good-key, other-key]`, realtime middleware, live handler) with an executor that runs
  the real Codex `PrepareRequest` and records the upstream request instead of sending
  it. Cases run in order against shared state (calls, client secrets). Random keys,
  session IDs and expiry times are masked. Headers are what Go's executor saw, before
  its transport adds Host, User-Agent, Accept-Encoding and Content-Length.
- `codex_live_ws_go.json`: the real sideband and standard Realtime handlers relaying to
  a local gorilla upstream: handshake failures, subprotocols, relayed frames and close
  codes on both sides. The package's capture executor does not apply `header:`
  attributes, so those are covered by the HTTP goldens only.
- `codex_live_ws_raw_go.json`: raw TCP handshakes no WebSocket client library sends
  (bad or missing challenge key, token-list `Upgrade`, version lists), with gorilla's
  status and whether the call survived.
- `codex_live_ws_end_go.json`: the close frame the upstream receives when the downstream
  sends a protocol violation (RSV1 set), closes TCP without a close frame, or resets the
  connection (`websocketCloseDetails`).

`codex_live_tunnel_go.json` comes from `zz_rsfix_live_tunnel_test.go` (copy it next to
the others in `internal/client/codex/live/`, run `-run TestRSFixLiveTunnelVectors`, copy
`/tmp/rsfix/live_tunnel_vectors.json` to `../../fixtures/codex_live_tunnel_go.json`). It
is replayed by `src/realtime/tunnel_tests.rs`, built with `--features media-relay`:

- `prepare`: `prepareProxiedUpstreamAnswer` on answers built from `testProxySDP`, with
  the error or the rewritten SDP (each tunnel listener masked as `LISTENn`), targets,
  listener families, the expected STUN user and the remote password.
- `frames`: `readValidatedICEBindingFrame` on frames pion/stun builds, including
  attributes after MESSAGE-INTEGRITY, which pion's strict decoder drops.
- `targets`: `isPublicProxyTarget` at every prefix boundary.
- `proxies`: `proxyutil.BuildDialer` mode and error, and `proxyScheme`.
- `dialers`: the bytes Go's SOCKS5 and HTTP `CONNECT` dialers send to a scripted local
  proxy, and what they return (error or the first bytes through the tunnel).

Never add a case that lets Go's sideband dial its default base URL: the HTTP harness
cannot redirect it. Run the generators with external network denied, for example inside
`unshare -rn` with only loopback up and `GOPROXY=off`.
