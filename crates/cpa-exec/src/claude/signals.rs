//! Request classification used by cloaking, betas and caching
//! (helps/claude_diagnostics.go): quota probes, title helpers, subagents, 1h cache
//! pools, prompt turns and billing continuity tags.

use cpa_core::gostr::GoStr;
use std::borrow::Cow;

use http::HeaderMap;

use super::detect::{header, header_values};
use super::session::{valid_prompt_id, valid_request_id};
use crate::rawjson;

fn probe(body: &str) -> bool {
    let max_tokens = rawjson::get(body, "max_tokens");
    if !max_tokens.exists() || max_tokens.i64() != 1 {
        return false;
    }
    let tools = rawjson::get(body, "tools");
    if tools.exists() && !tools.array().is_empty() {
        return false;
    }
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array || messages.array().is_empty() {
        return true;
    }
    let messages = messages.array();
    if messages.len() != 1 || messages[0].get("role").str() != "user" {
        return false;
    }
    let content = messages[0].get("content");
    let probe_text = |t: &str| matches!(t, "quota" | "test" | "." | "probe");
    match content.kind() {
        gjson::Kind::String => probe_text(content.str().trim()),
        gjson::Kind::Array => {
            let (mut count, mut matched) = (0, false);
            for part in content.array() {
                let t = part.get("text");
                let t = t.str().trim();
                if t.contains("<system-reminder>") {
                    continue;
                }
                count += 1;
                if probe_text(t) || (t == "Hi" && part.get("cache_control").exists()) {
                    matched = true;
                }
            }
            count == 1 && matched
        }
        _ => false,
    }
}

fn any_text(value: &rawjson::Value, matches: impl Fn(&str) -> bool) -> bool {
    if value.kind() == gjson::Kind::Array {
        value.array().iter().any(|p| matches(p.get("text").str()))
    } else {
        matches(value.str())
    }
}

/// `jsonMayContainASCII` (v8.0.27, 5dbce4f): whether a decoded key or string of the
/// JSON document `body` could contain one of `needles`, so the classifiers below can
/// skip their walks. Each needle is printable ASCII without `\` or `/`, with a `"` only
/// at either end (a string delimiter). JSON spells those characters as themselves or
/// as a `\u00XX` escape, so unless such an escape occurs, a needle missing from the raw
/// bytes is missing from every decoded string; with one the answer is yes and the
/// caller walks as before. The answer never changes the result, only the cost.
///
/// Per call: one scan of the body per needle, plus one for `\u00`.
pub(crate) fn may_contain(body: &str, needles: &[&str]) -> bool {
    printable_ascii_escape(body) || needles.iter().any(|needle| body.contains(needle))
}

/// `hasPrintableASCIIUnicodeEscape`: a `\u00` followed by `2`..`7`. Errs only toward
/// yes (DEL, malformed or truncated escapes, an escaped backslash before `u00`).
fn printable_ascii_escape(body: &str) -> bool {
    let mut rest = body;
    while let Some(i) = rest.find("\\u00") {
        match rest.as_bytes().get(i + 4) {
            None => return false,
            Some(b'2'..=b'7') => return true,
            // `\u00` is ASCII, so `i + 4` is a character boundary.
            Some(_) => rest = &rest[i + 4..],
        }
    }
    false
}

fn title_helper(body: &str) -> bool {
    // Without an output_config key the request is a helper only when one of the system
    // title instructions below appears.
    if !may_contain(
        body,
        &[
            "output_config",
            "naming a coding session",
            "Return a short title",
            "Write the title in the predominant language",
        ],
    ) {
        return false;
    }
    let system_title = |t: &str| {
        t.contains("naming a coding session")
            || t.contains("Return a short title")
            || t.contains("Write the title in the predominant language")
    };
    let props = rawjson::get(body, "output_config.format.schema.properties");
    if props.exists() {
        if !(props.get("title").exists() && props.map_len() == 1) {
            return false;
        }
        let prompt = |t: &str| system_title(t) || t.contains("<session>");
        let messages = rawjson::get(body, "messages");
        return any_text(&rawjson::get(body, "system"), prompt)
            || messages.items().iter().any(|m| any_text(&m.get("content"), prompt));
    }
    if any_text(&rawjson::get(body, "system"), system_title) {
        return true;
    }
    rawjson::get(body, "messages")
        .items()
        .iter()
        .filter(|m| m.get("role").str() == "system")
        .any(|m| any_text(&m.get("content"), system_title))
}

/// `IsClaudeProbeOrHelperRequest`.
pub(crate) fn probe_or_helper(body: &str) -> bool {
    probe(body) || title_helper(body)
}

