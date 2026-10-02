# Device-provider fixtures (Kimi, Meta, Devin)

Every JSON file here was produced by the unmodified Go executors and auth code of
CLIProxyAPI at `6fecc6e`, run against a local raw HTTP/1.1 capture server. The Rust
tests (`src/kimi_tests.rs` and siblings) replay the same scripted upstream responses and
compare what the Rust executors send and return. Expected values are never derived from
the Rust code.

Each executor fixture records:

- `credential`: the auth metadata the Go executor received. Tests point the base URL at
  their own mock.
- `request`: client format, model, stream flag and body.
- `responses`: the scripted upstream answers, in order.
- `upstream`: every request Go sent, with ordered, cased header lines and exact body.
- `downstream`: what the Go executor returned (body, stream chunks or error).

`vectors.json` holds pure-function input/output pairs from the Go helpers.

## Regenerating

```sh
git clone https://github.com/router-for-me/CLIProxyAPI && cd CLIProxyAPI
git checkout 6fecc6e
cp <cliproxy-rs>/crates/cpa-exec/tests/device_fixtures/go/zz_rsfix_*_test.go internal/runtime/executor/
RSFIX_OUT=/tmp/rsfix go test -count=1 -run 'TestRSFix' ./internal/runtime/executor/
cp -r /tmp/rsfix/* <cliproxy-rs>/crates/cpa-exec/tests/device_fixtures/
```

The generator pins `buildinfo.Version` to the Rust crate version so `User-Agent` and
`X-Msh-Version` compare exactly. Values that depend on the machine (Host port, hostname,
OS/arch, random device IDs, timestamps) are masked by the tests, which check the Rust
values against their own rules instead. Nothing here contacts a real provider.
