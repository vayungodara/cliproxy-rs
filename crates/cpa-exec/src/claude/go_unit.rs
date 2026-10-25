//! Go's own Claude unit tests, replayed. tests/reference/claude/unit/record.py runs the
//! Go test files of CLIProxyAPI 6fecc6e with recording wrappers around the functions
//! they test; every recorded call (inputs and Go's outputs) is replayed here through
//! the Rust port. One test per Go file, named after its PARITY.md item; a failure lists
//! every diverging call with the Go test that made it. The Claude prompt-cache and
//! prompt-shaping fixes this executor follows from v8.0.27 are replayed from Go's tests
//! at v8.0.27 the same way (`v8_0_27_*`).

use std::sync::OnceLock;

use serde_json::Value;

use super::go_exec::Verdict;
use super::{alias, stream};

/// Go's tests at the reference, 6fecc6e.
fn fixture() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("testdata/go_unit.json")).unwrap())
}

/// Go's tests at v8.0.27 for the Claude prompt-cache and prompt-shaping fixes this
/// executor follows from there (README.md in tests/reference/claude).
fn fixture_v8_0_27() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("testdata/go_unit_v8.0.27.json")).unwrap())
}

/// Go's tests at v8.0.35 whose answers changed with a v8.0.35 behaviour this executor
/// follows; 6fecc6e's recordings of the same tests are skipped ([`SUPERSEDED`]).
fn fixture_v8_0_35() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("testdata/go_unit_v8.0.35.json")).unwrap())
}

/// Go's v8.0.35 `claude_executor_tool_search_test.go`, recorded with the
/// `upstream.claude.tool-search` value Go pinned for each call in its config.
fn fixture_tool_search() -> &'static Value {
    static FIXTURE: OnceLock<Value> = OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("testdata/go_unit_tool_search.json")).unwrap())
}

/// 6fecc6e recordings replaced by their v8.0.35 recording: ad9e456d `disableThinking`
/// replaces the whole `thinking` object, and the payload rules run once, last (868e137).
const SUPERSEDED: &[&str] = &[
    "TestClaudeExecutor_DisabledThinkingStripsDisplayBeta",
    "TestClaudeExecutorPayloadOverrideDisabledThinking",
    "TestClaudeExecutorPayloadOverrideReenablesThinking",
    "TestClaudeExecutor_ChatCompletionsDefaultMaxTokensAndPayloadDefaultRule",
    "TestClaudeExecutor_PayloadCustomReportingOutcomesPreservedOnRewrite",
    "TestClaudeExecutor_PayloadFilterFallbacksPreservesRemovalOnFable",
    "TestClaudeExecutor_PayloadOverrideParentThinkingPreventsDisplayUpdates",
    "TestClaudeExecutor_PayloadOverridePreservesExplicitFallbacks",
    "TestClaudeExecutor_PayloadOverrideProbeWithExplicitDiagnosticsPreserved",
    "TestClaudeExecutor_PayloadOverrideRawPreservesExplicitFallbacks",
    "TestClaudeExecutor_PayloadStringSystemWithExactReportingPromptDoesNotDuplicate",
    "TestClaudeExecutor_PayloadOverrideCannotSmuggleLegacyMidSystemMessage",
    "TestClaudeExecutor_PayloadOverrideDoesNotClaimMatchingCallerTurn",
];

