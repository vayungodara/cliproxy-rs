//! Reasoning summary (visibility) intent, kept separate from effort (summary.go).

use crate::gostr::GoStr;
use crate::json::Kind;

use super::{ModelCaps, lookup_model_info, parse_suffix};
use super::{is_empty_object, with_bool, with_int, with_str, without};
use crate::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SummaryMode {
    #[default]
    Unspecified,
    Disabled,
    Enabled,
}

/// `SummaryConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SummaryConfig {
    pub mode: SummaryMode,
    pub detail: String,
}

impl SummaryConfig {
    fn enabled(detail: &str) -> Self {
        Self {
            mode: SummaryMode::Enabled,
            detail: detail.into(),
        }
    }
    fn disabled() -> Self {
        Self {
            mode: SummaryMode::Disabled,
            detail: String::new(),
        }
    }
}

fn supported(format: &str) -> bool {
    matches!(
        format,
        "openai" | "openai-response" | "codex" | "claude" | "gemini" | "antigravity" | "interactions"
    )
}

/// `ExtractSummaryConfig`.
pub fn extract_summary_config(body: &[u8], format: &str) -> SummaryConfig {
    let format = format.trim().go_lower();
    if !supported(&format) || body.is_empty() || !json::valid(body) {
        return SummaryConfig::default();
    }
    match format.as_str() {
        "openai" => {
            if let Some(config) = openai_explicit(body) {
                return config;
            }
            let effort = json::get(body, "reasoning_effort");
            if effort.kind == Kind::String {
                return match effort.str().trim().go_lower().as_str() {
                    "" => SummaryConfig::default(),
                    "none" => SummaryConfig::disabled(),
                    _ => SummaryConfig::enabled("auto"),
                };
            }
        }
        "openai-response" | "codex" => {
            for path in ["reasoning.summary", "reasoning.generate_summary"] {
                if let Some(config) = responses_summary(body, path) {
                    return config;
                }
            }
        }
        "claude" => {
            if claude_accepts_display(body) {
                let value = json::get(body, "thinking.display");
                if value.kind == Kind::String {
                    match value.str().trim().go_lower().as_str() {
                        "summarized" => return SummaryConfig::enabled("auto"),
                        "omitted" => return SummaryConfig::disabled(),
                        _ => {}
                    }
                }
            }
        }
        "gemini" => {
            if let Some(config) = first_bool(
                body,
                &[
                    "generationConfig.thinkingConfig.includeThoughts",
                    "generationConfig.thinkingConfig.include_thoughts",
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                ],
            ) {
                return config;
            }
        }
        "antigravity" => {
            if let Some(config) = first_bool(
                body,
                &[
                    "request.generationConfig.thinkingConfig.includeThoughts",
                    "request.generationConfig.thinkingConfig.include_thoughts",
                    "request.generationConfig.thinking_config.includeThoughts",
                    "request.generationConfig.thinking_config.include_thoughts",
                ],
            ) {
                return config;
            }
        }
        "interactions" => {
            for path in [
                "generation_config.thinking_summaries",
                "generation_config.thinkingSummaries",
                "reasoning.summary",
            ] {
                let value = json::get(body, path);
                if value.kind == Kind::String {
                    match value.str().trim().go_lower().as_str() {
                        "auto" => return SummaryConfig::enabled("auto"),
                        "none" => return SummaryConfig::disabled(),
                        _ => {}
                    }
                }
            }
            if let Some(config) = first_bool(
                body,
                &[
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                    "generation_config.thinkingConfig.include_thoughts",
                    "generation_config.thinkingConfig.includeThoughts",
                ],
            ) {
                return config;
            }
        }
        _ => {}
    }
    SummaryConfig::default()
}

/// `ExtractExplicitSummaryConfig`: like [`extract_summary_config`] but OpenAI chat
/// `reasoning_effort` alone does not imply a summary.
pub fn extract_explicit_summary_config(body: &[u8], format: &str) -> SummaryConfig {
    let format = format.trim().go_lower();
    if format != "openai" {
        return extract_summary_config(body, &format);
    }
    if body.is_empty() || !json::valid(body) {
        return SummaryConfig::default();
    }
    openai_explicit(body).unwrap_or_default()
}

