//! Capability validation and normalization (validate.go).

use crate::gostr::GoStr;
use cpa_core::registry::ThinkingSupport;

use super::{
    Capability, Config, Error, ErrorCode, LEVEL_AUTO, LEVEL_HIGH, LEVEL_LOW, LEVEL_MAX, LEVEL_MEDIUM, LEVEL_MINIMAL,
    LEVEL_NONE, LEVEL_XHIGH, Mode, ModelCaps, convert_budget_to_level, convert_level_to_budget, detect_capability,
};

/// `ValidateConfig`: converts between budget and level forms by model capability,
/// clamps to the supported range and rejects what cannot be represented. Budgets are
/// strict only for same-family requests without a suffix.
pub fn validate_config(
    mut config: Config,
    info: Option<&ModelCaps>,
    from: &str,
    to: &str,
    from_suffix: bool,
) -> Result<Config, Error> {
    let from = from.trim().go_lower();
    let to = to.trim().go_lower();
    let model = info
        .map(|i| i.id.as_str())
        .filter(|id| !id.is_empty())
        .unwrap_or("unknown");
    let Some(support) = info.and_then(|i| i.thinking.as_ref()) else {
        if config.mode != Mode::None {
            return Err(Error {
                code: Some(ErrorCode::ThinkingNotSupported),
                message: "thinking not supported for this model".into(),
                model: model.into(),
                body: None,
            });
        }
        return Ok(config);
    };
    let capability = detect_capability(info);
    let has_levels = matches!(capability, Capability::LevelOnly | Capability::Hybrid);
    let family_mismatch = info.is_some_and(|i| {
        let kind = i.kind.trim().go_lower();
        !kind.is_empty()
            && ((!from.is_empty() && !is_same_provider_family(&from, &kind))
                || (!to.is_empty() && !is_same_provider_family(&to, &kind)))
    });
    let allow_clamp_unsupported = has_levels && (!is_same_provider_family(&from, &to) || family_mismatch);
    let strict_budget = !from_suffix && !from.is_empty() && is_same_provider_family(&from, &to) && !family_mismatch;
    let mut budget_from_level = false;

    match capability {
        Capability::BudgetOnly if config.mode == Mode::Level && config.level != LEVEL_AUTO => {
            let Some(budget) = convert_level_to_budget(&config.level) else {
                return Err(Error::new(
                    ErrorCode::UnknownLevel,
                    format!("unknown level: {}", config.level),
                ));
            };
            config = Config::budget(budget);
            budget_from_level = true;
        }
        Capability::LevelOnly if config.mode == Mode::Budget => {
            let Some(level) = convert_budget_to_level(config.budget) else {
                return Err(Error::new(
                    ErrorCode::UnknownLevel,
                    format!("budget {} cannot be converted to a valid level", config.budget),
                ));
            };
            config = Config::level(clamp_level(level, info));
        }
        _ => {}
    }

    if config.mode == Mode::Level && config.level == LEVEL_NONE {
        config = Config::none();
    }
    if config.mode == Mode::Level && config.level == LEVEL_AUTO {
        config = Config::auto();
    }
    if config.mode == Mode::Budget && config.budget == 0 {
        config.mode = Mode::None;
        config.level.clear();
    }

    if !support.levels.is_empty() && config.mode == Mode::Level && !is_level_supported(&config.level, &support.levels) {
        if allow_clamp_unsupported {
            config.level = clamp_level(&config.level, info);
        }
        if !is_level_supported(&config.level, &support.levels) {
            let valid: Vec<String> = support.levels.iter().map(|l| l.trim().go_lower()).collect();
            return Err(Error::new(
                ErrorCode::LevelNotSupported,
                format!(
                    "level {} not supported, valid levels: {}",
                    crate::gostr::quote(config.level.go_lower()),
                    valid.join(", ")
                ),
            ));
        }
    }

    if strict_budget && config.mode == Mode::Budget && !budget_from_level {
        let (min, max) = (support.min, support.max);
        if (min != 0 || max != 0)
            && (config.budget < min || config.budget > max || (config.budget == 0 && !support.zero_allowed))
        {
            return Err(Error::new(
                ErrorCode::BudgetOutOfRange,
                format!("budget {} out of range [{min},{max}]", config.budget),
            ));
        }
    }

    if config.mode == Mode::Auto && !support.dynamic_allowed {
        config = auto_to_mid_range(config, support);
        if config.mode == Mode::Level
            && !support.levels.is_empty()
            && !is_level_supported(&config.level, &support.levels)
        {
            config.level = clamp_level(&config.level, info);
        }
    }

    if config.mode == Mode::None && to == "claude" {
        config.budget = 0;
        config.level.clear();
    } else {
        if matches!(config.mode, Mode::Budget | Mode::Auto | Mode::None) {
            config.budget = clamp_budget(config.budget, support);
        }
        let cannot_disable = !support.zero_allowed && !is_level_supported(LEVEL_NONE, &support.levels);
        if config.mode == Mode::None && !support.levels.is_empty() && (config.budget > 0 || cannot_disable) {
            config.level.clone_from(&support.levels[0]);
        }
    }
    Ok(config)
}

