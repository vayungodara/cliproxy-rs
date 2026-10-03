# Codex client translation and token-count vectors

Go generators run against CLIProxyAPI 6fecc6e. Expected values come from Go only;
nothing reaches the network (CountTokens counts locally).

| Generator | Copy into | Writes | Replayed by |
| --- | --- | --- | --- |
| `zz_rsfix_codex_client_translate_test.go` | `internal/runtime/executor/helps/` | `codex_client_translate_go.json` | `src/codex_client_tests.rs` |
| `zz_rsfix_codex_tokens_test.go` | `internal/runtime/executor/` | `codex_tokens_go.json` | `src/codex_tokens_tests.rs` |

```sh
cp zz_rsfix_codex_client_translate_test.go <CLIProxyAPI>/internal/runtime/executor/helps/
cp zz_rsfix_codex_tokens_test.go <CLIProxyAPI>/internal/runtime/executor/
cd <CLIProxyAPI>
RSFIX_OUT=/tmp/rsfix-cct go test -count=1 -run TestRSFixCodexClientTranslate ./internal/runtime/executor/helps/
RSFIX_OUT=/tmp/rsfix-cct go test -count=1 -run TestRSFixCodexTokens ./internal/runtime/executor/
cp /tmp/rsfix-cct/*.json <cliproxy-rs>/crates/cpa-exec/tests/fixtures/
```

The translate vectors cover `TranslateRequestWithAPIKeyModelCompatibilityForExecutor`
(Responses, Claude and Chat clients to every compat-relevant target, compat on and off,
target executor "" and "codex", Codex and plain user agents) and
`OptimizeCodexMultiAgentV2RequestForAuth` (optimize, orphan and compat combinations, with
an empty model registry so `spawn_agent` descriptions stay unchanged).