/// `ExtractTranslatedSummaryConfig`.
pub fn extract_translated_summary_config(body: &[u8], source_format: &str, target_format: &str) -> SummaryConfig {
    let source = source_format.trim().go_lower();
    if target_format.trim().go_lower() == "claude" && source == "openai" {
        return extract_explicit_summary_config(body, &source);
    }
    extract_summary_config(body, &source)
}

/// `ApplyTranslatedSummaryToClaude`.
pub fn apply_translated_summary_to_claude(out: &[u8], source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    let config = extract_translated_summary_config(source, source_format, "claude");
    if config.mode == SummaryMode::Unspecified {
        return out.to_vec();
    }
    apply_summary_config_for_model(out, "claude", model, config)
}

/// `ApplySummaryConfig`.
pub fn apply_summary_config(body: &[u8], format: &str, config: SummaryConfig) -> Vec<u8> {
    apply_summary_config_for_model(body, format, "", config)
}

/// `ApplySummaryConfigForModel`.
pub fn apply_summary_config_for_model(body: &[u8], format: &str, model: &str, config: SummaryConfig) -> Vec<u8> {
    apply_summary_config_for_provider(body, format, model, "", None, config)
}

pub(crate) fn apply_summary_config_for_provider(
    body: &[u8],
    format: &str,
    model: &str,
    provider: &str,
    info: Option<&ModelCaps>,
    config: SummaryConfig,
) -> Vec<u8> {
    let format = format.trim().go_lower();
    if config.mode == SummaryMode::Unspecified || !supported(&format) || body.is_empty() || !json::valid(body) {
        return body.to_vec();
    }
    let enabled = config.mode == SummaryMode::Enabled;
    let mut body = body.to_vec();
    match format.as_str() {
        "openai" => {
            if is_openrouter(provider) || json::get(&body, "reasoning.exclude").is_bool() {
                body = with_bool(&body, "reasoning.exclude", !enabled);
            }
            if json::get(&body, "include_reasoning").is_bool() {
                body = with_bool(&body, "include_reasoning", enabled);
            }
        }
        "claude" => {
            if enabled && !json::get(&body, "thinking.type").exists() {
                body = enable_claude_thinking_for_summary(&body, model, info);
            }
            if !claude_accepts_display(&body) {
                return body;
            }
            body = with_str(
                &body,
                "thinking.display",
                if enabled { "summarized" } else { "omitted" },
            );
        }
        "gemini" | "antigravity" => {
            let prefix = if format == "antigravity" { "request." } else { "" };
            body = with_bool(
                &body,
                &format!("{prefix}generationConfig.thinkingConfig.includeThoughts"),
                enabled,
            );
            let aliases: &[&str] = if format == "antigravity" {
                &[
                    "request.generationConfig.thinkingConfig.include_thoughts",
                    "request.generationConfig.thinking_config.include_thoughts",
                    "request.generationConfig.thinking_config.includeThoughts",
                ]
            } else {
                &[
                    "generationConfig.thinkingConfig.include_thoughts",
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                ]
            };
            for path in aliases {
                body = without(&body, path);
            }
        }
        "interactions" => {
            body = with_str(
                &body,
                "generation_config.thinking_summaries",
                if enabled { "auto" } else { "none" },
            );
            body = without(&body, "generation_config.thinkingSummaries");
        }
        "openai-response" | "codex" => {
            if enabled {
                body = with_str(&body, "reasoning.summary", normalized_detail(&config.detail));
                body = without(&body, "reasoning.generate_summary");
            } else {
                body = without(&body, "reasoning.summary");
                body = without(&body, "reasoning.generate_summary");
                if is_empty_object(&body, "reasoning") {
                    body = without(&body, "reasoning");
                }
            }
        }
        _ => {}
    }
    body
}

fn claude_accepts_display(body: &[u8]) -> bool {
    match json::get(body, "thinking.type").str().trim().go_lower().as_str() {
        "adaptive" => true,
        "enabled" => {
            let budget = json::get(body, "thinking.budget_tokens");
            if budget.kind != Kind::Number {
                return true;
            }
            let value = budget.int();
            value == -1 || value > 0
        }
        _ => false,
    }
}

fn is_openrouter(provider: &str) -> bool {
    let provider = provider.trim().go_lower();
    provider == "openrouter"
        || provider
            .split(['-', '_', '/', '.', ':'])
            .any(|part| part == "openrouter")
}

