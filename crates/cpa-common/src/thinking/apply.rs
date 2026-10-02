//! The canonical pipeline (apply.go) and the executor entry point
//! (executor/helps/model_capabilities.go, helps/thinking.go).

use super::providers::applier;
use super::summary::{
    SummaryConfig, SummaryMode, apply_summary_config_for_model, apply_summary_config_for_provider,
    extract_explicit_summary_config, extract_summary_config, extract_translated_summary_config,
    strip_inferred_claude_summary_activation,
};
use super::validate::{is_budget_capable_provider, is_level_supported, is_same_provider_family, validate_config};
use super::{
    Config, Error, LEVEL_AUTO, LEVEL_HIGH, LEVEL_MAX, LEVEL_NONE, LEVEL_XHIGH, Mode, ModelCaps, SuffixResult,
    convert_budget_to_level, convert_level_to_budget, extract_configuration_update_config, is_responses_format,
    is_user_defined_model, lookup_model_info, parse_level_suffix, parse_numeric_suffix, parse_special_suffix,
    parse_suffix, strip_configuration_updates, strip_responses_effort, strip_thinking_config,
};
use crate::gostr::GoStr;
use crate::json;

/// `ApplyThinking`: summary intent comes from the target body itself.
pub fn apply_thinking(body: &[u8], model: &str, from: &str, to: &str, provider: &str) -> Result<Vec<u8>, Error> {
    let summary = extract_summary_config(body, to);
    run(body, b"", model, from, to, provider, None, summary, false)
}

/// `ApplyThinkingWithSummary`.
pub fn apply_thinking_with_summary(
    body: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    summary: SummaryConfig,
) -> Result<Vec<u8>, Error> {
    run(body, b"", model, from, to, provider, None, summary, false)
}

/// `ApplyThinkingWithSourceAndSummary`.
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_source_and_summary(
    body: &[u8],
    source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    summary: SummaryConfig,
    updates_changed: bool,
) -> Result<Vec<u8>, Error> {
    run(body, source, model, from, to, provider, None, summary, updates_changed)
}

/// `ApplyThinkingWithModelInfo`: the exact model definition bound to this attempt.
pub fn apply_thinking_with_model_info(
    body: &[u8],
    source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    info: Option<&ModelCaps>,
) -> Result<Vec<u8>, Error> {
    let summary = if source.is_empty() {
        extract_summary_config(body, to)
    } else {
        extract_summary_config(source, from)
    };
    apply_thinking_with_model_info_and_summary(body, source, model, from, to, provider, info, summary, false)
}

/// `ApplyThinkingWithModelInfoAndSummary`.
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_model_info_and_summary(
    body: &[u8],
    source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    info: Option<&ModelCaps>,
    summary: SummaryConfig,
    updates_changed: bool,
) -> Result<Vec<u8>, Error> {
    run(
        body,
        source,
        model,
        from,
        to,
        provider,
        Some(info),
        summary,
        updates_changed,
    )
}

/// Inputs to `helps.ApplyRequestThinking`.
pub struct RequestThinking<'a> {
    /// Translated provider body.
    pub body: &'a [u8],
    /// The request payload that was translated (`req.Payload`).
    pub payload: &'a [u8],
    /// The inbound body before interceptors (`opts.OriginalRequest`).
    pub original: &'a [u8],
    /// Requested model, possibly with a `(suffix)`.
    pub model: &'a str,
    pub from: &'a str,
    pub to: &'a str,
    pub provider: &'a str,
    /// Capabilities bound to this attempt (Go `cliproxyauth.ResolvedModelInfo`), when
    /// the scheduler resolved an API-key or Home model definition.
    pub resolved: Option<Option<&'a ModelCaps>>,
    /// Whether a request translator is registered for `from -> to`
    /// (`sdktranslator.HasRequestTransformer`).
    pub has_request_transformer: bool,
    pub updates_changed: bool,
}

