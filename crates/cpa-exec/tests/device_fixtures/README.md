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
- `extra.usage`: the usage records Go's `UsageReporter` published for the attempt
  (tokens, response model, response service tier, translated reasoning effort), captured
  by a usage plugin. Fixtures recorded before the tier was captured lack `service_tier`.

`vectors.json` holds pure-function input/output pairs from the Go helpers;
`devin/paste_vectors.json` comes from `sdk/auth` (`parseDevinManualPaste`). Devin request
and response bodies are Connect frames, recorded as base64 (`body_b64`).

## Regenerating

```sh
git clone https://github.com/router-for-me/CLIProxyAPI && cd CLIProxyAPI
git checkout 6fecc6e
G=<cliproxy-rs>/crates/cpa-exec/tests/device_fixtures/go
cp $G/zz_rsfix_frames_test.go sdk/api/handlers/openai/
cp $G/zz_rsfix_devin_paste_test.go sdk/auth/
for f in $G/zz_rsfix_*_test.go; do case $(basename $f) in zz_rsfix_frames_test.go|zz_rsfix_devin_paste_test.go) ;; *) cp $f internal/runtime/executor/ ;; esac; done
# Run the generators with external network denied (loopback only), after `go mod download`;
# with GOTOOLCHAIN=local, put a Go >= 1.26 toolchain first on PATH. For example:
#   unshare -rn sh -c 'ip link set lo up && exec unshare --user --map-user='"$(id -u)"' \
#     --map-group='"$(id -g)"' -- env GOPROXY=off GOTOOLCHAIN=local sh'
RSFIX_OUT=/tmp/rsfix go test -count=1 -run 'TestRSFix' ./internal/runtime/executor/
RSFIX_OUT=/tmp/rsfix go test -count=1 -run 'TestRSFixDevinPaste' ./sdk/auth/
# Adds downstream.frames: Responses-route joining of the recorded stream chunks.
RSFIX_OUT=/tmp/rsfix go test -count=1 -run 'TestRSFixResponsesFrames' ./sdk/api/handlers/openai/
cp -r /tmp/rsfix/* <cliproxy-rs>/crates/cpa-exec/tests/device_fixtures/
```

The generator pins `buildinfo.Version` to `0.1.0`, and the fixture loader puts the running
crate version in its place, so `User-Agent` and `X-Msh-Version` still compare exactly after a
release bump. Values that depend on the machine (Host port, hostname,
OS/arch, random device IDs, timestamps, Devin message IDs, Sentry traces and unseeded
fingerprints) are masked by the tests, which check the Rust
values against their own rules instead. Nothing here contacts a real provider.
