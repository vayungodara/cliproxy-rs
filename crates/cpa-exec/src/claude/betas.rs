//! Anthropic-Beta assembly for the Claude Code profile (claude_executor_request.go).
//! The list is per request: model capability, body features and caller requests
//! decide which managed betas appear, in Claude Code 2.1.280's wire order.

use std::collections::HashSet;

use super::cloak::{canonical_model, is_fable51, is_opus55, legacy_system_reminder};
use super::signals;
use crate::rawjson;

pub(crate) const TOKEN_COUNTING: &str = "token-counting-2024-11-01";
pub(crate) const FAST_MODE: &str = "fast-mode-2026-02-01";
pub(crate) const OAUTH: &str = "oauth-2025-04-20";
pub(crate) const CLAUDE_CODE: &str = "claude-code-20250219";
const CONTEXT_1M: &str = "context-1m-2025-08-07";
const MID_CONV_SYSTEM: &str = "mid-conversation-system-2026-04-07";
const PER_TURN_CONTROL: &str = "per-turn-control-2026-07-01";
const PER_TURN_TIMING: &str = "timing-2026-09-09";
const MID_CONV_TOOL_CHANGES: &str = "mid-conversation-tool-changes-2026-07-01";
const INLINE_TOOLS: &str = "inline-tools-2026-09-15";
const MID_CONV_CLEAR_AT: &str = "mid-conversation-system-clear-at-2026-08-21";
const DANGEROUS_TOOL_USE: &str = "dangerous-tool-use-2026-09-03";
pub(crate) const ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";
const ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";
pub(crate) const EFFORT: &str = "effort-2025-11-24";
pub(crate) const SERVER_SIDE_FALLBACK: &str = "server-side-fallback-2026-06-01";
const FALLBACK_CREDIT: &str = "fallback-credit-2026-06-01";
const STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";
pub(crate) const THINKING_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";
const THINKING_BINDING: &str = "thinking-binding-controls-2026-08-01";
const THINKING_RESUMPTION: &str = "thinking-resumption-2026-07-17";
pub(crate) const EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";
const PROMPT_CACHING_EVICT: &str = "prompt-caching-evict-2026-05-12";
const CACHE_DIAGNOSIS: &str = "cache-diagnosis-2026-04-07";
const REDACT_THINKING: &str = "redact-thinking-2026-02-12";
const AFK_MODE: &str = "afk-mode-2026-01-31";

/// Sent on every "cli" entrypoint Messages request, after claude-code-20250219.
const CONSTANT: [&str; 5] = [
    "interleaved-thinking-2025-05-14",
    REDACT_THINKING,
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
];
const TRAILING: [&str; 3] = [SERVER_SIDE_FALLBACK, FALLBACK_CREDIT, STRUCTURED_OUTPUTS];

/// Every beta the proxy assembles or gates. Other caller betas are newer-client
/// features and pass through (#5738).
pub(crate) fn managed(beta: &str) -> bool {
    const MANAGED: [&str; 26] = [
        TOKEN_COUNTING,
        FAST_MODE,
        OAUTH,
        CLAUDE_CODE,
        CONTEXT_1M,
        MID_CONV_SYSTEM,
        PER_TURN_CONTROL,
        PER_TURN_TIMING,
        MID_CONV_TOOL_CHANGES,
        INLINE_TOOLS,
        MID_CONV_CLEAR_AT,
        DANGEROUS_TOOL_USE,
        ADVISOR_TOOL,
        ADVANCED_TOOL_USE,
        EFFORT,
        SERVER_SIDE_FALLBACK,
        FALLBACK_CREDIT,
        STRUCTURED_OUTPUTS,
        THINKING_DISPLAY_UPDATES,
        THINKING_BINDING,
        THINKING_RESUMPTION,
        EXTENDED_CACHE_TTL,
        PROMPT_CACHING_EVICT,
        CACHE_DIAGNOSIS,
        REDACT_THINKING,
        AFK_MODE,
    ];
    let beta = beta.trim();
    MANAGED.contains(&beta) || CONSTANT.contains(&beta) || TRAILING.contains(&beta)
}

