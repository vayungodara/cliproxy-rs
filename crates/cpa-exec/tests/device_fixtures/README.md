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
for f in $G/zz_rsfix_*_test.go; do case $(basename $f) in zz_rsfix_frames_test.go|zz_rsfix_devin_paste_test.go|zz_rsfix_devin_view_test.go) ;; *) cp $f internal/runtime/executor/ ;; esac; done
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

These files come from v8.0.35 (`ae3ee3be`) instead, after Go moved the payload rules
to the end of the pipeline (`868e137`): `meta/payload-rules-and-headers.json` (re-recorded:
the rules now run after the instructions default) and
`kimi/{chat,responses}-payload-override-after-normalize.json` (a configured temperature
survives the temperature normalization), and `kimi/chat-stream-read-error.json` and
`kimi/chat-stream-read-error-interactions-client.json` (`4f2e5270`: a read error ends the
stream before the synthetic `[DONE]` and keeps the usage; their scripted response's
`missing` promises 40 bytes the server never sends). Record them the same way from a
v8.0.35 checkout and copy only those files, for example with
`-run 'TestRSFixKimi$/chat-stream-read-error'`. `meta/claude-stream-input-estimate.json`
(a Claude client's `message_start` estimate, f3e36f19) is a v8.0.35 recording too:
`-run 'TestRSFixMeta$/claude-stream-input-estimate'`.

Devin's request logs follow v8.0.35 too, where Go logs the payload rules' JSON view of
the protobuf (`FinalizeDevinPayload`) instead of its readable request summary:

- `devin/{chat-payload-rules,chat-payload-nan,interactions-payload-bad-signature,interactions-temperature-infinite}.json`
  are v8.0.35 recordings (`TestRSFixDevin`, copied whole).
- `devin/user-turn-*.json` (refusals of user turns left with media Devin cannot send, Go
  f51624aa) and `devin/user_turns.json` (`CheckDevinUserTurns` after
  `parseInteractionsPayload`) are v8.0.35 recordings too, made with
  `-run 'TestRSFixDevin$/user-turn|TestRSFixDevinUserTurns'` and copied whole.
- In every other Devin fixture, only the `Body:` section of `extra.capture.request` is
  replaced, by v8.0.35's view of the frame that fixture's `upstream` recorded:

  ```sh
  cp $G/zz_rsfix_devin_view_test.go internal/runtime/executor/helps/
  RSFIX_IN=<cliproxy-rs>/crates/cpa-exec/tests/device_fixtures/devin RSFIX_OUT=/tmp/views.json \
    go test -count=1 -run '^TestRSFixDevinView$' ./internal/runtime/executor/helps/
  ```

  The view comes from the 6fecc6e frame rather than a v8.0.35 recording because v8.0.35
  also changed what Devin sends (its catalog caps `max_tokens` lower), which is not
  ported yet.

`devin/{responses-stream,responses-stream-ordering,responses-stream-trailer-after-content}.json`
carry the `downstream` of a recording at `6fecc6e` with `ed4d972e` and `b0db5448`
cherry-picked in that order (both apply cleanly): the Interactions to Responses
translator's reasoning summary events and per-response fallback item IDs
(`item_<uuid>_<n>`) of v8.0.35, without the Devin request changes that are not ported
yet. Record as above in that tree with `-run 'TestRSFixDevin$/responses-stream'`, after
deleting `TestRSFixDevinUserTurns` from the copied generator (it needs v8.0.35 helpers),
then run `TestRSFixResponsesFrames`, and replace only `downstream` (the other sections
stay as described above).

The generator pins `buildinfo.Version` to `0.1.0`, and the fixture loader puts the running
crate version in its place, so `User-Agent` and `X-Msh-Version` still compare exactly after a
release bump. Values that depend on the machine (Host port, hostname,
OS/arch, random device IDs, timestamps, Devin message IDs, Sentry traces and unseeded
fingerprints) are masked by the tests, which check the Rust
values against their own rules instead. Nothing here contacts a real provider.
