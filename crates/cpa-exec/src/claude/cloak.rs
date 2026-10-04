//! Claude Code cloaking (claude_executor_cloaking.go): billing and identity system
//! blocks, caller prompt relocation, current-date reminder, context management,
//! cache-control placement and TTL policy, and the fake/derived user identity.

use sha2::{Digest, Sha256};

use super::signals;
use crate::rawjson::{self, js_string};

pub(crate) const CLI_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
pub(crate) const BILLING_PREFIX: &str = "x-anthropic-billing-header:";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
pub(crate) const CONTEXT_MANAGEMENT: &str = r#"{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
pub(crate) const FABLE_REPORTING: &str = "# Reporting outcomes\n\nReport what actually happened, not what you intended. When you say something is done, sent, saved, fixed, or verified, that claim must rest on a result you observed in this session — tool output, the file as it now reads, the page as it now loads — not on what the step should have produced. If you did not check, say you did not check. If any step failed, was skipped, or came back different from what you expected, say so in the first sentence of your report, before anything else, even when the rest of the work succeeded. Never quietly work around a failure in a way that makes it look resolved; a problem the user can see is recoverable, one your summary hides is not. When you stop before the task is complete, your first line says so plainly and names what is left. Do not describe partial work as done, and do not let a summary read as more certain than the evidence behind it.";

/// Official model IDs that reject a mid-conversation `role: system` turn.
const LEGACY_SYSTEM_REMINDER_MODELS: &[&str] = &[
    "claude-3-5-haiku-20241022",
    "claude-3-5-haiku-latest",
    "claude-3-7-sonnet-20250219",
    "claude-3-7-sonnet-latest",
    "claude-haiku-4-5",
    "claude-haiku-4-5-20251001",
    "claude-opus-4",
    "claude-opus-4-20250514",
    "claude-opus-4-1",
    "claude-opus-4-1-20250805",
    "claude-opus-4-5",
    "claude-opus-4-5-20251101",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-sonnet-4",
    "claude-sonnet-4-20250514",
    "claude-sonnet-4-5",
    "claude-sonnet-4-5-20250929",
    "claude-sonnet-4-6",
];

/// Lowercased model without any `provider/` prefix.
pub(crate) fn canonical_model(model: &str) -> String {
    let m = model.trim().to_lowercase();
    m.rsplit('/').next().unwrap_or_default().to_owned()
}

pub(crate) fn legacy_system_reminder(body: &str) -> bool {
    LEGACY_SYSTEM_REMINDER_MODELS.contains(&canonical_model(rawjson::get(body, "model").str()).as_str())
}

pub(crate) fn is_fable51(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    ["fable-5-1", "fable-5.1", "mythos-5-1", "mythos-5.1"].iter().any(|t| {
        m.match_indices(t).take(1).any(|(i, _)| {
            let next = i + t.len();
            next >= m.len() || !m.as_bytes()[next].is_ascii_digit()
        })
    })
}
pub(crate) fn is_opus55(model: &str) -> bool {
    let m = canonical_model(model);
    m == "claude-opus-5-5" || m.starts_with("claude-opus-5-5[")
}

/// `{"type":"text","text":...}` with JSON.stringify escaping and an optional marker.
pub(crate) fn text_block(text: &str, cache: Option<&str>) -> String {
    match cache {
        Some(ttl) if !ttl.is_empty() => {
            format!(
                r#"{{"type":"text","text":{},"cache_control":{{"type":"ephemeral","ttl":{}}}}}"#,
                js_string(text),
                js_string(ttl)
            )
        }
        Some(_) => format!(
            r#"{{"type":"text","text":{},"cache_control":{{"type":"ephemeral"}}}}"#,
            js_string(text)
        ),
        None => format!(r#"{{"type":"text","text":{}}}"#, js_string(text)),
    }
}
const EPHEMERAL: Option<&str> = Some("");

/// The 3-character build hash Claude Code embeds in `cc_version`, sampling UTF-16
/// code units 4, 7 and 20 of the first user text.
fn fingerprint(message: &str, version: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    let sampled: Vec<u16> = [4, 7, 20]
        .iter()
        .map(|&i| units.get(i).copied().unwrap_or(u16::from(b'0')))
        .collect();
    let sampled = String::from_utf16_lossy(&sampled);
    let digest = Sha256::digest(format!("{FINGERPRINT_SALT}{sampled}{version}").as_bytes());
    super::session::hex(&digest)[..3].to_owned()
}

pub(crate) struct Billing<'a> {
    pub signed: bool,
    pub version: &'a str,
    pub message: &'a str,
    pub entrypoint: &'a str,
    pub workload: &'a str,
    pub subagent: bool,
    pub prev_req: &'a str,
    pub prompt_id: &'a str,
    pub human_turn: bool,
}

