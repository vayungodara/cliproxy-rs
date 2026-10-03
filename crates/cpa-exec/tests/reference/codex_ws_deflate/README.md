# Codex WebSocket permessage-deflate goldens

`../../fixtures/codex_ws_deflate_go.json` records gorilla/websocket v1.5.3 (the version
CLIProxyAPI `6fecc6e` uses) dialing with `EnableCompression` and write compression off, as
the Codex WebSocket executor does:

- `negotiations`: the `Sec-WebSocket-Extensions` offer the dialer sent, and for each
  server answer whether `Dial` failed (`websocket: invalid compression negotiation`) or
  read RFC 7692's compressed "Hello" (compression agreed) or failed reading it (not agreed).
- `frames`: raw server frame sequences after an agreed handshake (RFC 7692 examples,
  fragments with interleaved control frames, RSV bits, masking, corrupt data, a larger
  message compressed like gorilla's writer) and the messages gorilla read from each.

A loopback TCP server answers each upgrade itself, so headers and bytes are exact. Nothing
leaves the machine. Regenerate with the reference module's `go.mod` (it requires
gorilla/websocket), for example from the Codex goldens' temporary module:

```sh
cp main.go "$tmp/main.go"
(cd "$tmp" && GOPROXY=off go run . "$crate/tests/fixtures/codex_ws_deflate_go.json")
```

Replayed by `src/codex_ws_deflate_tests.rs`.
