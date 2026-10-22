# Codex `-siwc-bind` goldens

`../../fixtures/codex_siwc_bind_go.json` records what CLIProxyAPI v8.0.35 (`ae3ee3be`)
does around the `-siwc-bind` OAuth flow (7b0319fb, internal/cmd/siwc_bind.go):

- `resolve`: `ResolveSIWCBindPath` for a target and an auth-dir, the resolved path or
  the error. `$DIR` is the auth-dir and `$OUT` a directory beside it. `cause` marks a
  wrapped error; its OS text is cut from `error`.
- `device`: `ensureSIWCDeviceIDFromFile`: the stored device ID it keeps, or
  `generated` for a fresh lower-case UUID.
- `save`: `saveSIWCToCodexCredentialFile` with fixed SIWC tokens: the file it wrote
  (`result`) or the error.
- `callback`: `misc.ParseOAuthCallback` for pasted callbacks carrying the issued
  `client_id`.

`src/codex_siwc_tests.rs` `siwc_bind_matches_go` replays every case.

Regenerate with Go 1.26.4 (no network beyond module download):

```sh
cp zz_rsfix_siwc_bind_test.go <CLIProxyAPI>/internal/cmd/
cd <CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-siwc-bind go test -count=1 -run TestRSFixSIWCBind ./internal/cmd/
cp /tmp/rsfix-siwc-bind/codex_siwc_bind_go.json <cliproxy-rs>/crates/cpa-exec/tests/fixtures/
rm <CLIProxyAPI>/internal/cmd/zz_rsfix_siwc_bind_test.go
```
