//! Devin model resolution (helps/devin_models.go). The catalog updater lives in
//! `cpa_server::model_updater`.
//!
//! A client model (`devin/swe-2`, `glm-5-2(max)`, `claude-sonnet-4.5`) and its requested
//! effort become the upstream `chat_model_uid` (`swe-2-high`, `glm-5-2-max`,
//! `MODEL_PRIVATE_3`), clamped to the levels the Devin catalog lists.

use cpa_common::gostr::GoStr;
use cpa_common::thinking::parse_suffix;
use cpa_core::registry::devin;

const KNOWN_SUFFIXES: [&str; 32] = [
    "-none",
    "-low",
    "-medium",
    "-high",
    "-xhigh",
    "-max",
    "-fast",
    "-slow",
    "-priority",
    "-low-priority",
    "-medium-priority",
    "-high-priority",
    "-xhigh-priority",
    "-max-priority",
    "-low-fast",
    "-medium-fast",
    "-high-fast",
    "-xhigh-fast",
    "-max-fast",
    "-none-fast",
    "-thinking-1m",
    "-thinking",
    "-max-1m",
    "-none-1m",
    "_none",
    "_minimal",
    "_low",
    "_medium",
    "_high",
    "_xhigh",
    "_max",
    "_thinking",
];

/// `HasDevinEffortSuffix`.
fn has_effort_suffix(model: &str) -> bool {
    let lower = model.trim().go_lower();
    KNOWN_SUFFIXES.iter().any(|s| lower.ends_with(s))
}

/// `NormalizeThinkingLevel`: canonical effort from a level name or a token budget.
pub(crate) fn normalize_level(level: &str, budget: i64) -> String {
    let normalized = level.trim().go_lower();
    match normalized.as_str() {
        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "fast" => return normalized,
        "none" | "off" | "disabled" => return "none".into(),
        "auto" | "adaptive" => return "high".into(),
        _ => {}
    }
    match budget {
        b if b <= 0 => String::new(),
        b if b <= 4096 => "low".into(),
        b if b <= 16384 => "medium".into(),
        b if b <= 32768 => "high".into(),
        _ => "max".into(),
    }
}

/// `devinLevelIndex`.
fn level_index(level: &str) -> Option<usize> {
    let lower = level.trim().go_lower();
    ["minimal", "low", "medium", "high", "xhigh", "max"]
        .iter()
        .position(|l| *l == lower)
}

/// `clampEffort`: the requested level if allowed, else the nearest allowed standard
/// level (ties go to the higher one), else the default.
fn clamp_effort(requested: &str, allowed: &[String], default: &str) -> String {
    if requested.is_empty() {
        return default.to_owned();
    }
    let req = requested.trim().go_lower();
    if let Some(a) = allowed.iter().find(|a| a.trim().go_lower() == req) {
        return a.clone();
    }
    if req == "none" {
        return default.to_owned();
    }
    let Some(req_index) = level_index(&req) else {
        return default.to_owned();
    };
    let mut best = default.to_owned();
    let (mut best_dist, mut best_index) = (usize::MAX, None);
    for a in allowed {
        let Some(index) = level_index(a) else {
            continue;
        };
        let dist = req_index.abs_diff(index);
        if dist < best_dist || (dist == best_dist && Some(index) > best_index) {
            best_dist = dist;
            best.clone_from(a);
            best_index = Some(index);
        }
    }
    best
}

/// `selectDefaultDevinEffort`.
fn default_effort(base: &str, levels: &[String]) -> String {
    if base.contains("swe-2") {
        return "high".into();
    }
    let has = |l: &str| levels.iter().any(|x| x == l);
    if has("none") && has("low") && base.starts_with("gpt-5") {
        return "low".into();
    }
    let high_family = ["gemini", "grok", "glm", "deepseek", "kimi", "nemotron"]
        .iter()
        .any(|f| base.contains(f));
    if has("high") && high_family {
        return "high".into();
    }
    for l in ["medium", "high", "low"] {
        if has(l) {
            return l.into();
        }
    }
    levels[0].clone()
}

/// `ResolveDevinChatModelUID`.
pub(crate) fn resolve_chat_model_uid(raw_model: &str, thinking_level: &str, budget: i64) -> String {
    resolve_in(devin::global(), raw_model, thinking_level, budget)
}

