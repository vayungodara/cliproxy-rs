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

`zz_rsfix_codex_catalog_test.go` records Go's Codex client catalog builder at v8.0.35
(`internal/client/codex/models`) with the registry facts it looked up, including a vector
with a native tool_search capability (`upstream.claude.tool-search` on).
`src/codex_catalog_tests.rs` replays `tests/fixtures/codex_catalog_go.json.gz`. Run it in a
scratch worktree of the reference, never the reference itself:

```sh
cp zz_rsfix_codex_catalog_test.go <scratch CLIProxyAPI>/internal/client/codex/models/
cd <scratch CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-cat go test -count=1 -run TestRSFixCodexCatalog ./internal/client/codex/models/
python3 -c 'import gzip,sys; sys.stdout.buffer.write(gzip.compress(open("/tmp/rsfix-cat/codex_catalog_go.json","rb").read(), 9, mtime=0))' \
  > <cliproxy-rs>/crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz
```
