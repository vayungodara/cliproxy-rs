# Codex client catalog for a config

`zz_rsfix_codex_models_test.go` runs Go's config synthesis, model registration
(`Service.registerModelsForAuth`) and `/v1/models?client_version=` handler for one config
and records each catalog. `tests/codex_models.rs` serves the same config through the
Rust router and compares byte for byte. `TestRSFixCodexBuiltins` writes `codex_builtins_go.json`:
Go's plain `/v1/models` for a Codex API key without configured models, which carries the
gpt-image-* built-ins.

```sh
cp zz_rsfix_codex_models_test.go <CLIProxyAPI>/sdk/cliproxy/
cd <CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-cm go test -count=1 -run 'TestRSFixCodex(Models|Builtins)' ./sdk/cliproxy/
cp /tmp/rsfix-cm/codex_*_go.json <cliproxy-rs>/crates/cpa-server/tests/fixtures/
```

Run it with external network denied; nothing is sent upstream.
