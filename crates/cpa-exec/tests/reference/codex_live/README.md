# Codex live goldens

`../../fixtures/codex_live_go.json` holds outputs of the unexported functions in
`internal/client/codex/live` at CLIProxyAPI `6fecc6e`: call-request shaping
(`prepareCallRequest`, `applyClientSecretCallSession`, `rewriteCallRequestModel` and the
three chained as `Handle` runs them), SDP extraction and replacement, `modelFromJSON`,
`codexRealtimeModel`, `callIDFromLocation`, client-secret session normalization and
request decoding, lifetimes, sideband and direct URLs, and `bearerToken`. Bodies that
are not valid UTF-8 are stored as `b64:` + base64. `{"panic": true}` marks inputs on
which Go writes into a nil map (gin answers 500).

Replayed by `codex_live::tests`. The generator is a test file copied into the
reference tree; it opens no sockets:

```sh
cp zz_rsfix_live_vectors_test.go "$reference/internal/client/codex/live/"
RSFIX_OUT=/tmp/rsfix go test -count=1 -run TestRSFixLiveVectors ./internal/client/codex/live/
cp /tmp/rsfix/codex_live_go.json ../../fixtures/
```