pub(crate) type Requested = HashSet<String>;

/// `claudeRequestedBetas`: header and body-lifted betas.
pub(crate) fn requested(incoming: &str, extra: &[String]) -> Requested {
    incoming
        .split(',')
        .chain(extra.iter().map(String::as_str))
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .map(str::to_owned)
        .collect()
}

fn model(body: &str) -> String {
    rawjson::string(body, "model")
}

fn sonnet55(m: &str) -> bool {
    let m = canonical_model(m);
    m == "claude-sonnet-5-5" || m.starts_with("claude-sonnet-5-5-") || m.starts_with("claude-sonnet-5-5[")
}
fn sonnet5(m: &str) -> bool {
    let m = canonical_model(m);
    !sonnet55(&m) && (m == "claude-sonnet-5" || m.starts_with("claude-sonnet-5-") || m.starts_with("claude-sonnet-5["))
}
pub(crate) fn progress_display(m: &str) -> bool {
    is_opus55(m) || is_fable51(m) || sonnet5(m) || sonnet55(m)
}
fn per_turn_effort(m: &str) -> bool {
    is_opus55(m) || canonical_model(m).starts_with("claude-fable-5-1")
}
fn per_turn_timing_model(m: &str) -> bool {
    per_turn_effort(m) || canonical_model(m).starts_with("claude-mythos-5-1")
}

fn include_per_turn_control(body: &str, r: &Requested) -> bool {
    r.contains(PER_TURN_CONTROL) || per_turn_effort(&model(body))
}
fn include_per_turn_timing(body: &str, r: &Requested) -> bool {
    if r.contains(PER_TURN_TIMING) {
        return true;
    }
    per_turn_timing_model(&model(body))
        && (rawjson::get(body, "output_config.timing").exists()
            || rawjson::get(body, "messages")
                .array()
                .iter()
                .any(|m| m.get("output_config.timing").exists()))
}
fn include_inline_tools(body: &str, r: &Requested) -> bool {
    r.contains(INLINE_TOOLS)
        || rawjson::get(body, "messages").array().iter().any(|m| {
            m.get("content").array().iter().any(|b| {
                b.get("type").str().trim().eq_ignore_ascii_case("tool_addition") && b.get("tool.definition").exists()
            })
        })
}
fn include_clear_at(body: &str, r: &Requested) -> bool {
    r.contains(MID_CONV_CLEAR_AT)
        || progress_display(&model(body))
        || rawjson::get(body, "messages")
            .array()
            .iter()
            .any(|m| m.get("clear_at").exists())
}

/// `claudeRequestSupportsEffort`.
pub(crate) fn supports_effort(body: &str) -> bool {
    if body.is_empty() {
        return true;
    }
    if signals::probe_or_helper(body) {
        return false;
    }
    if model(body).trim().to_lowercase().contains("haiku") {
        return false;
    }
    rawjson::string(body, "thinking.type").trim().to_lowercase() != "disabled"
}

fn display_updates(body: &str) -> bool {
    let d = rawjson::get(body, "thinking.display");
    d.kind() == gjson::Kind::String && d.str().trim().eq_ignore_ascii_case("updates")
}
fn display_set(body: &str) -> bool {
    let d = rawjson::get(body, "thinking.display");
    d.kind() == gjson::Kind::String && !d.str().trim().is_empty()
}
fn tool_types(body: &str) -> Vec<String> {
    rawjson::get(body, "tools")
        .array()
        .iter()
        .map(|t| t.get("type").str().trim().to_lowercase())
        .collect()
}
fn advanced_tool_use(body: &str) -> bool {
    let tools = rawjson::get(body, "tools");
    tools.kind() == gjson::Kind::Array
        && tools.array().iter().any(|t| {
            t.get("type")
                .str()
                .trim()
                .to_lowercase()
                .starts_with("tool_search_tool_")
                || t.get("defer_loading").bool()
                || t.get("input_examples").exists()
                || t.get("allowed_callers").exists()
        })
}
pub(crate) fn has_advisor_tool(body: &str) -> bool {
    tool_types(body).iter().any(|t| t.starts_with("advisor_"))
}
pub(crate) fn uses_fast_mode(body: &str, r: &Requested) -> bool {
    let speed = rawjson::get(body, "speed");
    r.contains(FAST_MODE) || (speed.kind() == gjson::Kind::String && speed.str().trim().eq_ignore_ascii_case("fast"))
}

