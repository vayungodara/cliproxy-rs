# Home fixtures

Expected values in these files come from CLIProxyAPI at commit `6fecc6e`, not from this
crate.

- `concurrency_dispatch_accounted.json`, `concurrency_dispatch_busy.json`,
  `concurrency_release.json`, `credential_in_flight_contract.json`: copied from Go's
  `internal/home/testdata`.
- `go_home_golden.json`: output of `go/home_golden_test.go.txt` placed at
  `internal/home/zz_rustgolden_test.go`. It records the exact RESP commands Go's Home
  client sends for every operation (against Go's own recording test server), the auth
  dispatch request JSON, KV `SET` arguments, JWT claim validation, cluster node ordering
  and `SUBSCRIBE` arguments per recovery state.
- `go_auth_golden.json`: output of `go/auth_golden_test.go.txt` placed at
  `sdk/cliproxy/auth/zz_rustgolden_test.go`. It records in-flight snapshot frames for
  bounded configurations (including the inputs of Go's `TestEncodeHomeInFlightFreeze*`
  tests, named after them), Home error decoding, concurrency model keys and dispatch
  envelopes.

Regenerate with Go 1.26 from this directory, with `CPA` naming a CLIProxyAPI checkout:

```sh
out=$PWD
cp go/home_golden_test.go.txt "$CPA/internal/home/zz_rustgolden_test.go"
cp go/auth_golden_test.go.txt "$CPA/sdk/cliproxy/auth/zz_rustgolden_test.go"
cd "$CPA"
RUST_GOLDEN="$out/go_home_golden.json" go test -count=1 -run TestRustGolden ./internal/home/
RUST_GOLDEN="$out/go_auth_golden.json" go test -count=1 -run TestRustGoldenHome ./sdk/cliproxy/auth/
```