/// 6fecc6e tests Go deleted in 868e137: they checked the repairs that ran after the
/// payload rules. Their v8.0.35 successors are in the v8.0.35 recording under new names.
const REMOVED_IN_868E137: &[&str] = &[
    "TestClaudeExecutor_ExecuteTransportMatchesResponseFormat",
    "TestClaudeExecutor_PayloadFableThinkingAdaptiveToDisabledDropsInjectedDisplay",
    "TestClaudeExecutor_PayloadFableToSonnetWithSensitiveWordsRemovesReportingBlock",
    "TestClaudeExecutor_PayloadOverrideFableModelReconciled",
    "TestClaudeExecutor_PayloadOverrideFableToProbeStripsFableAdditions",
    "TestClaudeExecutor_PayloadOverrideMaxTokensTo1ReclassifiesAsProbe",
    "TestClaudeExecutor_PayloadOverrideNonFableToFableReconciled",
    "TestClaudeExecutor_PayloadOverrideProbeToNormalReinitializes",
    "TestClaudeExecutor_PayloadOverrideUnrelatedModelRuleDoesNotPreserveFableFallbacks",
    "TestClaudeExecutor_PayloadReplacesSystemOnOriginalFableReaddsReportingBlock",
    "TestClaudeExecutor_PayloadSonnetToFableWithSensitiveWordsObfuscatesInjectedReportingBlock",
    "TestClaudeExecutor_PayloadSonnetToFableWithSystemTTLEditsAddsReportingBlock",
    "TestClaudeExecutor_PayloadStringSystemFableAddsReportingBlock",
    "TestClaudeExecutor_PayloadStringSystemMentioningReportingOutcomesStillInjectsFableReportingBlock",
    "TestClaudeExecutor_PayloadSystemTTLOverrideStillRemovesInjectedFableReportingBlock",
    "TestClaudeExecutor_PayloadOverrideReconcilesRelocatedSystemPrompt",
];

fn bytes(value: &Value) -> Option<Vec<u8>> {
    use base64::Engine;
    match value {
        Value::String(s) => Some(s.as_bytes().to_vec()),
        Value::Object(o) => o
            .get("b64")
            .and_then(Value::as_str)
            .map(|b| base64::engine::general_purpose::STANDARD.decode(b).unwrap()),
        _ => None,
    }
}

fn text(value: &Value) -> String {
    String::from_utf8(bytes(value).unwrap_or_default()).unwrap()
}

fn reverse(value: &Value) -> alias::Reverse {
    value
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_owned()))
        .collect()
}

/// Replays one recorded call; `Err` describes how Rust differs from Go.
async fn replay(test: &str, record: &Value) -> Result<Verdict, String> {
    let field = |name: &str| &record[name];
    let checked = match field("fn").as_str().unwrap() {
        "execute" | "execute_stream" | "count_tokens" | "count_tokens_upstream" => {
            return super::go_exec::replay(test, record).await;
        }
        "remap" => {
            let (out, map) = alias::remap(&text(field("body")), field("secret").as_str().unwrap());
            check("body", &out, &text(field("out")))?;
            check("reverse map", &map, &reverse(field("reverse")))
        }
        // Go's batched half alone: it refuses malformed JSON (the legacy half then runs,
        // recorded as "remap"); otherwise it equals the full remap.
        "remap_batched" => {
            let body = text(field("body"));
            if !field("ok").as_bool().unwrap() {
                return check("valid JSON", &cpa_common::json::valid(body.as_bytes()), &false)
                    .map(|()| Verdict::Matched);
            }
            let (out, map) = alias::remap(&body, field("secret").as_str().unwrap());
            check("body", &out, &text(field("out")))?;
            check("reverse map", &map, &reverse(field("reverse")))
        }
        "restore" => {
            let got = alias::restore_response(&text(field("body")), &reverse(field("reverse")));
            match field("error").as_str() {
                Some(error) => check("error", &got.err(), &Some(error.to_owned())),
                None => check("body", &got?, &text(field("out"))),
            }
        }
        "restore_line" => {
            let got = stream::restore_line(&bytes(field("line")).unwrap(), &reverse(field("reverse")));
            match field("error").as_str() {
                Some(error) => check("error", &got.err(), &Some(error.to_owned())),
                None => check("line", &String::from_utf8(got?).unwrap(), &text(field("out"))),
            }
        }
        "parse_alias" => {
            let name = field("name").as_str().unwrap();
            let want = field("ok").as_bool().unwrap().then(|| {
                (
                    field("server").as_str().unwrap(),
                    field("tool_id").as_str().unwrap(),
                    field("semantic").as_str().unwrap(),
                )
            });
            check("parts", &alias::alias_parts(name), &want)
        }
        other => Err(format!("no Rust replay for recorded function {other:?}")),
    };
    checked.map(|()| Verdict::Matched)
}

fn check<T: PartialEq + std::fmt::Debug>(what: &str, got: &T, want: &T) -> Result<(), String> {
    if got == want {
        Ok(())
    } else {
        Err(format!("{what}:\n   rust: {got:?}\n     go: {want:?}"))
    }
}

