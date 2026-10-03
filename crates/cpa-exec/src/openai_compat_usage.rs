//! Test support: what the OpenAI-compatible and xAI executors report to their
//! `UsageSink`, and the usage record Go's reporter publishes from the same payloads
//! (helps/usage_helpers.go: SetTranslatedReasoningEffort, ParseOpenAIUsage,
//! ParseCodexUsage, StreamUsageBuffer.ObserveOpenAIStream, extractResponseModelEvent's
//! generic rule). The generators record Go's published record; the tests compare.

use std::sync::Mutex;

use cpa_common::json::{self as gj, Res};
use cpa_core::format::Format;
use serde_json::Value;

/// Every report, in order: (`request` | `body` | `line`, format, bytes).
#[derive(Default)]
pub(crate) struct Recorder(pub Mutex<Vec<(&'static str, Format, Vec<u8>)>>);

impl cpa_core::exec::UsageObserver for Recorder {
    fn response_body(&self, format: Format, body: &[u8]) {
        self.0.lock().unwrap().push(("body", format, body.to_vec()));
    }
    fn response_line(&self, format: Format, line: &[u8]) {
        self.0.lock().unwrap().push(("line", format, line.to_vec()));
    }
    fn request(&self, format: Format, payload: &[u8]) {
        self.0.lock().unwrap().push(("request", format, payload.to_vec()));
    }
}

/// `parseOpenAIStyleUsageNode` for the fields the fixtures record, when the node has
/// token fields (`hasOpenAIStyleUsageTokenFields`).
fn tokens(node: &Res<'_>) -> Option<[i64; 5]> {
    if !node.is_object() {
        return None;
    }
    let pick = |a: &str, b: &str| {
        let r = node.get(a);
        if r.exists() { r } else { node.get(b) }
    };
    let fields = [
        "total_tokens",
        "prompt_tokens",
        "input_tokens",
        "completion_tokens",
        "output_tokens",
    ];
    if !fields.iter().any(|f| node.get(*f).exists()) {
        return None;
    }
    Some([
        pick("prompt_tokens", "input_tokens").int(),
        pick("completion_tokens", "output_tokens").int(),
        pick(
            "completion_tokens_details.reasoning_tokens",
            "output_tokens_details.reasoning_tokens",
        )
        .int(),
        pick(
            "prompt_tokens_details.cached_tokens",
            "input_tokens_details.cached_tokens",
        )
        .int(),
        node.get("total_tokens").int(),
    ])
}

/// A stream line's JSON payload (`jsonPayload`).
fn payload(line: &[u8]) -> Option<&[u8]> {
    let mut trimmed = line.trim_ascii();
    if let Some(rest) = trimmed.strip_prefix(b"data:") {
        trimmed = rest.trim_ascii();
    }
    (trimmed.first() == Some(&b'{')).then_some(trimmed)
}

/// The record Go publishes for one attempt, derived from the reports.
pub(crate) fn derived(reports: &[(&'static str, Format, Vec<u8>)], failed: bool) -> Value {
    let mut effort = String::new();
    let mut usage: Option<[i64; 5]> = None;
    let (mut model, mut model_final) = (String::new(), false);
    for (kind, format, data) in reports {
        if *kind == "request" {
            effort = cpa_common::thinking::extract_translated_reasoning_effort(data, format.as_str());
            continue;
        }
        let Some(json) = payload(data) else { continue };
        let root = gj::parse(json);
        let event = root.get("type").str().into_owned();
        let terminal = matches!(
            event.as_str(),
            "response.completed" | "response.incomplete" | "response.done"
        );
        let found = match (*kind, format) {
            ("body", _) => tokens(&root.get("usage")),
            // Codex: the first terminal event's usage.
            (_, Format::Codex | Format::OpenAIResponse) if terminal && usage.is_none() => {
                tokens(&root.get("response.usage"))
            }
            // Chat Completions streams: the latest usage chunk.
            (_, Format::OpenAI) => tokens(&root.get("usage")),
            _ => None,
        };
        if found.is_some() {
            usage = found;
        }
        if model_final {
            continue;
        }
        let served = root.get("response.model").str().into_owned();
        if !served.is_empty() {
            model = served;
            model_final = terminal;
            continue;
        }
        let served = root.get("model").str().into_owned();
        if !served.is_empty() {
            model = served;
            let status = root.get("status").str().into_owned();
            let object = root.get("object").str().into_owned();
            let finish = root.get("choices.0.finish_reason").str().into_owned();
            model_final =
                object == "chat.completion" || !finish.is_empty() || status == "completed" || status == "incomplete";
        }
    }
    let mut t = if failed { [0; 5] } else { usage.unwrap_or_default() };
    // EnsureTokenBreakdownForProvider (server-side accounting): OpenAI-style providers
    // count cached input inside input and reasoning inside output, so a missing total is
    // their sum.
    if t[4] == 0 {
        t[4] = t[0] + t[1];
    }
    serde_json::json!({
        "input": t[0], "output": t[1], "reasoning": t[2], "cached": t[3], "total": t[4],
        "effort": effort, "response_model": model, "failed": failed,
    })
}