/// `helps.ApplyRequestThinking`.
pub fn apply_request_thinking(req: &RequestThinking<'_>) -> Result<Vec<u8>, Error> {
    let original = if req.original.is_empty() {
        req.payload
    } else {
        req.original
    };
    let source = if req.payload.is_empty() {
        req.original
    } else {
        req.payload
    };
    let summary = translated_request_summary_config(
        req.body,
        req.payload,
        original,
        req.model,
        req.from,
        req.to,
        req.has_request_transformer,
    );
    match req.resolved {
        Some(info) => apply_thinking_with_model_info_and_summary(
            req.body,
            source,
            req.model,
            req.from,
            req.to,
            req.provider,
            info,
            summary,
            req.updates_changed,
        ),
        None => apply_thinking_with_source_and_summary(
            req.body,
            source,
            req.model,
            req.from,
            req.to,
            req.provider,
            summary,
            req.updates_changed,
        ),
    }
}

/// `helps.ApplyThinkingWithSourcePayload`.
#[allow(clippy::too_many_arguments)]
pub fn apply_thinking_with_source_payload(
    body: &[u8],
    current_source: &[u8],
    original_source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    has_request_transformer: bool,
) -> Result<Vec<u8>, Error> {
    let summary = translated_request_summary_config(
        body,
        current_source,
        original_source,
        model,
        from,
        to,
        has_request_transformer,
    );
    apply_thinking_with_summary(body, model, from, to, provider, summary)
}

/// `helps.translatedRequestSummaryConfig`: the translated body wins, so a normalizer can
/// remove or rewrite the summary; the source is consulted only when the body lost the
/// intent or cannot represent it until the model-aware pass.
pub fn translated_request_summary_config(
    body: &[u8],
    current_source: &[u8],
    original_source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    has_request_transformer: bool,
) -> SummaryConfig {
    let from = from.trim().go_lower();
    let to = to.trim().go_lower();
    let target = if from == to {
        extract_summary_config(body, &to)
    } else {
        extract_explicit_summary_config(body, &to)
    };
    if target.mode != SummaryMode::Unspecified {
        return target;
    }
    let current = extract_translated_summary_config(current_source, &from, &to);
    let original = extract_translated_summary_config(original_source, &from, &to);
    if current.mode == SummaryMode::Unspecified {
        return original;
    }
    if !has_request_transformer {
        return SummaryConfig::default();
    }
    let candidate = apply_summary_config_for_model(body, &to, model, current.clone());
    if extract_explicit_summary_config(&candidate, &to).mode != SummaryMode::Unspecified {
        return SummaryConfig::default();
    }
    current
}

