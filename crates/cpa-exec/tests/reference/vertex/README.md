# Vertex executor goldens

`../../fixtures/vertex_go.json` holds outputs of the unmodified Go code at CLIProxyAPI
`6fecc6e`:

- `scenarios`: `GeminiVertexExecutor` (`Execute`, `ExecuteStream`, `CountTokens`) for
  service-account files and `api-keys.vertex` entries. Each records every raw HTTP
  request the executor sent, in order (the token exchange first), and what it returned.
- `normalize`: `vertex.NormalizeServiceAccountMap` on pasted private-key variants.
- `import`: `cmd.DoVertexImport` into an empty auth dir: the files it wrote, by name.
- `tls`: the throwaway CA and `*.googleapis.com` leaf the captures use; `keys`: the fake
  RSA and EC keys of the scenarios.

Google hosts (`oauth2.googleapis.com`, `<location>-aiplatform.googleapis.com`,
`aiplatform.googleapis.com`) are reached through a local CONNECT proxy (`PROXY`, the
credential's `proxy_url` or the key's `proxy-url`) that terminates TLS with the test CA.
The Go process trusts it through `SSL_CERT_FILE`; the Rust test trusts it through
`proxy::Hooks`. API keys with a `base-url` go to a plain capture server (`UPSTREAM`).
Nothing contacts Google. The mock servers offer only HTTP/1.1 (ALPN `http/1.1`), so
both sides speak it; in production Go's cloned default transport and Rust's shared
client (`crate::proxy`) both negotiate HTTP/2 with Google.

Go signs the JWT with RS256 (PKCS#1 v1.5, deterministic) at `time.Now()-10s`; the Rust
test reads Go's `iat` from the recorded assertion and signs at the same second, so the
token exchange compares byte for byte. Imagen responses carry `imagen-<UnixNano>` IDs,
normalized in the comparison. A Go error without a status code (status 0) is answered
500 by Go's handler.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate"/tests/reference/vertex/*.go "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/fixtures/vertex_go.json"
)
```

Every key in the fixture is generated at regeneration time and authorizes nothing.

## v8.0.35 recordings

`../../fixtures/vertex_go_v8.0.35.json` holds scenarios recorded from v8.0.35
(`ae3ee3be`, clean checkout) with the generator above; the Rust test replaces the
`6fecc6e` recordings of the same names with them and adds the new ones (`2d7f5377`'s
API-key base URLs, `ceb4397f`'s trailing thought-only turns, the Claude
`MALFORMED_FUNCTION_CALL` error of `7f2f17d7`, `5ec31442`'s read error without `[DONE]`,
two Responses answers whose usage keys moved, and a client that goes away mid-stream:
`key_stream_client_drop_*`, whose reply `stall`s after its body while the generator
cancels the request once the stream is idle, before and after the terminal chunk of
`dbc4aad9`; their usage records carry Go's `fail_status` and `fail_body`). Every case still runs; `RSFIX_ONLY`
(a Go regexp on the name) selects the ones written, and `RSFIX_KEYS` names the full
recording whose private keys the service-account scenarios sign with, so their token
requests match it:

```sh
RSFIX_KEYS="$crate/tests/fixtures/vertex_go.json" RSFIX_ONLY='^(key_stream_trailing_slash_base|key_publishers_prefix_base|key_publishers_prefix_count|key_version_suffix_base|key_execute_single_thought_turn|key_stream_single_thought_turn|key_stream_trailing_thought_turns|key_stream_trailing_thought_dropped|key_execute_thought_function_call_kept|key_claude_malformed_execute|key_claude_malformed_stream|sa_responses_tool_ids_stripped|key_codex_tool_integer_types|key_responses_stream_read_error|key_stream_client_drop_mid_read|key_stream_client_drop_after_terminal)$' \
  go run . "$crate/tests/fixtures/vertex_go_v8.0.35.json"
```
