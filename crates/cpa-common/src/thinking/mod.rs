//! Unified thinking/reasoning configuration (internal/thinking): parse the model suffix
//! (suffix wins over the body), extract a canonical [`Config`] from the source format,
//! validate it against the model's capabilities, then let the target provider's applier
//! write its own fields. Summary (visibility) intent is carried separately.
//!
//! Formats and providers are Go's lowercase identifiers (`openai`, `openai-response`,
//! `codex`, `claude`, `gemini`, `antigravity`, `interactions`, `kimi`, `xai`).
//!
//! ponytail: Go's plugin-registered appliers (RegisterPluginProvider, M6) are not
//! ported; only the built-in appliers exist. Debug logging is omitted.

use crate::gostr::GoStr;
mod apply;
mod providers;
mod summary;
mod validate;

pub use apply::*;
pub use providers::apply_provider;
pub use summary::*;
pub use validate::validate_config;

use cpa_core::registry::{ModelInfo, ThinkingSupport};
use serde_json::Value as Json;

use crate::json;

/// `ThinkingMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Budget,
    Level,
    None,
    Auto,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::Level => "level",
            Self::None => "none",
            Self::Auto => "auto",
        }
    }
}

pub const LEVEL_NONE: &str = "none";
pub const LEVEL_AUTO: &str = "auto";
pub const LEVEL_MINIMAL: &str = "minimal";
pub const LEVEL_LOW: &str = "low";
pub const LEVEL_MEDIUM: &str = "medium";
pub const LEVEL_HIGH: &str = "high";
pub const LEVEL_XHIGH: &str = "xhigh";
pub const LEVEL_MAX: &str = "max";

/// `ThinkingConfig`. The zero value (budget mode, budget 0, no level) means "no config".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Config {
    pub mode: Mode,
    pub budget: i64,
    /// Free-form like Go's `ThinkingLevel`; validation decides whether it is known.
    ///
    /// ponytail: a String, so a level read from invalid UTF-8 is decoded lossily. Go
    /// writes such bytes back as `\ufffd` escapes, this writes U+FFFD itself: the same
    /// JSON value with different bytes. Use bytes here if a caller needs exact garbage.
    pub level: String,
}

impl Config {
    pub fn none() -> Self {
        Self {
            mode: Mode::None,
            ..Self::default()
        }
    }
    pub fn auto() -> Self {
        Self {
            mode: Mode::Auto,
            budget: -1,
            level: String::new(),
        }
    }
    pub fn level(level: impl Into<String>) -> Self {
        Self {
            mode: Mode::Level,
            budget: 0,
            level: level.into(),
        }
    }
    pub fn budget(budget: i64) -> Self {
        Self {
            mode: Mode::Budget,
            budget,
            level: String::new(),
        }
    }
    /// `hasThinkingConfig`.
    pub fn is_set(&self) -> bool {
        self.mode != Mode::Budget || self.budget != 0 || !self.level.is_empty()
    }
}

/// `ErrorCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidSuffix,
    UnknownLevel,
    ThinkingNotSupported,
    LevelNotSupported,
    BudgetOutOfRange,
    ProviderMismatch,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidSuffix => "INVALID_SUFFIX",
            Self::UnknownLevel => "UNKNOWN_LEVEL",
            Self::ThinkingNotSupported => "THINKING_NOT_SUPPORTED",
            Self::LevelNotSupported => "LEVEL_NOT_SUPPORTED",
            Self::BudgetOutOfRange => "BUDGET_OUT_OF_RANGE",
            Self::ProviderMismatch => "PROVIDER_MISMATCH",
        }
    }
}

/// A thinking failure. `code` is set for Go's `ThinkingError` (HTTP 400); applier
/// failures without a code are plain errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub code: Option<ErrorCode>,
    pub message: String,
    pub model: String,
    /// The processed body Go returns alongside the error (ApplyThinking returns the target
    /// with configuration updates already stripped). Go's executors discard it.
    pub body: Option<Vec<u8>>,
}

impl Error {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            message: message.into(),
            model: String::new(),
            body: None,
        }
    }
    /// HTTP status for the client, as `ThinkingError.StatusCode()`.
    pub fn status(&self) -> u16 {
        if self.code.is_some() { 400 } else { 500 }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The registry fields thinking reads from Go's `registry.ModelInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelCaps {
    pub id: String,
    /// Go's `Type`.
    pub kind: String,
    pub thinking: Option<ThinkingSupport>,
    /// Models from config `models[]` arrays: thinking passes through unvalidated.
    pub user_defined: bool,
    pub support_configuration_update: bool,
    pub max_completion_tokens: i64,
}

impl From<&ModelInfo> for ModelCaps {
    fn from(info: &ModelInfo) -> Self {
        Self {
            id: info.id.clone(),
            kind: info.kind.clone(),
            thinking: info.thinking.clone(),
            user_defined: info.raw.get("user_defined") == Some(&Json::Bool(true)),
            support_configuration_update: info.raw.get("support_configuration_update") == Some(&Json::Bool(true)),
            max_completion_tokens: info
                .raw
                .get("max_completion_tokens")
                .and_then(Json::as_i64)
                .unwrap_or(0),
        }
    }
}