/// `IsClaudeSubagentRequest`.
pub(crate) fn subagent(headers: &HeaderMap, body: &str) -> bool {
    if !header(headers, "x-claude-code-agent-id").is_empty()
        || !header(headers, "x-claude-code-parent-agent-id").is_empty()
    {
        return true;
    }
    if rawjson::get(body, "metadata.user_id.parent_session_id").exists()
        || rawjson::get(body, "metadata.user_id")
            .str()
            .contains("\"parent_session_id\"")
    {
        return true;
    }
    let system = rawjson::get(body, "system");
    match system.kind() {
        gjson::Kind::Array => system
            .array()
            .first()
            .is_some_and(|b| b.get("text").str().contains("cc_is_subagent=true")),
        gjson::Kind::String => system.str().contains("cc_is_subagent=true"),
        _ => false,
    }
}

/// Every cache_control-capable block in tools → system → messages order
/// (`forEachClaudeCacheControlBlock`): its path and its raw JSON. One pass over `body`,
/// like Go's ForEach; looking each path up again would rescan the body per block.
pub(crate) fn blocks(body: &str) -> Vec<(String, Cow<'_, str>)> {
    // The items borrow from `body`; `rawjson::offset` recovers that borrow.
    let raw = |item: &rawjson::Value<'_>| match rawjson::offset(body, item) {
        Some(start) => Cow::Borrowed(&body[start..start + item.json().len()]),
        None => Cow::Owned(item.json().to_owned()),
    };
    let mut out = Vec::new();
    for section in ["tools", "system"] {
        let v = rawjson::get(body, section);
        if v.kind() == gjson::Kind::Array {
            out.extend(
                v.array()
                    .iter()
                    .enumerate()
                    .map(|(i, item)| (format!("{section}.{i}"), raw(item))),
            );
        }
    }
    let messages = rawjson::get(body, "messages");
    if messages.kind() == gjson::Kind::Array {
        for (m, message) in messages.array().iter().enumerate() {
            let content = message.get("content");
            if content.kind() == gjson::Kind::Array {
                out.extend(
                    content
                        .array()
                        .iter()
                        .enumerate()
                        .map(|(i, item)| (format!("messages.{m}.content.{i}"), raw(item))),
                );
            }
        }
    }
    out
}

/// `ClaudePayloadHas1hTTL`. A ttl of 1h is the JSON string `"1h"`, whose quotes are
/// never escaped.
pub(crate) fn has_1h_ttl(body: &str) -> bool {
    !body.is_empty()
        && may_contain(body, &["\"1h\""])
        && rawjson::valid(body)
        && blocks(body).iter().any(|(_, block)| {
            let cc = rawjson::get(block, "cache_control");
            cc.kind() == gjson::Kind::Object && cc.get("ttl").str() == "1h"
        })
}

/// `ClaudeSubagentRequests1h`.
pub(crate) fn subagent_requests_1h(headers: &HeaderMap, body: &str) -> bool {
    has_1h_ttl(body)
        || header_values(headers, "anthropic-beta")
            .join(",")
            .contains("extended-cache-ttl-2025-04-11")
}

/// `IsClaudeNewPromptTurn`.
pub(crate) fn new_prompt_turn(body: &str) -> bool {
    if probe_or_helper(body) {
        return false;
    }
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return true;
    }
    let messages = messages.array();
    let Some(last) = messages.last() else { return true };
    if last.get("role").str() != "user" {
        return false;
    }
    let content = last.get("content");
    !(content.kind() == gjson::Kind::Array && content.array().iter().any(|p| p.get("type").str() == "tool_result"))
}

const BILLING: &str = "x-anthropic-billing-header:";

fn billing_text(body: &str) -> Option<String> {
    let system = rawjson::get(body, "system");
    let text = match system.kind() {
        gjson::Kind::Array => system.array().first()?.get("text").str().to_owned(),
        gjson::Kind::String => system.str().to_owned(),
        _ => return None,
    };
    text.starts_with(BILLING).then_some(text)
}

/// `ExtractClaudeBillingTags`: (cc_prev_req, cc_prompt_id) when valid.
pub(crate) fn billing_tags(body: &str) -> (String, String) {
    let Some(text) = billing_text(body) else {
        return Default::default();
    };
    let tag = |name: &str| {
        text.find(name).map(|i| {
            let v = &text[i + name.len()..];
            v[..v.find(';').unwrap_or(v.len())].to_owned()
        })
    };
    let prev = tag("cc_prev_req=").filter(|v| valid_request_id(v)).unwrap_or_default();
    let prompt = tag("cc_prompt_id=")
        .filter(|v| valid_prompt_id(v))
        .map(|v| v.go_lower())
        .unwrap_or_default();
    (prev, prompt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_helper_and_tags() {
        assert!(probe_or_helper(
            r#"{"max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#
        ));
        assert!(!probe_or_helper(
            r#"{"max_tokens":2,"messages":[{"role":"user","content":"quota"}]}"#
        ));
        assert!(probe_or_helper(
            r#"{"max_tokens":9,"system":"Return a short title","messages":[]}"#
        ));
        let body = r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli; cch=00000; cc_prev_req=req_ab-1; cc_prompt_id=83EC619F-AB81-4D70-9C67-44C3865291B9;"}]}"#;
        assert_eq!(
            billing_tags(body),
            ("req_ab-1".into(), "83ec619f-ab81-4d70-9c67-44c3865291b9".into())
        );
    }
}
