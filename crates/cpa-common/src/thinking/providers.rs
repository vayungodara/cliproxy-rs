//! Per-provider appliers (internal/thinking/provider/*): write a validated [`Config`]
//! into the target body's own fields.

use super::{
    Config, Error, LEVEL_AUTO, LEVEL_HIGH, LEVEL_MAX, LEVEL_NONE, LEVEL_XHIGH, Mode, ModelCaps,
    convert_budget_to_level, has_level, is_user_defined_model,
};
use crate::json;

pub(crate) type ApplyFn = fn(&str, &Config, Option<&ModelCaps>) -> Result<String, Error>;

/// `GetProviderApplier` for the built-in providers.
pub(crate) fn applier(provider: &str) -> Option<ApplyFn> {
    Some(match provider.trim().to_lowercase().as_str() {
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
    body: &str,
    config: &Config,
    info: Option<&ModelCaps>,
) -> Option<Result<String, Error>> {
    applier(provider).map(|f| f(body, config, info))
}

fn object_or_empty(body: &str) -> String {
    if body.is_empty() || !json::valid(body) {
        "{}".into()
    } else {
        body.to_owned()
    }
}

fn delete_all(body: &str, paths: &[&str]) -> String {
    paths.iter().fold(body.to_owned(), |acc, path| json::delete(&acc, path))
}

fn delete_if_empty_object(body: String, path: &str) -> String {
    if json::is_empty_object(&body, path) {
        json::delete(&body, path)
    } else {
        body
    }
}

// ---- Gemini and Antigravity share one shape under different prefixes. ----

/// `Apply` of the gemini/antigravity appliers. `prefix` is the thinkingConfig path.
fn gemini_like(
    body: &str,
    config: &Config,
    info: Option<&ModelCaps>,
    prefix: &str,
    antigravity: bool,
) -> Result<String, Error> {
    if is_user_defined_model(info) {
        let body = object_or_empty(body);
        let claude = antigravity && info.is_some_and(|i| i.id.to_lowercase().contains("claude"));
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
        return Ok(body.to_owned());
    };
    let body = object_or_empty(body);
    if antigravity {
        let claude = info_ref.id.to_lowercase().contains("claude");
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

fn gemini(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    gemini_like(body, config, info, "generationConfig.thinkingConfig", false)
}

fn antigravity(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    gemini_like(body, config, info, "request.generationConfig.thinkingConfig", true)
}

fn gemini_level(body: &str, config: &Config, prefix: &str) -> String {
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
            return json::delete(&result, prefix);
        }
        if !config.level.is_empty() {
            result = json::set_str(&result, &p("thinkingLevel"), &config.level);
        }
        return include_thoughts(result, body, prefix);
    }
    if config.mode != Mode::Level {
        return body.to_owned();
    }
    result = json::set_str(&result, &p("thinkingLevel"), &config.level);
    include_thoughts(result, body, prefix)
}

fn gemini_budget(body: &str, config: &Config, info: Option<&ModelCaps>, prefix: &str, claude: bool) -> String {
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
        let (effective_max, from_model) = match gjson::get(&result, "request.generationConfig.maxOutputTokens") {
            v if v.exists() && json::go_int(&v) > 0 => (json::go_int(&v), false),
            _ if info.max_completion_tokens > 0 => (info.max_completion_tokens, true),
            _ => (0, false),
        };
        if effective_max > 0 && budget >= effective_max {
            budget = effective_max - 1;
        }
        let min = info.thinking.as_ref().map_or(0, |t| t.min);
        if min > 0 && budget >= 0 && budget < min {
            result = json::delete(&result, prefix);
            budget = -2;
        } else if from_model && effective_max > 0 {
            result = json::set_int(&result, "request.generationConfig.maxOutputTokens", effective_max);
        }
        // Go signals "drop thinking" with -2, so a caller budget of -2 also skips the field.
        if budget == -2 {
            return include_thoughts(result, body, prefix);
        }
    }
    result = json::set_int(&result, &p("thinkingBudget"), budget);
    include_thoughts(result, body, prefix)
}

/// Carries the original `includeThoughts`/`include_thoughts` boolean, canonically.
fn include_thoughts(result: String, original: &str, prefix: &str) -> String {
    for key in ["includeThoughts", "include_thoughts"] {
        let path = format!("{prefix}.{key}");
        match gjson::get(original, &path).kind() {
            gjson::Kind::True => return json::set_bool(&result, &format!("{prefix}.includeThoughts"), true),
            gjson::Kind::False => return json::set_bool(&result, &format!("{prefix}.includeThoughts"), false),
            _ => {}
        }
    }
    result
}

// ---- Interactions ----

fn interactions(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
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

fn interactions_budget(result: String, original: &str, budget: i64, info: Option<&ModelCaps>) -> String {
    match convert_budget_to_level(budget) {
        None | Some(LEVEL_NONE) | Some(LEVEL_AUTO) => interactions_summaries(result, original),
        Some(level) => interactions_level(result, original, level, info),
    }
}

fn interactions_level(mut result: String, original: &str, level: &str, info: Option<&ModelCaps>) -> String {
    let level = normalize_interactions_level(level, info);
    if !level.is_empty() {
        result = json::set_str(&result, "generation_config.thinking_level", &level);
    }
    interactions_summaries(result, original)
}

fn interactions_summaries(result: String, original: &str) -> String {
    for path in [
        "generation_config.thinking_summaries",
        "generation_config.thinkingSummaries",
    ] {
        let v = gjson::get(original, path);
        if v.kind() == gjson::Kind::String {
            let normalized = v.str().trim().to_lowercase();
            if normalized == "auto" || normalized == "none" {
                return json::set_str(&result, "generation_config.thinking_summaries", &normalized);
            }
        }
    }
    for path in [
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
        "generation_config.thinkingConfig.include_thoughts",
        "generation_config.thinkingConfig.includeThoughts",
    ] {
        let value = match gjson::get(original, path).kind() {
            gjson::Kind::True => "auto",
            gjson::Kind::False => "none",
            _ => continue,
        };
        return json::set_str(&result, "generation_config.thinking_summaries", value);
    }
    result
}

fn normalize_interactions_level(level: &str, info: Option<&ModelCaps>) -> String {
    let level = level.trim().to_lowercase();
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
            .find(|c| c.eq_ignore_ascii_case(&level))
            .unwrap_or(&levels[levels.len() - 1])
            .to_lowercase();
    }
    match level.as_str() {
        LEVEL_MAX | LEVEL_XHIGH => LEVEL_HIGH.into(),
        _ => level,
    }
}

// ---- Claude ----

fn claude_disable(body: &str, drop_display: bool) -> String {
    let mut result = json::set_str(body, "thinking.type", "disabled");
    result = json::delete(&result, "thinking.budget_tokens");
    if drop_display {
        result = json::delete(&result, "thinking.display");
    }
    result = json::delete(&result, "output_config.effort");
    delete_if_empty_object(result, "output_config")
}

fn claude_adaptive(body: &str, effort: Option<&str>) -> String {
    let mut result = json::set_str(body, "thinking.type", "adaptive");
    result = json::delete(&result, "thinking.budget_tokens");
    match effort {
        Some(effort) => json::set_str(&result, "output_config.effort", effort),
        None => delete_if_empty_object(json::delete(&result, "output_config.effort"), "output_config"),
    }
}

fn claude_enabled(body: &str, budget: Option<i64>) -> String {
    let mut result = json::set_str(body, "thinking.type", "enabled");
    result = match budget {
        Some(budget) => json::set_int(&result, "thinking.budget_tokens", budget),
        None => json::delete(&result, "thinking.budget_tokens"),
    };
    result = json::delete(&result, "output_config.effort");
    delete_if_empty_object(result, "output_config")
}

fn claude(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    if is_user_defined_model(info) {
        let body = object_or_empty(body);
        return Ok(match config.mode {
            Mode::None => claude_disable(&body, true),
            Mode::Auto => claude_enabled(&body, None),
            Mode::Level if config.level.is_empty() => body,
            Mode::Level => claude_adaptive(&body, Some(&config.level)),
            Mode::Budget => claude_enabled(&body, Some(config.budget)),
        });
    }
    let info = info.expect("user-defined handles None");
    let Some(support) = info.thinking.as_ref() else {
        return Ok(body.to_owned());
    };
    let body = object_or_empty(body);
    let adaptive = !support.levels.is_empty();
    let budget = match config.mode {
        Mode::None => return Ok(claude_disable(&body, true)),
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
        return Ok(claude_disable(&body, false));
    }
    let result = claude_enabled(&body, Some(budget));
    Ok(normalize_claude_budget(result, budget, info))
}

/// Keeps `budget_tokens` below `max_tokens`, filling `max_tokens` from the model.
fn normalize_claude_budget(mut body: String, budget: i64, info: &ModelCaps) -> String {
    if budget <= 0 {
        return body;
    }
    let max_tokens = gjson::get(&body, "max_tokens");
    let (effective_max, from_model) = if max_tokens.exists() && json::go_int(&max_tokens) > 0 {
        (json::go_int(&max_tokens), false)
    } else if info.max_completion_tokens > 0 {
        (info.max_completion_tokens, true)
    } else {
        (0, false)
    };
    if from_model && effective_max > 0 {
        body = json::set_int(&body, "max_tokens", effective_max);
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
        body = json::set_int(&body, "thinking.budget_tokens", adjusted);
    }
    body
}

// ---- OpenAI chat and Codex/xAI Responses share one effort rule. ----

fn effort_applier(body: &str, config: &Config, info: Option<&ModelCaps>, path: &str) -> Result<String, Error> {
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
        return Ok(json::set_str(&body, path, &effort));
    }
    let info = info.expect("user-defined handles None");
    let Some(support) = info.thinking.as_ref() else {
        return Ok(body.to_owned());
    };
    if config.mode != Mode::Level && config.mode != Mode::None {
        return Ok(body.to_owned());
    }
    let body = object_or_empty(body);
    if config.mode == Mode::Level {
        return Ok(json::set_str(&body, path, &config.level));
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
    Ok(json::set_str(&body, path, &effort))
}

fn openai(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    effort_applier(body, config, info, "reasoning_effort")
}

fn codex(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    effort_applier(body, config, info, "reasoning.effort")
}

// ---- Kimi ----

fn kimi(body: &str, config: &Config, info: Option<&ModelCaps>) -> Result<String, Error> {
    let user_defined = is_user_defined_model(info);
    if !user_defined && info.and_then(|i| i.thinking.as_ref()).is_none() {
        return Ok(body.to_owned());
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
    let result = json::delete(&body, "reasoning_effort");
    let result = json::set_str(&result, "thinking.type", "enabled");
    Ok(json::set_str(&result, "thinking.effort", &effort))
}

fn kimi_disabled(body: &str) -> String {
    let result = json::delete(body, "thinking");
    let result = json::delete(&result, "reasoning_effort");
    json::set_str(&result, "thinking.type", "disabled")
}