#[allow(clippy::too_many_arguments)]
fn run(
    body: &[u8],
    source: &[u8],
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    resolved: Option<Option<&ModelCaps>>,
    summary: SummaryConfig,
    updates_changed: bool,
) -> Result<Vec<u8>, Error> {
    let mut target = to.trim().go_lower();
    if target == "openai-response" {
        target = "codex".into();
    }
    let mut provider = provider.trim().go_lower();
    if provider.is_empty() {
        provider.clone_from(&target);
    }
    let mut from = from.trim().go_lower();
    if from.is_empty() {
        from.clone_from(&target);
    }
    let suffix = parse_suffix(model);
    let base = suffix.model_name.clone();
    let resolved_flag = resolved.is_some();
    let looked_up;
    let info: Option<&ModelCaps> = match resolved {
        Some(info) => info,
        None => {
            looked_up = lookup_model_info(&base, &provider);
            looked_up.as_ref()
        }
    };

    let mut source_config = Config::default();
    if is_responses_format(&from) {
        let request = if !updates_changed && !source.is_empty() {
            source
        } else {
            body
        };
        if !updates_changed || target == "codex" || target == "xai" {
            source_config = extract_codex_usage_config(request);
        }
    }
    let response_target = target == "codex" || target == "xai";
    let supports_updates = info.is_some_and(|i| i.support_configuration_update);
    let mut body = body.to_vec();
    if response_target && !supports_updates {
        body = strip_configuration_updates(&body);
    }
    let native_responses = response_target && is_responses_format(&from) && supports_updates;

    let Some(apply_fn) = applier(&target) else {
        return Ok(body);
    };
    if !suffix.has_suffix
        && !source.is_empty()
        && is_responses_format(&from)
        && !json::valid(&body)
        && extract_configuration_update_config(source).is_set()
    {
        return Ok(body);
    }
    if native_responses && !suffix.has_suffix {
        return Ok(body);
    }
    if is_user_defined_model(info) {
        return apply_user_defined_model(
            &body,
            info,
            &from,
            &target,
            &provider,
            &suffix,
            source_config,
            native_responses,
            summary,
        );
    }
    let info = info.expect("user-defined handles None");
    if info.thinking.is_none() {
        let config = extract_thinking_config(&body, &target);
        if config.is_set() || summary.mode != SummaryMode::Unspecified {
            return Ok(if response_target {
                strip_responses_effort(&body)
            } else {
                strip_thinking_config(&body, &target)
            });
        }
        return Ok(body);
    }

    let mut config = if suffix.has_suffix {
        parse_suffix_to_config(&suffix.raw_suffix)
    } else {
        let mut config = source_config;
        if !config.is_set() && !updates_changed && resolved_flag && !source.is_empty() {
            config = extract_source_thinking_config(source, &from);
        }
        if !config.is_set() {
            config = extract_thinking_config(&body, &target);
        }
        config
    };

    if !config.is_set() {
        if native_responses {
            return Ok(body);
        }
        if resolved_flag
            && target == "claude"
            && from != target
            && extract_summary_config(source, &from).mode == SummaryMode::Enabled
        {
            body = strip_inferred_claude_summary_activation(&body, Some(info));
        }
        return Ok(apply_summary_config_for_provider(
            &body,
            &target,
            &base,
            &provider,
            Some(info),
            summary,
        ));
    }
    if resolved_flag && config.mode == Mode::Level && should_map_configured_high_intent(&from, &target, info) {
        config.level = map_configured_high_intent(&config.level, info);
    }

    let validated = validate_config(config, Some(info), &from, &target, suffix.has_suffix).map_err(|mut e| {
        e.body = Some(body.clone());
        e
    })?;
    let applied = apply_fn(&body, &validated, Some(info))?;
    if fully_disabled(&validated) || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        &applied,
        &target,
        &base,
        &provider,
        Some(info),
        summary,
    ))
}

fn fully_disabled(config: &Config) -> bool {
    config.mode == Mode::None && config.budget == 0 && config.level.is_empty()
}

fn should_map_configured_high_intent(from: &str, to: &str, info: &ModelCaps) -> bool {
    let from = from.trim().go_lower();
    let to = to.trim().go_lower();
    if from != to {
        return true;
    }
    let kind = info.kind.trim().go_lower();
    !kind.is_empty() && !is_same_provider_family(&to, &kind)
}

fn map_configured_high_intent(level: &str, info: &ModelCaps) -> String {
    let Some(support) = info.thinking.as_ref().filter(|s| !s.levels.is_empty()) else {
        return level.to_owned();
    };
    let level = level.trim().go_lower();
    let candidates: &[&str] = match level.as_str() {
        LEVEL_XHIGH => &[LEVEL_XHIGH, LEVEL_MAX, LEVEL_HIGH],
        LEVEL_MAX => &[LEVEL_MAX, LEVEL_XHIGH, LEVEL_HIGH],
        _ => return level,
    };
    candidates
        .iter()
        .find(|c| is_level_supported(c, &support.levels))
        .map_or(level.clone(), |c| (*c).to_owned())
}

fn extract_source_thinking_config(body: &[u8], provider: &str) -> Config {
    let provider = provider.trim().go_lower();
    if provider == "openai-response" {
        return extract_codex_config(body);
    }
    extract_thinking_config(body, &provider)
}

