//! Translator entry points into internal/thinking, served by `cpa_common::thinking`.

use cpa_common::thinking as ct;
use cpa_core::registry::ModelInfo;

pub use ct::{convert_budget_to_level, convert_level_to_budget, has_level, map_to_claude_effort};

/// registry.LookupModelInfo: the server's dynamic registry (preferring `provider`'s
/// registration), then every static catalog in Go's search order, by trimmed ID.
pub fn lookup_model_info(model: &str, provider: &str) -> Option<ModelInfo> {
    let provider = provider.trim().to_lowercase();
    cpa_core::registry::lookup_model(model, (!provider.is_empty()).then_some(provider.as_str()))
}

/// The summary intent read from the client body (ExtractTranslatedSummaryConfig).
pub type Summary = ct::SummaryConfig;

pub fn extract_translated_summary(body: &[u8], source: &str, target: &str) -> Summary {
    ct::extract_translated_summary_config(body, source, target)
}

/// ExtractSummaryConfig.
pub fn extract_summary(body: &[u8], format: &str) -> Summary {
    ct::extract_summary_config(body, format)
}

/// ApplySummaryConfig.
pub fn apply_summary(body: Vec<u8>, format: &str, config: Summary) -> Vec<u8> {
    ct::apply_summary_config(&body, format, config)
}

/// ApplySummaryConfigForModel.
pub fn apply_summary_for_model(body: Vec<u8>, format: &str, model: &str, config: Summary) -> Vec<u8> {
    ct::apply_summary_config_for_model(&body, format, model, config)
}

/// ApplyTranslatedSummaryToClaude.
pub fn apply_translated_summary_to_claude(out: Vec<u8>, source: &[u8], source_format: &str, model: &str) -> Vec<u8> {
    ct::apply_translated_summary_to_claude(&out, source, source_format, model)
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