/// Replays every call recorded from one Go test file; panics listing all divergences.
async fn replay_file(fixture: &Value, file: &str) {
    let tests = fixture["files"][file]
        .as_object()
        .unwrap_or_else(|| panic!("no records for {file}"));
    let mut failures = Vec::new();
    let mut count = 0;
    for (test, records) in tests {
        if std::ptr::eq(fixture, self::fixture()) && SUPERSEDED.contains(&test.as_str()) {
            assert!(
                fixture_v8_0_35()["files"][file][test]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty()),
                "{test}: superseded but no v8.0.35 call recorded"
            );
            continue;
        }
        if std::ptr::eq(fixture, self::fixture()) && REMOVED_IN_868E137.contains(&test.as_str()) {
            continue;
        }
        for (i, record) in records.as_array().unwrap().iter().enumerate() {
            count += 1;
            match replay(test, record).await {
                Ok(Verdict::Matched) => {}
                // Not a pass: listed in the test output and in UNVERIFIED below.
                Ok(Verdict::Unverified(why)) => {
                    eprintln!("UNVERIFIED {file}: {test} call {i}: {why}");
                    assert!(
                        UNVERIFIED.contains(&test.as_str()),
                        "{test}: unverified but not listed in UNVERIFIED"
                    );
                }
                Err(why) => failures.push(format!("{test} call {i} ({}): {why}", record["fn"])),
            }
        }
    }
    assert!(count > 0, "{file}: nothing recorded");
    assert!(
        failures.is_empty(),
        "{file}: {} of {count} Go calls differ\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Go executor tests the replay cannot reproduce (context cancellation mid-call); their
/// behaviour is covered by Rust tests of the same contract instead.
const UNVERIFIED: &[&str] = &[
    "TestClaudeExecutor_ExecuteStreamOAuthCancellationIsRequestScoped",
    "TestClaudeExecutor_ExecuteStreamOAuthStartupCancellationIsRequestScoped",
];

/// Listed so cargo's summary counts them: Go cancels the request context mid-call and
/// expects a request-scoped cancellation error. A Rust caller cancels by dropping the
/// stream, so no error result reaches the scheduler at all.
#[test]
#[ignore = "unverified: Go cancels the request context mid-call (see UNVERIFIED)"]
fn unverified_go_cancellation_cases() {
    for test in UNVERIFIED {
        assert!(fixture()["files"]["claude_executor_test.go"][test].is_array(), "{test}");
    }
}

#[test]
fn go_assertions_held_while_recording() {
    assert_eq!(fixture()["go_failures"], serde_json::json!([]));
    assert_eq!(fixture_v8_0_27()["go_failures"], serde_json::json!([]));
    assert_eq!(fixture_v8_0_35()["go_failures"], serde_json::json!([]));
    assert_eq!(fixture_tool_search()["go_failures"], serde_json::json!([]));
}

macro_rules! go_file {
    ($(#[$doc:meta])* $name:ident, $file:literal) => {
        $(#[$doc])*
        #[tokio::test]
        async fn $name() {
            replay_file(fixture(), $file).await;
        }
    };
}

go_file!(
    /// M1-0072.
    m1_0072_cloaked_cache_repro, "claude_cloaked_cache_repro_test.go"
);
go_file!(
    /// M1-0074.
    m1_0074_executor_auth, "claude_executor_auth_test.go"
);
go_file!(
    /// M1-0076.
    m1_0076_beta_policy, "claude_executor_beta_policy_test.go"
);
go_file!(
    /// M1-0078.
    m1_0078_diagnostics, "claude_executor_diagnostics_test.go"
);
go_file!(
    /// M1-0079.
    m1_0079_fable_ratelimit, "claude_executor_fable_ratelimit_test.go"
);
go_file!(
    /// M1-0080.
    m1_0080_fast_error, "claude_executor_fast_error_test.go"
);
go_file!(
    /// M1-0081.
    m1_0081_native_helper, "claude_executor_native_helper_test.go"
);
go_file!(
    /// M1-0082.
    m1_0082_ratelimit, "claude_executor_ratelimit_test.go"
);
go_file!(
    /// M1-0083: MCP aliasing, mangled-alias recovery, malformed-JSON fallback.
    m1_0083_request_remap, "claude_executor_request_remap_test.go"
);
go_file!(
    /// M1-0084.
    m1_0084_stream_terminal, "claude_executor_stream_terminal_test.go"
);
go_file!(
    /// M1-0085.
    m1_0085_subagent_ttl, "claude_executor_subagent_ttl_regression_test.go"
);
go_file!(
    /// M1-0086.
    m1_0086_executor, "claude_executor_test.go"
);
go_file!(
    /// M1-0087.
    m1_0087_thinking_signature, "claude_executor_thinking_signature_test.go"
);
go_file!(
    /// M1-0089.
    m1_0089_fingerprint_policy, "claude_fingerprint_policy_test.go"
);
go_file!(
    /// M1-0093.
    m1_0093_mid_system_model, "claude_mid_system_model_test.go"
);
go_file!(
    /// M1-0095.
    m1_0095_thinking_replay, "claude_thinking_replay_test.go"
);
go_file!(
    /// The Claude executor's calls in Go's shared apply_patch integration suite (API key
    /// and OAuth; stream, non-stream, EOF, empty and scanner failures).
    apply_patch_integration_claude, "apply_patch_integration_test.go"
);

macro_rules! go_file_v8_0_27 {
    ($(#[$doc:meta])* $name:ident, $file:literal) => {
        $(#[$doc])*
        #[tokio::test]
        async fn $name() {
            replay_file(fixture_v8_0_27(), $file).await;
        }
    };
}

go_file_v8_0_27!(
    /// eb6a768 explicit prompt cache options, 14dc4e0 OAuth system relocation for
    /// count_tokens; API-key placement as in `go_exec::RELOCATES_API_KEY_SYSTEM`.
    v8_0_27_prompt_cache_options,
    "claude_prompt_cache_options_test.go"
);
go_file_v8_0_27!(
    /// 6d57ac9: the replay runs on Go's swapped clock, so the reminder date compares
    /// as sent. TestClaudeCloakedDateReminderProbeOverrideDoesNotReinitializeSessionDate
    /// is replayed from v8.0.35 (`v8_0_35_cloaked_date_pin`).
    v8_0_27_cloaked_date_pin,
    "claude_cloaked_date_pin_test.go"
);
go_file_v8_0_27!(
    /// 2d8ec73.
    v8_0_27_effort_directive,
    "claude_effort_directive_test.go"
);
go_file_v8_0_27!(
    /// The test of this file whose answer changed in v8.0.27 for API keys (5d89040),
    /// replayed with the caller system text where cliproxy-rs keeps it
    /// (`go_exec::RELOCATES_API_KEY_SYSTEM`); 6fecc6e's recording passes as recorded.
    v8_0_27_executor,
    "claude_executor_test.go"
);

macro_rules! go_file_v8_0_35 {
    ($(#[$doc:meta])* $name:ident, $file:literal) => {
        $(#[$doc])*
        #[tokio::test]
        async fn $name() {
            replay_file(fixture_v8_0_35(), $file).await;
        }
    };
}

go_file_v8_0_35!(
    /// ad9e456d and 868e137: Go's v8.0.35 recording of the 6fecc6e tests listed in
    /// [`SUPERSEDED`], and the payload-barrier tests that replace those 868e137 removed.
    v8_0_35_executor,
    "claude_executor_test.go"
);
go_file_v8_0_35!(
    /// 868e137: payload rules no longer rerun system placement.
    v8_0_35_mid_system_model,
    "claude_mid_system_model_test.go"
);
go_file_v8_0_35!(
    /// 868e137: a probe a payload rule declassifies keeps its own date.
    v8_0_35_cloaked_date_pin,
    "claude_cloaked_date_pin_test.go"
);
go_file_v8_0_35!(
    /// 868e137: filters are not replayed and identity is not reinjected after the rules.
    v8_0_35_payload_barrier,
    "payload_barrier_test.go"
);
go_file_v8_0_35!(
    /// 868e137: rule conditions are evaluated once, on the final body.
    v8_0_35_payload_review,
    "payload_review_test.go"
);

/// Codex tool_search through the executor (Execute, ExecuteStream and an upstream
/// CountTokens): native references on Anthropic's origin and OAuth aliases, the
/// pre-tool_search body on a gateway, with an original request and for a legacy model.
#[tokio::test]
async fn v8_0_35_tool_search() {
    replay_file(fixture_tool_search(), "claude_executor_tool_search_test.go").await;
}