/// `claudeCodeCLIBetas`.
pub(crate) fn cli(body: &str, r: &Requested, oauth: bool) -> String {
    let mut b: Vec<&str> = vec![CLAUDE_CODE];
    if oauth {
        b.push(OAUTH);
    }
    if r.contains(CONTEXT_1M) {
        b.push(CONTEXT_1M);
    }
    let redact = !display_set(body);
    b.extend(CONSTANT.iter().filter(|x| **x != REDACT_THINKING || redact));
    let legacy = legacy_system_reminder(body);
    if !legacy {
        b.push(MID_CONV_SYSTEM);
        if include_per_turn_control(body, r) {
            b.push(PER_TURN_CONTROL);
        }
        if include_per_turn_timing(body, r) {
            b.push(PER_TURN_TIMING);
        }
        if !sonnet5(&model(body)) {
            b.push(MID_CONV_TOOL_CHANGES);
        }
        if include_inline_tools(body, r) {
            b.push(INLINE_TOOLS);
        }
    } else {
        if include_per_turn_control(body, r) {
            b.push(PER_TURN_CONTROL);
        }
        if include_per_turn_timing(body, r) {
            b.push(PER_TURN_TIMING);
        }
    }
    if r.contains(ADVISOR_TOOL) || has_advisor_tool(body) {
        b.push(ADVISOR_TOOL);
    }
    if r.contains(ADVANCED_TOOL_USE) || advanced_tool_use(body) {
        b.push(ADVANCED_TOOL_USE);
    }
    if !legacy && include_clear_at(body, r) {
        b.push(MID_CONV_CLEAR_AT);
    }
    if r.contains(DANGEROUS_TOOL_USE) || rawjson::get(body, "safeguards").exists() {
        b.push(DANGEROUS_TOOL_USE);
    }
    if supports_effort(body) {
        b.push(EFFORT);
    }
    let probe = signals::probe_or_helper(body);
    let fallbacks = rawjson::get(body, "fallbacks").exists();
    if !probe && (r.contains(SERVER_SIDE_FALLBACK) || fallbacks) {
        b.push(SERVER_SIDE_FALLBACK);
    }
    if r.contains(FALLBACK_CREDIT) || rawjson::get(body, "fallback_credit_token").exists() || (oauth && fallbacks) {
        b.push(FALLBACK_CREDIT);
    }
    if r.contains(STRUCTURED_OUTPUTS) {
        b.push(STRUCTURED_OUTPUTS);
    }
    let thinking = rawjson::string(body, "thinking.type");
    if r.contains(THINKING_BINDING)
        || rawjson::get(body, "thinking.block_binding").exists()
        || (progress_display(&model(body)) && thinking == "adaptive")
    {
        b.push(THINKING_BINDING);
    }
    if !probe && thinking != "disabled" && (r.contains(THINKING_DISPLAY_UPDATES) || display_updates(body)) {
        b.push(THINKING_DISPLAY_UPDATES);
    }
    if r.contains(THINKING_RESUMPTION) {
        b.push(THINKING_RESUMPTION);
    }
    if uses_fast_mode(body, r) {
        b.push(FAST_MODE);
    }
    if r.contains(AFK_MODE) {
        b.push(AFK_MODE);
    }
    if !probe
        && ((oauth && !signals::subagent(&Default::default(), body))
            || r.contains(EXTENDED_CACHE_TTL)
            || signals::has_1h_ttl(body))
    {
        b.push(EXTENDED_CACHE_TTL);
    }
    if r.contains(PROMPT_CACHING_EVICT) || body.contains("\"evict_on_complete\"") {
        b.push(PROMPT_CACHING_EVICT);
    }
    if rawjson::get(body, "diagnostics").kind() == gjson::Kind::Object {
        b.push(CACHE_DIAGNOSIS);
    }
    b.join(",")
}

