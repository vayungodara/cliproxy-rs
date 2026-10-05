//! Per-provider appliers (internal/thinking/provider/*): write a validated [`Config`]
//! into the target body's own fields.

use super::{
    Config, Error, LEVEL_AUTO, LEVEL_HIGH, LEVEL_MAX, LEVEL_NONE, LEVEL_XHIGH, Mode, ModelCaps,
    convert_budget_to_level, has_level, is_user_defined_model,
};
use super::{is_empty_object, with_bool, with_int, with_str, without};
use crate::gostr::GoStr;
use crate::json;

pub(crate) type ApplyFn = fn(&[u8], &Config, Option<&ModelCaps>) -> Result<Vec<u8>, Error>;

/// `GetProviderApplier` for the built-in providers.
pub(crate) fn applier(provider: &str) -> Option<ApplyFn> {
    Some(match provider.trim().go_lower().as_str() {
        "gemini" => gemini,
        "antigravity" => antigravity,
        "interactions" => interactions,
        "claude" => claude,
        "openai" => openai,
        "codex" | "xai" => codex,
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => kimi,
        _ => return None,
    })
}

/// Public entry for callers that need one applier directly (Go `GetProviderApplier`).
pub fn apply_provider(
    provider: &str,
    body: &[u8],
    config: &Config,
    info: Option<&ModelCaps>,
) -> Option<Result<Vec<u8>, Error>> {
    applier(provider).map(|f| f(body, config, info))
}

fn object_or_empty(body: &[u8]) -> Vec<u8> {
    if body.is_empty() || !json::valid(body) {
        b"{}".to_vec()
    } else {
        body.to_vec()
    }
}

fn delete_all(body: &[u8], paths: &[&str]) -> Vec<u8> {
    paths.iter().fold(body.to_vec(), |acc, path| without(&acc, path))
}

fn delete_if_empty_object(body: Vec<u8>, path: &str) -> Vec<u8> {
    if is_empty_object(&body, path) {
        without(&body, path)
    } else {
        body
    }
}

// ---- Gemini and Antigravity share one shape under different prefixes. ----

/// `Apply` of the gemini/antigravity appliers. `prefix` is the thinkingConfig path.
fn gemini_like(
    body: &[u8],
    config: &Config,
    info: Option<&ModelCaps>,
    prefix: &str,
    antigravity: bool,
) -> Result<Vec<u8>, Error> {
    if is_user_defined_model(info) {
        let body = object_or_empty(body);
        let claude = antigravity && info.is_some_and(|i| i.id.go_lower().contains("claude"));
        if config.mode == Mode::Auto {
            return Ok(gemini_budget(&body, config, info, prefix, claude));
        }
        if config.mode == Mode::Level || (config.mode == Mode::None && !config.level.is_empty()) {
            return Ok(gemini_level(&body, config, prefix));
        }
        return Ok(gemini_budget(&body, config, info, prefix, claude));
    }
    let info_ref = info.expect("user-defined handles None");
    let Some(support) = info_ref.thinking.as_ref() else {
        return Ok(body.to_vec());
    };
    let body = object_or_empty(body);
    if antigravity {
        let claude = info_ref.id.go_lower().contains("claude");
        if matches!(config.mode, Mode::Auto | Mode::Budget) {
            return Ok(gemini_budget(&body, config, info, prefix, claude));
        }
        if !support.levels.is_empty() {
            return Ok(gemini_level(&body, config, prefix));
        }
        return Ok(gemini_budget(&body, config, info, prefix, claude));
    }
    Ok(match config.mode {
        Mode::Level => gemini_level(&body, config, prefix),
        Mode::None if !support.levels.is_empty() => gemini_level(&body, config, prefix),
        _ => gemini_budget(&body, config, info, prefix, false),
    })
}

fn gemini(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    gemini_like(body, config, info, "generationConfig.thinkingConfig", false)
}

fn antigravity(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    gemini_like(body, config, info, "request.generationConfig.thinkingConfig", true)
}

fn gemini_level(body: &[u8], config: &Config, prefix: &str) -> Vec<u8> {
    let p = |k: &str| format!("{prefix}.{k}");
    let mut result = delete_all(
        body,
        &[
            &p("thinkingBudget"),
            &p("thinking_budget"),
            &p("thinking_level"),
            &p("includeThoughts"),
            &p("include_thoughts"),
        ],
    );
    if config.mode == Mode::None {
        if config.budget == 0 && config.level.is_empty() {
            return without(&result, prefix);
        }
        if !config.level.is_empty() {
            result = with_str(&result, &p("thinkingLevel"), &config.level);
        }
        return include_thoughts(result, body, prefix);
    }
    if config.mode != Mode::Level {
        return body.to_vec();
    }
    result = with_str(&result, &p("thinkingLevel"), &config.level);
    include_thoughts(result, body, prefix)
}

