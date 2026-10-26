# Go URL serialization

`zz_rsfix_url_string_test.go` records, for each input, Go's `url.Parse` followed by
`URL.String()` and what the xAI and Codex WebSocket URL builders
(`buildXAIResponsesWebsocketURL`, `buildCodexResponsesWebsocketURL`) return. The
inputs include spaces, non-ASCII and non-UTF-8 path and host bytes, empty and escaped
fragments, escaped user info, omitted and opaque hosts. `src/xai_url.rs`
(`string_follows_go`), `src/xai_ws_tests.rs` and `src/codex_ws_tests.rs`
(`ws_url_follows_go`) replay `../../fixtures/go_url_string.json`.

```sh
git -C <CLIProxyAPI> worktree add /tmp/cpa-url ae3ee3be
cp zz_rsfix_url_string_test.go /tmp/cpa-url/internal/runtime/executor/
(cd /tmp/cpa-url && RSFIX_OUT=/tmp/rsfix-url go test -count=1 -run TestRSFixURLString ./internal/runtime/executor/)
cp /tmp/rsfix-url/go_url_string.json <cliproxy-rs>/crates/cpa-exec/tests/fixtures/
```

Recorded with Go 1.26.4. Nothing is dialed; run it with external network denied.
