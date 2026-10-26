# Antigravity executor and login goldens

`../../fixtures/antigravity_go.json` holds outputs of the unmodified Go
`AntigravityExecutor`, its login helpers and user agents at CLIProxyAPI `6fecc6e`.
`../../fixtures/antigravity_v8_0_27_go.json` holds the executor scenarios of
`v8_0_27.go`, recorded from v8.0.27 (`bf9341629ef2300ac9d846a2e80fe2756f63a671`) for the
behaviour ported from it. `antigravity_tests.rs` replays both on the Rust executor.

Every executor scenario runs `Execute`, `ExecuteStream` or `CountTokens` against a
one-shot raw TCP capture server and records the exact upstream request and what the
executor returned. Nothing contacts Google: the version cache is never refreshed, so
every user agent uses the fallback version, and every token is fake.

The generator imports Go's internal packages, so it runs from a scratch copy of the Go
tree (never the reference checkout itself), with external network denied:

```sh
copy=$(mktemp -d)
git -C CLIProxyAPI archive 6fecc6e | tar -C "$copy" -xf -
mkdir -p "$copy/cmd/antigravity-goldens"
cp *.go "$copy/cmd/antigravity-goldens/"
cd "$copy" && GOTOOLCHAIN=local GOPROXY=off go run ./cmd/antigravity-goldens out.json
```

For the v8.0.27 scenarios, archive `v8.0.27` instead and run
`ANTIGRAVITY_GO_SET=v8.0.27 go run -tags v8027 ./cmd/antigravity-goldens out.json`.
`../../fixtures/antigravity_v8_0_35_go.json` holds the scenarios of `v8_0_35.go`, recorded
the same way from v8.0.35 (`ae3ee3be303fb07fb8173279923413005bf32177`) with
`ANTIGRAVITY_GO_SET=v8.0.35 go run -tags v8035 ./cmd/antigravity-goldens out.json`: a
client that goes away mid-stream (the upstream `stall`s after its body and the generator
cancels the request once the stream is idle; the usage record carries Go's
`fail_status` and `fail_body`), and base URLs Go refuses (`execute_escaped_host_base`,
`stream_hostless_base`, `count_schemeless_base`), and when a stream commits its reasoning
replay (Go a4acc9f7 and, for `replay_after_completed_*`, c997cbb5's commit at a
Responses client's `response.completed`; the `replay_*` scenarios each send a
`Session-Id` and record under `replay` what the ledger holds for `replay_key` once the
attempt ended), and the `trailing_thought_*` scenarios: trailing thought-only model
turns dropped before the boundary user turns (Go a2718314; the Responses cases are Go's
`TestAntigravityDropsTrailingStructuredReasoningTurn`). It also re-records, under their
own names, the base scenarios whose
OpenAI chat output v8.0.35 changed (thoughts tokens in `completion_tokens`, ab151f5d;
no `"tool_calls": null` in stream deltas, dfe0a0ef): `go_reference_scenarios` skips
every base scenario the v8.0.35 file names, and `go_v8_0_35_scenarios` replays it. Its
`requestId` is random; the replay normalizes it.
