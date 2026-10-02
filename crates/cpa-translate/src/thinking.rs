//! Adapters for internal/thinking and the model registry, as called by translators.
//!
//! ponytail: adapter. internal/thinking belongs to cpa_common::thinking (owner: Google
//! thread). This module keeps only the functions translators call, ported from Go so the
//! goldens pass; at integration each function body becomes a call into
//! cpa_common::thinking with the same signature, and the tests here keep running.

use cpa_common::json::{self as gj, Kind};
use cpa_core::registry::ModelInfo;

/// registry.LookupModelInfo: the server's dynamic registry (preferring `provider`'s
/// registration), then every static catalog in Go's search order, by trimmed ID.
pub fn lookup_model_info(model: &str, provider: &str) -> Option<ModelInfo> {
    let provider = provider.trim().to_lowercase();
    cpa_core::registry::lookup_model(model, (!provider.is_empty()).then_some(provider.as_str()))
}

/// thinking.ConvertLevelToBudget.
pub fn convert_level_to_budget(level: &str) -> Option<i64> {
    Some(match level.to_lowercase().as_str() {
        "none" => 0,
        "auto" => -1,
        "minimal" => 512,
        "low" => 1024,
        "medium" => 8192,
        "high" => 24576,
        "xhigh" => 32768,
        "max" => 128000,
        _ => return None,
    })
}

/// thinking.HasLevel.
pub fn has_level(levels: &[String], target: &str) -> bool {
    levels.iter().any(|l| l.trim().eq_ignore_ascii_case(target))
}

/// thinking.MapToClaudeEffort.
pub fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    Some(match level.trim().to_lowercase().as_str() {
        "minimal" => "low",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "max" if supports_max => "max",
        "xhigh" | "max" | "auto" => "high",
        _ => return None,
    })
}

/// thinking.ParseSuffix: `model(suffix)` splits at the last `(` when the name ends in `)`.
pub fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => (&model[..open], Some(&model[open + 1..model.len() - 1])),
        _ => (model, None),
    }
}

// ---------------------------------------------------------------------------------------
// Summary visibility (thinking/summary.go)

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Summary {
    #[default]
    Unspecified,
    Disabled,
    /// Enabled with a detail level (`auto`, `concise`, `detailed`).
    Enabled(&'static str),
}

const ENABLED: Summary = Summary::Enabled("auto");

fn supported(format: &str) -> bool {
    matches!(
        format,
        "openai" | "openai-response" | "codex" | "claude" | "gemini" | "antigravity" | "interactions"
    )
}

fn bool_config(body: &[u8], path: &str) -> Option<Summary> {
    match gj::get(body, path).kind {
        Kind::True => Some(ENABLED),
        Kind::False => Some(Summary::Disabled),
        _ => None,
    }
}

fn first_bool_config(body: &[u8], paths: &[&str]) -> Option<Summary> {
    paths.iter().find_map(|p| bool_config(body, p))
}

fn responses_config(body: &[u8], path: &str) -> Option<Summary> {
    let value = gj::get(body, path);
    if value.raw.is_empty() {
        return None;
    }
    match value.kind {
        Kind::Null => Some(Summary::Disabled),
        Kind::String => match value.str().trim().to_lowercase().as_str() {
            "auto" => Some(Summary::Enabled("auto")),
            "concise" => Some(Summary::Enabled("concise")),
            "detailed" => Some(Summary::Enabled("detailed")),
            "none" => Some(Summary::Disabled),
            _ => None,
        },
        _ => None,
    }
}

fn string_config(body: &[u8], path: &str, on: &str, off: &str) -> Option<Summary> {
    let value = gj::get(body, path);
    if value.kind != Kind::String {
        return None;
    }
    let v = value.str().trim().to_lowercase();
    if v == on {
        Some(ENABLED)
    } else if v == off {
        Some(Summary::Disabled)
    } else {
        None
    }
}

fn openai_explicit(body: &[u8]) -> Option<Summary> {
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
    .or_else(|| bool_config(body, "reasoning.exclude").map(|s| if s == ENABLED { Summary::Disabled } else { ENABLED }))
    .or_else(|| bool_config(body, "include_reasoning"))
    .or_else(|| bool_config(body, "reasoning.enabled"))
}

/// thinking.ExtractSummaryConfig.
pub fn extract_summary(body: &[u8], format: &str) -> Summary {
    let format = format.trim().to_lowercase();
    if !supported(&format) || body.is_empty() || !gj::valid(body) {
        return Summary::Unspecified;
    }
    let found = match format.as_str() {
        "openai" => openai_explicit(body).or_else(|| {
            let effort = gj::get(body, "reasoning_effort");
            if effort.kind != Kind::String {
                return None;
            }
            match effort.str().trim().to_lowercase().as_str() {
                "" => Some(Summary::Unspecified),
                "none" => Some(Summary::Disabled),
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
pub fn extract_translated_summary(body: &[u8], source: &str, target: &str) -> Summary {
    let source = source.trim().to_lowercase();
    if target.trim().eq_ignore_ascii_case("claude") && source == "openai" {
        if body.is_empty() || !gj::valid(body) {
            return Summary::Unspecified;
        }
        return openai_explicit(body).unwrap_or_default();
    }
    extract_summary(body, &source)
}

/// thinking.ApplyTranslatedSummaryToClaude.
pub fn apply_translated_summary_to_claude(out: Vec<u8>, source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    match extract_translated_summary(source, source_format, "claude") {
        Summary::Unspecified => out,
        config => apply_summary_for_model(out, "claude", model, config),
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
pub fn apply_summary_for_model(mut body: Vec<u8>, format: &str, model: &str, config: Summary) -> Vec<u8> {
    let format = format.trim().to_lowercase();
    if config == Summary::Unspecified || !supported(&format) || body.is_empty() || !gj::valid(&body) {
        return body;
    }
    let enabled = matches!(config, Summary::Enabled(_));
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
            if let Summary::Enabled(detail) = config {
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