/// `parseSuffixToConfig`: special values, then levels, then budgets.
fn parse_suffix_to_config(raw: &str) -> Config {
    match parse_special_suffix(raw) {
        Some(Mode::None) => return Config::none(),
        Some(Mode::Auto) => return Config::auto(),
        _ => {}
    }
    if let Some(level) = parse_level_suffix(raw) {
        return Config::level(level);
    }
    match parse_numeric_suffix(raw) {
        Some(0) => Config::none(),
        Some(budget) => Config::budget(budget),
        None => Config::default(),
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_user_defined_model(
    body: &[u8],
    info: Option<&ModelCaps>,
    from: &str,
    to: &str,
    provider: &str,
    suffix: &SuffixResult,
    source_config: Config,
    native_responses: bool,
    summary: SummaryConfig,
) -> Result<Vec<u8>, Error> {
    let model_id = info.map_or(suffix.model_name.as_str(), |i| i.id.as_str()).to_owned();
    let config = if suffix.has_suffix {
        parse_suffix_to_config(&suffix.raw_suffix)
    } else {
        let mut config = source_config;
        if !config.is_set() {
            config = extract_thinking_config(body, from);
        }
        if !config.is_set() && from != to {
            config = extract_thinking_config(body, to);
        }
        config
    };
    if !config.is_set() {
        return Ok(apply_summary_config_for_provider(
            body, to, &model_id, provider, info, summary,
        ));
    }
    let Some(apply_fn) = applier(to) else {
        return Ok(body.to_vec());
    };
    let config = normalize_user_defined_config(config, to);
    let applied = apply_fn(body, &config, info)?;
    if fully_disabled(&config) || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary_config_for_provider(
        &applied, to, &model_id, provider, info, summary,
    ))
}

fn normalize_user_defined_config(config: Config, to: &str) -> Config {
    if config.mode != Mode::Level || to == "claude" || !is_budget_capable_provider(to) {
        return config;
    }
    match convert_level_to_budget(&config.level) {
        Some(budget) => Config::budget(budget),
        None => config,
    }
}

/// `extractThinkingConfig`: the canonical config a provider body carries.
pub fn extract_thinking_config(body: &[u8], provider: &str) -> Config {
    if body.is_empty() || !json::valid(body) {
        return Config::default();
    }
    match provider {
        "claude" => extract_claude_config(body),
        "gemini" | "antigravity" => extract_gemini_config(body, provider),
        "interactions" => extract_interactions_config(body),
        "openai" => extract_openai_config(body),
        "codex" | "xai" => extract_codex_config(body),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => extract_kimi_config(body),
        _ => Config::default(),
    }
}

/// `ExtractReasoningEffort`: the source request's effort for usage reporting.
pub fn extract_reasoning_effort(body: &[u8], provider: &str, model: &str) -> String {
    let provider = provider.trim().go_lower();
    if is_responses_format(&provider) {
        let effort = effort_from_config(&extract_configuration_update_config(body));
        if !effort.is_empty() {
            return effort;
        }
    }
    let suffix = parse_suffix(model);
    if suffix.has_suffix {
        let effort = effort_from_config(&parse_suffix_to_config(&suffix.raw_suffix));
        if !effort.is_empty() {
            return effort;
        }
    }
    let mut config = extract_for_usage(body, &provider);
    if !config.is_set() && (provider == "openai-response" || provider == "openai") {
        config = extract_codex_usage_config(body);
    }
    effort_from_config(&config)
}

/// `ExtractTranslatedReasoningEffort`: the final provider payload's effort.
pub fn extract_translated_reasoning_effort(body: &[u8], provider: &str) -> String {
    let provider = provider.trim().go_lower();
    let mut config = extract_for_usage(body, &provider);
    if !config.is_set() && (provider == "openai" || provider == "openai-response") {
        config = extract_codex_usage_config(body);
        if !config.is_set() {
            config = extract_openai_config(body);
        }
    }
    effort_from_config(&config)
}

fn extract_for_usage(body: &[u8], provider: &str) -> Config {
    match provider.trim().go_lower().as_str() {
        "codex" | "xai" | "openai-response" => extract_codex_usage_config(body),
        p => extract_thinking_config(body, p),
    }
}

fn effort_from_config(config: &Config) -> String {
    if !config.is_set() {
        return String::new();
    }
    match config.mode {
        Mode::None => LEVEL_NONE.into(),
        Mode::Auto => LEVEL_AUTO.into(),
        Mode::Level => config.level.trim().go_lower(),
        Mode::Budget => convert_budget_to_level(config.budget).unwrap_or_default().into(),
    }
}

fn level_or_special(value: &str) -> Config {
    match value {
        "none" => Config::none(),
        "auto" => Config::auto(),
        _ => Config::level(value),
    }
}

fn budget_config(value: i64) -> Config {
    match value {
        0 => Config::none(),
        -1 => Config::auto(),
        _ => Config::budget(value),
    }
}

/// Claude: `thinking.type` disabled wins, adaptive uses `output_config.effort`, then
/// `thinking.budget_tokens`, then enabled with effort or auto.
fn extract_claude_config(body: &[u8]) -> Config {
    let kind = json::get(body, "thinking.type").str().into_owned();
    let effort = || {
        let effort = json::get(body, "output_config.effort");
        (effort.kind == json::Kind::String).then(|| effort.str().trim().go_lower())
    };
    if kind == "disabled" {
        return Config::none();
    }
    if kind == "adaptive" || kind == "auto" {
        return match effort() {
            Some(value) if value.is_empty() => Config::default(),
            Some(value) => level_or_special(&value),
            None => Config::default(),
        };
    }
    let budget = json::get(body, "thinking.budget_tokens");
    if budget.exists() {
        return budget_config(budget.int());
    }
    if kind == "enabled" {
        if let Some(value) = effort().filter(|v| !v.is_empty()) {
            return level_or_special(&value);
        }
        return Config::auto();
    }
    Config::default()
}

/// Gemini/Antigravity: `thinkingLevel` (Gemini 3) before `thinkingBudget` (2.5).
fn extract_gemini_config(body: &[u8], provider: &str) -> Config {
    let prefix = if provider == "antigravity" {
        "request.generationConfig.thinkingConfig"
    } else {
        "generationConfig.thinkingConfig"
    };
    for key in ["thinkingLevel", "thinking_level"] {
        let path = format!("{prefix}.{key}");
        let level = json::get(body, &path);
        if level.exists() {
            return level_or_special(&level.str());
        }
    }
    for key in ["thinkingBudget", "thinking_budget"] {
        let path = format!("{prefix}.{key}");
        let budget = json::get(body, &path);
        if budget.exists() {
            return budget_config(budget.int());
        }
    }
    Config::default()
}

fn extract_interactions_config(body: &[u8]) -> Config {
    for path in [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_config.thinking_level",
        "generation_config.thinking_config.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
    ] {
        let level = json::get(body, path);
        if level.exists() {
            return level_or_special(&level.str().trim().go_lower());
        }
    }
    for path in [
        "generation_config.thinking_budget",
        "generation_config.thinkingBudget",
        "generation_config.thinking_config.thinking_budget",
        "generation_config.thinking_config.thinkingBudget",
        "generation_config.thinkingConfig.thinking_budget",
        "generation_config.thinkingConfig.thinkingBudget",
    ] {
        let budget = json::get(body, path);
        if budget.exists() {
            return budget_config(budget.int());
        }
    }
    Config::default()
}

fn extract_openai_config(body: &[u8]) -> Config {
    let effort = json::get(body, "reasoning_effort");
    if !effort.exists() {
        return Config::default();
    }
    let value = effort.str();
    if value == "none" {
        Config::none()
    } else {
        Config::level(value)
    }
}

/// Kimi: native `thinking` fields win over `reasoning_effort`.
fn extract_kimi_config(body: &[u8]) -> Config {
    let kind = json::get(body, "thinking.type");
    if kind.exists() {
        match kind.str().trim().go_lower().as_str() {
            "disabled" => return Config::none(),
            "enabled" if !json::get(body, "thinking.effort").exists() => return Config::default(),
            _ => {}
        }
    }
    let effort = json::get(body, "thinking.effort");
    if effort.exists() {
        let value = effort.str().trim().go_lower();
        return if value.is_empty() {
            Config::default()
        } else {
            level_or_special(&value)
        };
    }
    if kind.exists() {
        return Config::default();
    }
    extract_openai_config(body)
}

fn extract_codex_config(body: &[u8]) -> Config {
    let effort = json::get(body, "reasoning.effort");
    if !effort.exists() {
        return Config::default();
    }
    let value = effort.str();
    if value == "none" {
        Config::none()
    } else {
        Config::level(value)
    }
}

fn extract_codex_usage_config(body: &[u8]) -> Config {
    if body.is_empty() || !json::valid(body) {
        return Config::default();
    }
    let config = extract_configuration_update_config(body);
    if config.is_set() {
        return config;
    }
    extract_codex_config(body)
}
