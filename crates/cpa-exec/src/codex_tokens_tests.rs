//! Replays tests/fixtures/codex_tokens_go.json: Go's `CodexExecutor.CountTokens` at
//! 6fecc6e (tests/reference/codex_client/README.md), through the Rust executor.

use super::*;
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{Caller, ExecRequest, Operation, ResponseBody};
use cpa_core::format::Format;
use serde_json::Value;

fn request(v: &Value) -> ExecRequest {
    let source = Format::parse(v["from"].as_str().unwrap()).unwrap();
    let mut headers = http::HeaderMap::new();
    for (k, val) in v["headers"].as_object().into_iter().flatten() {
        headers.insert(
            http::HeaderName::try_from(k.as_str()).unwrap(),
            val.as_str().unwrap().parse().unwrap(),
        );
    }
    let body = Bytes::from(v["in"].as_str().unwrap().to_owned());
    ExecRequest {
        operation: Operation::CountTokens,
        source_format: source,
        response_format: Format::parse(v["response"].as_str().unwrap()).unwrap(),
        requested_model: v["model"].as_str().unwrap().into(),
        model: v["model"].as_str().unwrap().into(),
        original_body: body.clone(),
        body,
        stream: false,
        alt: None,
        session: None,
        headers,
        execution_session: None,
        derived_session: None,
        request_path: String::new(),
        resolved_model: None,
        usage: Default::default(),
        caller: Caller {
            principal: "client-key-FAKE".into(),
            source: "authorization",
        },
    }
}

#[tokio::test]
async fn count_tokens_matches_go() {
    let vectors: Vec<Value> = serde_json::from_str(include_str!("../tests/fixtures/codex_tokens_go.json")).unwrap();
    assert_eq!(vectors.len(), 30);
    // Nothing is sent: the base URL is unroutable and the token stays unused.
    let credential = Credential {
        id: "codex-tokens.json".into(),
        provider: "codex".into(),
        source: Source::File("/fake/codex-tokens.json".into()),
        disabled: false,
        label: "codex".into(),
        attributes: [("base_url".to_owned(), "http://127.0.0.1:1".to_owned())].into(),
        metadata: Default::default(),
        revision: 0,
    };
    let executor = crate::codex::CodexExecutor::new().unwrap();
    let cfg = Config::default();
    for v in &vectors {
        assert!(v["err"].is_null(), "Go failed: {v}");
        let response = executor.execute(&credential, request(v), &cfg).await.unwrap();
        let ResponseBody::Buffered(body) = response.body else {
            panic!("buffered count expected")
        };
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            v["out"].as_str().unwrap(),
            "{} {}",
            v["from"],
            v["model"]
        );
    }
}

#[test]
fn encodings_follow_go_model_prefixes() {
    for (model, want) in [
        ("gpt-5.5", Encoding::O200kBase),
        (" GPT-4.1-mini", Encoding::O200kBase),
        ("gpt-4o", Encoding::O200kBase),
        ("gpt-4", Encoding::Cl100kBase),
        ("gpt-3.5-turbo", Encoding::Cl100kBase),
        ("o3", Encoding::Cl100kBase),
        ("", Encoding::Cl100kBase),
    ] {
        assert_eq!(encoding_for_model(model), want, "{model}");
    }
}
