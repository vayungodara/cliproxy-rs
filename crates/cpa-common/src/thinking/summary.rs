//! Reasoning summary (visibility) intent, kept separate from effort (summary.go).

use gjson::Kind;

use super::{ModelCaps, lookup_model_info, parse_suffix};
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
pub fn extract_summary_config(body: &str, format: &str) -> SummaryConfig {
    let format = format.trim().to_lowercase();
    if !supported(&format) || body.is_empty() || !json::valid(body) {
        return SummaryConfig::default();
    }
    match format.as_str() {
        "openai" => {
            if let Some(config) = openai_explicit(body) {
                return config;
            }
            let effort = gjson::get(body, "reasoning_effort");
            if effort.kind() == Kind::String {
                return match effort.str().trim().to_lowercase().as_str() {
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
                let value = gjson::get(body, "thinking.display");
                if value.kind() == Kind::String {
                    match value.str().trim().to_lowercase().as_str() {
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
                let value = gjson::get(body, path);
                if value.kind() == Kind::String {
                    match value.str().trim().to_lowercase().as_str() {
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
pub fn extract_explicit_summary_config(body: &str, format: &str) -> SummaryConfig {
    let format = format.trim().to_lowercase();
    if format != "openai" {
        return extract_summary_config(body, &format);
    }
    if body.is_empty() || !json::valid(body) {
        return SummaryConfig::default();
    }
    openai_explicit(body).unwrap_or_default()
}

/// `ExtractTranslatedSummaryConfig`.
pub fn extract_translated_summary_config(body: &str, source_format: &str, target_format: &str) -> SummaryConfig {
    let source = source_format.trim().to_lowercase();
    if target_format.trim().eq_ignore_ascii_case("claude") && source == "openai" {
        return extract_explicit_summary_config(body, &source);
    }
    extract_summary_config(body, &source)
}

/// `ApplyTranslatedSummaryToClaude`.
pub fn apply_translated_summary_to_claude(out: &str, source: &str, source_format: &str, model: &str) -> String {
    let config = extract_translated_summary_config(source, source_format, "claude");
    if config.mode == SummaryMode::Unspecified {
        return out.to_owned();
    }
    apply_summary_config_for_model(out, "claude", model, config)
}

/// `ApplySummaryConfig`.
pub fn apply_summary_config(body: &str, format: &str, config: SummaryConfig) -> String {
    apply_summary_config_for_model(body, format, "", config)
}

/// `ApplySummaryConfigForModel`.
pub fn apply_summary_config_for_model(body: &str, format: &str, model: &str, config: SummaryConfig) -> String {
    apply_summary_config_for_provider(body, format, model, "", None, config)
}

pub(crate) fn apply_summary_config_for_provider(
    body: &str,
    format: &str,
    model: &str,
    provider: &str,
    info: Option<&ModelCaps>,
    config: SummaryConfig,
) -> String {
    let format = format.trim().to_lowercase();
    if config.mode == SummaryMode::Unspecified || !supported(&format) || body.is_empty() || !json::valid(body) {
        return body.to_owned();
    }
    let enabled = config.mode == SummaryMode::Enabled;
    let mut body = body.to_owned();
    match format.as_str() {
        "openai" => {
            if is_openrouter(provider) || json::is_bool(&gjson::get(&body, "reasoning.exclude")) {
                body = json::set_bool(&body, "reasoning.exclude", !enabled);
            }
            if json::is_bool(&gjson::get(&body, "include_reasoning")) {
                body = json::set_bool(&body, "include_reasoning", enabled);
            }
        }
        "claude" => {
            if enabled && !gjson::get(&body, "thinking.type").exists() {
                body = enable_claude_thinking_for_summary(&body, model, info);
            }
            if !claude_accepts_display(&body) {
                return body;
            }
            body = json::set_str(
                &body,
                "thinking.display",
                if enabled { "summarized" } else { "omitted" },
            );
        }
        "gemini" | "antigravity" => {
            let prefix = if format == "antigravity" { "request." } else { "" };
            body = json::set_bool(
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
                body = json::delete(&body, path);
            }
        }
        "interactions" => {
            body = json::set_str(
                &body,
                "generation_config.thinking_summaries",
                if enabled { "auto" } else { "none" },
            );
            body = json::delete(&body, "generation_config.thinkingSummaries");
        }
        "openai-response" | "codex" => {
            if enabled {
                body = json::set_str(&body, "reasoning.summary", normalized_detail(&config.detail));
                body = json::delete(&body, "reasoning.generate_summary");
            } else {
                body = json::delete(&body, "reasoning.summary");
                body = json::delete(&body, "reasoning.generate_summary");
                if json::is_empty_object(&body, "reasoning") {
                    body = json::delete(&body, "reasoning");
                }
            }
        }
        _ => {}
    }
    body
}

fn claude_accepts_display(body: &str) -> bool {
    match json::go_str(&gjson::get(body, "thinking.type"))
        .trim()
        .to_lowercase()
        .as_str()
    {
        "adaptive" => true,
        "enabled" => {
            let budget = gjson::get(body, "thinking.budget_tokens");
            if budget.kind() != Kind::Number {
                return true;
            }
            let value = json::go_int(&budget);
            value == -1 || value > 0
        }
        _ => false,
    }
}

fn is_openrouter(provider: &str) -> bool {
    let provider = provider.trim().to_lowercase();
    provider == "openrouter"
        || provider
            .split(['-', '_', '/', '.', ':'])
            .any(|part| part == "openrouter")
}

fn openai_explicit(body: &str) -> Option<SummaryConfig> {
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
        let value = gjson::get(body, path);
        if json::is_bool(&value) {
            let on = (value.kind() == Kind::True) != invert;
            return Some(if on {
                SummaryConfig::enabled("auto")
            } else {
                SummaryConfig::disabled()
            });
        }
    }
    None
}

fn first_bool(body: &str, paths: &[&str]) -> Option<SummaryConfig> {
    paths.iter().find_map(|path| bool_config(body, path))
}

fn bool_config(body: &str, path: &str) -> Option<SummaryConfig> {
    match gjson::get(body, path).kind() {
        Kind::True => Some(SummaryConfig::enabled("auto")),
        Kind::False => Some(SummaryConfig::disabled()),
        _ => None,
    }
}

fn responses_summary(body: &str, path: &str) -> Option<SummaryConfig> {
    let value = gjson::get(body, path);
    if !value.exists() {
        return None;
    }
    match value.kind() {
        Kind::Null => Some(SummaryConfig::disabled()),
        Kind::String => match value.str().trim().to_lowercase().as_str() {
            raw @ ("auto" | "concise" | "detailed") => Some(SummaryConfig::enabled(raw)),
            "none" => Some(SummaryConfig::disabled()),
            _ => None,
        },
        _ => None,
    }
}

/// Removes adaptive Claude thinking that only a summary-only request activated, when
/// the bound model supports manual extended thinking only.
pub(crate) fn strip_inferred_claude_summary_activation(body: &str, info: Option<&ModelCaps>) -> String {
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        return body.to_owned();
    };
    if !support.levels.is_empty() || support.min <= 0 {
        return body.to_owned();
    }
    if !json::go_str(&gjson::get(body, "thinking.type"))
        .trim()
        .eq_ignore_ascii_case("adaptive")
    {
        return body.to_owned();
    }
    let mut body = body.to_owned();
    for path in [
        "thinking.type",
        "thinking.budget_tokens",
        "thinking.display",
        "output_config.effort",
    ] {
        body = json::delete(&body, path);
    }
    for path in ["thinking", "output_config"] {
        if json::is_empty_object(&body, path) {
            body = json::delete(&body, path);
        }
    }
    body
}

fn enable_claude_thinking_for_summary(body: &str, model: &str, resolved: Option<&ModelCaps>) -> String {
    let looked_up;
    let info = match resolved {
        Some(info) => Some(info),
        None => {
            let mut base = parse_suffix(model).model_name;
            if base.is_empty() {
                base = parse_suffix(&json::go_str(&gjson::get(body, "model"))).model_name;
            }
            looked_up = lookup_model_info(&base, "claude");
            looked_up.as_ref()
        }
    };
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        return body.to_owned();
    };
    if !support.levels.is_empty() {
        let body = json::set_str(body, "thinking.type", "adaptive");
        return json::delete(&body, "thinking.budget_tokens");
    }
    let budget = support.min;
    if budget <= 0 {
        return body.to_owned();
    }
    let max_tokens = gjson::get(body, "max_tokens");
    if max_tokens.exists() && json::go_int(&max_tokens) <= budget {
        return body.to_owned();
    }
    let body = json::set_str(body, "thinking.type", "enabled");
    json::set_int(&body, "thinking.budget_tokens", budget)
}

fn normalized_detail(detail: &str) -> &'static str {
    match detail.trim().to_lowercase().as_str() {
        "concise" => "concise",
        "detailed" => "detailed",
        _ => "auto",
    }
}
