//! The server's dynamic registry is consulted before the static catalogs (Go's
//! registry.LookupModelInfo). Separate test binary: the overlay is process-global.
use cpa_core::{
    format::Format,
    registry::{self, ModelInfo, Overlay},
};
use cpa_translate::{RequestCtx, translate_request};
use std::sync::Arc;

struct Fixed;

impl Overlay for Fixed {
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelInfo> {
        assert_eq!(
            provider,
            Some("claude"),
            "Claude-bound translation asks for the claude registration"
        );
        let levels = match id {
            // A config-defined model unknown to the static catalogs.
            "custom-adaptive" => r#"["low","high","max"]"#,
            // Overrides a static budget-only model.
            "claude-sonnet-4-5-20250929" => r#"["low","medium","high"]"#,
            _ => return None,
        };
        let raw = format!(r#"{{"id":"{id}","type":"claude","thinking":{{"levels":{levels}}}}}"#);
        ModelInfo::from_raw(serde_json::from_str(&raw).unwrap()).ok()
    }

    fn for_credential(&self, _: &str, _: &str) -> Option<ModelInfo> {
        None
    }
}

fn thinking(model: &str, effort: &str) -> String {
    let body = format!(r#"{{"reasoning_effort":"{effort}","messages":[{{"role":"user","content":"hi"}}]}}"#);
    let out = translate_request(
        Format::OpenAI,
        Format::Claude,
        &RequestCtx { model, stream: false },
        body.as_bytes(),
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    format!("{} {}", v["thinking"], v["output_config"])
}

#[test]
fn dynamic_registry_overrides_static_capabilities() {
    // Without the overlay: unknown models get budgets, the static model is budget-only.
    assert_eq!(
        thinking("custom-adaptive", "xhigh"),
        r#"{"type":"enabled","budget_tokens":32768} null"#
    );
    assert_eq!(
        thinking("claude-sonnet-4-5-20250929", "xhigh"),
        r#"{"type":"enabled","budget_tokens":32768} null"#
    );
    registry::install_overlay(Some(Arc::new(Fixed)));
    // Levels with max: xhigh maps to max. Levels without max: xhigh maps to high.
    assert_eq!(
        thinking("custom-adaptive", "xhigh"),
        r#"{"type":"adaptive"} {"effort":"max"}"#
    );
    assert_eq!(
        thinking("claude-sonnet-4-5-20250929", "xhigh"),
        r#"{"type":"adaptive"} {"effort":"high"}"#
    );
    // Models the overlay does not know still fall back to the static catalogs.
    assert_eq!(
        thinking("kimi-k2.5", "xhigh"),
        r#"{"type":"adaptive"} {"effort":"high"}"#
    );
    registry::install_overlay(None);
}
