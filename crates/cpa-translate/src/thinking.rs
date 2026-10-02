//! Translator entry points into internal/thinking, served by `cpa_common::thinking`.
//!
//! ponytail: `cpa_common::thinking` takes `&str` bodies. Bodies that are not UTF-8 (Go
//! handles them as bytes) use the byte port below until that module moves onto
//! `cpa_common::json` (in progress, owner: Google thread); then delete the `local_*`
//! functions.

use cpa_common::json::{self as gj, Kind};
use cpa_common::thinking as ct;
use cpa_core::registry::ModelInfo;

pub use ct::{convert_level_to_budget, has_level, map_to_claude_effort};

/// registry.LookupModelInfo: the server's dynamic registry (preferring `provider`'s
/// registration), then every static catalog in Go's search order, by trimmed ID.
pub fn lookup_model_info(model: &str, provider: &str) -> Option<ModelInfo> {
    let provider = provider.trim().to_lowercase();
    cpa_core::registry::lookup_model(model, (!provider.is_empty()).then_some(provider.as_str()))
}

/// The summary intent read from the client body (ExtractTranslatedSummaryConfig).
pub enum Summary {
    Shared(ct::SummaryConfig),
    Local(LocalSummary),
}

pub fn extract_translated_summary(body: &[u8], source: &str, target: &str) -> Summary {
    match std::str::from_utf8(body) {
        Ok(text) => Summary::Shared(ct::extract_translated_summary_config(text, source, target)),
        Err(_) => Summary::Local(local_extract_translated_summary(body, source, target)),
    }
}

/// ApplySummaryConfigForModel.
pub fn apply_summary_for_model(body: Vec<u8>, format: &str, model: &str, config: Summary) -> Vec<u8> {
    match (config, std::str::from_utf8(&body)) {
        (Summary::Shared(config), Ok(text)) => {
            ct::apply_summary_config_for_model(text, format, model, config).into_bytes()
        }
        (Summary::Shared(config), Err(_)) => {
            local_apply_summary_for_model(body, format, model, LocalSummary::from(&config))
        }
        (Summary::Local(config), _) => local_apply_summary_for_model(body, format, model, config),
    }
}

/// ApplyTranslatedSummaryToClaude.
pub fn apply_translated_summary_to_claude(out: Vec<u8>, source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    match (std::str::from_utf8(&out), std::str::from_utf8(source)) {
        (Ok(o), Ok(src)) => ct::apply_translated_summary_to_claude(o, src, source_format, model).into_bytes(),
        _ => local_apply_translated_summary_to_claude(out, source, source_format, model),
    }
}

impl From<&ct::SummaryConfig> for LocalSummary {
    fn from(c: &ct::SummaryConfig) -> Self {
        match c.mode {
            ct::SummaryMode::Unspecified => LocalSummary::Unspecified,
            ct::SummaryMode::Disabled => LocalSummary::Disabled,
            ct::SummaryMode::Enabled => LocalSummary::Enabled(match c.detail.trim().to_lowercase().as_str() {
                "concise" => "concise",
                "detailed" => "detailed",
                _ => "auto",
            }),
        }
    }
}

/// thinking.ParseSuffix: `model(suffix)` splits at the last `(` when the name ends in `)`.
fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => (&model[..open], Some(&model[open + 1..model.len() - 1])),
        _ => (model, None),
    }
}