/// `generateBillingHeader`.
pub(crate) fn billing_header(b: &Billing<'_>) -> String {
    let entrypoint = if b.entrypoint.is_empty() { "cli" } else { b.entrypoint };
    let mut s = format!(
        "{BILLING_PREFIX} cc_version={}.{}; cc_entrypoint={entrypoint};",
        b.version,
        fingerprint(b.message, b.version)
    );
    if b.signed {
        s.push_str(" cch=00000;");
    }
    if !b.workload.is_empty() {
        s.push_str(&format!(" cc_workload={};", b.workload));
    }
    if b.subagent {
        s.push_str(" cc_is_subagent=true;");
    }
    if b.signed {
        if !b.prev_req.is_empty() {
            s.push_str(&format!(" cc_prev_req={};", b.prev_req));
        }
        if !b.prompt_id.is_empty() {
            s.push_str(&format!(" cc_prompt_id={};", b.prompt_id));
        }
        if b.human_turn {
            s.push_str(" cc_turn_origin=human;");
        }
    }
    s
}

pub(crate) fn first_user_index(body: &str) -> Option<usize> {
    rawjson::get(body, "messages")
        .array()
        .iter()
        .position(|m| m.get("role").str() == "user")
}

fn date_reminder_prefix(text: &str) -> bool {
    text.starts_with("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is ")
}
fn context_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>") && text.contains("</system-reminder>")
}

