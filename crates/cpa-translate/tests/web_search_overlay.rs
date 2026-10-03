//! Responses -> Gemini web search follows Go's ModelSupportsWebSearch: catalog
//! `native_capabilities.web_search` (an explicit false vetoes), then the available
//! Antigravity models' `supports_web_search` from the dynamic registry. Separate test
//! binary: the overlay is process-global.
use cpa_core::{
    format::Format,
    registry::{self, ModelInfo, Overlay},
};
use cpa_translate::{RequestCtx, translate_request};
use std::sync::Arc;

fn info(raw: &str) -> ModelInfo {
    ModelInfo::from_raw(serde_json::from_str(raw).unwrap()).unwrap()
}

struct Registry;

impl Overlay for Registry {
    fn lookup(&self, id: &str, _: Option<&str>) -> Option<ModelInfo> {
        (id == "vetoed").then(|| info(r#"{"id":"vetoed","type":"gemini","native_capabilities":{"web_search":false}}"#))
    }

    fn for_credential(&self, _: &str, _: &str) -> Option<ModelInfo> {
        None
    }

    fn available_by_provider(&self, provider: &str) -> Vec<ModelInfo> {
        assert_eq!(provider, "antigravity");
        vec![
            info(r#"{"id":" Custom-Search (preview)","type":"antigravity","supports_web_search":true}"#),
            info(r#"{"id":"plain-model","type":"antigravity"}"#),
            info(r#"{"id":"plain-model","type":"antigravity","supports_web_search":true}"#),
            info(r#"{"id":"vetoed","type":"antigravity","supports_web_search":true}"#),
        ]
    }
}

fn searches(model: &str) -> bool {
    let body = br#"{"input":"q","tools":[{"type":"web_search"}]}"#;
    let ctx = RequestCtx { model, stream: false };
    let out = translate_request(Format::OpenAIResponse, Format::Gemini, &ctx, body).unwrap();
    String::from_utf8(out).unwrap().contains(r#""googleSearch":{}"#)
}

#[test]
fn antigravity_capabilities_come_from_the_dynamic_registry() {
    // Static catalog only: Gemini models declare native search, unknown models do not.
    assert!(searches("gemini-2.5-pro"));
    assert!(!searches("custom-search"));
    registry::install_overlay(Some(Arc::new(Registry)));
    // IDs compare lower-cased and trimmed, without a trailing "(...)".
    assert!(searches("custom-search"));
    assert!(searches("CUSTOM-SEARCH(high)"));
    // The first matching Antigravity model decides, even when a later one supports it.
    assert!(!searches("plain-model"));
    // An explicit false in the registry vetoes Antigravity's capability.
    assert!(!searches("vetoed"));
    assert!(searches("gemini-2.5-pro"));
    registry::install_overlay(None);
}
