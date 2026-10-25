//! Plugin translator hooks around the registry entry points: Go sdk/translator
//! registry_test.go and registry_summary_test.go hook cases, with the registry's real
//! pairs standing in for Go's test registrations. The hooks are process-wide, so this
//! binary runs its tests one at a time.

use std::sync::{Arc, Mutex, MutexGuard};

use cpa_core::format::Format;
use cpa_translate::hooks::{self, Hooks, Response};
use cpa_translate::{RequestCtx, ResponseCtx};
use serde_json::Value;

type Edit = Box<dyn Fn(&[u8]) -> Vec<u8> + Send + Sync>;

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    request_translate: Option<Vec<u8>>,
    response_translate: Option<Vec<u8>>,
    normalize_request: Option<Edit>,
    normalize_before: Option<Edit>,
    normalize_after: Option<Edit>,
}

impl Fake {
    fn call(&self, name: &str, from: &str, to: &str) {
        self.calls.lock().unwrap().push(format!("{name} {from}>{to}"));
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

/// Go's fake returns its input when it has no edit; `None` is this trait's "unchanged".
impl Hooks for Fake {
    fn normalize_request(&self, from: &str, to: &str, _: &str, body: &[u8], _: bool) -> Option<Vec<u8>> {
        self.call("normalize-request", from, to);
        self.normalize_request.as_ref().map(|f| f(body))
    }
    fn translate_request(&self, from: &str, to: &str, _: &str, _: &[u8], _: bool) -> Option<Vec<u8>> {
        self.call("translate-request", from, to);
        self.request_translate.clone()
    }
    fn normalize_response_before(&self, t: &Response<'_>, body: &[u8]) -> Option<Vec<u8>> {
        self.call("normalize-response-before", t.from, t.to);
        self.normalize_before.as_ref().map(|f| f(body))
    }
    fn translate_response(&self, t: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
        self.call("translate-response", t.from, t.to);
        self.response_translate.clone()
    }
    fn normalize_response_after(&self, t: &Response<'_>, body: &[u8]) -> Option<Vec<u8>> {
        self.call("normalize-response-after", t.from, t.to);
        self.normalize_after.as_ref().map(|f| f(body))
    }
}

/// Installs `fake` until the guard drops.
struct Installed(#[allow(dead_code)] MutexGuard<'static, ()>);

impl Drop for Installed {
    fn drop(&mut self) {
        hooks::set_plugin_hooks_active(false);
        hooks::set_plugin_hooks(None);
    }
}

fn install(fake: Arc<dyn Hooks>) -> Installed {
    static SERIAL: Mutex<()> = Mutex::new(());
    let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    hooks::set_plugin_hooks(Some(fake));
    hooks::set_plugin_hooks_active(true);
    Installed(guard)
}

fn json(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(bytes)))
}

fn ctx(stream: bool) -> RequestCtx<'static> {
    RequestCtx {
        model: "gemini-3.6-flash",
        stream,
        tool_search: false,
    }
}

const RESPONSE_CTX: ResponseCtx<'static> = ResponseCtx {
    model: "m",
    original_request: b"{}",
    translated_request: b"{}",
    tool_search: false,
};