/// `claudeBillingFingerprintMessageText`: the first user text that is not a reminder.
pub(crate) fn fingerprint_message(body: &str) -> String {
    let Some(idx) = first_user_index(body) else {
        return String::new();
    };
    let content = rawjson::get(body, "messages").array()[idx]
        .get("content")
        .json()
        .to_owned();
    let content = gjson::parse(&content);
    match content.kind() {
        gjson::Kind::String => content.str().to_owned(),
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter(|p| p.get("type").str() == "text")
            .map(|p| p.get("text").str().to_owned())
            .find(|t| !date_reminder_prefix(t) && !context_reminder(t))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// `collectForwardedClaudeSystemPromptBlocks`.
pub(crate) fn forwarded_system_blocks(body: &str) -> Vec<String> {
    let system = rawjson::get(body, "system");
    let keep = |t: &str| !t.trim().is_empty() && !t.trim_start().starts_with(BILLING_PREFIX) && t != CLI_IDENTITY;
    match system.kind() {
        gjson::Kind::Array => system
            .array()
            .iter()
            .filter(|p| p.get("type").str() == "text")
            .map(|p| p.get("text").str().to_owned())
            .filter(|t| keep(t))
            .collect(),
        gjson::Kind::String if keep(system.str()) => vec![system.str().to_owned()],
        _ => Vec::new(),
    }
}

/// `validateClaudeCallerSystemBlocks`: non-text system blocks have no destination.
pub(crate) fn invalid_system_block(body: &str) -> Option<String> {
    let system = rawjson::get(body, "system");
    if system.kind() != gjson::Kind::Array {
        return None;
    }
    system.array().iter().enumerate().find_map(|(i, p)| {
        let kind = p.get("type").str().trim().to_owned();
        (kind != "text").then(|| {
            let kind = if kind.is_empty() { "unknown".to_owned() } else { kind };
            format!(
                "invalid_request_error: system.{i}.type: Input should be 'text'. System instructions support text only, but this block has type {}. Move non-text content into a user message.",
                rawjson::go_string(&kind).replace("\\u003c", "<").replace("\\u003e", ">").replace("\\u0026", "&")
            )
        })
    })
}

fn reminder(text: &str) -> String {
    let mut r = format!("<system-reminder>\n{text}");
    if !text.ends_with('\n') {
        r.push('\n');
    }
    r.push_str("</system-reminder>");
    r
}

/// Legacy models: caller system prompts become reminders in the first user turn.
pub(crate) fn prepend_reminders(body: &str, texts: &[String]) -> String {
    let Some(idx) = first_user_index(body) else {
        return body.to_owned();
    };
    if texts.is_empty() {
        return body.to_owned();
    }
    let reminders: Vec<String> = texts.iter().map(|t| reminder(t)).collect();
    let path = format!("messages.{idx}.content");
    let content = rawjson::get(body, &path);
    if content.kind() == gjson::Kind::Array {
        let blocks = content.array();
        let mut existing: std::collections::HashMap<String, usize> = Default::default();
        for b in &blocks {
            if b.get("type").str() == "text" {
                *existing.entry(b.get("text").str().to_owned()).or_default() += 1;
            }
        }
        let mut new_blocks = Vec::new();
        for r in &reminders {
            match existing.get_mut(r) {
                Some(n) if *n > 0 => *n -= 1,
                _ => new_blocks.push(text_block(r, None)),
            }
        }
        if new_blocks.is_empty() {
            return body.to_owned();
        }
        let at = blocks
            .iter()
            .take_while(|b| b.get("type").str() == "tool_result")
            .count();
        let mut raw: Vec<String> = blocks.iter().map(|b| b.json().to_owned()).collect();
        raw.splice(at..at, new_blocks);
        return rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")));
    }
    if content.kind() == gjson::Kind::String {
        let mut raw: Vec<String> = reminders.iter().map(|r| text_block(r, None)).collect();
        raw.push(text_block(content.str(), None));
        return rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")));
    }
    body.to_owned()
}

pub(crate) fn message_text(content: &gjson::Value) -> String {
    match content.kind() {
        gjson::Kind::String => content.str().to_owned(),
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter(|b| b.get("type").str() == "text")
            .map(|b| b.get("text").str().to_owned())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// Current models: each caller system block becomes an operator-level system turn
/// after the first user turn(s), leaving the cached top-level prefix untouched.
pub(crate) fn insert_mid_system(body: &str, texts: &[String]) -> String {
    let Some(first) = first_user_index(body) else {
        return body.to_owned();
    };
    let messages = rawjson::get(body, "messages");
    if texts.is_empty() || messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let msgs = messages.array();
    let mut at = first + 1;
    while at < msgs.len() && msgs[at].get("role").str() == "user" {
        at += 1;
    }
    if msgs.len() - at >= texts.len()
        && texts.iter().enumerate().all(|(i, t)| {
            let m = &msgs[at + i];
            m.get("role").str() == "system" && message_text(&m.get("content")) == *t
        })
    {
        return body.to_owned();
    }
    let inserted: Vec<String> = texts
        .iter()
        .map(|t| format!(r#"{{"role":"system","content":[{}]}}"#, text_block(t, EPHEMERAL)))
        .collect();
    let mut raw: Vec<String> = msgs.iter().map(|m| m.json().to_owned()).collect();
    raw.splice(at..at, inserted);
    rawjson::set_raw(body, "messages", &format!("[{}]", raw.join(",")))
}

/// Advisor results are bound to message positions; splicing would break them.
pub(crate) fn has_advisor_history(body: &str) -> bool {
    rawjson::get(body, "messages").array().iter().any(|msg| {
        let content = msg.get("content");
        match content.kind() {
            gjson::Kind::Array => content.array().iter().any(|b| match b.get("type").str() {
                "advisor_tool_result" | "advisor_redacted_result" => true,
                "server_tool_use" => b.get("name").str() == "advisor",
                "tool_result" => {
                    let inner = b.get("content");
                    match inner.kind() {
                        gjson::Kind::Array => inner
                            .array()
                            .iter()
                            .any(|x| x.get("type").str() == "advisor_redacted_result"),
                        gjson::Kind::Object => inner.get("type").str() == "advisor_redacted_result",
                        _ => false,
                    }
                }
                _ => false,
            }),
            gjson::Kind::Object => matches!(
                content.get("type").str(),
                "advisor_tool_result" | "advisor_redacted_result"
            ),
            _ => false,
        }
    })
}

pub(crate) fn date_reminder(date: &str) -> String {
    format!(
        "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# currentDate\nToday's date is {date}.\n\n      IMPORTANT: this context may or may not be relevant to your tasks. You should not respond to this context unless it is highly relevant to your task.\n</system-reminder>\n\n"
    )
}

/// `withEphemeralCacheControl`.
fn with_ephemeral(raw: &str) -> String {
    rawjson::set_raw(raw, "cache_control", r#"{"type":"ephemeral"}"#)
}

/// `injectClaudeCodeCurrentDate`: date reminder first (after tool results), and the
/// first real user text carries the default breakpoint.
pub(crate) fn inject_current_date(body: &str, date: &str) -> String {
    let Some(idx) = first_user_index(body) else {
        return body.to_owned();
    };
    let path = format!("messages.{idx}.content");
    let content = rawjson::get(body, &path);
    let date_block = text_block(&date_reminder(date), None);
    if content.kind() == gjson::Kind::String {
        let user = text_block(content.str(), EPHEMERAL);
        return rawjson::set_raw(body, &path, &format!("[{date_block},{user}]"));
    }
    if content.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut raw = Vec::new();
    let mut cached = false;
    for block in content.array() {
        if block.get("type").str() == "text" {
            let text = block.get("text").str().to_owned();
            if date_reminder_prefix(&text) {
                continue;
            }
            if !cached && !context_reminder(&text) {
                raw.push(with_ephemeral(block.json()));
                cached = true;
                continue;
            }
        }
        raw.push(block.json().to_owned());
    }
    let at = raw
        .iter()
        .take_while(|b| gjson::get(b, "type").str() == "tool_result")
        .count();
    raw.insert(at, date_block);
    rawjson::set_raw(body, &path, &format!("[{}]", raw.join(",")))
}

pub(crate) struct SystemPlan<'a> {
    pub strict: bool,
    pub billing: Billing<'a>,
    pub date: &'a str,
}

/// `checkSystemInstructionsWithSigningModeAt`.
pub(crate) fn install_system(body: &str, plan: &SystemPlan<'_>) -> String {
    let forwarded = forwarded_system_blocks(body);
    let message = fingerprint_message(body);
    let billing = Billing {
        message: &message,
        ..plan.billing
    };
    let mut blocks = vec![
        text_block(&billing_header(&billing), None),
        text_block(CLI_IDENTITY, EPHEMERAL),
    ];
    if is_fable51(rawjson::get(body, "model").str()) && !signals::probe_or_helper(body) {
        blocks.push(text_block(FABLE_REPORTING, None));
    }
    let mut body = rawjson::set_raw(body, "system", &format!("[{}]", blocks.join(",")));
    if !plan.strict && !forwarded.is_empty() {
        if has_advisor_history(&body) {
            blocks.extend(forwarded.iter().map(|t| text_block(t, None)));
            body = rawjson::set_raw(&body, "system", &format!("[{}]", blocks.join(",")));
        } else if legacy_system_reminder(&body) {
            body = prepend_reminders(&body, &forwarded);
        } else {
            body = insert_mid_system(&body, &forwarded);
        }
    }
    inject_current_date(&body, plan.date)
}

/// `relocateClaudeSystemPromptForCountTokens`: count_tokens carries no CLI blocks
/// but caller system text is still counted in its Messages position.
pub(crate) fn relocate_system_for_count(body: &str, strict: bool) -> String {
    if !rawjson::get(body, "system").exists() {
        return body.to_owned();
    }
    let forwarded = if strict {
        Vec::new()
    } else {
        forwarded_system_blocks(body)
    };
    if forwarded.is_empty() {
        return rawjson::delete(body, "system");
    }
    if has_advisor_history(body) {
        let blocks: Vec<_> = forwarded.iter().map(|t| text_block(t, None)).collect();
        return rawjson::set_raw(body, "system", &format!("[{}]", blocks.join(",")));
    }
    let body = rawjson::delete(body, "system");
    if legacy_system_reminder(&body) {
        prepend_reminders(&body, &forwarded)
    } else {
        insert_mid_system(&body, &forwarded)
    }
}

pub(crate) fn thinking_accepts_clear(body: &str) -> bool {
    matches!(rawjson::get(body, "thinking.type").str(), "enabled" | "adaptive")
}

/// `injectClaudeCodeContextManagement`.
pub(crate) fn inject_context_management(body: &str) -> Option<String> {
    (!rawjson::get(body, "context_management").exists() && thinking_accepts_clear(body))
        .then(|| rawjson::set_raw(body, "context_management", CONTEXT_MANAGEMENT))
}

/// `reconcileClaudeCodeContextManagement` without payload-rule ownership.
pub(crate) fn reconcile_context_management(
    body: &str,
    eligible: bool,
    caller_owned: bool,
    injected: bool,
    payload_touched: bool,
) -> String {
    let current = rawjson::get(body, "context_management");
    if !thinking_accepts_clear(body) {
        if caller_owned || !injected || payload_touched || current.json() != CONTEXT_MANAGEMENT {
            return body.to_owned();
        }
        return rawjson::delete(body, "context_management");
    }
    if !eligible || caller_owned || payload_touched || current.exists() {
        return body.to_owned();
    }
    rawjson::set_raw(body, "context_management", CONTEXT_MANAGEMENT)
}

fn has_cache_control(body: &str, path: &str) -> bool {
    gjson::get(body, &format!("{path}.cache_control")).exists()
}

pub(crate) fn count_cache_controls(body: &str) -> usize {
    // Go counts system, tools, then messages; the total is order-independent.
    signals::blocks(body)
        .iter()
        .filter(|(_, block)| gjson::get(block, "cache_control").exists())
        .count()
}

/// `upgradeClaudeCacheControlTTL`: every marker without an explicit ttl joins `ttl`.
pub(crate) fn upgrade_ttl(body: &str, ttl: &str) -> String {
    if !gjson::valid(body) {
        return body.to_owned();
    }
    // Go visits the blocks of the payload it was given; each edit touches only its own
    // block, so the remaining blocks read the same from either copy.
    let mut out = body.to_owned();
    for (path, block) in signals::blocks(body) {
        let parsed = gjson::get(&block, "cache_control");
        if parsed.kind() != gjson::Kind::Object
            || parsed.get("ttl").exists()
            || parsed.get("type").kind() != gjson::Kind::String
        {
            continue;
        }
        let mut upgraded = format!(
            r#"{{"type":{},"ttl":{}"#,
            js_string(parsed.get("type").str()),
            js_string(ttl)
        );
        let scope = parsed.get("scope");
        if scope.exists() {
            upgraded.push_str(&format!(r#","scope":{}"#, scope.json()));
        }
        upgraded.push('}');
        out = rawjson::set_raw(&out, &format!("{path}.cache_control"), &upgraded);
    }
    out
}

/// `stripClaudeCacheControlTTL`.
pub(crate) fn strip_ttl(body: &str) -> String {
    if !gjson::valid(body) {
        return body.to_owned();
    }
    let mut out = body.to_owned();
    for (path, block) in signals::blocks(body) {
        let cc = gjson::get(&block, "cache_control");
        if cc.kind() == gjson::Kind::Object && cc.get("ttl").exists() {
            out = rawjson::delete(&out, &format!("{path}.cache_control.ttl"));
        }
    }
    out
}

/// `normalizeCacheControlTTL`: a 1h marker may not follow a 5m one in evaluation order.
pub(crate) fn normalize_ttl(body: &str) -> String {
    if !gjson::valid(body) {
        return body.to_owned();
    }
    let mut out = body.to_owned();
    let mut seen_5m = false;
    for (path, block) in signals::blocks(body) {
        let parsed = gjson::get(&block, "cache_control");
        if !parsed.exists() {
            continue;
        }
        let ttl = parsed.get("ttl");
        if parsed.kind() != gjson::Kind::Object || ttl.kind() != gjson::Kind::String || ttl.str() != "1h" {
            seen_5m = true;
            continue;
        }
        if seen_5m {
            out = rawjson::delete(&out, &format!("{path}.cache_control.ttl"));
        }
    }
    out
}

/// `enforceCacheControlLimit`: drop the earliest non-final system/tool markers, then
/// message markers, then any remaining system/tool markers.
pub(crate) fn enforce_cache_limit(body: &str, max: usize) -> String {
    if !gjson::valid(body) {
        return body.to_owned();
    }
    let total = count_cache_controls(body);
    if total <= max {
        return body.to_owned();
    }
    let mut excess = total - max;
    let mut body = body.to_owned();
    let remove = |body: &mut String, section: &str, keep_last: bool, excess: &mut usize| {
        let items = gjson::get(body, section).array().len();
        let marked: Vec<usize> = (0..items)
            .filter(|i| has_cache_control(body, &format!("{section}.{i}")))
            .collect();
        let last = if keep_last { marked.last().copied() } else { None };
        for i in marked {
            if *excess == 0 {
                break;
            }
            if Some(i) == last {
                continue;
            }
            *body = rawjson::delete(body, &format!("{section}.{i}.cache_control"));
            *excess -= 1;
        }
    };
    remove(&mut body, "system", true, &mut excess);
    if excess > 0 {
        remove(&mut body, "tools", true, &mut excess);
    }
    if excess > 0 {
        let messages = gjson::get(&body, "messages").array().len();
        'outer: for m in 0..messages {
            let content = gjson::get(&body, &format!("messages.{m}.content")).json().to_owned();
            if gjson::parse(&content).kind() != gjson::Kind::Array {
                continue;
            }
            for i in 0..gjson::parse(&content).array().len() {
                if excess == 0 {
                    break 'outer;
                }
                let path = format!("messages.{m}.content.{i}");
                if has_cache_control(&body, &path) {
                    body = rawjson::delete(&body, &format!("{path}.cache_control"));
                    excess -= 1;
                }
            }
        }
    }
    if excess > 0 {
        remove(&mut body, "system", false, &mut excess);
    }
    if excess > 0 {
        remove(&mut body, "tools", false, &mut excess);
    }
    body
}

fn cacheable_system(body: &str) -> bool {
    let system = rawjson::get(body, "system");
    match system.kind() {
        gjson::Kind::Array => !system.array().is_empty(),
        gjson::Kind::String => !system.str().trim().is_empty(),
        _ => false,
    }
}

/// `ensureCacheControl`: CPA-owned breakpoints for non-native callers.
pub(crate) fn ensure_cache_control(body: &str) -> String {
    let mut body = body.to_owned();
    if !cacheable_system(&body) {
        body = inject_tools_cache(&body);
    }
    body = inject_system_cache(&body);
    inject_messages_cache(&body)
}

const MARKER: &str = r#"{"type":"ephemeral"}"#;

fn inject_tools_cache(body: &str) -> String {
    let tools = rawjson::get(body, "tools");
    if tools.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut last = None;
    for (i, tool) in tools.array().iter().enumerate() {
        if tool.get("cache_control").exists() {
            return body.to_owned();
        }
        if !tool.get("defer_loading").bool() {
            last = Some(i);
        }
    }
    match last {
        Some(i) => rawjson::set_raw(body, &format!("tools.{i}.cache_control"), MARKER),
        None => body.to_owned(),
    }
}

fn inject_system_cache(body: &str) -> String {
    let system = rawjson::get(body, "system");
    match system.kind() {
        gjson::Kind::Array => {
            let items = system.array();
            if items.is_empty() || items.iter().any(|b| b.get("cache_control").exists()) {
                return body.to_owned();
            }
            rawjson::set_raw(body, &format!("system.{}.cache_control", items.len() - 1), MARKER)
        }
        gjson::Kind::String if !system.str().trim().is_empty() => {
            rawjson::set_raw(body, "system", &format!("[{}]", text_block(system.str(), EPHEMERAL)))
        }
        _ => body.to_owned(),
    }
}

fn eligible_for_rolling_cache(message: &gjson::Value) -> bool {
    let content = message.get("content");
    if content.kind() == gjson::Kind::String {
        return true;
    }
    let blocks = content.array();
    if content.kind() != gjson::Kind::Array || blocks.is_empty() {
        return false;
    }
    if message.get("role").str() != "assistant" {
        return true;
    }
    !matches!(
        blocks.last().map(|b| b.get("type").str().to_owned()).as_deref(),
        Some("thinking" | "redacted_thinking")
    )
}

fn inject_messages_cache(body: &str) -> String {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let msgs = messages.array();
    let last_eligible = msgs
        .iter()
        .rposition(|m| matches!(m.get("role").str(), "user" | "assistant") && eligible_for_rolling_cache(m));
    let Some(idx) = last_eligible else {
        return body.to_owned();
    };
    if let Some(final_msg) = msgs.last() {
        let content = final_msg.get("content");
        if final_msg.get("role").str() == "system"
            && content.kind() == gjson::Kind::String
            && !content.str().trim().is_empty()
        {
            return rawjson::set_raw(
                body,
                &format!("messages.{}.content", msgs.len() - 1),
                &format!("[{}]", text_block(content.str(), EPHEMERAL)),
            );
        }
    }
    let path = format!("messages.{idx}.content");
    let content = rawjson::get(body, &path);
    match content.kind() {
        gjson::Kind::Array => {
            let blocks = content.array();
            if blocks.iter().any(|b| b.get("cache_control").exists()) || blocks.is_empty() {
                return body.to_owned();
            }
            rawjson::set_raw(body, &format!("{path}.{}.cache_control", blocks.len() - 1), MARKER)
        }
        gjson::Kind::String => rawjson::set_raw(body, &path, &format!("[{}]", text_block(content.str(), EPHEMERAL))),
        _ => body.to_owned(),
    }
}

/// `generateFakeUserIDWithSessionID` for non-CLI-profile cloaking.
pub(crate) fn fake_user_id(session_id: &str) -> String {
    let session = uuid::Uuid::parse_str(session_id)
        .map(|_| session_id.to_owned())
        .unwrap_or_else(|_| super::session::new_v4());
    let mut bytes = [0u8; 32];
    let _ = getrandom::fill(&mut bytes);
    format!(
        r#"{{"device_id":"{}","account_uuid":"","session_id":{}}}"#,
        super::session::hex(&bytes),
        rawjson::go_string(&session)
    )
}

/// Case-insensitive literal matcher, longest words first (`BuildSensitiveWordMatcher`).
pub(crate) struct SensitiveWords(regex::Regex);

impl SensitiveWords {
    pub fn new(words: &[String]) -> Option<Self> {
        let mut valid: Vec<&str> = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| w.chars().count() >= 2 && !w.contains('\u{200B}'))
            .collect();
        if valid.is_empty() {
            return None;
        }
        // Go's sort.Slice is not stable; equal-length order cannot change which
        // text matches because alternatives of equal length cannot both match at
        // one position unless they are equal ignoring case.
        valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let pattern = format!(
            "(?i){}",
            valid.iter().map(|w| regex::escape(w)).collect::<Vec<_>>().join("|")
        );
        regex::Regex::new(&pattern).ok().map(Self)
    }

    fn apply(&self, text: &str) -> String {
        self.0
            .replace_all(text, |c: &regex::Captures<'_>| {
                let word = &c[0];
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) if !word.contains('\u{200B}') && first.len_utf8() < word.len() => {
                        format!("{first}\u{200B}{}", chars.as_str())
                    }
                    _ => word.to_owned(),
                }
            })
            .into_owned()
    }

    /// `ObfuscateSensitiveWords`: system (except billing) and message text.
    pub fn obfuscate(&self, body: &str) -> String {
        let mut body = body.to_owned();
        let system = rawjson::get(&body, "system");
        match system.kind() {
            gjson::Kind::Array => {
                let edits: Vec<(String, String)> = system
                    .array()
                    .iter()
                    .enumerate()
                    .filter(|(_, b)| b.get("type").str() == "text" && !b.get("text").str().starts_with(BILLING_PREFIX))
                    .filter_map(|(i, b)| {
                        let text = b.get("text").str().to_owned();
                        let out = self.apply(&text);
                        (out != text).then(|| (format!("system.{i}.text"), out))
                    })
                    .collect();
                for (path, text) in edits {
                    body = rawjson::set_str(&body, &path, &text);
                }
            }
            gjson::Kind::String if !system.str().starts_with(BILLING_PREFIX) => {
                let text = system.str().to_owned();
                let out = self.apply(&text);
                if out != text {
                    body = rawjson::set_str(&body, "system", &out);
                }
            }
            _ => {}
        }
        let count = rawjson::get(&body, "messages").array().len();
        for m in 0..count {
            let path = format!("messages.{m}.content");
            let content = gjson::get(&body, &path).json().to_owned();
            let parsed = gjson::parse(&content);
            match parsed.kind() {
                gjson::Kind::String => {
                    let out = self.apply(parsed.str());
                    if out != parsed.str() {
                        body = rawjson::set_str(&body, &path, &out);
                    }
                }
                gjson::Kind::Array => {
                    for (i, block) in parsed.array().iter().enumerate() {
                        if block.get("type").str() != "text" {
                            continue;
                        }
                        let text = block.get("text").str().to_owned();
                        let out = self.apply(&text);
                        if out != text {
                            body = rawjson::set_str(&body, &format!("{path}.{i}.text"), &out);
                        }
                    }
                }
                _ => {}
            }
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_header_matches_go_capture() {
        let b = Billing {
            signed: true,
            version: "2.1.280",
            message: "Local question",
            entrypoint: "cli",
            workload: "",
            subagent: false,
            prev_req: "",
            prompt_id: "83ec619f-ab81-4d70-9c67-44c3865291b9",
            human_turn: true,
        };
        assert_eq!(
            billing_header(&b),
            "x-anthropic-billing-header: cc_version=2.1.280.b43; cc_entrypoint=cli; cch=00000; cc_prompt_id=83ec619f-ab81-4d70-9c67-44c3865291b9; cc_turn_origin=human;"
        );
    }

    #[test]
    fn cache_control_walk_reads_each_block_in_go_order() {
        // Evaluation order is tools, system, messages; the 1h marker on tools comes
        // first and stays, the system marker is 5m, so the later 1h message marker loses
        // its ttl. Text that mentions cache_control and a string content are not blocks.
        let body = r#"{"messages":[{"role":"user","content":"cache_control"},{"role":"user","content":[{"type":"text","text":"\"cache_control\":{}"},{"type":"text","text":"b","cache_control":{"type":"ephemeral","ttl":"1h"}}]}],"system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral"}}],"tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#;
        assert_eq!(count_cache_controls(body), 3);
        assert!(signals::has_1h_ttl(body));
        assert_eq!(
            normalize_ttl(body),
            body.replace(
                r#""text":"b","cache_control":{"type":"ephemeral","ttl":"1h"}"#,
                r#""text":"b","cache_control":{"type":"ephemeral"}"#
            )
        );
        assert_eq!(strip_ttl(body).matches(r#""ttl""#).count(), 0);
        let upgraded = upgrade_ttl(body, "1h");
        assert!(
            upgraded
                .contains(r#""system":[{"type":"text","text":"s","cache_control":{"type":"ephemeral","ttl":"1h"}}]"#)
        );
        assert_eq!(upgraded.matches(r#""ttl":"1h""#).count(), 3);
        let paths: Vec<String> = signals::blocks(body).into_iter().map(|(p, _)| p).collect();
        assert_eq!(
            paths,
            ["tools.0", "system.0", "messages.1.content.0", "messages.1.content.1"]
        );
    }

    #[test]
    fn sensitive_words_split_first_rune() {
        let m = SensitiveWords::new(&["Proxy".into(), "x".into(), "proxy api".into()]).unwrap();
        assert_eq!(
            m.apply("my PROXY API and proxy"),
            "my P\u{200B}ROXY API and p\u{200B}roxy"
        );
    }
}
