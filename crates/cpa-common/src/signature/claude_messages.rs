//! Claude Messages history sanitizing for a target provider (claude_messages_sanitize.go).

use gjson::{Kind, Value};

use super::claude::is_empty_thinking_placeholder;
use super::{Action, BlockKind, Decision, Provider, decide_compatibility_for_model, normalize_target};
use crate::gojson as json;

/// `ClaudeMessagesSignatureSanitizeOptions`.
#[derive(Debug, Clone, Default)]
pub struct ClaudeMessagesSanitizeOptions {
    /// `Provider::Empty` is Go's zero value.
    pub target_provider: Provider,
    pub target_model: String,
    pub drop_empty_messages: bool,
    pub drop_tool_signatures: bool,
    pub drop_empty_thinking_placeholders: bool,
    pub preserve_empty_thinking_blocks: bool,
}

/// `SignatureSanitizeReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizeReport {
    pub target_provider: Provider,
    pub preserved: usize,
    pub dropped_blocks: usize,
    pub dropped_signatures: usize,
    pub replaced_signatures: usize,
    pub decisions: Vec<Decision>,
}

/// `SanitizeClaudeMessagesSignaturesForModel`.
pub fn sanitize_claude_messages_signatures_for_model(payload: &str, target_model: &str) -> (String, SanitizeReport) {
    sanitize_claude_messages_signatures_for_target(
        payload,
        &ClaudeMessagesSanitizeOptions {
            target_provider: super::provider_from_model_name(target_model),
            target_model: target_model.into(),
            drop_empty_messages: true,
            ..Default::default()
        },
    )
}

/// `SanitizeClaudeMessagesForClaudeUpstream`.
pub fn sanitize_claude_messages_for_claude_upstream(
    payload: &str,
    target_model: &str,
    preserve_empty_thinking_blocks: bool,
) -> (String, SanitizeReport) {
    sanitize_claude_messages_signatures_for_target(
        payload,
        &ClaudeMessagesSanitizeOptions {
            target_provider: Provider::Claude,
            target_model: target_model.into(),
            drop_empty_messages: true,
            drop_tool_signatures: true,
            drop_empty_thinking_placeholders: !preserve_empty_thinking_blocks,
            preserve_empty_thinking_blocks,
        },
    )
}

/// `SanitizeClaudeMessagesSignaturesForTarget`.
pub fn sanitize_claude_messages_signatures_for_target(
    payload: &str,
    opts: &ClaudeMessagesSanitizeOptions,
) -> (String, SanitizeReport) {
    let mut target = normalize_target(opts.target_provider);
    if target == Provider::Unknown && !opts.target_model.is_empty() {
        target = super::provider_from_model_name(&opts.target_model);
    }
    let mut report = SanitizeReport {
        target_provider: target,
        preserved: 0,
        dropped_blocks: 0,
        dropped_signatures: 0,
        replaced_signatures: 0,
        decisions: Vec::new(),
    };
    let messages = gjson::get(payload, "messages");
    if messages.kind() != Kind::Array {
        return (payload.to_owned(), report);
    }
    let mut kept_messages = Vec::new();
    let mut modified = false;
    for (i, message) in messages.array().iter().enumerate() {
        let content = message.get("content");
        if content.kind() != Kind::Array {
            kept_messages.push(message.json().to_owned());
            continue;
        }
        let mut kept_parts: Vec<String> = Vec::new();
        let mut message_modified = false;
        for (j, part) in content.array().iter().enumerate() {
            let part_type = json::go_str(&part.get("type"));
            if part_type == "tool_use" {
                if opts.drop_tool_signatures {
                    let (updated, changed) = strip_tool_use_signature_fields(part);
                    if changed {
                        message_modified = true;
                        report.dropped_signatures += 1;
                    }
                    kept_parts.push(updated);
                    continue;
                }
                let (updated, changed, decisions) = sanitize_tool_use_signature(part, target, &opts.target_model, i, j);
                if changed {
                    message_modified = true;
                }
                for decision in &decisions {
                    match decision.action {
                        Action::Preserve => report.preserved += 1,
                        Action::ReplaceWithGeminiBypass => report.replaced_signatures += 1,
                        _ => report.dropped_signatures += 1,
                    }
                }
                report.decisions.extend(decisions);
                kept_parts.push(updated);
                continue;
            }
            if part_type != "thinking" {
                kept_parts.push(part.json().to_owned());
                continue;
            }
            let raw_signature = json::go_str(&part.get("signature"));
            if opts.preserve_empty_thinking_blocks {
                report.preserved += 1;
                kept_parts.push(part.json().to_owned());
                continue;
            }
            if target == Provider::Claude
                && is_empty_thinking_placeholder(part)
                && !opts.drop_empty_thinking_placeholders
            {
                kept_parts.push(part.json().to_owned());
                continue;
            }
            let mut decision =
                decide_compatibility_for_model(target, &opts.target_model, &raw_signature, BlockKind::ClaudeThinking);
            decision.reason = format!("messages[{i}].content[{j}]: {}", decision.reason);
            let action = decision.action;
            let normalized = decision.normalized_signature.clone();
            let replacement = decision.replacement_signature.clone();
            report.decisions.push(decision);
            match action {
                Action::Preserve => {
                    report.preserved += 1;
                    if !normalized.is_empty() && normalized != raw_signature {
                        kept_parts.push(json::set_str(part.json(), "signature", &normalized));
                        message_modified = true;
                    } else {
                        kept_parts.push(part.json().to_owned());
                    }
                }
                Action::ReplaceWithGeminiBypass => {
                    report.replaced_signatures += 1;
                    kept_parts.push(json::set_str(part.json(), "signature", &replacement));
                    message_modified = true;
                }
                Action::DropSignature => {
                    report.dropped_signatures += 1;
                    kept_parts.push(json::delete(part.json(), "signature"));
                    message_modified = true;
                }
                _ => {
                    report.dropped_blocks += 1;
                    message_modified = true;
                }
            }
        }
        if message_modified {
            modified = true;
            if kept_parts.is_empty() && opts.drop_empty_messages {
                continue;
            }
            kept_messages.push(json::set_raw(message.json(), "content", &json::join_array(&kept_parts)));
            continue;
        }
        kept_messages.push(message.json().to_owned());
    }
    if !modified {
        return (payload.to_owned(), report);
    }
    (
        json::set_raw(payload, "messages", &json::join_array(&kept_messages)),
        report,
    )
}

