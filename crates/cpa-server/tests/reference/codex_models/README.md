# Codex client catalog for a config

`zz_rsfix_codex_models_test.go` runs Go's config synthesis, model registration
(`Service.registerModelsForAuth`) and `/v1/models?client_version=` handler for one config
and records each catalog. `tests/codex_models.rs` serves the same config through the
Rust router and compares byte for byte.

```sh
cp zz_rsfix_codex_models_test.go <CLIProxyAPI>/sdk/cliproxy/
cd <CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-cm go test -count=1 -run TestRSFixCodexModels ./sdk/cliproxy/
cp /tmp/rsfix-cm/codex_models_go.json <cliproxy-rs>/crates/cpa-server/tests/fixtures/
```

Run it with external network denied; nothing is sent upstream.