fn auto_to_mid_range(mut config: Config, support: &ThinkingSupport) -> Config {
    if !support.levels.is_empty() && support.min == 0 && support.max == 0 {
        return Config::level(LEVEL_MEDIUM);
    }
    let mid = (support.min + support.max) / 2;
    if mid <= 0 && support.zero_allowed {
        config.mode = Mode::None;
        config.budget = 0;
    } else if mid <= 0 {
        config.mode = Mode::Budget;
        config.budget = support.min;
    } else {
        config.mode = Mode::Budget;
        config.budget = mid;
    }
    config
}

const STANDARD_LEVELS: [&str; 6] = [
    LEVEL_MINIMAL,
    LEVEL_LOW,
    LEVEL_MEDIUM,
    LEVEL_HIGH,
    LEVEL_XHIGH,
    LEVEL_MAX,
];

/// `clampLevel`: the nearest supported standard level, the lower on a tie.
fn clamp_level(level: &str, info: Option<&ModelCaps>) -> String {
    let supported = info
        .and_then(|i| i.thinking.as_ref())
        .map(|s| s.levels.as_slice())
        .unwrap_or(&[]);
    if supported.is_empty() || is_level_supported(level, supported) {
        return level.to_owned();
    }
    let Some(pos) = level_index(level) else {
        return level.to_owned();
    };
    let mut best: Option<(usize, usize)> = None;
    for s in supported {
        if let Some(idx) = level_index(s.trim()) {
            let dist = pos.abs_diff(idx);
            if best.is_none_or(|(best_idx, best_dist)| dist < best_dist || (dist == best_dist && idx < best_idx)) {
                best = Some((idx, dist));
            }
        }
    }
    best.map_or(level.to_owned(), |(idx, _)| STANDARD_LEVELS[idx].to_owned())
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
        if value == 0 && support.zero_allowed {
            return 0;
        }
        return min;
    }
    value.min(max)
}

pub(crate) fn is_level_supported(level: &str, supported: &[String]) -> bool {
    supported.iter().any(|s| level.go_eq_fold(s.trim()))
}

fn level_index(level: &str) -> Option<usize> {
    STANDARD_LEVELS.iter().position(|l| level.go_eq_fold(l))
}

pub(crate) fn is_budget_capable_provider(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity" | "claude")
}

fn is_gemini_family(provider: &str) -> bool {
    matches!(provider, "gemini" | "antigravity")
}

fn is_openai_family(provider: &str) -> bool {
    matches!(provider, "openai" | "openai-response" | "codex")
}

pub(crate) fn is_same_provider_family(from: &str, to: &str) -> bool {
    from == to || (is_gemini_family(from) && is_gemini_family(to)) || (is_openai_family(from) && is_openai_family(to))
}