const TOOL_USE_SIGNATURE_PATHS: [&str; 4] = [
    "signature",
    "thoughtSignature",
    "thought_signature",
    "extra_content.google.thought_signature",
];

fn strip_tool_use_signature_fields(part: &Value<'_>) -> (String, bool) {
    let mut updated = part.json().to_owned();
    let mut changed = false;
    for path in TOOL_USE_SIGNATURE_PATHS.iter().copied().chain(["model"]) {
        if !gjson::get(&updated, path).exists() {
            continue;
        }
        updated = json::delete(&updated, path);
        changed = true;
    }
    for path in ["extra_content.google", "extra_content"] {
        if let Some(cleaned) = delete_empty_object(&updated, path) {
            updated = cleaned;
            changed = true;
        }
    }
    (updated, changed)
}

fn sanitize_tool_use_signature(
    part: &Value<'_>,
    target: Provider,
    target_model: &str,
    message_idx: usize,
    part_idx: usize,
) -> (String, bool, Vec<Decision>) {
    let mut updated = part.json().to_owned();
    let mut changed = false;
    let mut decisions = Vec::new();
    for path in TOOL_USE_SIGNATURE_PATHS {
        let sig = part.get(path);
        if !sig.exists() {
            continue;
        }
        let block_kind = match target {
            Provider::Claude => BlockKind::ClaudeThinking,
            Provider::Gpt => BlockKind::GptReasoning,
            _ => BlockKind::GeminiFunctionCall,
        };
        let raw = json::go_str(&sig);
        let mut decision = decide_compatibility_for_model(target, target_model, &raw, block_kind);
        decision.reason = format!(
            "messages[{message_idx}].content[{part_idx}].{path}: {}",
            decision.reason
        );
        match decision.action {
            Action::Preserve => {
                if !decision.normalized_signature.is_empty() && decision.normalized_signature != raw {
                    updated = json::set_str(&updated, path, &decision.normalized_signature);
                    changed = true;
                }
            }
            Action::ReplaceWithGeminiBypass => {
                updated = json::set_str(&updated, path, &decision.replacement_signature);
                changed = true;
            }
            _ => {
                updated = json::delete(&updated, path);
                changed = true;
            }
        }
        decisions.push(decision);
    }
    for path in ["extra_content.google", "extra_content"] {
        if let Some(cleaned) = delete_empty_object(&updated, path) {
            updated = cleaned;
            changed = true;
        }
    }
    (updated, changed, decisions)
}

fn delete_empty_object(raw: &str, path: &str) -> Option<String> {
    json::is_empty_object(raw, path).then(|| json::delete(raw, path))
}