fn gemini_budget(body: &[u8], config: &Config, info: Option<&ModelCaps>, prefix: &str, claude: bool) -> Vec<u8> {
    let p = |k: &str| format!("{prefix}.{k}");
    let mut result = delete_all(
        body,
        &[
            &p("thinkingLevel"),
            &p("thinking_level"),
            &p("thinking_budget"),
            &p("includeThoughts"),
            &p("include_thoughts"),
        ],
    );
    let mut budget = config.budget;
    if claude && let Some(info) = info {
        // Antigravity Claude: keep the budget below max output tokens and drop
        // thinking entirely below the model minimum.
        let (effective_max, from_model) = match json::get(&result, "request.generationConfig.maxOutputTokens") {
            v if v.exists() && v.int() > 0 => (v.int(), false),
            _ if info.max_completion_tokens > 0 => (info.max_completion_tokens, true),
            _ => (0, false),
        };
        if effective_max > 0 && budget >= effective_max {
            budget = effective_max - 1;
        }
        let min = info.thinking.as_ref().map_or(0, |t| t.min);
        if min > 0 && budget >= 0 && budget < min {
            result = without(&result, prefix);
            budget = -2;
        } else if from_model && effective_max > 0 {
            result = with_int(&result, "request.generationConfig.maxOutputTokens", effective_max);
        }
        // Go signals "drop thinking" with -2, so a caller budget of -2 also skips the field.
        if budget == -2 {
            return include_thoughts(result, body, prefix);
        }
    }
    result = with_int(&result, &p("thinkingBudget"), budget);
    include_thoughts(result, body, prefix)
}

/// Carries the original `includeThoughts`/`include_thoughts` boolean, canonically.
fn include_thoughts(result: Vec<u8>, original: &[u8], prefix: &str) -> Vec<u8> {
    for key in ["includeThoughts", "include_thoughts"] {
        let path = format!("{prefix}.{key}");
        match json::get(original, &path).kind {
            json::Kind::True => return with_bool(&result, &format!("{prefix}.includeThoughts"), true),
            json::Kind::False => return with_bool(&result, &format!("{prefix}.includeThoughts"), false),
            _ => {}
        }
    }
    result
}

// ---- Interactions ----

fn interactions(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    let body = object_or_empty(body);
    let result = delete_all(
        &body,
        &[
            "generation_config.thinking_level",
            "generation_config.thinkingLevel",
            "generation_config.thinking_budget",
            "generation_config.thinkingBudget",
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
            "generation_config.thinking_config",
            "generation_config.thinkingConfig",
            "generationConfig.thinkingLevel",
            "generationConfig.thinking_level",
            "generationConfig.thinkingBudget",
            "generationConfig.thinking_budget",
            "generationConfig.thinkingSummaries",
            "generationConfig.thinking_summaries",
            "generationConfig.thinkingConfig",
        ],
    );
    Ok(match config.mode {
        Mode::Level => interactions_level(result, &body, &config.level, info),
        Mode::Budget => interactions_budget(result, &body, config.budget, info),
        Mode::Auto => interactions_summaries(result, &body),
        Mode::None if !config.level.is_empty() => interactions_level(result, &body, &config.level, info),
        Mode::None if config.budget > 0 => interactions_budget(result, &body, config.budget, info),
        Mode::None => result,
    })
}

fn interactions_budget(result: Vec<u8>, original: &[u8], budget: i64, info: Option<&ModelCaps>) -> Vec<u8> {
    match convert_budget_to_level(budget) {
        None | Some(LEVEL_NONE) | Some(LEVEL_AUTO) => interactions_summaries(result, original),
        Some(level) => interactions_level(result, original, level, info),
    }
}

fn interactions_level(mut result: Vec<u8>, original: &[u8], level: &str, info: Option<&ModelCaps>) -> Vec<u8> {
    let level = normalize_interactions_level(level, info);
    if !level.is_empty() {
        result = with_str(&result, "generation_config.thinking_level", &level);
    }
    interactions_summaries(result, original)
}

fn interactions_summaries(result: Vec<u8>, original: &[u8]) -> Vec<u8> {
    for path in [
        "generation_config.thinking_summaries",
        "generation_config.thinkingSummaries",
    ] {
        let v = json::get(original, path);
        if v.kind == json::Kind::String {
            let normalized = v.str().trim().go_lower();
            if normalized == "auto" || normalized == "none" {
                return with_str(&result, "generation_config.thinking_summaries", &normalized);
            }
        }
    }
    for path in [
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
        "generation_config.thinkingConfig.include_thoughts",
        "generation_config.thinkingConfig.includeThoughts",
    ] {
        let value = match json::get(original, path).kind {
            json::Kind::True => "auto",
            json::Kind::False => "none",
            _ => continue,
        };
        return with_str(&result, "generation_config.thinking_summaries", value);
    }
    result
}