/// [`resolve_chat_model_uid`] against `catalog`.
pub(crate) fn resolve_in(catalog: &devin::Store, raw_model: &str, thinking_level: &str, budget: i64) -> String {
    let model = raw_model.trim();
    if model.is_empty() {
        return "swe-2-high".into();
    }
    let clean = if model.go_lower().starts_with("devin/") {
        &model[6..]
    } else {
        model
    };
    if has_effort_suffix(clean) {
        return clean.to_owned();
    }
    let parsed = parse_suffix(clean);
    let mut base = parsed.model_name.trim().to_owned();
    let mut level = thinking_level.to_owned();
    if parsed.has_suffix {
        level = parsed.raw_suffix;
    } else if let Some(colon) = clean.rfind(':') {
        base = clean[..colon].trim().to_owned();
        level = clean[colon + 1..].trim().to_owned();
    }
    let effort = normalize_level(&level, budget);
    let lower = base.go_lower();
    let mut canonical = lower.replace('.', "-");
    match canonical.as_str() {
        "claude-haiku-4-5" => return "MODEL_PRIVATE_11".into(),
        "gpt-4-1" => return "MODEL_CHAT_GPT_4_1_2025_04_14".into(),
        _ => {}
    }
    let thinking_on = !effort.is_empty() && effort != "none";
    if canonical == "claude-sonnet-4-5" || canonical.contains("sonnet-4-5") {
        return if thinking_on {
            "MODEL_PRIVATE_3"
        } else {
            "MODEL_PRIVATE_2"
        }
        .into();
    }
    if canonical == "gemini-3-flash" {
        canonical = "gemini-3-8-flash".into();
    }
    let levels = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    match canonical.replace('-', "_").as_str() {
        "model_gpt_5_2" => {
            let e = clamp_effort(&effort, &levels(&["none", "low", "medium", "high", "xhigh"]), "low");
            return format!("MODEL_GPT_5_2_{}", e.go_upper());
        }
        "model_google_gemini_3_0_flash" => {
            let e = clamp_effort(&effort, &levels(&["minimal", "low", "medium", "high"]), "high");
            return format!("MODEL_GOOGLE_GEMINI_3_0_FLASH_{}", e.go_upper());
        }
        "model_claude_4_5_opus" => {
            return if thinking_on {
                "MODEL_CLAUDE_4_5_OPUS_THINKING"
            } else {
                "MODEL_CLAUDE_4_5_OPUS"
            }
            .into();
        }
        _ => {}
    }
    let info = catalog
        .lookup(&canonical)
        .or_else(|| (canonical != lower).then(|| catalog.lookup(&lower)).flatten());
    let allowed: Vec<String> = info.and_then(|m| m.thinking).map(|t| t.levels).unwrap_or_default();
    match canonical.as_str() {
        "swe-1-7" => {
            return if effort == "medium" {
                "swe-1-7-medium"
            } else {
                "swe-1-7"
            }
            .into();
        }
        "swe-1-6" => {
            return if effort == "fast" { "swe-1-6-fast" } else { "swe-1-6" }.into();
        }
        "glm-5-2" => {
            return match effort.as_str() {
                "none" => "glm-5-2-none",
                "max" => "glm-5-2-max",
                _ => "glm-5-2",
            }
            .into();
        }
        "glm-5-2-1m" => {
            return match effort.as_str() {
                "none" => "glm-5-2-none-1m",
                "max" => "glm-5-2-max-1m",
                _ => "glm-5-2-1m",
            }
            .into();
        }
        "claude-opus-4-6" | "claude-sonnet-4-6" => {
            return if thinking_on {
                format!("{canonical}-thinking")
            } else {
                canonical
            };
        }
        "claude-opus-4-6-1m" => {
            return if thinking_on {
                "claude-opus-4-6-thinking-1m".into()
            } else {
                canonical
            };
        }
        "claude-sonnet-4-6-1m" => {
            return if thinking_on {
                "claude-sonnet-4-6-thinking-1m".into()
            } else {
                canonical
            };
        }
        _ => {}
    }
    if allowed.is_empty() {
        return canonical;
    }
    let default = default_effort(&canonical, &allowed);
    let clamped = clamp_effort(&effort, &allowed, &default);
    format!("{canonical}-{clamped}")
}
