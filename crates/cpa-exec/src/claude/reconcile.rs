//! Repairs after `requests.payload` rules: Go re-checks the cloak's model-specific
//! additions once payload rules have run, because a rule may rewrite the model, the
//! messages or the probe shape long after cloaking decided them.

use super::cloak::{self, FABLE_REPORTING};
use crate::rawjson;

/// `claudeCodeSystemPlacementState`: only the `role: system` turns CPA itself inserted
/// while cloaking a current model.
#[derive(Default)]
pub(crate) struct SystemPlacement {
    insert_at: usize,
    inserted: Vec<String>,
    texts: Vec<String>,
}

impl SystemPlacement {
    /// `captureClaudeCodeSystemPlacement`. The message-count increase is part of the
    /// proof: turns that already existed belong to the caller.
    pub(crate) fn capture(before: &str, after: &str, cloaked: bool) -> Self {
        if !cloaked || cloak::legacy_system_reminder(before) {
            return Self::default();
        }
        let texts = cloak::forwarded_system_blocks(before);
        if texts.is_empty() {
            return Self::default();
        }
        let (before_msgs, after_msgs) = (rawjson::get(before, "messages"), rawjson::get(after, "messages"));
        let (before_msgs, after_msgs) = (before_msgs.array(), after_msgs.array());
        if after_msgs.len() != before_msgs.len() + texts.len() {
            return Self::default();
        }
        let Some(first) = cloak::first_user_index(before) else {
            return Self::default();
        };
        let mut at = first + 1;
        while at < before_msgs.len() && before_msgs[at].get("role").str() == "user" {
            at += 1;
        }
        if at + texts.len() > after_msgs.len() {
            return Self::default();
        }
        let mut inserted = Vec::with_capacity(texts.len());
        for (i, text) in texts.iter().enumerate() {
            let message = &after_msgs[at + i];
            if message.get("role").str() != "system" || cloak::message_text(&message.get("content")) != *text {
                return Self::default();
            }
            inserted.push(message.json().to_owned());
        }
        Self {
            insert_at: at,
            inserted,
            texts,
        }
    }

    /// `reconcileClaudeCodeSystemPlacementAfterPayload`: when payload rules made the
    /// final model a legacy one, the exact turns captured above move to
    /// `<system-reminder>` blocks. Any rule edit to those turns fails closed and leaves
    /// the final mid-system validation to reject the request.
    pub(crate) fn reconcile(&self, body: &str) -> String {
        if self.inserted.is_empty() || !cloak::legacy_system_reminder(body) {
            return body.to_owned();
        }
        let messages = rawjson::get(body, "messages");
        let messages = messages.array();
        let end = self.insert_at + self.inserted.len();
        if end > messages.len()
            || (self.insert_at..end).any(|i| messages[i].json() != self.inserted[i - self.insert_at])
        {
            return body.to_owned();
        }
        let kept: Vec<&str> = messages
            .iter()
            .enumerate()
            .filter(|(i, _)| !(self.insert_at..end).contains(i))
            .map(|(_, m)| m.json())
            .collect();
        let updated = rawjson::set_raw(body, "messages", &format!("[{}]", kept.join(",")));
        cloak::prepend_reminders(&updated, &self.texts)
    }
}

/// `claudeCodeFableState`: which Fable additions the cloak injected.
#[derive(Default)]
pub(crate) struct FableState {
    fallbacks: bool,
    display: bool,
    reporting: bool,
}

impl FableState {
    /// `captureClaudeCodeFableState`.
    pub(crate) fn capture(before: &str, after: &str, cloaked: bool) -> Self {
        if !cloaked || before.is_empty() || after.is_empty() {
            return Self::default();
        }
        let added = |path: &str| !rawjson::get(before, path).exists() && rawjson::get(after, path).exists();
        Self {
            fallbacks: added("fallbacks"),
            display: added("thinking.display"),
            reporting: !has_reporting_block(before) && has_reporting_block(after),
        }
    }
}

fn unmarked(text: &str) -> String {
    text.replace('\u{200B}', "")
}

/// `hasFableReportingBlock`.
fn has_reporting_block(body: &str) -> bool {
    let system = rawjson::get(body, "system");
    if system.kind() != gjson::Kind::Array {
        return unmarked(system.str()).contains(FABLE_REPORTING);
    }
    system
        .array()
        .iter()
        .any(|b| unmarked(b.get("text").str()) == FABLE_REPORTING)
}

fn remove_reporting_block(body: String) -> String {
    let system = rawjson::get(&body, "system");
    if system.kind() == gjson::Kind::Array {
        let blocks = system.array();
        let kept: Vec<&str> = blocks
            .iter()
            .filter(|b| unmarked(b.get("text").str()) != FABLE_REPORTING)
            .map(|b| b.json())
            .collect();
        if kept.len() == blocks.len() {
            return body;
        }
        return rawjson::set_raw(&body, "system", &format!("[{}]", kept.join(",")));
    }
    if unmarked(system.str()) == FABLE_REPORTING {
        return rawjson::delete(&body, "system");
    }
    body
}