/// count_tokens profile: claude-code, OAuth when applicable, then the fixed trio.
pub(crate) fn count_tokens(oauth: bool) -> String {
    let mut b = vec![CLAUDE_CODE];
    if oauth {
        b.push(OAUTH);
    }
    b.extend([
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        TOKEN_COUNTING,
    ]);
    b.join(",")
}

fn dedup(betas: &str, skip: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    betas
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty() && *b != skip && seen.insert(b.to_string()))
        .map(str::to_owned)
        .collect()
}

fn insert_oauth(parts: &mut Vec<String>) {
    if !parts.iter().any(|p| p == OAUTH) {
        let at = usize::from(parts.first().is_some_and(|p| p == CLAUDE_CODE));
        parts.insert(at, OAUTH.into());
    }
}

/// `withClaudeCountTokensOAuthBeta`.
pub(crate) fn with_count_oauth(betas: &str) -> String {
    let mut parts = dedup(betas, "");
    insert_oauth(&mut parts);
    parts.join(",")
}

/// `withClaudeOAuthCredentialBetas`: the selected OAuth account must be described
/// even when a native caller did not know CPA would pick one.
pub(crate) fn with_oauth_credential(betas: &str, extended_ttl: bool) -> String {
    let mut parts = dedup(betas, "");
    let had_ttl = parts.iter().any(|p| p == EXTENDED_CACHE_TTL);
    insert_oauth(&mut parts);
    if extended_ttl && !had_ttl {
        parts.push(EXTENDED_CACHE_TTL.into());
    }
    parts.join(",")
}

/// `withoutClaudeBeta`.
pub(crate) fn without(betas: &str, remove: &str) -> String {
    betas
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty() && *p != remove)
        .collect::<Vec<_>>()
        .join(",")
}

/// `withClaudeExtendedCacheTTLBeta`.
pub(crate) fn with_extended_ttl(betas: &str) -> String {
    let mut parts = dedup(betas, "");
    if !parts.iter().any(|p| p == EXTENDED_CACHE_TTL) {
        parts.push(EXTENDED_CACHE_TTL.into());
    }
    parts.join(",")
}

/// `withClaudeAdvisorToolBeta`: advisor goes before the first beta that follows it
/// on the native wire.
pub(crate) fn with_advisor(betas: &str) -> String {
    if betas.trim().is_empty() {
        return ADVISOR_TOOL.into();
    }
    let mut parts = dedup(betas, ADVISOR_TOOL);
    let boundary = [
        ADVANCED_TOOL_USE,
        EFFORT,
        SERVER_SIDE_FALLBACK,
        FALLBACK_CREDIT,
        STRUCTURED_OUTPUTS,
        FAST_MODE,
        AFK_MODE,
        EXTENDED_CACHE_TTL,
        CACHE_DIAGNOSIS,
    ];
    let at = parts
        .iter()
        .position(|p| boundary.contains(&p.as_str()))
        .unwrap_or(parts.len());
    parts.insert(at, ADVISOR_TOOL.into());
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_profiles_match_go_captures() {
        let body = r#"{"model":"claude-sonnet-4-6","diagnostics":{"previous_message_id":null}}"#;
        assert_eq!(
            cli(body, &Requested::new(), true),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,effort-2025-11-24,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07"
        );
        assert_eq!(
            count_tokens(true),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,token-counting-2024-11-01"
        );
        assert_eq!(
            with_oauth_credential("claude-code-20250219,interleaved-thinking-2025-05-14", true),
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,extended-cache-ttl-2025-04-11"
        );
        assert_eq!(
            with_advisor("a,effort-2025-11-24,b"),
            "a,advisor-tool-2026-03-01,effort-2025-11-24,b"
        );
    }
}
