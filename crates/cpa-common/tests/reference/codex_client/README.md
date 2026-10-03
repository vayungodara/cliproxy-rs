# codex_client Go vectors

`zz_rsfix_codex_client_test.go` records what Go's
`internal/client/codex/optimize-multi-agent-v2` functions return at CLIProxyAPI 6fecc6e.
`src/codex_client_tests.rs` replays the output, `tests/fixtures/codex_client_go.json`.

Regenerate (Go 1.26, no network beyond module download):

```sh
cp zz_rsfix_codex_client_test.go <CLIProxyAPI>/internal/client/codex/optimize-multi-agent-v2/
cd <CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-cc go test -count=1 -run TestRSFixCodexClient ./internal/client/codex/optimize-multi-agent-v2/
cp /tmp/rsfix-cc/codex_client_go.json <cliproxy-rs>/crates/cpa-common/tests/fixtures/
```

The `spawn_models` vector looks models up in Go's static catalog, so the Rust replay runs
without an installed registry and falls back to the pinned catalog the same way.
The `prepare_tools` and `optimize` vectors register two models first and store the model
list Go built from them in `markdown`; the replay passes that list in.