// ---------------------------------------------------------------------------------------
// Summary visibility (thinking/summary.go)

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalSummary {
    #[default]
    Unspecified,
    Disabled,
    /// Enabled with a detail level (`auto`, `concise`, `detailed`).
    Enabled(&'static str),
}

const ENABLED: LocalSummary = LocalSummary::Enabled("auto");

fn supported(format: &str) -> bool {
    matches!(
        format,
        "openai" | "openai-response" | "codex" | "claude" | "gemini" | "antigravity" | "interactions"
    )
}

fn bool_config(body: &[u8], path: &str) -> Option<LocalSummary> {
    match gj::get(body, path).kind {
        Kind::True => Some(ENABLED),
        Kind::False => Some(LocalSummary::Disabled),
        _ => None,
    }
}

fn first_bool_config(body: &[u8], paths: &[&str]) -> Option<LocalSummary> {
    paths.iter().find_map(|p| bool_config(body, p))
}

fn responses_config(body: &[u8], path: &str) -> Option<LocalSummary> {
    let value = gj::get(body, path);
    if value.raw.is_empty() {
        return None;
    }
    match value.kind {
        Kind::Null => Some(LocalSummary::Disabled),
        Kind::String => match value.str().trim().to_lowercase().as_str() {
            "auto" => Some(LocalSummary::Enabled("auto")),
            "concise" => Some(LocalSummary::Enabled("concise")),
            "detailed" => Some(LocalSummary::Enabled("detailed")),
            "none" => Some(LocalSummary::Disabled),
            _ => None,
        },
        _ => None,
    }
}

fn string_config(body: &[u8], path: &str, on: &str, off: &str) -> Option<LocalSummary> {
    let value = gj::get(body, path);
    if value.kind != Kind::String {
        return None;
    }
    let v = value.str().trim().to_lowercase();
    if v == on {
        Some(ENABLED)
    } else if v == off {
        Some(LocalSummary::Disabled)
    } else {
        None
    }
}

fn openai_explicit(body: &[u8]) -> Option<LocalSummary> {
    first_bool_config(
        body,
        &[
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
        ],
    )
    .or_else(|| responses_config(body, "reasoning.summary"))
    .or_else(|| responses_config(body, "reasoning.generate_summary"))
    .or_else(|| {
        bool_config(body, "reasoning.exclude").map(|s| if s == ENABLED { LocalSummary::Disabled } else { ENABLED })
    })
    .or_else(|| bool_config(body, "include_reasoning"))
    .or_else(|| bool_config(body, "reasoning.enabled"))
}

/// thinking.ExtractSummaryConfig.
fn extract_summary(body: &[u8], format: &str) -> LocalSummary {
    let format = format.trim().to_lowercase();
    if !supported(&format) || body.is_empty() || !gj::valid(body) {
        return LocalSummary::Unspecified;
    }
    let found = match format.as_str() {
        "openai" => openai_explicit(body).or_else(|| {
            let effort = gj::get(body, "reasoning_effort");
            if effort.kind != Kind::String {
                return None;
            }
            match effort.str().trim().to_lowercase().as_str() {
                "" => Some(LocalSummary::Unspecified),
                "none" => Some(LocalSummary::Disabled),
                _ => Some(ENABLED),
            }
        }),
        "openai-response" | "codex" => {
            responses_config(body, "reasoning.summary").or_else(|| responses_config(body, "reasoning.generate_summary"))
        }
        "claude" if claude_accepts_display(body) => string_config(body, "thinking.display", "summarized", "omitted"),
        "gemini" => first_bool_config(
            body,
            &[
                "generationConfig.thinkingConfig.includeThoughts",
                "generationConfig.thinkingConfig.include_thoughts",
                "generation_config.thinking_config.include_thoughts",
                "generation_config.thinking_config.includeThoughts",
            ],
        ),
        "antigravity" => first_bool_config(
            body,
            &[
                "request.generationConfig.thinkingConfig.includeThoughts",
                "request.generationConfig.thinkingConfig.include_thoughts",
                "request.generationConfig.thinking_config.includeThoughts",
                "request.generationConfig.thinking_config.include_thoughts",
            ],
        ),
        "interactions" => string_config(body, "generation_config.thinking_summaries", "auto", "none")
            .or_else(|| string_config(body, "generation_config.thinkingSummaries", "auto", "none"))
            .or_else(|| string_config(body, "reasoning.summary", "auto", "none"))
            .or_else(|| {
                first_bool_config(
                    body,
                    &[
                        "generation_config.thinking_config.include_thoughts",
                        "generation_config.thinking_config.includeThoughts",
                        "generation_config.thinkingConfig.include_thoughts",
                        "generation_config.thinkingConfig.includeThoughts",
                    ],
                )
            }),
        _ => None,
    };
    found.unwrap_or_default()
}

/// thinking.ExtractTranslatedSummaryConfig.
fn local_extract_translated_summary(body: &[u8], source: &str, target: &str) -> LocalSummary {
    let source = source.trim().to_lowercase();
    if target.trim().eq_ignore_ascii_case("claude") && source == "openai" {
        if body.is_empty() || !gj::valid(body) {
            return LocalSummary::Unspecified;
        }
        return openai_explicit(body).unwrap_or_default();
    }
    extract_summary(body, &source)
}

/// thinking.ApplyTranslatedSummaryToClaude.
fn local_apply_translated_summary_to_claude(out: Vec<u8>, source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    match local_extract_translated_summary(source, source_format, "claude") {
        LocalSummary::Unspecified => out,
        config => local_apply_summary_for_model(out, "claude", model, config),
    }
}

fn claude_accepts_display(body: &[u8]) -> bool {
    match gj::get(body, "thinking.type").str().trim().to_lowercase().as_str() {
        "adaptive" => true,
        "enabled" => {
            let budget = gj::get(body, "thinking.budget_tokens");
            if budget.kind != Kind::Number {
                return true;
            }
            let v = budget.int();
            v == -1 || v > 0
        }
        _ => false,
    }
}

/// thinking.ApplySummaryConfigForModel.
fn local_apply_summary_for_model(mut body: Vec<u8>, format: &str, model: &str, config: LocalSummary) -> Vec<u8> {
    let format = format.trim().to_lowercase();
    if config == LocalSummary::Unspecified || !supported(&format) || body.is_empty() || !gj::valid(&body) {
        return body;
    }
    let enabled = matches!(config, LocalSummary::Enabled(_));
    match format.as_str() {
        "openai" => {
            if gj::get(&body, "reasoning.exclude").is_bool() {
                gj::set_bool(&mut body, "reasoning.exclude", !enabled);
            }
            if gj::get(&body, "include_reasoning").is_bool() {
                gj::set_bool(&mut body, "include_reasoning", enabled);
            }
        }
        "claude" => {
            if enabled && !gj::get(&body, "thinking.type").exists() {
                body = enable_claude_thinking_for_summary(body, model);
            }
            if !claude_accepts_display(&body) {
                return body;
            }
            gj::set_str(
                &mut body,
                "thinking.display",
                if enabled { "summarized" } else { "omitted" },
            );
        }
        "gemini" | "antigravity" => {
            let prefix = if format == "antigravity" { "request." } else { "" };
            gj::set_bool(
                &mut body,
                &format!("{prefix}generationConfig.thinkingConfig.includeThoughts"),
                enabled,
            );
            let paths: &[&str] = if format == "antigravity" {
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
            for path in paths {
                gj::delete(&mut body, path);
            }
        }
        "interactions" => {
            gj::set_str(
                &mut body,
                "generation_config.thinking_summaries",
                if enabled { "auto" } else { "none" },
            );
            gj::delete(&mut body, "generation_config.thinkingSummaries");
        }
        _ => {
            if let LocalSummary::Enabled(detail) = config {
                gj::set_str(&mut body, "reasoning.summary", detail);
                gj::delete(&mut body, "reasoning.generate_summary");
            } else {
                gj::delete(&mut body, "reasoning.summary");
                gj::delete(&mut body, "reasoning.generate_summary");
                let reasoning = gj::get(&body, "reasoning");
                if reasoning.is_object() && reasoning.map().is_empty() {
                    gj::delete(&mut body, "reasoning");
                }
            }
        }
    }
    body
}

fn enable_claude_thinking_for_summary(mut body: Vec<u8>, model: &str) -> Vec<u8> {
    let mut base = parse_suffix(model).0.to_owned();
    if base.is_empty() {
        base = parse_suffix(&gj::get(&body, "model").str()).0.to_owned();
    }
    let Some(support) = lookup_model_info(&base, "claude").and_then(|m| m.thinking) else {
        return body;
    };
    if !support.levels.is_empty() {
        gj::set_str(&mut body, "thinking.type", "adaptive");
        gj::delete(&mut body, "thinking.budget_tokens");
        return body;
    }
    let budget = support.min;
    if budget <= 0 {
        return body;
    }
    let max_tokens = gj::get(&body, "max_tokens");
    if max_tokens.exists() && max_tokens.int() <= budget {
        return body;
    }
    gj::set_str(&mut body, "thinking.type", "enabled");
    gj::set_int(&mut body, "thinking.budget_tokens", budget);
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_come_from_every_static_catalog() {
        // kimi-k2.5 is not a Claude model; Go still finds its levels through the static
        // lookup, which is why Claude-bound translation uses adaptive thinking for it.
        let kimi = lookup_model_info(" kimi-k2.5 ", "claude").unwrap().thinking.unwrap();
        assert!(!kimi.levels.is_empty());
        assert!(!has_level(&kimi.levels, "max"));
        assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
        assert_eq!(map_to_claude_effort("xhigh", true), Some("max"));
        assert_eq!(map_to_claude_effort("auto", true), Some("high"));
        assert_eq!(map_to_claude_effort("bogus", true), None);
    }

    #[test]
    fn level_budgets_are_case_insensitive() {
        assert_eq!(convert_level_to_budget("MAX"), Some(128000));
        assert_eq!(convert_level_to_budget(" high"), None);
    }
}