/// `registry.LookupModelInfo(model, provider)`: the server's dynamic registry
/// (provider-specific registration, then the last registration), then the pinned catalog.
pub fn lookup_model_info(model: &str, provider: &str) -> Option<ModelCaps> {
    #[cfg(test)]
    if let Some(found) = tests::lookup_override(model, provider) {
        return found;
    }
    let provider = Some(provider).filter(|p| !p.is_empty());
    cpa_core::registry::lookup_model(model, provider).map(|info| ModelCaps::from(&info))
}

/// `SuffixResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuffixResult {
    pub model_name: String,
    pub has_suffix: bool,
    pub raw_suffix: String,
}

/// `ParseSuffix`: `model(value)` splits at the last `(` when the name ends with `)`.
pub fn parse_suffix(model: &str) -> SuffixResult {
    match model.rfind('(') {
        Some(open) if model.ends_with(')') => SuffixResult {
            model_name: model[..open].to_owned(),
            has_suffix: true,
            raw_suffix: model[open + 1..model.len() - 1].to_owned(),
        },
        _ => SuffixResult {
            model_name: model.to_owned(),
            has_suffix: false,
            raw_suffix: String::new(),
        },
    }
}

/// `ParseNumericSuffix`: `strconv.Atoi` (optional sign, decimal), negatives rejected.
pub fn parse_numeric_suffix(raw: &str) -> Option<i64> {
    raw.parse::<i64>().ok().filter(|v| *v >= 0)
}

/// `ParseSpecialSuffix`: `none`, `auto` or `-1`.
pub fn parse_special_suffix(raw: &str) -> Option<Mode> {
    match raw.go_lower().as_str() {
        "none" => Some(Mode::None),
        "auto" | "-1" => Some(Mode::Auto),
        _ => None,
    }
}

/// `ParseLevelSuffix`: one of the discrete effort levels.
pub fn parse_level_suffix(raw: &str) -> Option<&'static str> {
    match raw.go_lower().as_str() {
        "minimal" => Some(LEVEL_MINIMAL),
        "low" => Some(LEVEL_LOW),
        "medium" => Some(LEVEL_MEDIUM),
        "high" => Some(LEVEL_HIGH),
        "xhigh" => Some(LEVEL_XHIGH),
        "max" => Some(LEVEL_MAX),
        _ => None,
    }
}

/// `ConvertLevelToBudget`.
pub fn convert_level_to_budget(level: &str) -> Option<i64> {
    match level.go_lower().as_str() {
        "none" => Some(0),
        "auto" => Some(-1),
        "minimal" => Some(512),
        "low" => Some(1024),
        "medium" => Some(8192),
        "high" => Some(24576),
        "xhigh" => Some(32768),
        "max" => Some(128000),
        _ => None,
    }
}

/// `ConvertBudgetToLevel`.
pub fn convert_budget_to_level(budget: i64) -> Option<&'static str> {
    Some(match budget {
        ..-1 => return None,
        -1 => LEVEL_AUTO,
        0 => LEVEL_NONE,
        1..=512 => LEVEL_MINIMAL,
        513..=1024 => LEVEL_LOW,
        1025..=8192 => LEVEL_MEDIUM,
        8193..=24576 => LEVEL_HIGH,
        _ => LEVEL_XHIGH,
    })
}

/// `HasLevel`.
pub fn has_level(levels: &[String], target: &str) -> bool {
    levels.iter().any(|l| l.trim().go_eq_fold(target))
}

/// `MapToClaudeEffort`.
pub fn map_to_claude_effort(level: &str, supports_max: bool) -> Option<&'static str> {
    match level.trim().go_lower().as_str() {
        "minimal" => Some("low"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" => Some("high"),
        "xhigh" | "max" => Some(if supports_max { "max" } else { "high" }),
        "auto" => Some("high"),
        _ => None,
    }
}

/// `ModelCapability`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capability {
    Unknown,
    None,
    BudgetOnly,
    LevelOnly,
    Hybrid,
}

pub(crate) fn detect_capability(info: Option<&ModelCaps>) -> Capability {
    let Some(info) = info else { return Capability::Unknown };
    let Some(support) = &info.thinking else {
        return Capability::None;
    };
    let budget = support.min > 0 || support.max > 0;
    let levels = !support.levels.is_empty();
    match (budget, levels) {
        (true, true) => Capability::Hybrid,
        (true, false) => Capability::BudgetOnly,
        (false, true) => Capability::LevelOnly,
        (false, false) => Capability::None,
    }
}

/// `IsUserDefinedModel`: unknown models count as user-defined.
pub fn is_user_defined_model(info: Option<&ModelCaps>) -> bool {
    info.is_none_or(|i| i.user_defined)
}