fn normalize_interactions_level(level: &str, info: Option<&ModelCaps>) -> String {
    let level = level.trim().go_lower();
    if level.is_empty() || level == LEVEL_NONE || level == LEVEL_AUTO {
        return String::new();
    }
    if let Some(levels) = info
        .and_then(|i| i.thinking.as_ref())
        .map(|t| &t.levels)
        .filter(|l| !l.is_empty())
    {
        return levels
            .iter()
            .find(|c| c.go_eq_fold(&level))
            .unwrap_or(&levels[levels.len() - 1])
            .go_lower();
    }
    match level.as_str() {
        LEVEL_MAX | LEVEL_XHIGH => LEVEL_HIGH.into(),
        _ => level,
    }
}

// ---- Claude ----

fn claude_disable(body: &[u8], drop_display: bool) -> Vec<u8> {
    let mut result = with_str(body, "thinking.type", "disabled");
    result = without(&result, "thinking.budget_tokens");
    if drop_display {
        result = without(&result, "thinking.display");
    }
    result = without(&result, "output_config.effort");
    delete_if_empty_object(result, "output_config")
}

fn claude_adaptive(body: &[u8], effort: Option<&str>) -> Vec<u8> {
    let mut result = with_str(body, "thinking.type", "adaptive");
    result = without(&result, "thinking.budget_tokens");
    match effort {
        Some(effort) => with_str(&result, "output_config.effort", effort),
        None => delete_if_empty_object(without(&result, "output_config.effort"), "output_config"),
    }
}

fn claude_enabled(body: &[u8], budget: Option<i64>) -> Vec<u8> {
    let mut result = with_str(body, "thinking.type", "enabled");
    result = match budget {
        Some(budget) => with_int(&result, "thinking.budget_tokens", budget),
        None => without(&result, "thinking.budget_tokens"),
    };
    result = without(&result, "output_config.effort");
    delete_if_empty_object(result, "output_config")
}

/// Claude models that answer 400 to `thinking: {"type": "disabled"}` because thinking
/// is always on (Opus 5.5, Fable 5 and 5.1, Mythos) or is turned down with
/// `between_tools` instead (Sonnet 5.5). Source: Anthropic's "Troubleshooting thinking"
/// table, checked 2026-10-05. Go sends `disabled` to them and gets the 400.
fn claude_rejects_disabled(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    [
        "claude-opus-5-5",
        "claude-sonnet-5-5",
        "claude-fable-5",
        "claude-mythos-5",
        "claude-mythos-preview",
    ]
    .iter()
    .any(|prefix| model.starts_with(prefix))
}

/// Thinking off for `model` (the registry ID, or the body's `model` for an alias):
/// `disabled`, or the lowest effort on a model that refuses `disabled`
/// (docs/DIFFERENCES-FROM-GO.md).
// ponytail: a fixed model list; move it into the model registry as a capability if
// Anthropic keeps adding always-on models.
fn claude_off(body: &[u8], model: &str, drop_display: bool) -> Vec<u8> {
    if claude_rejects_disabled(model) || claude_rejects_disabled(&json::get(body, "model").str()) {
        return claude_adaptive(body, Some("low"));
    }
    claude_disable(body, drop_display)
}

fn claude(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    let model = info.map_or("", |i| i.id.as_str());
    if is_user_defined_model(info) {
        let body = object_or_empty(body);
        return Ok(match config.mode {
            Mode::None => claude_off(&body, model, true),
            Mode::Auto => claude_enabled(&body, None),
            Mode::Level if config.level.is_empty() => body,
            Mode::Level => claude_adaptive(&body, Some(&config.level)),
            Mode::Budget => claude_enabled(&body, Some(config.budget)),
        });
    }
    let info = info.expect("user-defined handles None");
    let Some(support) = info.thinking.as_ref() else {
        return Ok(body.to_vec());
    };
    let body = object_or_empty(body);
    let adaptive = !support.levels.is_empty();
    let budget = match config.mode {
        Mode::None => return Ok(claude_off(&body, model, true)),
        Mode::Level if adaptive && !config.level.is_empty() => return Ok(claude_adaptive(&body, Some(&config.level))),
        Mode::Level => match super::convert_level_to_budget(&config.level) {
            Some(budget) => budget,
            None => return Ok(body),
        },
        Mode::Budget => config.budget,
        Mode::Auto if adaptive => return Ok(claude_adaptive(&body, None)),
        Mode::Auto => return Ok(claude_enabled(&body, None)),
    };
    if budget == 0 {
        return Ok(claude_off(&body, model, false));
    }
    let result = claude_enabled(&body, Some(budget));
    Ok(normalize_claude_budget(result, budget, info))
}

