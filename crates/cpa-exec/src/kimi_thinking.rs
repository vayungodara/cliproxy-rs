//! Go's canonical thinking pipeline (internal/thinking, helps.ApplyRequestThinking):
//! model suffix (wins over body), source/target extraction, validation against the
//! model's capabilities, then the provider applier and summary restoration.
//!
//! ponytail: only the `kimi` and `codex` appliers are ported, the targets Kimi (chat
//! completions and Responses) and Meta (Responses) need. Every other target passes the
//! body through, as Go does for an unknown applier. Model info comes from the pinned
//! static catalog (Go's dynamic registry holds the same entries for OAuth credentials);
//! API-key `models[]` capabilities and Home-resolved model info are not consulted.
//! Hoist into a shared module when PARITY M2-0032 lands.

use cpa_core::registry::{ModelInfo, ThinkingSupport};

use crate::kimi_json::{delete, gstr, set_raw, set_str, valid};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Budget,
    Level,
    None,
    Auto,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Config {
    mode: Mode,
    budget: i64,
    level: String,
}

impl Config {
    fn none() -> Self {
        Self {
            mode: Mode::None,
            ..Self::default()
        }
    }
    fn auto() -> Self {
        Self {
            mode: Mode::Auto,
            budget: -1,
            level: String::new(),
        }
    }
    fn level(level: impl Into<String>) -> Self {
        Self {
            mode: Mode::Level,
            budget: 0,
            level: level.into(),
        }
    }
    fn budget(budget: i64) -> Self {
        Self {
            mode: Mode::Budget,
            budget,
            level: String::new(),
        }
    }
    fn present(&self) -> bool {
        self.mode != Mode::Budget || self.budget != 0 || !self.level.is_empty()
    }
    fn fully_disabled(&self) -> bool {
        self.mode == Mode::None && self.budget == 0 && self.level.is_empty()
    }
}

/// A thinking validation failure: Go answers 400 with the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ThinkingError(pub String);

/// `thinking.ParseSuffix`: `model(raw)` splits at the last `(` when the name ends in `)`.
pub(crate) fn parse_suffix(model: &str) -> (&str, Option<&str>) {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => (&model[..open], Some(&model[open + 1..model.len() - 1])),
        _ => (model, None),
    }
}

/// `registry.LookupModelInfo(id, provider)`: the provider's catalog, then Go's static search order.
pub(crate) fn lookup_model(id: &str, provider: &str) -> Option<&'static ModelInfo> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    let catalog = cpa_core::registry::pinned();
    catalog
        .channel(&provider.trim().to_ascii_lowercase())
        .iter()
        .find(|m| m.id == id)
        .or_else(|| catalog.lookup(id))
}