fn openai_explicit(body: &[u8]) -> Option<SummaryConfig> {
    for path in [
        "extra_body.google.thinking_config.include_thoughts",
        "extra_body.google.thinking_config.includeThoughts",
        "extra_body.google.thinkingConfig.include_thoughts",
        "extra_body.google.thinkingConfig.includeThoughts",
        "extra_body.extra_body.google.thinking_config.include_thoughts",
        "extra_body.extra_body.google.thinking_config.includeThoughts",
        "google.thinking_config.include_thoughts",
        "google.thinking_config.includeThoughts",
        "thinking.includeThoughts",
        "thinking.include_thoughts",
        "reasoning.includeThoughts",
        "reasoning.include_thoughts",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ] {
        if let Some(config) = bool_config(body, path) {
            return Some(config);
        }
    }
    for path in ["reasoning.summary", "reasoning.generate_summary"] {
        if let Some(config) = responses_summary(body, path) {
            return Some(config);
        }
    }
    for (path, invert) in [
        ("reasoning.exclude", true),
        ("include_reasoning", false),
        ("reasoning.enabled", false),
    ] {
        let value = json::get(body, path);
        if value.is_bool() {
            let on = (value.kind == Kind::True) != invert;
            return Some(if on {
                SummaryConfig::enabled("auto")
            } else {
                SummaryConfig::disabled()
            });
        }
    }
    None
}

fn first_bool(body: &[u8], paths: &[&str]) -> Option<SummaryConfig> {
    paths.iter().find_map(|path| bool_config(body, path))
}

fn bool_config(body: &[u8], path: &str) -> Option<SummaryConfig> {
    match json::get(body, path).kind {
        Kind::True => Some(SummaryConfig::enabled("auto")),
        Kind::False => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

fn responses_summary(body: &[u8], path: &str) -> Option<SummaryConfig> {
    let value = json::get(body, path);
    if !value.exists() {
        return None;
    }
    match value.kind {
        Kind::Null => Some(SummaryConfig::disabled()),
        Kind::String => match value.str().trim().go_lower().as_str() {
            raw @ ("auto" | "concise" | "detailed") => Some(SummaryConfig::enabled(raw)),
            "none" => Some(SummaryConfig::disabled()),
            _ => None,
        },
        _ => None,
    }
}

/// Removes adaptive Claude thinking that only a summary-only request activated, when
/// the bound model supports manual extended thinking only.
pub(crate) fn strip_inferred_claude_summary_activation(body: &[u8], info: Option<&ModelCaps>) -> Vec<u8> {
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        return body.to_vec();
    };
    if !support.levels.is_empty() || support.min <= 0 {
        return body.to_vec();
    }
    if !json::get(body, "thinking.type").str().trim().go_eq_fold("adaptive") {
        return body.to_vec();
    }
    let mut body = body.to_vec();
    for path in [
        "thinking.type",
        "thinking.budget_tokens",
        "thinking.display",
        "output_config.effort",
    ] {
        body = without(&body, path);
    }
    for path in ["thinking", "output_config"] {
        if is_empty_object(&body, path) {
            body = without(&body, path);
        }
    }
    body
}

fn enable_claude_thinking_for_summary(body: &[u8], model: &str, resolved: Option<&ModelCaps>) -> Vec<u8> {
    let looked_up;
    let info = match resolved {
        Some(info) => Some(info),
        None => {
            let mut base = parse_suffix(model).model_name;
            if base.is_empty() {
                base = parse_suffix(&json::get(body, "model").str()).model_name;
            }
            looked_up = lookup_model_info(&base, "claude");
            looked_up.as_ref()
        }
    };
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        return body.to_vec();
    };
    if !support.levels.is_empty() {
        let body = with_str(body, "thinking.type", "adaptive");
        return without(&body, "thinking.budget_tokens");
    }
    let budget = support.min;
    if budget <= 0 {
        return body.to_vec();
    }
    let max_tokens = json::get(body, "max_tokens");
    if max_tokens.exists() && max_tokens.int() <= budget {
        return body.to_vec();
    }
    let body = with_str(body, "thinking.type", "enabled");
    with_int(&body, "thinking.budget_tokens", budget)
}

fn normalized_detail(detail: &str) -> &'static str {
    match detail.trim().go_lower().as_str() {
        "concise" => "concise",
        "detailed" => "detailed",
        _ => "auto",
    }
}