/// Go `TestTranslateRequest_PluginTranslatorOnlyWhenNativeMissing`: Codex to Gemini has
/// no native translator, so the plugin translator answers; OpenAI to Claude has one, so
/// it is never asked.
#[test]
fn a_plugin_request_translator_runs_only_without_a_native_pair() {
    let fake = Arc::new(Fake {
        request_translate: Some(br#"{"model":"plugin-request"}"#.to_vec()),
        ..Default::default()
    });
    let _installed = install(fake.clone());
    let got =
        cpa_translate::translate_request(Format::Codex, Format::Gemini, &ctx(false), br#"{"model":"p/x"}"#).unwrap();
    assert_eq!(json(&got)["model"], "plugin-request");
    assert_eq!(
        fake.calls(),
        ["normalize-request codex>gemini", "translate-request codex>gemini"],
        "the normalizers run first, on the fallback body"
    );

    fake.calls.lock().unwrap().clear();
    let body = br#"{"model":"claude-x","messages":[{"role":"user","content":"hi"}]}"#;
    let got = cpa_translate::translate_request(
        Format::OpenAI,
        Format::Claude,
        &RequestCtx {
            model: "claude-x",
            stream: false,
            tool_search: false,
        },
        body,
    )
    .unwrap();
    assert_eq!(
        json(&got)["messages"][0]["content"][0]["text"],
        "hi",
        "the native translator's body"
    );
    assert_eq!(fake.calls(), ["normalize-request openai>claude"]);
}

/// Go `TestPluginNormalizersChainAfterNative`: the request normalizer sees the native
/// translation and owns the result; the response before normalizer feeds the native
/// response translator, and the after normalizer sees its output and owns the result.
/// Plugin translators are not asked while a native pair exists.
#[test]
fn plugin_normalizers_wrap_the_native_translators() {
    let fake = Arc::new(Fake {
        normalize_request: Some(Box::new(|body| {
            let body = json(body);
            assert_eq!(body["messages"][0]["content"], "native", "{body}");
            br#"{"stage":"normalized-request"}"#.to_vec()
        })),
        normalize_before: Some(Box::new(|body| {
            assert_eq!(body, br#"{"stage":"raw-response"}"#);
            br#"{"id":"chatcmpl-1","object":"chat.completion","model":"gpt","choices":[{"index":0,"message":{"role":"assistant","content":"before"},"finish_reason":"stop"}]}"#.to_vec()
        })),
        normalize_after: Some(Box::new(|body| {
            let body = json(body);
            assert_eq!(body["content"][0]["text"], "before", "the native Claude answer: {body}");
            br#"{"stage":"after-response"}"#.to_vec()
        })),
        request_translate: Some(b"{}".to_vec()),
        response_translate: Some(b"{}".to_vec()),
        ..Default::default()
    });
    let _installed = install(fake.clone());
    let request = br#"{"model":"gpt","max_tokens":5,"messages":[{"role":"user","content":"native"}]}"#;
    let got = cpa_translate::translate_request(
        Format::Claude,
        Format::OpenAI,
        &RequestCtx {
            model: "gpt",
            stream: false,
            tool_search: false,
        },
        request,
    )
    .unwrap();
    assert_eq!(got, br#"{"stage":"normalized-request"}"#);

    let pair = cpa_translate::pair(Format::Claude, Format::OpenAI).unwrap();
    let got = (pair.non_stream)(&RESPONSE_CTX, br#"{"stage":"raw-response"}"#).unwrap();
    assert_eq!(got, br#"{"stage":"after-response"}"#);
    assert_eq!(
        fake.calls(),
        [
            "normalize-request claude>openai",
            "normalize-response-before openai>claude",
            "normalize-response-after openai>claude",
        ],
        "response hooks get the upstream format as `from`"
    );
}

/// Go `TestTranslateNonStream_PluginTranslatorOnlyWhenNativeMissing` and
/// `TestTranslateStream_PluginTranslatorUsedWhenNativeStreamMissing`: without a native
/// pair the plugin response translator answers, between the normalizers, for buffered
/// answers and for each stream chunk.
#[test]
fn a_plugin_response_translator_serves_a_pair_without_a_native_one() {
    let fake = Arc::new(Fake {
        response_translate: Some(br#"data: {"plugin":true}"#.to_vec()),
        normalize_after: Some(Box::new(|body| [body, b"!"].concat())),
        ..Default::default()
    });
    let _installed = install(fake.clone());
    let got = hooks::unregistered_non_stream("claude", "claude", &RESPONSE_CTX, br#"{"raw":true}"#).unwrap();
    assert_eq!(got, br#"data: {"plugin":true}!"#);

    let mut stream =
        hooks::wrap_stream("private-v1", "openai", &RESPONSE_CTX, None).expect("hooks wrap a missing pair");
    assert_eq!(
        stream.line(br#"data: {"raw":true}"#).unwrap(),
        [br#"data: {"plugin":true}!"#.to_vec()]
    );
    assert_eq!(
        fake.calls(),
        [
            "normalize-response-before claude>claude",
            "translate-response claude>claude",
            "normalize-response-after claude>claude",
            "normalize-response-before private-v1>openai",
            "translate-response private-v1>openai",
            "normalize-response-after private-v1>openai",
        ]
    );
}

/// A native stream translator runs between the normalizers for every chunk, and a
/// plugin translator is not asked.
#[test]
fn plugin_normalizers_wrap_each_native_stream_chunk() {
    let fake = Arc::new(Fake {
        // The native translator also answers the event's blank line with an empty chunk,
        // which the client's framing drops; leave it empty.
        normalize_after: Some(Box::new(|body| {
            if body.is_empty() {
                vec![]
            } else {
                [b"after:", body].concat()
            }
        })),
        response_translate: Some(b"plugin".to_vec()),
        ..Default::default()
    });
    let _installed = install(fake.clone());
    let pair = cpa_translate::pair(Format::OpenAI, Format::OpenAI).unwrap();
    let mut stream = (pair.stream)(&RESPONSE_CTX);
    let out = stream
        .event(b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n")
        .unwrap();
    let text: Vec<String> = out.iter().map(|b| String::from_utf8_lossy(b).into_owned()).collect();
    assert_eq!(text.len(), 1, "{text:?}");
    assert!(
        text[0].starts_with("data: after:{"),
        "the after normalizer sees the native chunk: {text:?}"
    );
    assert!(
        !fake.calls().iter().any(|c| c.starts_with("translate-response")),
        "{:?}",
        fake.calls()
    );
}

/// Go `TestRegistryTranslateRequestPluginMissDoesNotMixSummary`: a plugin translator
/// that does not answer leaves the fallback body unchanged.
#[test]
fn a_plugin_translator_miss_keeps_the_fallback_body() {
    let _installed = install(Arc::new(Fake::default()));
    let body = br#"{"model":"gemini-3.6-flash","reasoning":{"summary":"auto"},"input":"hi"}"#;
    let got = cpa_translate::translate_request(Format::Codex, Format::Gemini, &ctx(false), body).unwrap();
    assert_eq!(got, body);
}

/// Go `TestRegistryTranslateRequestAppliesSummaryAfterPluginTranslation` and
/// `TestRegistryTranslateRequestPluginNormalizerOwnsSourceSummaryIntent`: summary intent
/// is read from the normalized source and applied to the plugin's translation.
#[test]
fn summary_intent_comes_from_the_normalized_source_of_a_plugin_translation() {
    let translated = br#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high"}}}"#.to_vec();
    let source = br#"{"reasoning":{"summary":"auto"},"input":"hi"}"#;
    let cases: [(&str, Option<Edit>, Option<bool>); 3] = [
        ("kept", None, Some(true)),
        (
            "removed",
            Some(Box::new(|_: &[u8]| br#"{"reasoning":{},"input":"hi"}"#.to_vec())),
            None,
        ),
        (
            "disabled",
            Some(Box::new(|_: &[u8]| {
                br#"{"reasoning":{"summary":null},"input":"hi"}"#.to_vec()
            })),
            Some(false),
        ),
    ];
    for (name, normalize, want) in cases {
        let _installed = install(Arc::new(Fake {
            normalize_request: normalize,
            request_translate: Some(translated.clone()),
            ..Default::default()
        }));
        let got = json(&cpa_translate::translate_request(Format::Codex, Format::Gemini, &ctx(false), source).unwrap());
        let include = &got["generationConfig"]["thinkingConfig"]["includeThoughts"];
        assert_eq!(include.as_bool(), want, "{name}: {got}");
    }
}

/// Go `TestRegistryTranslateRequestNormalizerOwnsFinalSummaryField`: after a native
/// translation the normalizer sees the applied summary and may remove it.
#[test]
fn the_request_normalizer_owns_the_final_summary_field() {
    let _installed = install(Arc::new(Fake {
        normalize_request: Some(Box::new(|body| {
            let mut body = json(body);
            let config = &mut body["generationConfig"]["thinkingConfig"];
            assert_eq!(config["includeThoughts"], true, "{config}");
            config.as_object_mut().unwrap().remove("includeThoughts");
            serde_json::to_vec(&body).unwrap()
        })),
        ..Default::default()
    }));
    let body = br#"{"model":"gemini-3.6-flash","reasoning":{"effort":"high","summary":"auto"},"input":"hi"}"#;
    let got =
        json(&cpa_translate::translate_request(Format::OpenAIResponse, Format::Gemini, &ctx(false), body).unwrap());
    assert!(
        got["generationConfig"]["thinkingConfig"]
            .get("includeThoughts")
            .is_none(),
        "{got}"
    );
}

/// Go `ConfigurationUpdatesChanged`: set only when a normalizer changes the Responses
/// `configuration_update` input items, not when it edits anything else.
#[test]
fn configuration_update_changes_are_tracked_across_the_normalizer() {
    let body = br#"{"model":"m","input":[{"type":"configuration_update","reasoning":{"effort":"low"}},{"type":"message","role":"user","content":"hi"}]}"#;
    let edit_message: Edit = Box::new(|b| String::from_utf8_lossy(b).replace("\"hi\"", "\"hello\"").into_bytes());
    let edit_update: Edit = Box::new(|b| String::from_utf8_lossy(b).replace("\"low\"", "\"high\"").into_bytes());
    for (normalize, want) in [(edit_message, false), (edit_update, true)] {
        let _installed = install(Arc::new(Fake {
            normalize_request: Some(normalize),
            ..Default::default()
        }));
        let got = cpa_translate::translate_request_tracked(
            Format::OpenAIResponse,
            Format::Codex,
            &RequestCtx {
                model: "m",
                stream: false,
                tool_search: false,
            },
            body,
        )
        .unwrap();
        assert_eq!(
            got.configuration_updates_changed,
            want,
            "{}",
            String::from_utf8_lossy(&got.body)
        );
    }
}

/// Installed hooks that no plugin backs (the host's flag is off) are skipped: the
/// native result, untouched.
#[test]
fn inactive_hooks_change_nothing() {
    struct Inactive;
    impl Hooks for Inactive {
        fn normalize_request(&self, _: &str, _: &str, _: &str, _: &[u8], _: bool) -> Option<Vec<u8>> {
            panic!("an inactive hook ran")
        }
        fn translate_request(&self, _: &str, _: &str, _: &str, _: &[u8], _: bool) -> Option<Vec<u8>> {
            panic!("an inactive hook ran")
        }
        fn normalize_response_before(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
            panic!("an inactive hook ran")
        }
        fn translate_response(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
            panic!("an inactive hook ran")
        }
        fn normalize_response_after(&self, _: &Response<'_>, _: &[u8]) -> Option<Vec<u8>> {
            panic!("an inactive hook ran")
        }
    }
    let body = br#"{"model":"other","input":"hi"}"#;
    let installed = install(Arc::new(Inactive));
    hooks::set_plugin_hooks_active(false);
    let with = cpa_translate::translate_request(Format::Codex, Format::Gemini, &ctx(false), body).unwrap();
    assert!(hooks::unregistered_non_stream("claude", "claude", &RESPONSE_CTX, b"{}").is_none());
    assert!(hooks::wrap_stream("claude", "claude", &RESPONSE_CTX, None).is_none());
    // Still serialized: another test's hooks must not see this translation.
    hooks::set_plugin_hooks(None);
    let without = cpa_translate::translate_request(Format::Codex, Format::Gemini, &ctx(false), body).unwrap();
    drop(installed);
    assert_eq!(with, without);
    assert_eq!(json(&with)["model"], "gemini-3.6-flash", "the fallback's model rewrite");
}
