//! Request classification used by cloaking, betas and caching
//! (helps/claude_diagnostics.go): quota probes, title helpers, subagents, 1h cache
//! pools, prompt turns and billing continuity tags.

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

fn any_text(value: &gjson::Value, matches: impl Fn(&str) -> bool) -> bool {
    if value.kind() == gjson::Kind::Array {
        value.array().iter().any(|p| matches(p.get("text").str()))
    } else {
        matches(value.str())
    }
}

fn title_helper(body: &str) -> bool {
    let system_title = |t: &str| {
        t.contains("naming a coding session")
            || t.contains("Return a short title")
            || t.contains("Write the title in the predominant language")
    };
    let props = rawjson::get(body, "output_config.format.schema.properties");
    if props.exists() {
        let mut count = 0;
        props.each(|_, _| {
            count += 1;
            true
        });
        if !(props.get("title").exists() && count == 1) {
            return false;
        }
        let prompt = |t: &str| system_title(t) || t.contains("<session>");
        let messages = rawjson::get(body, "messages");
        return any_text(&rawjson::get(body, "system"), prompt)
            || messages.array().iter().any(|m| any_text(&m.get("content"), prompt));
    }
    if any_text(&rawjson::get(body, "system"), system_title) {
        return true;
    }
    rawjson::get(body, "messages")
        .array()
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
    let raw = |item: &gjson::Value<'_>| match rawjson::offset(body, item) {
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

/// `ClaudePayloadHas1hTTL`.
pub(crate) fn has_1h_ttl(body: &str) -> bool {
    !body.is_empty()
        && gjson::valid(body)
        && blocks(body).iter().any(|(_, block)| {
            let cc = gjson::get(block, "cache_control");
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
        .map(|v| v.to_lowercase())
        .unwrap_or_default();
    (prev, prompt)
}

/// Removes `\s*cc_prev_req=...;` and `\s*cc_prompt_id=...;` occurrences.
fn remove_tags(text: &str) -> String {
    let mut out = text.to_owned();
    for name in ["cc_prev_req=", "cc_prompt_id="] {
        let mut result = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(i) = rest.find(name) {
            let value = &rest[i + name.len()..];
            match value.find(';') {
                Some(end) if end > 0 => {
                    // RE2 `\s` is [\t\n\f\r ], without \v.
                    let before = rest[..i].trim_end_matches([' ', '\t', '\n', '\r', '\x0c']);
                    result.push_str(before);
                    rest = &value[end + 1..];
                }
                _ => {
                    result.push_str(&rest[..i + name.len()]);
                    rest = value;
                }
            }
        }
        result.push_str(rest);
        out = result;
    }
    out
}

/// `InjectClaudeBillingTags`: replaces the billing header's continuity tags.
pub(crate) fn inject_billing_tags(body: &str, prev: &str, prompt: &str) -> String {
    let system = rawjson::get(body, "system");
    if system.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let Some(text) = system.array().first().map(|b| b.get("text").str().to_owned()) else {
        return body.to_owned();
    };
    if !text.starts_with(BILLING) {
        return body.to_owned();
    }
    let mut cleaned = remove_tags(&text).trim().to_owned();
    if !cleaned.ends_with(';') {
        cleaned.push(';');
    }
    if !prev.is_empty() {
        cleaned.push_str(&format!(" cc_prev_req={prev};"));
    }
    if !prompt.is_empty() {
        cleaned.push_str(&format!(" cc_prompt_id={prompt};"));
    }
    rawjson::set_str(body, "system.0.text", &cleaned)
}

/// `StripClaudeBillingTags` (probe and helper requests carry none).
pub(crate) fn strip_billing_tags(body: &str) -> String {
    let system = rawjson::get(body, "system");
    if system.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let Some(text) = system.array().first().map(|b| b.get("text").str().to_owned()) else {
        return body.to_owned();
    };
    if !text.starts_with(BILLING) {
        return body.to_owned();
    }
    let cleaned = remove_tags(&text);
    if cleaned == text {
        return body.to_owned();
    }
    rawjson::set_str(body, "system.0.text", &cleaned)
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
        assert_eq!(
            rawjson::get(&strip_billing_tags(body), "system.0.text").str(),
            "x-anthropic-billing-header: cc_version=2.1.280.abc; cc_entrypoint=cli; cch=00000;"
        );
    }
}