/// `GetThinkingText`: a thinking block's text from `text`, `thinking` or a nested object.
pub fn get_thinking_text(part: &json::Res<'_>) -> Vec<u8> {
    crate::signature::thinking_block_text(part)
}

/// `StripThinkingConfig`: removes the provider's thinking fields.
pub fn strip_thinking_config(body: &[u8], provider: &str) -> Vec<u8> {
    if body.is_empty() || !json::valid(body) {
        return body.to_vec();
    }
    let paths: &[&str] = match provider {
        "claude" => &["thinking", "output_config.effort"],
        "gemini" => &["generationConfig.thinkingConfig"],
        "antigravity" => &["request.generationConfig.thinkingConfig"],
        "interactions" => &[
            "generation_config.thinking_level",
            "generation_config.thinkingLevel",
            "generation_config.thinking_budget",
            "generation_config.thinkingBudget",
            "generation_config.thinking_summaries",
            "generation_config.thinkingSummaries",
            "generation_config.thinking_config",
            "generation_config.thinkingConfig",
        ],
        "openai" => &["reasoning_effort", "reasoning"],
        "kimi" | "kimi-ai" | "kimi.ai" | "kimi.com" => &["reasoning_effort", "thinking"],
        "codex" | "xai" => &["reasoning"],
        _ => return body.to_vec(),
    };
    let mut result = body.to_vec();
    for path in paths {
        json::delete(&mut result, path);
    }
    if provider == "claude" && is_empty_object(&result, "output_config") {
        json::delete(&mut result, "output_config");
    }
    result
}

/// `sjson.SetBytes(body, path, string)` on a copy, keeping `body` when sjson errors.
pub(crate) fn with_str(body: &[u8], path: &str, value: &str) -> Vec<u8> {
    let mut out = body.to_vec();
    json::set_str(&mut out, path, value);
    out
}

/// `sjson.SetBytes(body, path, int)`.
pub(crate) fn with_int(body: &[u8], path: &str, value: i64) -> Vec<u8> {
    let mut out = body.to_vec();
    json::set_int(&mut out, path, value);
    out
}

/// `sjson.SetBytes(body, path, bool)`.
pub(crate) fn with_bool(body: &[u8], path: &str, value: bool) -> Vec<u8> {
    let mut out = body.to_vec();
    json::set_bool(&mut out, path, value);
    out
}

/// `sjson.DeleteBytes(body, path)`.
pub(crate) fn without(body: &[u8], path: &str) -> Vec<u8> {
    let mut out = body.to_vec();
    json::delete(&mut out, path);
    out
}

/// `result.Exists() && result.IsObject() && len(result.Map()) == 0` at `path`.
pub(crate) fn is_empty_object(body: &[u8], path: &str) -> bool {
    let value = json::get(body, path);
    value.is_object() && value.map().is_empty()
}

pub(crate) fn is_responses_format(format: &str) -> bool {
    format == "codex" || format == "openai-response"
}

/// The last `configuration_update` input item's `reasoning.effort`.
pub(crate) fn extract_configuration_update_config(body: &[u8]) -> Config {
    if body.is_empty() || !json::valid(body) {
        return Config::default();
    }
    let input = json::get(body, "input");
    if !input.is_array() {
        return Config::default();
    }
    let mut effort = String::new();
    input.each(|_, item| {
        if &*item.get("type").bytes() == b"configuration_update" {
            let value = item.get("reasoning.effort");
            if value.kind == json::Kind::String {
                let normalized = value.str().trim().go_lower();
                if !normalized.is_empty() {
                    effort = normalized;
                }
            }
        }
        true
    });
    match effort.as_str() {
        "" => Config::default(),
        "none" => Config::none(),
        "auto" => Config::auto(),
        _ => Config::level(effort),
    }
}

pub(crate) fn strip_configuration_updates(body: &[u8]) -> Vec<u8> {
    if body.is_empty() || !json::valid(body) {
        return body.to_vec();
    }
    let input = json::get(body, "input");
    if !input.is_array() {
        return body.to_vec();
    }
    let mut kept = Vec::new();
    let mut removed = false;
    input.each(|_, item| {
        if &*item.get("type").bytes() == b"configuration_update" {
            removed = true;
        } else {
            kept.push(item.raw().to_vec());
        }
        true
    });
    if !removed {
        return body.to_vec();
    }
    let mut out = body.to_vec();
    json::set_raw(&mut out, "input", json::join(&kept));
    out
}

pub(crate) fn strip_responses_effort(body: &[u8]) -> Vec<u8> {
    if body.is_empty() || !json::valid(body) || !json::get(body, "reasoning.effort").exists() {
        return body.to_vec();
    }
    let mut result = body.to_vec();
    if json::try_delete(body, "reasoning.effort")
        .map(|next| result = next)
        .is_err()
    {
        return body.to_vec();
    }
    if is_empty_object(&result, "reasoning") {
        json::delete(&mut result, "reasoning");
    }
    result
}

#[cfg(test)]
mod tests;
