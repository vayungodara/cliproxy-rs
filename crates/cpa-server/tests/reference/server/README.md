# Server-core goldens

`../../fixtures/server_go.json` holds outputs of CLIProxyAPI at `6fecc6e`, produced
in-process by `main.go`. It runs real Go code only and opens no sockets:

- `sanitize`: `auth.SanitizeUpstreamErrorSummary`.
- `extract`: `auth.ExtractUpstreamErrorSummary`.
- `availability`: `registry.RegisterClient`, `ApplyClientModelProjections` and
  `GetAvailableModels` on the real model registry (Go `modelRegistrationAvailability`).
- `cooldown`: `auth.Manager.MarkResult` sequences on fresh credentials, reported as
  whole seconds until each model's `NextRetryAfter`.
- `session`: `session.ExtractSessionInfo`, `session.Enrich` and `auth.ExtractSessionID`.
- `affinity`: scripted `SessionAffinitySelector` runs (`Enrich`, `Pick` over fill-first,
  `OnResult`). The `lcp_*` cases pick through the mixed-provider picker with a caller
  scope, so requests without an explicit session reach the Merkle LCP matcher, and
  record the LCP metadata each pick writes.
- `cooldown_files`: the `.cds` files `FileCooldownStateStore` writes after a
  `MarkResult` sequence, timestamps masked as `<time>`.
- `alt`: `handlers.BaseAPIHandler.GetAlt` on raw query strings.
- `auth_kind`: `auth.Auth.AuthKind` over attribute and metadata shapes.
- `by_provider`: `GetAvailableModelsByProvider` and `GetModelInfo` web-search
  aggregation on the real registry.
- `resolved_config` / `resolved`: the unexported `attachResolvedExecutionModelInfo`
  per attempt, through `GoldenResolvedModelInfo` in `auth_export.go.overlay`, on
  `ConfigSynthesizer` auths (some with a mutated `config_index`) and file-style auths.

Replayed by `sanitize::tests`, `registry::tests`, `scheduler::tests`, `session::tests`,
`cooldown_store::tests`, `claude::tests` and `capabilities::tests`.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # at 6fecc6e
crate=/absolute/path/to/cliproxy-rs/crates/cpa-server
tmp=$(mktemp -d)
cp "$crate/tests/reference/server/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  # The overlay compiles one golden-only file into sdk/cliproxy/auth without
  # touching the reference tree.
  cp "$crate/tests/reference/server/auth_export.go.overlay" "$tmp/auth_export.go.src"
  printf '{"Replace":{"%s/sdk/cliproxy/auth/zz_golden_export.go":"%s/auth_export.go.src"}}' \
    "$reference" "$tmp" > overlay.json
  go run -overlay overlay.json . "$crate/tests/fixtures/server_go.json"
)
```

Inputs that carry secrets use `fake` values only.
