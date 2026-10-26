# Chat Completions streams without a finish reason

`zz_rsfix_stream_truncation_test.go` streams fixed chunks through Go's own Chat
Completions and legacy Completions handlers and records what the client receives
(Go 97dd9eec: a stream that closes cleanly before any chunk carried a `finish_reason`
ends with an error frame instead of `data: [DONE]`). The first nine scenarios are the
cases of Go's `openai_handlers_stream_truncation_test.go`; the rest pin gjson edges.
`tests/openai_compat_routes.rs` serves the same chunks from a mock OpenAI-compatible
upstream through the Rust router and compares byte for byte.

```sh
git -C <CLIProxyAPI> worktree add /tmp/cpa-trunc ae3ee3be
cp zz_rsfix_stream_truncation_test.go /tmp/cpa-trunc/sdk/api/handlers/openai/
(cd /tmp/cpa-trunc && RSFIX_OUT=/tmp/rsfix-trunc go test -count=1 -run TestRSFixStreamTruncation ./sdk/api/handlers/openai/)
cp /tmp/rsfix-trunc/stream_truncation_go.json <cliproxy-rs>/crates/cpa-server/tests/fixtures/
```

Run it with external network denied; nothing is sent upstream.