/// Keeps `budget_tokens` below `max_tokens`, filling `max_tokens` from the model.
fn normalize_claude_budget(mut body: Vec<u8>, budget: i64, info: &ModelCaps) -> Vec<u8> {
    if budget <= 0 {
        return body;
    }
    let max_tokens = json::get(&body, "max_tokens");
    let (effective_max, from_model) = if max_tokens.exists() && max_tokens.int() > 0 {
        (max_tokens.int(), false)
    } else if info.max_completion_tokens > 0 {
        (info.max_completion_tokens, true)
    } else {
        (0, false)
    };
    if from_model && effective_max > 0 {
        body = with_int(&body, "max_tokens", effective_max);
    }
    let mut adjusted = budget;
    if effective_max > 0 && adjusted >= effective_max {
        adjusted = effective_max - 1;
    }
    let min = info.thinking.as_ref().map_or(0, |t| t.min);
    if min > 0 && adjusted > 0 && adjusted < min {
        return body;
    }
    if adjusted != budget {
        body = with_int(&body, "thinking.budget_tokens", adjusted);
    }
    body
}

// ---- OpenAI chat and Codex/xAI Responses share one effort rule. ----

fn effort_applier(body: &[u8], config: &Config, info: Option<&ModelCaps>, path: &str) -> Result<Vec<u8>, Error> {
    if is_user_defined_model(info) {
        let body = object_or_empty(body);
        let effort = match config.mode {
            Mode::Level if config.level.is_empty() => return Ok(body),
            Mode::Level => config.level.clone(),
            Mode::None if !config.level.is_empty() => config.level.clone(),
            Mode::None => LEVEL_NONE.into(),
            Mode::Auto => LEVEL_AUTO.into(),
            Mode::Budget => match convert_budget_to_level(config.budget) {
                Some(level) => level.into(),
                None => return Ok(body),
            },
        };
        return Ok(with_str(&body, path, &effort));
    }
    let info = info.expect("user-defined handles None");
    let Some(support) = info.thinking.as_ref() else {
        return Ok(body.to_vec());
    };
    if config.mode != Mode::Level && config.mode != Mode::None {
        return Ok(body.to_vec());
    }
    let body = object_or_empty(body);
    if config.mode == Mode::Level {
        return Ok(with_str(&body, path, &config.level));
    }
    let mut effort = String::new();
    if config.budget == 0 && (support.zero_allowed || has_level(&support.levels, LEVEL_NONE)) {
        effort = LEVEL_NONE.into();
    }
    if effort.is_empty() && !config.level.is_empty() {
        effort.clone_from(&config.level);
    }
    if effort.is_empty()
        && let Some(first) = support.levels.first()
    {
        effort.clone_from(first);
    }
    if effort.is_empty() {
        return Ok(body);
    }
    Ok(with_str(&body, path, &effort))
}

fn openai(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    effort_applier(body, config, info, "reasoning_effort")
}

fn codex(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    effort_applier(body, config, info, "reasoning.effort")
}

// ---- Kimi ----

fn kimi(body: &[u8], config: &Config, info: Option<&ModelCaps>) -> Result<Vec<u8>, Error> {
    let user_defined = is_user_defined_model(info);
    if !user_defined && info.and_then(|i| i.thinking.as_ref()).is_none() {
        return Ok(body.to_vec());
    }
    let body = object_or_empty(body);
    let effort = match config.mode {
        Mode::Level if config.level.is_empty() => return Ok(body),
        Mode::Level => config.level.clone(),
        Mode::None if config.level.is_empty() || config.level == LEVEL_NONE => return Ok(kimi_disabled(&body)),
        Mode::None => config.level.clone(),
        Mode::Budget => match convert_budget_to_level(config.budget) {
            Some(level) => level.into(),
            None => return Ok(body),
        },
        Mode::Auto => LEVEL_AUTO.into(),
    };
    let result = without(&body, "reasoning_effort");
    let result = with_str(&result, "thinking.type", "enabled");
    Ok(with_str(&result, "thinking.effort", &effort))
}

fn kimi_disabled(body: &[u8]) -> Vec<u8> {
    let result = without(body, "thinking");
    let result = without(&result, "reasoning_effort");
    with_str(&result, "thinking.type", "disabled")
}