/// `reconcileClaudeCodeFableModelAfterPayload`: the Opus fallback, `thinking.display`
/// and the `# Reporting outcomes` block follow the final model. Additions a payload
/// rule touched, or the caller sent, are never changed.
pub(crate) fn fable(
    body: &str,
    state: &FableState,
    touched_fallbacks: bool,
    touched_display: bool,
    cloaked: bool,
    probe: bool,
) -> String {
    let mut body = body.to_owned();
    if !cloaked || body.is_empty() {
        return body;
    }
    if probe {
        if state.fallbacks && !touched_fallbacks {
            body = rawjson::delete(&body, "fallbacks");
        }
        if state.display && !touched_display {
            body = rawjson::delete(&body, "thinking.display");
        }
        if state.reporting {
            body = remove_reporting_block(body);
        }
        return body;
    }
    let model = rawjson::string(&body, "model").trim().to_lowercase();
    if state.fallbacks && !touched_fallbacks {
        let want = if cloak::is_fable51(&model) {
            "claude-opus-5"
        } else if cloak::is_opus55(&model) {
            "claude-opus-4-8"
        } else {
            ""
        };
        if rawjson::string(&body, "fallbacks.0.model") != want {
            body = rawjson::delete(&body, "fallbacks");
        }
    }
    let exists = |b: &str, path: &str| rawjson::get(b, path).exists();
    if cloak::is_fable51(&model) {
        if !exists(&body, "fallbacks") && !touched_fallbacks {
            body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-5"}]"#);
        }
        if exists(&body, "thinking") {
            let kind = rawjson::string(&body, "thinking.type");
            if kind == "adaptive" && !exists(&body, "thinking.display") && !touched_display {
                body = rawjson::set_str(&body, "thinking.display", "updates");
            } else if kind != "adaptive" && state.display && !touched_display {
                body = rawjson::delete(&body, "thinking.display");
            }
        } else if state.display && !touched_display {
            body = rawjson::delete(&body, "thinking.display");
        }
        if !has_reporting_block(&body) {
            let system = rawjson::get(&body, "system");
            let reporting = cloak::text_block(FABLE_REPORTING, None);
            let blocks = match system.kind() {
                gjson::Kind::Array => {
                    let mut raw: Vec<String> = system.array().iter().map(|b| b.json().to_owned()).collect();
                    raw.push(reporting);
                    Some(raw)
                }
                gjson::Kind::String => Some(vec![cloak::text_block(system.str(), None), reporting]),
                _ if !system.exists() => Some(vec![reporting]),
                _ => None,
            };
            if let Some(blocks) = blocks {
                body = rawjson::set_raw(&body, "system", &format!("[{}]", blocks.join(",")));
            }
        }
        return if touched_display {
            body
        } else {
            super::cloak_thinking_display(&body)
        };
    }
    if cloak::is_opus55(&model) && !exists(&body, "fallbacks") && !touched_fallbacks {
        body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-4-8"}]"#);
    }
    let kind = rawjson::string(&body, "thinking.type").trim().to_lowercase();
    let active = kind == "adaptive" || kind == "enabled";
    // Payload rules can disable thinking without touching display: only CPA's injected
    // value goes.
    if state.display && !touched_display && (!active || !super::betas::progress_display(&model)) {
        body = rawjson::delete(&body, "thinking.display");
    }
    if !touched_display {
        body = super::cloak_thinking_display(&body);
    }
    if state.reporting {
        body = remove_reporting_block(body);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID: &str = r#"{"model":"claude-opus-5","system":[{"type":"text","text":"rules"}],"messages":[{"role":"user","content":"hi"}]}"#;

    #[test]
    fn system_placement_moves_only_cpa_turns_to_reminders() {
        let after = cloak::insert_mid_system(MID, &["rules".to_owned()]);
        let placement = SystemPlacement::capture(MID, &after, true);
        assert_eq!(placement.inserted.len(), 1);
        // A rule that keeps a current model leaves the turn where it is.
        assert_eq!(placement.reconcile(&after), after);
        // A rule that selects a legacy model turns it into a reminder.
        let legacy = rawjson::set_str(&after, "model", "claude-3-5-haiku-20241022");
        let fixed = placement.reconcile(&legacy);
        assert_eq!(rawjson::get(&fixed, "messages.#").i64(), 1);
        assert!(rawjson::string(&fixed, "messages.0.content.0.text").starts_with("<system-reminder>"));
        // A rule that edited the inserted turn fails closed.
        let edited = rawjson::set_str(&legacy, "messages.1.content.0.text", "changed");
        assert_eq!(placement.reconcile(&edited), edited);
        // Turns the caller already had are not CPA's.
        assert!(SystemPlacement::capture(&after, &after, true).inserted.is_empty());
    }
}
