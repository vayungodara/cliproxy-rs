//! Plugin request normalizers through the executors: Go's
//! helps/configuration_update_plugin_test.go, helps/codex_multi_agent_v2_test.go and
//! codex_executor_tokens_test.go plugin cases, and the order in which the Meta and Gemini
//! executors translate. The hooks are process-wide, so this binary runs its tests one at
//! a time.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::{Credential, Source};
use cpa_core::exec::{Caller, ExecRequest, Operation, ResolvedModel, ResolvedSource};
use cpa_core::format::Format;
use cpa_core::registry::{ModelInfo, ThinkingSupport};
use cpa_exec::codex::CodexExecutor;
use cpa_exec::codex_oauth::CodexOAuth;
use cpa_translate::hooks::{self, Hooks, Response};
use http::HeaderMap;
use serde_json::Value;

type Edit = Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

/// Records every request body it normalizes and applies `edit` (`None`: unchanged).
#[derive(Default)]
struct Fake {
    seen: Mutex<Vec<Vec<u8>>>,
    edit: Option<Edit>,
}

impl Hooks for Fake {
    fn normalize_request(&self, _: &str, _: &str, _: &str, body: &[u8], _: bool) -> Option<Vec<u8>> {
        self.seen.lock().unwrap().push(body.to_vec());
        self.edit.as_ref().map(|f| f(body))
    }
    fn translate_request(&self, _: &str, _: &str, _: &str, _: &[u8], _: bool) -> Option<Vec<u8>> {
        None
    }
    fn normalize_response_before(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn translate_response(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn normalize_response_after(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

static SERIAL: Mutex<()> = Mutex::new(());

/// Installs `fake` (active, as when a plugin offers a hook) until the guard drops.
struct Installed(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for Installed {
    fn drop(&mut self) {
        hooks::set_plugin_hooks(None);
    }
}

fn install(fake: Option<Arc<Fake>>, active: bool) -> Installed {
    let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    hooks::set_plugin_hooks(fake.map(|f| f as Arc<dyn Hooks>));
    hooks::set_plugin_hooks_active(active);
    Installed(guard)
}

/// Go's `sjson.SetBytes(body, "input.0.reasoning.effort", effort)`.
fn set_effort(body: &[u8], effort: &str) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(body).unwrap();
    v["input"][0]["reasoning"]["effort"] = Value::String(effort.into());
    serde_json::to_vec(&v).unwrap()
}

fn mark(body: &[u8]) -> Vec<u8> {
    let mut v: Value = serde_json::from_slice(body).unwrap();
    v["plugin_call"] = Value::from(1);
    serde_json::to_vec(&v).unwrap()
}

/// A loopback Codex upstream that records each request body and completes the response.
async fn upstream() -> (String, Arc<Mutex<Vec<Bytes>>>) {
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    let app = axum::Router::new().fallback(move |body: Bytes| {
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(body);
            (
                [("content-type", "text/event-stream")],
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"output\":[]}}\n\n",
            )
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, bodies)
}

fn credential(base_url: &str) -> Credential {
    provider_credential("codex", base_url)
}

fn provider_credential(provider: &str, base_url: &str) -> Credential {
    let mut credential = Credential {
        id: format!("{provider}-hooks.json"),
        provider: provider.into(),
        source: Source::File(PathBuf::from(format!("/fake/{provider}-hooks.json"))),
        disabled: false,
        label: provider.into(),
        attributes: Default::default(),
        metadata: Default::default(),
        revision: 0,
        next_refresh_after: None,
    };
    credential.attributes.insert("api_key".into(), "test".into());
    credential.attributes.insert("base_url".into(), base_url.into());
    credential
}

/// A configured API-key model with Go's three effort levels, compat as given.
fn resolved(model: &str, compat: bool) -> ResolvedModel {
    let mut raw = serde_json::Map::new();
    if compat {
        raw.insert("is_compat".into(), Value::Bool(true));
    }
    ResolvedModel {
        info: ModelInfo {
            id: model.into(),
            kind: String::new(),
            thinking: Some(ThinkingSupport {
                levels: vec!["low".into(), "medium".into(), "high".into()],
                ..Default::default()
            }),
            raw,
        },
        source: ResolvedSource::ApiKey,
    }
}

fn request(source: Format, model: &str, body: &str, original: &str, compat: bool) -> ExecRequest {
    let mut headers = HeaderMap::new();
    headers.insert("user-agent", "codex_cli_rs/0.1".parse().unwrap());
    ExecRequest {
        operation: Operation::Generate,
        source_format: source,
        response_format: source,
        stream_payloads: false,
        requested_model: model.into(),
        model: model.into(),
        original_body: Bytes::from(original.to_owned()),
        body: Bytes::from(body.to_owned()),
        stream: false,
        alt: None,
        session: None,
        headers,
        execution_session: None,
        derived_session: None,
        resolved_model: Some(resolved(model, compat)),
        usage: Default::default(),
        request_path: String::new(),
        caller: Caller {
            principal: String::new(),
            source: "authorization",
            provider: String::new(),
        },
    }
}

fn executor() -> CodexExecutor {
    CodexExecutor::with_client(wreq::Client::new(), CodexOAuth::new(wreq::Client::new()))
}

/// The body the Codex upstream received for `req`.
async fn sent(req: ExecRequest) -> Value {
    let (url, bodies) = upstream().await;
    executor()
        .execute(&credential(&url), req, &Config::default())
        .await
        .unwrap_or_else(|e| panic!("{e:?}"));
    let body = bodies.lock().unwrap().pop().expect("one upstream request");
    serde_json::from_slice(&body).unwrap()
}

const UPDATE: &str = r#"{"model":"opaque-route","reasoning":{"effort":"medium"},"input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"role":"user","content":"hi"}]}"#;

/// `TestPluginConfigurationUpdateNormalizationIsAuthoritative` and
/// `TestRequestPairPreservesPluginConfigurationUpdateIntent` through the executor: a
/// plugin that deletes the `configuration_update` item leaves the request's own effort,
/// one that rewrites it wins, and thinking reads the working translation's intent.
/// Equal payloads are normalized once, distinct ones once each.
#[tokio::test]
async fn plugin_configuration_update_edits_decide_codex_thinking() {
    for (name, effort, want) in [("delete", None, "medium"), ("rewrite", Some("high"), "high")] {
        for distinct in [false, true] {
            let fake = Arc::new(Fake {
                edit: Some(match effort {
                    None => Box::new(|body: &[u8]| {
                        let mut v: Value = serde_json::from_slice(body).unwrap();
                        v["input"].as_array_mut().unwrap().remove(0);
                        serde_json::to_vec(&v).unwrap()
                    }),
                    Some(effort) => Box::new(move |body: &[u8]| set_effort(body, effort)),
                }),
                ..Default::default()
            });
            let _hooks = install(Some(fake.clone()), true);
            let original = if distinct {
                r#"{"input":[{"role":"user","content":"different"}]}"#
            } else {
                UPDATE
            };
            let body = sent(request(Format::OpenAIResponse, "opaque-route", UPDATE, original, false)).await;
            assert_eq!(body["reasoning"]["effort"], want, "{name} distinct={distinct}: {body}");
            assert_eq!(
                fake.seen.lock().unwrap().len(),
                1 + distinct as usize,
                "{name} distinct={distinct}"
            );
        }
    }
    // Without a plugin the source's configuration_update decides.
    let _hooks = install(None, false);
    let body = sent(request(Format::OpenAIResponse, "opaque-route", UPDATE, UPDATE, false)).await;
    assert_eq!(body["reasoning"]["effort"], "low", "{body}");
}

/// `TestTranslateRequestWithAPIKeyModelCompatibility_InvokesPluginNormalizers` for the
/// Codex executor's compat path (Claude to Codex): the normalizer runs once, after the
/// compat translator and the summary config, and its edit reaches the upstream. Hooks
/// installed with no plugin offering one leave the upstream bytes unchanged.
#[tokio::test]
async fn plugin_normalizers_run_after_the_codex_compat_translator() {
    let claude = r#"{"model":"opaque-route","max_tokens":64,"thinking":{"type":"enabled","budget_tokens":2048,"display":"summarized"},"messages":[{"role":"user","content":"hello"}]}"#;
    let plain = {
        let _hooks = install(None, false);
        sent(request(Format::Claude, "opaque-route", claude, claude, true)).await
    };
    let fake = Arc::new(Fake {
        edit: Some(Box::new(mark)),
        ..Default::default()
    });
    {
        let _hooks = install(Some(fake.clone()), false);
        let inactive = sent(request(Format::Claude, "opaque-route", claude, claude, true)).await;
        assert_eq!(inactive, plain);
        assert!(fake.seen.lock().unwrap().is_empty());
    }
    let _hooks = install(Some(fake.clone()), true);
    let body = sent(request(Format::Claude, "opaque-route", claude, claude, true)).await;
    assert_eq!(body["plugin_call"], 1, "{body}");
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let hooked: Value = serde_json::from_slice(&seen[0]).unwrap();
    // The hook sees the Codex body with the summary config already applied.
    assert!(hooked["input"].is_array(), "{hooked}");
    assert_eq!(
        hooked["reasoning"]["summary"], plain["reasoning"]["summary"],
        "{hooked}"
    );
    assert!(plain["reasoning"]["summary"].is_string(), "{plain}");
}

/// `TestCodexExecutorCountTokensPreservesToolNumberSchemas`: the normalizer sees the
/// translated Codex body with `yield_time_ms` still a number, with and without compat.
#[tokio::test]
async fn count_tokens_normalizes_the_translated_tool_schemas() {
    let sources = [
        (
            Format::OpenAIResponse,
            r#"{"input":"hi","tools":[{"type":"function","name":"exec_command","parameters":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#,
        ),
        (
            Format::Claude,
            r#"{"messages":[{"role":"user","content":"hi"}],"tools":[{"name":"exec_command","input_schema":{"type":"object","properties":{"yield_time_ms":{"type":"number"}}}}]}"#,
        ),
    ];
    for (source, payload) in sources {
        for compat in [false, true] {
            let fake = Arc::new(Fake::default());
            let _hooks = install(Some(fake.clone()), true);
            let mut req = request(source, "gpt-5.4", payload, payload, compat);
            req.operation = Operation::CountTokens;
            let response = executor()
                .execute(&credential("http://127.0.0.1:9"), req, &Config::default())
                .await
                .unwrap_or_else(|e| panic!("{e:?}"));
            assert!(matches!(response.body, cpa_core::exec::ResponseBody::Buffered(ref b) if !b.is_empty()));
            let seen = fake.seen.lock().unwrap();
            let hooked: Value = serde_json::from_slice(seen.last().expect("normalized")).unwrap();
            assert_eq!(
                hooked["tools"][0]["parameters"]["properties"]["yield_time_ms"]["type"], "number",
                "{source:?} compat={compat}: {hooked}"
            );
        }
    }
}

/// Go's counting test hook (helps/codex_multi_agent_v2_test.go): each call sets
/// `plugin_call` to its own number, so the body sent upstream shows which translation
/// ran last. Go translates the original request before the working one, so the working
/// body carries the higher number: the Meta executor translates both even when they are
/// equal, and the Gemini executor's pair translates distinct payloads once each.
#[tokio::test]
async fn executors_translate_the_original_request_before_the_working_one() {
    let claude = r#"{"model":"opaque-route","max_tokens":64,"messages":[{"role":"user","content":"hello"}]}"#;
    let changed = r#"{"model":"opaque-route","max_tokens":64,"messages":[{"role":"user","content":"changed"}]}"#;
    for (provider, original) in [("meta", claude), ("gemini", changed)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let fake = Arc::new(Fake {
            edit: Some(Box::new(move |body: &[u8]| {
                let mut v: Value = serde_json::from_slice(body).unwrap();
                v["plugin_call"] = Value::from(counter.fetch_add(1, Ordering::SeqCst) + 1);
                serde_json::to_vec(&v).unwrap()
            })),
            ..Default::default()
        });
        let _hooks = install(Some(fake), true);
        let (url, bodies) = upstream().await;
        let req = request(Format::Claude, "opaque-route", claude, original, true);
        let credential = provider_credential(provider, &url);
        // Only the upstream request matters; the reply need not suit the provider.
        let _ = match provider {
            "meta" => {
                cpa_exec::meta::MetaExecutor::with_client(wreq::Client::new())
                    .execute(&credential, req, &Config::default())
                    .await
            }
            _ => {
                cpa_exec::gemini::GeminiExecutor::with_client(wreq::Client::new())
                    .execute(&credential, req, &Config::default())
                    .await
            }
        };
        assert_eq!(calls.load(Ordering::SeqCst), 2, "{provider}");
        let body = bodies.lock().unwrap().pop().expect("one upstream request");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["plugin_call"], 2, "{provider}: {body}");
    }
}