fn supports_updates(model: Option<&ModelInfo>) -> bool {
    model.is_some_and(|m| {
        m.raw
            .get("support_configuration_update")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

/// `helps.ApplyRequestThinking`: `body` is the translated target body, `source` the
/// request payload and `original` the inbound body before payload rules.
pub(crate) fn apply(
    body: &str,
    source: &str,
    original: &str,
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
) -> Result<String, ThinkingError> {
    let source = if source.is_empty() { original } else { source };
    let original = if original.is_empty() { source } else { original };
    let summary = translated_summary(body, source, original, model, from, to);
    apply_thinking(body, source, model, from, to, provider, summary)
}

fn is_responses(format: &str) -> bool {
    format == "codex" || format == "openai-response"
}

fn apply_thinking(
    body: &str,
    source: &str,
    model: &str,
    from: &str,
    to: &str,
    provider: &str,
    summary: Summary,
) -> Result<String, ThinkingError> {
    let mut target = to.trim().to_ascii_lowercase();
    if target == "openai-response" {
        target = "codex".into();
    }
    let mut provider = provider.trim().to_ascii_lowercase();
    if provider.is_empty() {
        provider = target.clone();
    }
    let mut from = from.trim().to_ascii_lowercase();
    if from.is_empty() {
        from = target.clone();
    }
    let (base, suffix) = parse_suffix(model);
    let info = lookup_model(base, &provider);
    let mut source_config = Config::default();
    if is_responses(&from) {
        let request = if source.is_empty() { body } else { source };
        source_config = extract_codex_usage(request);
    }
    let response_target = target == "codex" || target == "xai";
    let updates = supports_updates(info);
    let mut body = body.to_owned();
    if response_target && !updates {
        body = strip_configuration_updates(&body);
    }
    let native_responses = response_target && is_responses(&from) && updates;
    if !matches!(target.as_str(), "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" | "codex") {
        return Ok(body);
    }
    if suffix.is_none()
        && !source.is_empty()
        && is_responses(&from)
        && !valid(&body)
        && extract_configuration_update(source).present()
    {
        return Ok(body);
    }
    if native_responses && suffix.is_none() {
        return Ok(body);
    }
    let Some(info) = info else {
        return Ok(apply_user_defined(
            body,
            None,
            &from,
            &target,
            suffix,
            source_config,
            native_responses,
            summary,
        ));
    };
    let Some(support) = info.thinking.as_ref() else {
        let config = extract(&body, &target);
        if config.present() || summary.mode != SummaryMode::Unspecified {
            return Ok(if response_target {
                strip_responses_effort(&body)
            } else {
                strip_thinking(&body, &target)
            });
        }
        return Ok(body);
    };
    let config = match suffix {
        Some(raw) => suffix_config(raw),
        None => {
            let mut config = source_config;
            if !config.present() {
                config = extract(&body, &target);
            }
            config
        }
    };
    if !config.present() {
        if native_responses {
            return Ok(body);
        }
        return Ok(apply_summary(&body, &target, summary));
    }
    let validated = validate(config, info, support, &from, &target, suffix.is_some())?;
    let applied = apply_provider(&body, &target, &validated, Some(info))?;
    if validated.fully_disabled() || native_responses {
        return Ok(applied);
    }
    Ok(apply_summary(&applied, &target, summary))
}

#[allow(clippy::too_many_arguments)]
fn apply_user_defined(
    body: String,
    info: Option<&ModelInfo>,
    from: &str,
    to: &str,
    suffix: Option<&str>,
    source_config: Config,
    native_responses: bool,
    summary: Summary,
) -> String {
    let config = match suffix {
        Some(raw) => suffix_config(raw),
        None => {
            let mut config = source_config;
            if !config.present() {
                config = extract(&body, from);
            }
            if !config.present() && from != to {
                config = extract(&body, to);
            }
            config
        }
    };
    if !config.present() {
        return apply_summary(&body, to, summary);
    }
    let config = normalize_user_defined(config, to);
    let Ok(applied) = apply_provider(&body, to, &config, info) else {
        return body;
    };
    if config.fully_disabled() || native_responses {
        return applied;
    }
    apply_summary(&applied, to, summary)
}

fn normalize_user_defined(config: Config, to: &str) -> Config {
    if config.mode != Mode::Level || to == "claude" || !matches!(to, "gemini" | "antigravity" | "claude") {
        return config;
    }
    match level_to_budget(&config.level) {
        Some(budget) => Config::budget(budget),
        None => config,
    }
}

fn suffix_config(raw: &str) -> Config {
    match raw.to_ascii_lowercase().as_str() {
        "none" => return Config::none(),
        "auto" | "-1" => return Config::auto(),
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" => return Config::level(raw.to_ascii_lowercase()),
        _ => {}
    }
    // strconv.Atoi: optional sign, decimal digits, fits in a 64-bit int; negatives rejected.
    match raw.parse::<i64>() {
        Ok(0) => Config::none(),
        Ok(budget) if budget > 0 => Config::budget(budget),
        _ => Config::default(),
    }
}

fn get<'a>(body: &'a str, path: &'a str) -> gjson::Value<'a> {
    gjson::get(body, path)
}

fn level_or_special(value: &str) -> Config {
    match value {
        "none" => Config::none(),
        "auto" => Config::auto(),
        _ => Config::level(value),
    }
}

fn extract(body: &str, provider: &str) -> Config {
    if body.is_empty() || !valid(body) {
        return Config::default();
    }
    match provider {
        "claude" => extract_claude(body),
        "gemini" | "antigravity" => extract_gemini(body, provider),
        "interactions" => extract_interactions(body),
        "openai" => extract_openai(body),
        "codex" | "xai" => extract_codex(body),
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => extract_kimi(body),
        _ => Config::default(),
    }
}

fn budget_config(value: i64) -> Config {
    match value {
        0 => Config::none(),
        -1 => Config::auto(),
        _ => Config::budget(value),
    }
}

fn extract_claude(body: &str) -> Config {
    let kind = gstr(&get(body, "thinking.type")).to_owned();
    let effort = || {
        let effort = get(body, "output_config.effort");
        (effort.kind() == gjson::Kind::String).then(|| gstr(&effort).trim().to_ascii_lowercase())
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
    let budget = get(body, "thinking.budget_tokens");
    if budget.exists() {
        return budget_config(budget.i64());
    }
    if kind == "enabled" {
        if let Some(value) = effort().filter(|v| !v.is_empty()) {
            return level_or_special(&value);
        }
        return Config::auto();
    }
    Config::default()
}

fn extract_gemini(body: &str, provider: &str) -> Config {
    let prefix = if provider == "antigravity" {
        "request.generationConfig.thinkingConfig"
    } else {
        "generationConfig.thinkingConfig"
    };
    for key in ["thinkingLevel", "thinking_level"] {
        let path = format!("{prefix}.{key}");
        let level = get(body, &path);
        if level.exists() {
            return level_or_special(&gstr(&level));
        }
    }
    for key in ["thinkingBudget", "thinking_budget"] {
        let path = format!("{prefix}.{key}");
        let budget = get(body, &path);
        if budget.exists() {
            return budget_config(budget.i64());
        }
    }
    Config::default()
}

fn extract_interactions(body: &str) -> Config {
    for path in [
        "generation_config.thinking_level",
        "generation_config.thinkingLevel",
        "generation_config.thinking_config.thinking_level",
        "generation_config.thinking_config.thinkingLevel",
        "generation_config.thinkingConfig.thinking_level",
        "generation_config.thinkingConfig.thinkingLevel",
    ] {
        let level = get(body, path);
        if level.exists() {
            return level_or_special(&gstr(&level).trim().to_ascii_lowercase());
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
        let budget = get(body, path);
        if budget.exists() {
            return budget_config(budget.i64());
        }
    }
    Config::default()
}

fn extract_openai(body: &str) -> Config {
    let effort = get(body, "reasoning_effort");
    if !effort.exists() {
        return Config::default();
    }
    if gstr(&effort) == "none" {
        Config::none()
    } else {
        Config::level(gstr(&effort))
    }
}

fn extract_kimi(body: &str) -> Config {
    let kind = get(body, "thinking.type");
    if kind.exists() {
        match gstr(&kind).trim().to_ascii_lowercase().as_str() {
            "disabled" => return Config::none(),
            "enabled" if !get(body, "thinking.effort").exists() => return Config::default(),
            _ => {}
        }
    }
    let effort = get(body, "thinking.effort");
    if effort.exists() {
        let value = gstr(&effort).trim().to_ascii_lowercase();
        return if value.is_empty() {
            Config::default()
        } else {
            level_or_special(&value)
        };
    }
    if kind.exists() {
        return Config::default();
    }
    extract_openai(body)
}

fn extract_codex(body: &str) -> Config {
    let effort = get(body, "reasoning.effort");
    if !effort.exists() {
        return Config::default();
    }
    if gstr(&effort) == "none" {
        Config::none()
    } else {
        Config::level(gstr(&effort))
    }
}

fn extract_codex_usage(body: &str) -> Config {
    if body.is_empty() || !valid(body) {
        return Config::default();
    }
    let update = extract_configuration_update(body);
    if update.present() { update } else { extract_codex(body) }
}

fn extract_configuration_update(body: &str) -> Config {
    if body.is_empty() || !valid(body) {
        return Config::default();
    }
    let input = get(body, "input");
    if input.kind() != gjson::Kind::Array {
        return Config::default();
    }
    let mut effort = String::new();
    for item in input.array() {
        if gstr(&item.get("type")) == "configuration_update" {
            let value = item.get("reasoning.effort");
            if value.kind() == gjson::Kind::String {
                let normalized = gstr(&value).trim().to_ascii_lowercase();
                if !normalized.is_empty() {
                    effort = normalized;
                }
            }
        }
    }
    if effort.is_empty() {
        Config::default()
    } else {
        level_or_special(&effort)
    }
}

fn strip_configuration_updates(body: &str) -> String {
    if body.is_empty() || !valid(body) {
        return body.to_owned();
    }
    let input = get(body, "input");
    if input.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let items = input.array();
    let kept: Vec<String> = items
        .iter()
        .filter(|item| gstr(&item.get("type")) != "configuration_update")
        .map(|item| item.json().to_owned())
        .collect();
    if kept.len() == items.len() {
        return body.to_owned();
    }
    set_raw(body, "input", &format!("[{}]", kept.join(","))).unwrap_or_else(|_| body.to_owned())
}

fn strip_responses_effort(body: &str) -> String {
    if body.is_empty() || !valid(body) || !get(body, "reasoning.effort").exists() {
        return body.to_owned();
    }
    let result = delete(body, "reasoning.effort");
    if empty_object(&result, "reasoning") {
        delete(&result, "reasoning")
    } else {
        result
    }
}

fn empty_object(body: &str, path: &str) -> bool {
    let value = get(body, path);
    if value.kind() != gjson::Kind::Object {
        return false;
    }
    let mut empty = true;
    value.each(|_, _| {
        empty = false;
        false
    });
    empty
}

fn strip_thinking(body: &str, provider: &str) -> String {
    if body.is_empty() || !valid(body) {
        return body.to_owned();
    }
    let paths: &[&str] = match provider {
        "openai" => &["reasoning_effort", "reasoning"],
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => &["reasoning_effort", "thinking"],
        "codex" | "xai" => &["reasoning"],
        _ => return body.to_owned(),
    };
    paths.iter().fold(body.to_owned(), |acc, path| delete(&acc, path))
}

fn level_to_budget(level: &str) -> Option<i64> {
    Some(match level.to_ascii_lowercase().as_str() {
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

fn budget_to_level(budget: i64) -> Option<&'static str> {
    Some(match budget {
        i64::MIN..=-2 => return None,
        -1 => "auto",
        0 => "none",
        1..=512 => "minimal",
        513..=1024 => "low",
        1025..=8192 => "medium",
        8193..=24576 => "high",
        _ => "xhigh",
    })
}

const LEVEL_ORDER: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

fn level_supported(level: &str, supported: &[String]) -> bool {
    supported.iter().any(|s| s.trim().eq_ignore_ascii_case(level))
}

fn level_index(level: &str) -> Option<usize> {
    LEVEL_ORDER.iter().position(|l| l.eq_ignore_ascii_case(level))
}

fn clamp_level(level: &str, support: &ThinkingSupport) -> String {
    if support.levels.is_empty() || level_supported(level, &support.levels) {
        return level.to_owned();
    }
    let Some(pos) = level_index(level) else {
        return level.to_owned();
    };
    let mut best: Option<(usize, usize)> = None;
    for s in &support.levels {
        if let Some(idx) = level_index(s.trim()) {
            let dist = pos.abs_diff(idx);
            if best.is_none_or(|(best_idx, best_dist)| dist < best_dist || (dist == best_dist && idx < best_idx)) {
                best = Some((idx, dist));
            }
        }
    }
    best.map_or_else(|| level.to_owned(), |(idx, _)| LEVEL_ORDER[idx].to_owned())
}

fn clamp_budget(value: i64, support: &ThinkingSupport) -> i64 {
    if value == -1 {
        return value;
    }
    let (min, max) = (support.min, support.max);
    if value == 0 && !support.zero_allowed {
        return min;
    }
    if min == 0 && max == 0 {
        return value;
    }
    if value < min {
        return if value == 0 && support.zero_allowed { 0 } else { min };
    }
    value.min(max)
}

fn gemini_family(p: &str) -> bool {
    matches!(p, "gemini" | "antigravity")
}

fn openai_family(p: &str) -> bool {
    matches!(p, "openai" | "openai-response" | "codex")
}

fn same_family(a: &str, b: &str) -> bool {
    a == b || (gemini_family(a) && gemini_family(b)) || (openai_family(a) && openai_family(b))
}

fn validate(
    mut config: Config,
    info: &ModelInfo,
    support: &ThinkingSupport,
    from: &str,
    to: &str,
    from_suffix: bool,
) -> Result<Config, ThinkingError> {
    let has_budget = support.min > 0 || support.max > 0;
    let has_levels = !support.levels.is_empty();
    let model_type = info.kind.trim().to_ascii_lowercase();
    let family_mismatch = !model_type.is_empty()
        && ((!from.is_empty() && !same_family(from, &model_type)) || (!to.is_empty() && !same_family(to, &model_type)));
    let allow_clamp = has_levels && (!same_family(from, to) || family_mismatch);
    let strict_budget = !from_suffix && !from.is_empty() && same_family(from, to) && !family_mismatch;
    let mut from_level = false;
    if has_budget && !has_levels {
        if config.mode == Mode::Level && config.level != "auto" {
            let budget = level_to_budget(&config.level)
                .ok_or_else(|| ThinkingError(format!("unknown level: {}", config.level)))?;
            config = Config::budget(budget);
            from_level = true;
        }
    } else if has_levels && !has_budget && config.mode == Mode::Budget {
        let level = budget_to_level(config.budget)
            .ok_or_else(|| ThinkingError(format!("budget {} cannot be converted to a valid level", config.budget)))?;
        config = Config::level(clamp_level(level, support));
    }
    if config.mode == Mode::Level && config.level == "none" {
        config = Config::none();
    }
    if config.mode == Mode::Level && config.level == "auto" {
        config = Config::auto();
    }
    if config.mode == Mode::Budget && config.budget == 0 {
        config.mode = Mode::None;
        config.level.clear();
    }
    if has_levels && config.mode == Mode::Level && !level_supported(&config.level, &support.levels) {
        if allow_clamp {
            config.level = clamp_level(&config.level, support);
        }
        if !level_supported(&config.level, &support.levels) {
            let valid: Vec<String> = support.levels.iter().map(|l| l.trim().to_ascii_lowercase()).collect();
            return Err(ThinkingError(format!(
                "level {:?} not supported, valid levels: {}",
                config.level.to_ascii_lowercase(),
                valid.join(", ")
            )));
        }
    }
    if strict_budget && config.mode == Mode::Budget && !from_level && (support.min != 0 || support.max != 0) {
        let out =
            config.budget < support.min || config.budget > support.max || (config.budget == 0 && !support.zero_allowed);
        if out {
            return Err(ThinkingError(format!(
                "budget {} out of range [{},{}]",
                config.budget, support.min, support.max
            )));
        }
    }
    if config.mode == Mode::Auto && !support.dynamic_allowed {
        if has_levels && support.min == 0 && support.max == 0 {
            config = Config::level("medium");
        } else {
            let mid = (support.min + support.max) / 2;
            config = if mid <= 0 && support.zero_allowed {
                Config::none()
            } else if mid <= 0 {
                Config::budget(support.min)
            } else {
                Config::budget(mid)
            };
        }
        if config.mode == Mode::Level && has_levels && !level_supported(&config.level, &support.levels) {
            config.level = clamp_level(&config.level, support);
        }
    }
    if config.mode == Mode::None && to == "claude" {
        config.budget = 0;
        config.level.clear();
    } else {
        if matches!(config.mode, Mode::Budget | Mode::Auto | Mode::None) {
            config.budget = clamp_budget(config.budget, support);
        }
        let cannot_disable = !support.zero_allowed && !level_supported("none", &support.levels);
        if config.mode == Mode::None && has_levels && (config.budget > 0 || cannot_disable) {
            config.level = support.levels[0].clone();
        }
    }
    Ok(config)
}

fn apply_provider(
    body: &str,
    target: &str,
    config: &Config,
    info: Option<&ModelInfo>,
) -> Result<String, ThinkingError> {
    match target {
        "codex" => Ok(apply_codex(body, config, info)),
        _ => apply_kimi(body, config, info),
    }
}

fn apply_kimi(body: &str, config: &Config, info: Option<&ModelInfo>) -> Result<String, ThinkingError> {
    let user_defined = info.is_none();
    if !user_defined && info.and_then(|i| i.thinking.as_ref()).is_none() {
        return Ok(body.to_owned());
    }
    let body = if body.is_empty() || !valid(body) { "{}" } else { body };
    let effort = match config.mode {
        Mode::Level if config.level.is_empty() => return Ok(body.to_owned()),
        Mode::Level => config.level.clone(),
        Mode::None if config.level.is_empty() || config.level == "none" => return Ok(kimi_disabled(body)),
        Mode::None => config.level.clone(),
        Mode::Budget => match budget_to_level(config.budget) {
            Some(level) => level.to_owned(),
            None => return Ok(body.to_owned()),
        },
        Mode::Auto => "auto".to_owned(),
    };
    if effort.is_empty() {
        return Ok(body.to_owned());
    }
    let result = delete(body, "reasoning_effort");
    let result = set_str(&result, "thinking.type", "enabled")
        .map_err(|e| ThinkingError(format!("kimi thinking: failed to set thinking.type: {e}")))?;
    set_str(&result, "thinking.effort", &effort)
        .map_err(|e| ThinkingError(format!("kimi thinking: failed to set thinking.effort: {e}")))
}

fn kimi_disabled(body: &str) -> String {
    let result = delete(body, "thinking");
    let result = delete(&result, "reasoning_effort");
    set_str(&result, "thinking.type", "disabled").unwrap_or(result)
}

fn apply_codex(body: &str, config: &Config, info: Option<&ModelInfo>) -> String {
    let user_defined = info.is_none();
    if !user_defined
        && (info.and_then(|i| i.thinking.as_ref()).is_none() || !matches!(config.mode, Mode::Level | Mode::None))
    {
        return body.to_owned();
    }
    let body = if body.is_empty() || !valid(body) { "{}" } else { body };
    let set_effort = |effort: &str| set_str(body, "reasoning.effort", effort).unwrap_or_else(|_| body.to_owned());
    if user_defined {
        let effort = match config.mode {
            Mode::Level if config.level.is_empty() => return body.to_owned(),
            Mode::Level => config.level.clone(),
            Mode::None if config.level.is_empty() => "none".to_owned(),
            Mode::None => config.level.clone(),
            Mode::Auto => "auto".to_owned(),
            Mode::Budget => match budget_to_level(config.budget) {
                Some(level) => level.to_owned(),
                None => return body.to_owned(),
            },
        };
        return set_effort(&effort);
    }
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        return body.to_owned();
    };
    match config.mode {
        Mode::Level => set_effort(&config.level),
        Mode::None => {
            let mut effort = String::new();
            if config.budget == 0 && (support.zero_allowed || level_supported("none", &support.levels)) {
                effort = "none".into();
            }
            if effort.is_empty() && !config.level.is_empty() {
                effort = config.level.clone();
            }
            if effort.is_empty()
                && let Some(first) = support.levels.first()
            {
                effort = first.clone();
            }
            if effort.is_empty() {
                body.to_owned()
            } else {
                set_effort(&effort)
            }
        }
        _ => body.to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SummaryMode {
    #[default]
    Unspecified,
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Copy, Default)]
struct Summary {
    mode: SummaryMode,
    detail: &'static str,
}

fn summary_enabled(detail: &str) -> Summary {
    let detail = match detail {
        "concise" => "concise",
        "detailed" => "detailed",
        _ => "auto",
    };
    Summary {
        mode: SummaryMode::Enabled,
        detail,
    }
}

fn summary_disabled() -> Summary {
    Summary {
        mode: SummaryMode::Disabled,
        detail: "",
    }
}

fn bool_summary(body: &str, path: &str) -> Option<Summary> {
    match get(body, path).kind() {
        gjson::Kind::True => Some(summary_enabled("auto")),
        gjson::Kind::False => Some(summary_disabled()),
        _ => None,
    }
}

fn responses_summary(body: &str, path: &str) -> Option<Summary> {
    let value = get(body, path);
    if !value.exists() {
        return None;
    }
    match value.kind() {
        gjson::Kind::Null => Some(summary_disabled()),
        gjson::Kind::String => match gstr(&value).trim().to_ascii_lowercase().as_str() {
            raw @ ("auto" | "concise" | "detailed") => Some(summary_enabled(raw)),
            "none" => Some(summary_disabled()),
            _ => None,
        },
        _ => None,
    }
}

fn string_summary(body: &str, path: &str, on: &str, off: &str) -> Option<Summary> {
    let value = get(body, path);
    if value.kind() != gjson::Kind::String {
        return None;
    }
    let raw = gstr(&value).trim().to_ascii_lowercase();
    if raw == on {
        Some(summary_enabled("auto"))
    } else if raw == off {
        Some(summary_disabled())
    } else {
        None
    }
}

fn claude_accepts_display(body: &str) -> bool {
    match gstr(&get(body, "thinking.type")).trim().to_ascii_lowercase().as_str() {
        "adaptive" => true,
        "enabled" => {
            let budget = get(body, "thinking.budget_tokens");
            budget.kind() != gjson::Kind::Number || budget.i64() == -1 || budget.i64() > 0
        }
        _ => false,
    }
}

fn openai_explicit_summary(body: &str) -> Option<Summary> {
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
        if let Some(summary) = bool_summary(body, path) {
            return Some(summary);
        }
    }
    for path in ["reasoning.summary", "reasoning.generate_summary"] {
        if let Some(summary) = responses_summary(body, path) {
            return Some(summary);
        }
    }
    for (path, inverted) in [
        ("reasoning.exclude", true),
        ("include_reasoning", false),
        ("reasoning.enabled", false),
    ] {
        let value = get(body, path);
        if matches!(value.kind(), gjson::Kind::True | gjson::Kind::False) {
            let on = (value.kind() == gjson::Kind::True) != inverted;
            return Some(if on {
                summary_enabled("auto")
            } else {
                summary_disabled()
            });
        }
    }
    None
}

fn summary_format(format: &str) -> bool {
    matches!(
        format,
        "openai" | "openai-response" | "codex" | "claude" | "gemini" | "antigravity" | "interactions"
    )
}

fn extract_summary(body: &str, format: &str) -> Summary {
    let format = format.trim().to_ascii_lowercase();
    if !summary_format(&format) || body.is_empty() || !valid(body) {
        return Summary::default();
    }
    let found = match format.as_str() {
        "openai" => openai_explicit_summary(body).or_else(|| {
            let effort = get(body, "reasoning_effort");
            if effort.kind() != gjson::Kind::String {
                return None;
            }
            match gstr(&effort).trim().to_ascii_lowercase().as_str() {
                "" => Some(Summary::default()),
                "none" => Some(summary_disabled()),
                _ => Some(summary_enabled("auto")),
            }
        }),
        "openai-response" | "codex" => responses_summary(body, "reasoning.summary")
            .or_else(|| responses_summary(body, "reasoning.generate_summary")),
        "claude" if claude_accepts_display(body) => string_summary(body, "thinking.display", "summarized", "omitted"),
        "gemini" => [
            "generationConfig.thinkingConfig.includeThoughts",
            "generationConfig.thinkingConfig.include_thoughts",
            "generation_config.thinking_config.include_thoughts",
            "generation_config.thinking_config.includeThoughts",
        ]
        .iter()
        .find_map(|p| bool_summary(body, p)),
        "antigravity" => [
            "request.generationConfig.thinkingConfig.includeThoughts",
            "request.generationConfig.thinkingConfig.include_thoughts",
            "request.generationConfig.thinking_config.includeThoughts",
            "request.generationConfig.thinking_config.include_thoughts",
        ]
        .iter()
        .find_map(|p| bool_summary(body, p)),
        "interactions" => string_summary(body, "generation_config.thinking_summaries", "auto", "none")
            .or_else(|| string_summary(body, "generation_config.thinkingSummaries", "auto", "none"))
            .or_else(|| string_summary(body, "reasoning.summary", "auto", "none"))
            .or_else(|| {
                [
                    "generation_config.thinking_config.include_thoughts",
                    "generation_config.thinking_config.includeThoughts",
                    "generation_config.thinkingConfig.include_thoughts",
                    "generation_config.thinkingConfig.includeThoughts",
                ]
                .iter()
                .find_map(|p| bool_summary(body, p))
            }),
        _ => None,
    };
    found.unwrap_or_default()
}

fn extract_explicit_summary(body: &str, format: &str) -> Summary {
    let format = format.trim().to_ascii_lowercase();
    if format != "openai" {
        return extract_summary(body, &format);
    }
    if body.is_empty() || !valid(body) {
        return Summary::default();
    }
    openai_explicit_summary(body).unwrap_or_default()
}

fn extract_translated_summary(body: &str, from: &str, to: &str) -> Summary {
    let from = from.trim().to_ascii_lowercase();
    if to.trim().eq_ignore_ascii_case("claude") && from == "openai" {
        return extract_explicit_summary(body, &from);
    }
    extract_summary(body, &from)
}

/// Go's registered request translators, restricted to the targets this pipeline serves.
fn has_request_transformer(from: &str, to: &str) -> bool {
    to == "codex"
        && matches!(
            from,
            "openai" | "openai-response" | "claude" | "gemini" | "interactions" | "codex"
        )
}

fn translated_summary(body: &str, current: &str, original: &str, model: &str, from: &str, to: &str) -> Summary {
    let from = from.trim().to_ascii_lowercase();
    let to = to.trim().to_ascii_lowercase();
    let target = if from == to {
        extract_summary(body, &to)
    } else {
        extract_explicit_summary(body, &to)
    };
    if target.mode != SummaryMode::Unspecified {
        return target;
    }
    let current_summary = extract_translated_summary(current, &from, &to);
    let original_summary = extract_translated_summary(original, &from, &to);
    if current_summary.mode == SummaryMode::Unspecified {
        return original_summary;
    }
    if !has_request_transformer(&from, &to) {
        return Summary::default();
    }
    let _ = model;
    let candidate = apply_summary(body, &to, current_summary);
    if extract_explicit_summary(&candidate, &to).mode != SummaryMode::Unspecified {
        return Summary::default();
    }
    current_summary
}

/// `applySummaryConfigForProvider` for the Responses target; other targets are unchanged.
fn apply_summary(body: &str, format: &str, summary: Summary) -> String {
    let format = format.trim().to_ascii_lowercase();
    if summary.mode == SummaryMode::Unspecified
        || !matches!(format.as_str(), "openai-response" | "codex")
        || body.is_empty()
        || !valid(body)
    {
        return body.to_owned();
    }
    if summary.mode == SummaryMode::Enabled {
        let body = set_str(body, "reasoning.summary", summary.detail).unwrap_or_else(|_| body.to_owned());
        return delete(&body, "reasoning.generate_summary");
    }
    let body = delete(body, "reasoning.summary");
    let body = delete(&body, "reasoning.generate_summary");
    if empty_object(&body, "reasoning") {
        delete(&body, "reasoning")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kimi(body: &str, model: &str) -> Result<String, ThinkingError> {
        apply(body, body, body, model, "openai", "kimi", "kimi")
    }

    #[test]
    fn suffix_wins_and_levels_clamp_like_go() {
        // Expected values from Go fixtures (zz_rsfix_kimi_test.go) and thinking_test.go.
        assert_eq!(
            kimi(r#"{"model":"x","reasoning_effort":"low"}"#, "kimi-k2.8(max)").unwrap(),
            r#"{"model":"x","thinking":{"type":"enabled","effort":"max"}}"#
        );
        // kimi-k2.7-code cannot disable thinking: none clamps to its first level.
        assert_eq!(
            kimi(r#"{"reasoning_effort":"none"}"#, "kimi-k2.7-code").unwrap(),
            r#"{"thinking":{"type":"enabled","effort":"low"}}"#
        );
        assert_eq!(
            kimi(r#"{"reasoning_effort":"none"}"#, "kimi-k2.5").unwrap(),
            r#"{"thinking":{"type":"disabled"}}"#
        );
        // Same family (openai -> kimi target differs) allows clamping xhigh to the nearest level.
        assert_eq!(
            kimi(r#"{"reasoning_effort":"xhigh"}"#, "kimi-k2.5").unwrap(),
            r#"{"thinking":{"type":"enabled","effort":"high"}}"#
        );
        // kimi-k2 has no thinking support: config is stripped.
        assert_eq!(
            kimi(r#"{"a":1,"reasoning_effort":"high"}"#, "kimi-k2").unwrap(),
            r#"{"a":1}"#
        );
        // Unknown models are user-defined: applied without validation.
        assert_eq!(
            kimi(r#"{"reasoning_effort":"ultra"}"#, "kimi-custom").unwrap(),
            r#"{"thinking":{"type":"enabled","effort":"ultra"}}"#
        );
        // Budget suffix converts to a level.
        assert_eq!(
            kimi("{}", "kimi-k2.5(9000)").unwrap(),
            r#"{"thinking":{"type":"enabled","effort":"high"}}"#
        );
    }

    #[test]
    fn unsupported_level_without_clamp_is_a_400_message() {
        // Same family on both sides (kimi -> kimi, kimi model): Go refuses to clamp.
        let body = r#"{"thinking":{"type":"enabled","effort":"xhigh"}}"#;
        let err = apply(body, body, body, "kimi-k2.5", "kimi", "kimi", "kimi").unwrap_err();
        assert_eq!(err.0, r#"level "xhigh" not supported, valid levels: low, high"#);
        // From OpenAI the families differ, so the same level clamps instead.
        assert!(kimi(r#"{"reasoning_effort":"xhigh"}"#, "kimi-k2.5").is_ok());
    }

    #[test]
    fn codex_target_keeps_summary_and_strips_updates() {
        let body = r#"{"model":"k3","reasoning":{"effort":"low","summary":"auto"},"input":[{"type":"configuration_update","reasoning":{"effort":"max"}}]}"#;
        let out = apply(body, body, body, "kimi-k3(high)", "openai-response", "codex", "kimi").unwrap();
        assert_eq!(
            out,
            r#"{"model":"k3","reasoning":{"effort":"high","summary":"auto"},"input":[]}"#
        );
    }
}
