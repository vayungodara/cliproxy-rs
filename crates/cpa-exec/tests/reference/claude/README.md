# Claude executor goldens

`../../../src/claude/testdata/go_executor.json` holds outputs of CLIProxyAPI at
`6fecc6e`, produced in-process by `main.go` from the inputs in `scenarios.json` (the
v8.0.27 goldens below use the same generator). `../../../src/claude/testdata/go_unit.json`
holds Go's own executor tests at `6fecc6e`, recorded by `unit/record.py`.

## Translator commits in the recorded trees

The translators to Claude follow v8.0.35 (`docs/parity-audit/upstream-v8.0.35.md`), and
Go's executor recordings carry the translated bodies. Each recorded tree is therefore
its reference commit with the translator commits that change those bodies
cherry-picked on top, in a scratch clone (the reference checkout is not modified):

| Recording | Tree |
| --- | --- |
| `go_executor.json`, `go_unit.json`, `../../fixtures/xai_go.json` | `6fecc6e` + `a881c036`, `d510dccf`, `2c1dcc7c`, `0d18d3b3` |
| `go_executor_v8.0.27.json`, `go_unit_v8.0.27.json` | `bf93416` (v8.0.27, which already has the first three) + `0d18d3b3` |

- `a881c036` and `0d18d3b3`: Responses and Chat Completions clients that send no
  output limit get the registry's `max_tokens` for the model instead of 32000.
  `0d18d3b3` also strips that synthesized limit from the original translation that
  payload rules read, so a `payload.default` rule for `max_tokens` still applies
  (Go's `TestClaudeExecutor_ChatCompletionsDefaultMaxTokensAndPayloadDefaultRule`,
  recorded in `go_unit.json`).
- `d510dccf`: a Gemini client's camelCase `systemInstruction` reaches Claude.
- `2c1dcc7c`: Claude clients with a web search tool ask Codex and xAI for
  `web_search_call.action.sources`.

The first three apply cleanly to `6fecc6e`. `0d18d3b3` conflicts only where `6fecc6e`
calls the payload rules from another place: the resolution keeps `6fecc6e`'s code,
passes `stripTranslatedDefaultMaxTokens(from, originalPayload, originalTranslated)` to
its `ApplyPayloadConfigWithTrackedPaths` calls in `claude_executor_execute.go` and
`claude_executor_stream.go`, imports `sdktranslator` in `claude_executor.go`, and fixes
the two translator tests to `6fecc6e`'s single-value converter signatures (and the
model the upstream test switched to); that resolved commit is
[`0d18d3b3-on-6fecc6e.patch`](0d18d3b3-on-6fecc6e.patch). On v8.0.27, `0d18d3b3`
cherry-picks cleanly. Rebuild and check the trees with:

```sh
git checkout 6fecc6e && git cherry-pick a881c036 d510dccf 2c1dcc7c \
  && git am /absolute/path/to/cliproxy-rs/crates/cpa-exec/tests/reference/claude/0d18d3b3-on-6fecc6e.patch
git rev-parse 'HEAD^{tree}'   # e3c8744a1617762452581257505d5d72a12700b1
git checkout bf93416 && git cherry-pick 0d18d3b3
git rev-parse 'HEAD^{tree}'   # f24923ca2defe533990dd22bd29ebad6e0ee07db
```

`go test` passes for the touched translator
packages and that executor test. Re-recording changed only `max_tokens`, the Gemini
system text, the Codex/xAI `include` list, and the values the replays already
normalize (dates, ports, random IDs and the CCH over them).

