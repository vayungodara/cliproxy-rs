# xAI OAuth goldens

`../../fixtures/xai_auth_go.json` holds outputs of unmodified Go code from CLIProxyAPI
`6fecc6e`: `ValidateOAuthEndpoint`, `CredentialFileName`, the login manager with the
xAI authenticator and the real `FileTokenStore` (existing-file merge included), and
`XAIExecutor.Refresh`. Requests to `https://auth.x.ai` are rewritten in the generator's
transport to a local raw-TCP capture server; nothing contacts xAI and every token is fake.

Normalization: the capture server address becomes `UPSTREAM` and RFC 3339 UTC
timestamps in saved files become `TIME`. Go waits its real 5 s poll interval, so
generation takes about 20 s; the Rust tests use the `minPollInterval` knob instead,
which changes timing only.

The `validate` vectors were re-run with Go 1.26.4 against v8.0.35 (`ae3ee3be`): only
`https://attacker.example\[::1%25.x.ai]/oauth2/token` changed, because Go 1.26.4's
`url.Parse` refuses a host whose last `[` does not start it (`invalid IP-literal`).

Regenerate like the OpenAI-compatible goldens (see ../openai_compat/README.md), with
this directory's `main.go` and output `xai_auth_go.json`.