For each scenario the generator writes the scenario's `config.yaml` and fake auth
file, loads them with Go's `config.LoadConfig` and watcher synthesizers, and runs the
real `ClaudeExecutor` (`Execute`, `ExecuteStream` or `CountTokens`) after
`session.Enrich`, as the conductor does. Upstream traffic goes to a capturing
`http.RoundTripper` passed through the executor's `cliproxy.roundtripper` context
hook, so nothing leaves the process. The captured upstream URL, header map (after
Go's wire-casing pass) and body are recorded with Go's downstream output.

Reply bodies may contain `{{ALIAS:name}}` (the MCP alias Go assigned to `name`) and
`{{DRIFT:name}}` (that alias with a different tool word); the substituted reply is
recorded so the Rust replay restores exactly what Go restored. The file also records
sjson `SetRawBytes`/`DeleteBytes`/`SetBytes` results used by `rawjson.rs`.

The Rust replay (`claude::tests::executor_scenarios_match_go`) feeds the same config
and auth files through `cpa_core::config::credentials::load`, runs the request
pipeline and compares upstream bytes and headers. Random values Go and Rust generate
independently (`x-client-request-id`, the device and session in non-CLI fake user IDs
and the CCH over them) are normalized; everything else is byte-exact. Wire header
order is not observable through a `RoundTripper`; `./harness/run` covers it.

Regenerate with a temporary module inside the reference module's internal-package
boundary (the reference checkout is not modified):

```sh
reference=/absolute/path/to/CLIProxyAPI   # 6fecc6e with the commits above
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/claude/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/reference/claude/scenarios.json" "$crate/src/claude/testdata/go_executor.json" \
    '6fecc6e with 0d18d3b3, a881c036, d510dccf and 2c1dcc7c'
)
RSFIX_REFERENCE='6fecc6e with 0d18d3b3, a881c036, d510dccf and 2c1dcc7c' \
  python3 "$crate/tests/reference/claude/unit/record.py" "$reference" "$crate/src/claude/testdata/go_unit.json"
```

The date in the current-date reminder is the generation date in the credential's
time zone and is recorded per scenario; the replay pins its clock to it.

## v8.0.27 goldens

The Claude prompt-cache and prompt-shaping fixes this executor follows from v8.0.27
(see `docs/parity-audit/upstream-v8.0.35.md`) have their own goldens, recorded from a
v8.0.27 checkout (`bf93416`, with `0d18d3b3` as above) the same way; everything else
stays on `6fecc6e`.

- `../../../src/claude/testdata/go_executor_v8.0.27.json`: this generator on
  `scenarios-v8.0.27.json`, replayed by
  `claude::tests::v8_0_27_cache_and_prompt_shaping_scenarios_match_go`. Scenarios that
  name the same `executor` share one Go executor, so state it keeps between requests
  (a thread's tool aliases) carries over as it does in the Rust replay. The recorded
  `date` follows the credential's `timezone`; the date-pin scenarios use `Etc/GMT+12`
  and `Etc/GMT-14`, which are always a calendar day apart, to cross midnight without a
  clock hook. The continuation's reply names the alias Go gave `Read` in the scenario
  before it (`mcp__private_pattern__magic_Read`); a continuation declares no tools, so
  `{{ALIAS:...}}` cannot find it. Update that reply if the alias derivation changes.
- `../../../src/claude/testdata/go_unit_v8.0.27.json`: `unit/record.py` on Go's own
  v8.0.27 tests for those fixes, replayed by `claude::go_unit::v8_0_27_*`.

Where a cloaked API key's caller system prompt goes stays as at `6fecc6e`
(`docs/DIFFERENCES-FROM-GO.md`). The recordings in which Go v8.0.27 sends that prompt
in the top-level `system` are listed by name (`RELOCATES_API_KEY_SYSTEM` in
`claude/tests.rs` and `claude/go_exec.rs`) and compared with Go's body after
`claude::tests::with_relocated_system` moves the prompt where cliproxy-rs sends it;
every other byte is compared as recorded. `6fecc6e`'s recordings of the same requests
replay unchanged.

```sh
reference=/absolute/path/to/CLIProxyAPI   # v8.0.27 with 0d18d3b3
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/claude/main.go" "$tmp/main.go"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/reference/claude/scenarios-v8.0.27.json" \
    "$crate/src/claude/testdata/go_executor_v8.0.27.json" 'v8.0.27 with 0d18d3b3'
)
RSFIX_REFERENCE='v8.0.27 with 0d18d3b3' \
RSFIX_FILES=claude_prompt_cache_options_test.go,claude_cloaked_date_pin_test.go,claude_effort_directive_test.go,claude_executor_test.go \
RSFIX_ONLY='^(TestClaudeExecutor_PromptCacheOptions\w*|TestClaudeExecutor_CountTokens_StripsPromptCacheOptions|TestClaudeExecutor_CountTokensUpstream_\w*|TestClaudeCloakedDateReminder(PinnedToSessionAcrossMidnight|SessionByteStability)|TestClaudeExecutor_EffortDirectiveAfterCompaction|TestClaudeExecutor_CopiedVSCodeAgentSDKHeadersWithoutMetadataAreCloaked)$' \
  python3 "$crate/tests/reference/claude/unit/record.py" "$reference" "$crate/src/claude/testdata/go_unit_v8.0.27.json"
```

All of them ran with the network denied (`unshare -rn` with loopback up), `GOTOOLCHAIN=local
GOPROXY=off` and Go 1.26 after a `go mod download` in each checkout. The tests left out
of the v8.0.27 recording are those whose answers depend on v8.0.27's later move of
payload rules to the end of the pipeline (`868e137`); the v8.0.35 recordings below
cover them.

## v8.0.35 recordings

`../../../src/claude/testdata/go_unit_v8.0.35.json` holds Go's own v8.0.35 tests whose
answers changed with a v8.0.35 behaviour the executor follows. It is replayed by
`claude::go_unit::v8_0_35_executor`, and the 6fecc6e recordings of the same tests are
skipped (`SUPERSEDED` in `claude/go_unit.rs`; the 6fecc6e tests 868e137 deleted are
in `REMOVED_IN_868E137`). It holds `TestClaudeExecutor_DisabledThinkingStripsDisplayBeta`,
whose `thinking` object Go ad9e456d (`disableThinking`) now replaces whole, and the 868e137
payload-barrier tests: payload rules run once, on the fully prepared body, and nothing
repairs their result. They are replayed by `v8_0_35_executor`, `v8_0_35_mid_system_model`,
`v8_0_35_cloaked_date_pin`, `v8_0_35_payload_barrier` and `v8_0_35_payload_review`. The
preserved-thinking tests in the same files (911f49cb) are not recorded: that commit is not
ported.

The recording's `source` names the checkout's full commit (`git rev-parse HEAD` in the
reference, `ae3ee3be303fb07fb8173279923413005bf32177`, with a clean worktree).

```sh
reference=/absolute/path/to/CLIProxyAPI   # at v8.0.35
RSFIX_REFERENCE="v8.0.35 ($(git -C "$reference" rev-parse HEAD))" \
RSFIX_FILES=claude_executor_test.go,claude_mid_system_model_test.go,claude_cloaked_date_pin_test.go,payload_barrier_test.go,payload_review_test.go \
RSFIX_ONLY='^(TestClaudeExecutor_DisabledThinkingStripsDisplayBeta|TestClaudeExecutorPayloadOverrideDisabledThinking|TestClaudeExecutorPayloadOverrideReenablesThinking|TestClaudeExecutor_ChatCompletionsDefaultMaxTokensAndPayloadDefaultRule|TestClaudeExecutor_ExecutePayloadStreamOverrideDoesNotChangeResponseParser|TestClaudeExecutor_Payload(CustomReportingOutcomesPreservedOnRewrite|FableProbeOverrideKeepsBuiltinAdditions|FableThinkingAdaptiveToDisabledKeepsBuiltinDisplay|FableToSonnetPreservesObfuscatedBuiltins|FilterFallbacksPreservesRemovalOnFable|MaxTokensOverrideDoesNotReclassifyBuiltins|OverrideFableModelLeavesBuiltinsUnchanged|OverrideNonFableModelDoesNotInjectFableBuiltins|OverrideParentThinkingPreventsDisplayUpdates|OverridePreservesExplicitFallbacks|OverrideProbeWithExplicitDiagnosticsPreserved|OverrideRawPreservesExplicitFallbacks|ProbeToNormalDoesNotReinitializeBuiltins|ReplacesSystemOnOriginalFableWithoutReinjection|SonnetToFableDoesNotAddReporting|SonnetToFableWithSensitiveWordsDoesNotInjectReporting|StringSystemFableWinsReportingBlock|StringSystemMentioningReportingOutcomesStaysExact|StringSystemWithExactReportingPromptDoesNotDuplicate|SystemTTLOverrideKeepsBuiltinFableReporting|UnrelatedRuleLeavesBuiltinFallbacksUnchanged)|TestClaudeExecutorPayloadRepairsLegacyMidSystemMessage|TestClaudeExecutor_PayloadOverride(CannotSmuggleLegacyMidSystemMessage|DoesNotClaimMatchingCallerTurn|DoesNotRerunSystemPlacement)|TestClaudeCloakedDateReminderProbeOverrideDoesNotReinitializeSessionDate|TestPayloadBarrierClaudeDoesNotReplayFiltersOrReinjectIdentity|TestClaudePayloadConditions(EvaluateOnceAtFinalBarrier|ObserveBuiltinThinkingRemoval))$' \
  python3 "$crate/tests/reference/claude/unit/record.py" "$reference" "$crate/src/claude/testdata/go_unit_v8.0.35.json"
```

The recorder no longer prunes `params` maps, which dropped
`false` values; recordings made before that fix may lack `false` params.

`../../../src/claude/testdata/go_executor_v8.0.35.json` runs `scenarios-v8.0.35.json`
through v8.0.35's executor with the same `main.go` as the v8.0.27 recording (a temporary
module whose `replace` points at the v8.0.35 checkout). It holds the 6fecc6e payload
scenarios Go now answers differently (`FOLLOWS_V8_0_35` in `claude/tests.rs`) and two
count_tokens scenarios with rules: upstream through OAuth and local through a gateway key.
Three scenarios without rules (`NO_RULES_V8_0_35`) cover requests that cloaking turns
into a probe or a helper, or out of one: Go classifies the cloaked body, so a quota
probe whose system text becomes a role:system turn on a modern model is no longer a
probe, a legacy model's title prompt moved into a user reminder stays a helper, and
`max_tokens: 1` with `Hi` becomes a probe once cloaking adds `cache_control`.
Four more cover the steps that now run after cloaking with no rules configured:
`disable-image-generation` stripping an image tool after the request is classified
(as the last tool, and as the only tool of a `quota` probe, which stays a normal turn),
and duplicate `prompt_cache_options` members, both stripped, where a later member's
`mode: explicit` counts.
Duplicate `thinking`, `tool_choice` and block `cache_control` members check that the
executor reads them as Go's gjson does, moving on to a later member when the first
lacks the path. A duplicate `metadata` member is refused with 400 on both sides; the
recorder reads an error's status with `errors.As`, as the handler does, since the
executor wraps that error.
`claude::tests::v8_0_35_payload_barrier_scenarios_match_go` replays them, and checks that
each sent body's CCH signs that body.

```sh
go run . "$crate/tests/reference/claude/scenarios-v8.0.35.json" \
  "$crate/src/claude/testdata/go_executor_v8.0.35.json" 'v8.0.35 (ae3ee3be303fb07fb8173279923413005bf32177)'
```

## tool_search recording

`../../../src/claude/testdata/go_executor_tool_search.json` runs `scenarios-tool-search.json`
(Responses requests with Codex `tool_search`, through Anthropic's origin, a key with
`native-tool-search`, an untrusted gateway, a legacy model, the switch off, a payload rule
and count_tokens) through v8.0.35's executor. The server, not the executor, publishes
`upstream.claude.tool-search` to Go's translator, so this recording also copies
`tool_search_hook.go`, which does that after each scenario's config load.
`claude::tests::tool_search_scenarios_match_go` compares the upstream requests and
`claude::tests::tool_search_responses_match_go` the translated answers.

```sh
reference=/absolute/path/to/CLIProxyAPI   # at v8.0.35
crate=/absolute/path/to/cliproxy-rs/crates/cpa-exec
tmp=$(mktemp -d)
cp "$crate/tests/reference/claude/main.go" "$crate/tests/reference/claude/tool_search_hook.go" "$tmp/"
(
  cd "$tmp"
  go mod init github.com/router-for-me/CLIProxyAPI/v8/fixture
  go mod edit -require=github.com/router-for-me/CLIProxyAPI/v8@v8.0.0
  go mod edit -replace=github.com/router-for-me/CLIProxyAPI/v8="$reference"
  go mod tidy
  go run . "$crate/tests/reference/claude/scenarios-tool-search.json" \
    "$crate/src/claude/testdata/go_executor_tool_search.json" "v8.0.35 ($(git -C "$reference" rev-parse HEAD))"
)
```
