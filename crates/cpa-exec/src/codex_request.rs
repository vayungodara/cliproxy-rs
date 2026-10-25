//! Codex request shaping: body rules and upstream headers, applied in Go's order so
//! appended keys land where Go puts them (codex_executor_execute.go, codex_executor_stream.go,
//! codex_executor_request.go, codex_websockets_request.go, helps/codex_input_ids.go,
//! helps/codex_tool_schema.go, openai_responses_signature.go).

use cpa_core::gostr::GoStr;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, FailureScope};
use cpa_core::format::Format;
use gjson::Kind;
use http::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};

use crate::codex_json::{delete, set_bool_if_different, set_raw, set_str, set_str_if_different};

pub const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub(crate) const USER_AGENT: &str = "codex-tui/0.154.0 (Mac OS 26.5.2; arm64) iTerm.app/3.6.11 (codex-tui; 0.154.0)";
pub(crate) const ORIGINATOR: &str = "codex-tui";
pub(crate) const WS_BETA: &str = "responses_websockets=2026-02-06";
const ROUTING_HINT: &str = "x-codex-routing-hint";
const LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";

/// Which upstream call a body is shaped for; each has its own Go rule list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    Stream,
    NonStream,
    Compact,
    Websocket,
}

impl Call {
    /// The upstream protocol: compact goes to the OpenAI Responses endpoint.
    pub fn target(self) -> Format {
        if self == Call::Compact {
            Format::OpenAIResponse
        } else {
            Format::Codex
        }
    }
}

/// Codex settings read from the config snapshot (v8 paths; legacy keys are already moved).
#[derive(Debug, Clone, Default)]
pub(crate) struct Settings {
    pub disable_cloaking: bool,
    pub bootstrap_buffering: bool,
    pub bootstrap_timeout: Option<Duration>,
    pub model_level_cooling: bool,
    pub default_user_agent: String,
    pub default_beta_features: String,
    pub image_generation: bool,
    /// `requests.payload` rules and image-generation mode for this config snapshot.
    pub payload: cpa_common::payload::Rules,
    /// Multi-agent v2 and orphan-delegation settings, both shared by every credential kind.
    pub client: cpa_common::codex_client::Settings,
    /// The `models` of the `codex-api-key` entry the credential resolves to.
    pub key_models: Vec<serde_json::Value>,
    /// The credential, when Home dispatched it (Go `cfg.Home.Enabled`): its
    /// `credential_options` decide is-compat when nothing else does.
    pub home: Option<cpa_core::credential::Credential>,
    /// `response-steering`: full-duplex WebSocket turns (codex_websockets_duplex.go).
    pub response_steering: bool,
    /// `multimedia.gpt-image-2-base-model`, trimmed: the Images API's tool-call model.
    pub image_base_model: String,
    /// `codexSIWCUserAgent`, read from the whole config (Go uses `e.cfg`, never the API-key view).
    pub siwc_user_agent: String,
}

/// `upstream.codex.response-steering`; the older spellings are already moved there.
pub(crate) fn response_steering(cfg: &Config) -> bool {
    setting(cfg, &["upstream", "codex", "response-steering"])
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

/// `oauth.providers.codex.chatgpt-keep-alive`, a cliproxy-rs addition: off unless set to
/// true (YAML 1.1 spellings such as `yes` are normalised by the config schema).
pub(crate) fn chatgpt_keep_alive(cfg: &Config) -> bool {
    setting(cfg, &["oauth", "providers", "codex", "chatgpt-keep-alive"])
        .and_then(serde_yaml_ng::Value::as_bool)
        .unwrap_or(false)
}

fn setting<'a>(cfg: &'a Config, path: &[&str]) -> Option<&'a serde_yaml_ng::Value> {
    path.iter().try_fold(&cfg.document, |v, k| v.get(*k))
}

impl Settings {
    /// The settings `view`'s credential sees: API keys get Go's `ForAPIKey` view, where
    /// the OAuth-only `oauth.providers.codex.*` settings (the header defaults) do not
    /// apply.
    pub fn scoped(cfg: &Config, view: &View<'_>) -> Self {
        let mut settings = if view.api_key {
            Self::from(&cfg.for_api_key())
        } else {
            Self::from(cfg)
        };
        settings.siwc_user_agent = crate::codex_siwc::user_agent(cfg);
        // ponytail: Go's executor-side resolveCodexKeyConfig skips the prefix/proxy
        // tie-break of the shared resolver; they differ only for duplicate key+URL entries.
        settings.key_models = cpa_core::config::credentials::resolve_api_key_entry(cfg, "codex", view.credential)
            .map(|entry| entry.models)
            .unwrap_or_default();
        if view
            .credential
            .attributes
            .contains_key(cpa_core::config::credentials::HOME_PROVIDER)
        {
            settings.home = Some(view.credential.clone());
        }
        settings
    }

    /// `resolveCodexModelIsCompat`: the model dispatch bound to the attempt, else the
    /// credential's config entry, where the first model whose name or alias equals the
    /// base or requested model decides; none means not compat. Without config models,
    /// `CodexAPIKeyModelIsCompat` for the base, then the requested model: in Home mode
    /// the credential's `credential_options` entry for that model.
    pub fn is_compat(&self, req: &ExecRequest) -> bool {
        if let Some(resolved) = &req.resolved_model {
            return resolved.is_compat();
        }
        let fold = |a: &str, b: &str| !b.is_empty() && a.go_eq_fold(b);
        let requested = req.model.trim();
        let base = base_model(&req.model);
        let target = base.trim();
        if self.key_models.is_empty() {
            let home = |model: &str| {
                self.home
                    .as_ref()
                    .and_then(|c| cpa_home::dispatch::credential_model_options(c, model, model))
                    .is_some_and(|options| options.get("is-compat") == Some(&serde_json::Value::Bool(true)))
            };
            return home(target) || home(&req.model);
        }
        self.key_models
            .iter()
            .find(|m| {
                let field = |k: &str| m.get(k).and_then(serde_json::Value::as_str).unwrap_or_default().trim();
                let (name, alias) = (field("name"), field("alias"));
                fold(name, target) || fold(alias, target) || fold(name, requested) || fold(alias, requested)
            })
            .and_then(|m| m.get("is-compat"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    pub fn from(cfg: &Config) -> Self {
        // Shared settings (`upstream.codex`); the header defaults stay OAuth-only.
        let codex = |k: &str| setting(cfg, &["upstream", "codex", k]);
        let flag = |k: &str| codex(k).and_then(|v| v.as_bool()).unwrap_or(false);
        let text = |path: &[&str]| {
            setting(cfg, path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let image_off = match setting(cfg, &["multimedia", "disable-image-generation"]) {
            None => true,
            Some(v) => v.as_bool() == Some(false) || v.as_str().is_some_and(|s| matches!(s.trim(), "" | "false")),
        };
        Self {
            disable_cloaking: flag("disable-codex-cloaking"),
            bootstrap_buffering: flag("stream-bootstrap-buffering"),
            bootstrap_timeout: codex("stream-bootstrap-timeout")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .and_then(|s| go_duration(&s))
                .filter(|d| !d.is_zero()),
            model_level_cooling: flag("model-level-cooling"),
            default_user_agent: text(&["oauth", "providers", "codex", "header-defaults", "user-agent"]),
            default_beta_features: text(&["oauth", "providers", "codex", "header-defaults", "beta-features"]),
            image_generation: image_off,
            payload: cpa_common::payload::Rules::from_config(cfg),
            client: cpa_common::codex_client::Settings::from_config(cfg),
            key_models: Vec::new(),
            home: None,
            response_steering: response_steering(cfg),
            image_base_model: text(&["multimedia", "gpt-image-2-base-model"]),
            siwc_user_agent: crate::codex_siwc::user_agent(cfg),
        }
    }
}

/// `StreamBootstrapTimeoutDuration`: Go durations, bare seconds, or off words.
/// Anything else is Go's default of 0, meaning no timeout.
fn go_duration(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw == "0"
        || ["none", "unlimited", "disabled", "off", "never"]
            .iter()
            .any(|w| raw.eq_ignore_ascii_case(w))
    {
        return None;
    }
    if let Some(nanos) = cpa_core::config::parse_duration(raw).filter(|n| *n >= 0) {
        return Some(Duration::from_nanos(nanos as u64));
    }
    // strconv.Atoi, capped where seconds still fit a time.Duration.
    raw.parse::<i64>()
        .ok()
        .filter(|secs| (0..=i64::MAX / 1_000_000_000).contains(secs))
        .map(|secs| Duration::from_secs(secs as u64))
}

/// The credential fields the Codex executor reads (`codexCreds`, `codexAuthUsesAPIKey`).
pub(crate) struct View<'a> {
    pub credential: &'a Credential,
    pub token: &'a str,
    pub api_key: bool,
    /// `codexUseSIWC`: the credential sends its Sign in with ChatGPT token (7b0319fb).
    pub siwc: bool,
    pub base_url: &'a str,
    /// Effective proxy (`effectiveProxyURL`): the credential's, then `requests.proxy-url`.
    pub proxy: crate::proxy::Proxy,
    /// `$CPA-SESSION-ID`: the request's explicit session, never derived fallbacks.
    pub session: Option<String>,
    /// `oauth.providers.codex.chatgpt-keep-alive` (cliproxy-rs addition, off by default):
    /// keep idle chatgpt.com connections instead of Go's connection per request. Read
    /// from the config snapshot of each request, so a reload applies to the next one.
    pub chatgpt_keep_alive: bool,
    /// `requests.tls-handshake-timeout-seconds` for chatgpt.com ([`crate::tls::handshake_timeout`]).
    pub handshake: Option<std::time::Duration>,
}

impl<'a> View<'a> {
    /// The view one request uses, with its effective proxy.
    pub fn for_request(credential: &'a Credential, cfg: &Config) -> Self {
        Self {
            proxy: crate::proxy::Proxy::effective(credential, cfg),
            chatgpt_keep_alive: chatgpt_keep_alive(cfg),
            handshake: crate::tls::handshake_timeout(cfg),
            ..Self::new(credential)
        }
    }

    /// Binds the request's explicit session for `$CPA-SESSION-ID` custom headers.
    pub fn with_session(mut self, session: Option<String>) -> Self {
        self.session = session;
        self
    }

    pub fn new(credential: &'a Credential) -> Self {
        let attr = |k: &str| credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let api_key = attr("api_key");
        let siwc = crate::codex_siwc::enabled(credential);
        // `codexCreds`: a Sign in with ChatGPT credential sends its nested token to the
        // OpenAI API unless `base_url` names another endpoint.
        let (token, base_url) = if siwc {
            let base = attr("base_url");
            (
                crate::codex_siwc::access_token(credential),
                if base.trim().is_empty() {
                    crate::codex_siwc::RESOURCE
                } else {
                    base
                },
            )
        } else {
            let token = if api_key.is_empty() {
                credential.str("access_token").unwrap_or_default()
            } else {
                api_key
            };
            let base = match attr("base_url") {
                "" => DEFAULT_BASE_URL,
                url => url,
            };
            (token, base)
        };
        let kind = attr("auth_kind").trim().to_ascii_lowercase().replace(['_', '-'], "");
        let is_api_key = kind == "apikey" || (kind.is_empty() && !api_key.trim().is_empty());
        Self {
            credential,
            token,
            api_key: is_api_key,
            siwc,
            base_url: base_url.trim_end_matches('/'),
            proxy: crate::proxy::Proxy::Inherit,
            session: None,
            chatgpt_keep_alive: false,
            handshake: None,
        }
    }

    pub fn attr(&self, key: &str) -> &str {
        self.credential
            .attributes
            .get(key)
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// Plan used for the free-plan image tool rule: refreshed metadata wins over the
    /// attribute computed at load time.
    fn plan_type(&self) -> &str {
        self.credential
            .str("plan_type")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(self.attr("plan_type"))
    }

    /// `isCodexCloakingDisabled`: the attribute (config synthesis copies the matching
    /// `codex-api-key` entry's `disable-codex-cloaking` into it), then the setting.
    pub fn cloaking_disabled(&self, settings: &Settings) -> bool {
        go_bool(self.attr("codex_disable_cloaking")).unwrap_or(settings.disable_cloaking)
    }

    /// `codexWebsocketsEnabled`: attribute first, then metadata.
    pub fn websockets(&self) -> bool {
        match self.attr("websockets").trim() {
            "" => match self.credential.metadata.get("websockets") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::String(s)) => go_bool(s).unwrap_or(false),
                _ => false,
            },
            raw => go_bool(raw).unwrap_or(false),
        }
    }
}

/// Go `strconv.ParseBool`.
fn go_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// `thinking.ParseSuffix(model).ModelName`.
pub(crate) fn base_model(model: &str) -> String {
    cpa_common::thinking::parse_suffix(model).model_name
}

pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or_default()
}

/// `util.IsCodexResponsesLiteRequest`. The header is decoded as UTF-8 rather than through
/// [`header`], so Go's `TrimSpace` also strips non-ASCII spaces such as U+00A0.
pub(crate) fn is_lite(body: &str, headers: &HeaderMap) -> bool {
    let lite = headers
        .get(LITE_HEADER)
        .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
        .unwrap_or_default();
    if lite.trim().go_eq_fold("true") {
        return true;
    }
    let v = gjson::get(
        body,
        "client_metadata.ws_request_header_x_openai_internal_codex_responses_lite",
    );
    v.kind() == Kind::True || (v.kind() == Kind::String && v.str().trim().eq_ignore_ascii_case("true"))
}

/// `helps.IsNativeCodexRequest`: Codex/Responses formats on both sides plus the lite marker.
pub(crate) fn is_native(req: &ExecRequest) -> bool {
    let codexish = |f: Format| matches!(f, Format::Codex | Format::OpenAIResponse);
    codexish(req.source_format)
        && codexish(req.response_format)
        && is_lite(&String::from_utf8_lossy(&req.body), &req.headers)
}

/// `translateCodexRequestPairWithUpdateIntent` + `helps.ApplyRequestThinking`: the
/// registered pair (Go's model-rewrite fallback for same-format bodies), then the
/// thinking pipeline for `codex` (`openai-response` for compact). Also returns the
/// translated original request, which payload default rules consult. Thinking reads the
/// working translation's configuration-update intent (a plugin normalizer's edit).
fn translate_request(req: &ExecRequest, call: Call, is_compat: bool) -> Result<(String, Vec<u8>), ExecError> {
    let target = call.target();
    let registered = cpa_translate::pair(req.source_format, target).is_some();
    if !registered && !matches!(req.source_format, Format::Codex | Format::OpenAIResponse) {
        // ponytail: Go forwards an untranslatable body as-is; refusing locally is clearer.
        return Err(ExecError::local(
            501,
            FailureScope::Request,
            format!(
                "{} -> {} request translation is not registered",
                req.source_format.as_str(),
                target.as_str()
            ),
        ));
    }
    let model = base_model(&req.model);
    let ctx = cpa_translate::RequestCtx {
        model: &model,
        stream: matches!(call, Call::Stream | Call::Websocket),
        tool_search: false,
    };
    // `translateCodexRequestPairWithUpdateIntent`: an is-compat model's Claude requests
    // use the compat translator, with no client headers or config (Go passes nil).
    let no_headers = HeaderMap::new();
    let compat = crate::codex_client::Client {
        headers: &no_headers,
        settings: Default::default(),
        target_executor: "",
        is_compat: true,
    };
    let translate = |raw: &[u8]| {
        if is_compat && req.source_format == Format::Claude && target == Format::Codex {
            crate::codex_client::translate_request(req.source_format, target, &ctx, raw, &compat)
        } else {
            cpa_translate::translate_request_tracked(req.source_format, target, &ctx, raw)
        }
        .map_err(|e| ExecError::local(400, FailureScope::Request, e.to_string()))
    };
    // Go translates the original first, and equal payloads once.
    let (original, working) = if req.original_body.is_empty() || req.original_body == req.body {
        let working = translate(&req.body)?;
        (working.body.clone(), working)
    } else {
        (translate(&req.original_body)?.body, translate(&req.body)?)
    };
    let body = working.body;
    let caps = req
        .resolved_model
        .as_ref()
        .map(|r| cpa_common::thinking::ModelCaps::from(&r.info));
    let thought = cpa_common::thinking::apply_request_thinking(&cpa_common::thinking::RequestThinking {
        body: &body,
        payload: &req.body,
        original: &req.original_body,
        model: &req.model,
        from: req.source_format.as_str(),
        to: target.as_str(),
        provider: "codex",
        resolved: caps.as_ref().map(Some),
        has_request_transformer: registered,
        updates_changed: working.configuration_updates_changed,
    })
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    let body = String::from_utf8(thought)
        .map_err(|_| ExecError::local(400, FailureScope::Request, "request body is not valid UTF-8 JSON"))?;
    Ok((body, original))
}

/// Go's Codex payload finalizer: the payload rules on the finished body, after the
/// prompt cache key, the input id sanitizing and the SIWC WebSocket rules (868e137).
pub(crate) struct Finalizer {
    call: Call,
    /// The translated original request, the baseline for `default` rules.
    original: Vec<u8>,
}

impl Finalizer {
    pub(crate) fn apply(&self, req: &ExecRequest, settings: &Settings, body: String) -> String {
        apply_payload(req, settings, self.call, &base_model(&req.model), body, &self.original)
    }
}

/// The payload rules on a `response.steer` frame: the frame is its own baseline, and
/// its `type` stays `response.steer`.
pub(crate) fn steer_rules(req: &ExecRequest, settings: &Settings, payload: String) -> String {
    let original = payload.clone().into_bytes();
    let out = apply_payload(
        req,
        settings,
        Call::Websocket,
        &base_model(&req.model),
        payload,
        &original,
    );
    set_str(&out, "type", "response.steer")
}

/// `helps.ApplyPayloadConfigWithRequestForExecutor` for the Codex executors.
fn apply_payload(
    req: &ExecRequest,
    settings: &Settings,
    call: Call,
    model: &str,
    body: String,
    original: &[u8],
) -> String {
    let requested = if req.requested_model.trim().is_empty() {
        req.model.trim()
    } else {
        req.requested_model.trim()
    };
    let rules = cpa_common::payload::Request {
        target_executor: if call == Call::Websocket {
            "codex-websockets"
        } else {
            "codex"
        },
        model,
        requested_model: requested,
        protocol: call.target().as_str(),
        from_protocol: req.source_format.as_str(),
        root: "",
        original,
        request_path: &req.request_path,
        headers: Some(&req.headers),
    };
    let out = cpa_common::payload::apply(&settings.payload, &rules, body.into_bytes());
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// A request body after the Codex rules ([`shape`]).
pub(crate) struct Shaped {
    pub body: String,
    /// The multi-agent v2 optimization renamed the `collaboration` namespace, so upstream
    /// payloads need `cpa_common::codex_client::restore_response`.
    pub optimized: bool,
    /// `HasCodexMultiAgentV2NamespaceConflict` before the optimization: the client already
    /// uses the optimized name, so nothing may be restored.
    pub conflict: bool,
    /// The payload rules, which callers run last.
    pub finalize: Finalizer,
}

/// Applies the Codex body rules for `call`.
// ponytail: the Claude-source reasoning replay cache is not applied here yet.
// ponytail: each rule that edits the body returns a new copy while the previous one is
// still alive, so a streamed 1.9 MB /v1/responses request peaks at 16.8 MB of live heap
// (8.8x the body; alloc.codex.* in bench/budgets.txt). At that peak, heaptrack counted
// about four body-sized buffers from `sanitize_reasoning`'s edits, one each from
// `translate_request`, `ensure_image_tool` and the route's
// `rewrite_orphan_delegation_input`, and about three more in string formatting and JSON
// serialization whose callers it did not resolve. Walking the body once and splicing the
// edits into one output buffer (as crates/cpa-exec/src/claude/signals.rs does) is the
// way toward the 3x target.
pub(crate) fn shape(req: &ExecRequest, view: &View<'_>, settings: &Settings, call: Call) -> Result<Shaped, ExecError> {
    let model = base_model(&req.model);
    let model = model.as_str();
    let is_compat = settings.is_compat(req);
    let (mut body, original) = translate_request(req, call, is_compat)?;
    match call {
        Call::NonStream => {
            body = set_str_if_different(body, "model", model);
            body = set_bool_if_different(body, "stream", true);
            for key in [
                "previous_response_id",
                "generate",
                "prompt_cache_retention",
                "safety_identifier",
                "stream_options",
            ] {
                body = delete(&body, key);
            }
        }
        Call::Stream => {
            for key in [
                "previous_response_id",
                "generate",
                "prompt_cache_retention",
                "safety_identifier",
            ] {
                body = delete(&body, key);
            }
            let delivery = gjson::get(&body, "stream_options.reasoning_summary_delivery");
            let delivery = delivery
                .exists()
                .then(|| (delivery.kind(), delivery.str().to_owned(), delivery.json().to_owned()));
            body = delete(&body, "stream_options");
            if let Some((kind, text, raw)) = delivery {
                // sjson.SetBytes with the decoded value: strings re-quote, scalars stay raw.
                body = if kind == Kind::String {
                    set_str(&body, "stream_options.reasoning_summary_delivery", &text)
                } else {
                    set_raw(&body, "stream_options.reasoning_summary_delivery", &raw)
                };
            }
            body = set_str_if_different(body, "model", model);
        }
        Call::Websocket => {
            body = set_str_if_different(body, "model", model);
            body = set_bool_if_different(body, "stream", true);
            body = delete(&body, "prompt_cache_retention");
            body = delete(&body, "safety_identifier");
        }
        Call::Compact => {
            body = set_str_if_different(body, "model", model);
            body = delete(&body, "stream");
        }
    }
    // Go runs this before the instructions default, except on compact: an appended
    // `tools` key lands before or after an appended `instructions` key accordingly.
    if call != Call::Compact {
        body = ensure_web_search_for_history(body, &req.headers);
    }
    if view.siwc {
        // WebSocket turns get theirs after the prompt cache key ([`siwc_request_body`]).
        if call != Call::Websocket {
            body = siwc_request_body(body, req, call);
        }
    } else if !is_native(req) {
        let instructions = gjson::get(&body, "instructions");
        if !instructions.exists() || instructions.kind() == Kind::Null {
            body = set_str(&body, "instructions", "");
        }
    }
    if call == Call::Compact {
        body = ensure_web_search_for_history(body, &req.headers);
    }
    if call != Call::Compact && settings.image_generation && !view.siwc {
        body = ensure_image_tool(body, model, view, &req.headers);
    }
    body = sanitize_reasoning(body, is_compat);
    body = if call == Call::Websocket {
        if is_lite(&body, &req.headers) {
            set_bool_if_different(body, "parallel_tool_calls", false)
        } else {
            body
        }
    } else {
        normalize_parallel_tool_calls(body, &req.headers)
    };
    let body = normalize_tool_schemas(body);
    let conflict = cpa_common::codex_client::has_namespace_conflict(body.as_bytes());
    let (body, optimized) = crate::codex_client::optimize_for_auth(
        &req.headers,
        body.as_bytes(),
        &settings.client,
        is_compat,
        crate::codex_client::tools_prepared(&req.request_path),
    );
    let body = String::from_utf8(body).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    Ok(Shaped {
        body,
        optimized,
        conflict,
        finalize: Finalizer { call, original },
    })
}

/// The Sign in with ChatGPT body rules for `call` (`applyCodexSIWCRequestBodyRules`,
/// `applyCodexSIWCWebsocketBodyRules`, `applyCodexSIWCCompactBodyRules`). They replace
/// the instructions default and the image tool; WebSocket callers apply them after the
/// prompt cache key, as Go does. `explicit_instructions` is Go's
/// `hasExplicitInstructions` of the client payload; `keep_effort` its
/// `HasThinkingSuffix(req.Model)`.
pub(crate) fn siwc_body(mut body: String, call: Call, explicit_instructions: bool, keep_effort: bool) -> String {
    match call {
        Call::Stream | Call::NonStream => {
            body = set_bool_if_different(body, "store", false);
            body = set_bool_if_different(body, "stream", true);
        }
        Call::Websocket => {
            body = set_bool_if_different(body, "store", false);
            body = delete(&body, "stream");
            body = delete(&body, "background");
        }
        Call::Compact => {
            body = delete(&body, "stream");
            body = delete(&body, "store");
        }
    }
    // stripCodexSIWCUnsupportedFields: SIWC rejects configuration_update items, so they
    // collapse into the top-level effort unless a model suffix already set it.
    let collapsed = cpa_common::thinking::collapse_configuration_updates(body.as_bytes(), keep_effort);
    if collapsed != body.as_bytes() {
        body = String::from_utf8(collapsed).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    }
    for key in [
        "prompt_cache_retention",
        "prompt_cache_options",
        "max_output_tokens",
        "max_completion_tokens",
        "temperature",
    ] {
        body = delete(&body, key);
    }
    if !explicit_instructions {
        let instructions = gjson::get(&body, "instructions");
        let blank = match instructions.kind() {
            Kind::Null => instructions.exists(),
            Kind::String => instructions.str().trim().is_empty(),
            _ => false,
        };
        if blank {
            body = delete(&body, "instructions");
        }
    }
    body
}

/// Go's `hasExplicitInstructions`: the client payload's `instructions` is present and
/// its text is not blank.
pub(crate) fn explicit_instructions(payload: &[u8]) -> bool {
    let value = cpa_common::json::get(payload, "instructions");
    match value.kind {
        cpa_common::json::Kind::Null => false,
        cpa_common::json::Kind::String => !value.str().trim().is_empty(),
        _ => value.exists(),
    }
}

/// [`siwc_body`] with the request's own instruction and suffix flags.
pub(crate) fn siwc_request_body(body: String, req: &ExecRequest, call: Call) -> String {
    siwc_body(
        body,
        call,
        explicit_instructions(&req.body),
        cpa_common::thinking::has_thinking_suffix(&req.model),
    )
}

/// `ensureWebSearchToolForHistory` (e7d4c731): upstream rejects replayed
/// `web_search_call` input items without a web search tool ("response protection is
/// unavailable"). A standard request gets `{"type":"web_search"}`; a Responses Lite
/// request, which carries no top-level hosted tools, drops the replayed items instead.
fn ensure_web_search_for_history(body: String, headers: &HeaderMap) -> String {
    use cpa_common::json as gj;
    if !body.contains("web_search_call") {
        return body;
    }
    let input = gj::get(body.as_bytes(), "input");
    if !input.is_array() {
        return body;
    }
    let is_call = |item: &gj::Res<'_>| item.get("type").str().trim() == "web_search_call";
    // Detection walks with ForEach and the rebuild with Array(), as Go does: on a malformed
    // array such as `[{}{...}]`, ForEach stops after the first element.
    let mut has_call = false;
    input.each(|_, item| {
        has_call = is_call(&item);
        !has_call
    });
    if !has_call {
        return body;
    }
    let tools = gj::get(body.as_bytes(), "tools");
    if tools.is_array()
        && tools.array().iter().any(|t| {
            matches!(
                t.get("type").str().trim(),
                "web_search" | "web_search_preview" | "web_search_preview_2025_03_11"
            )
        })
    {
        return body;
    }
    if is_lite(&body, headers) {
        let items = input.array();
        let kept: Vec<&[u8]> = items.iter().filter(|i| !is_call(i)).map(|i| i.raw()).collect();
        let joined = kept.join(&b","[..]);
        return set_raw(&body, "input", &format!("[{}]", String::from_utf8_lossy(&joined)));
    }
    if !tools.is_array() {
        return set_raw(&body, "tools", r#"[{"type":"web_search"}]"#);
    }
    set_raw(&body, "tools.-1", r#"{"type":"web_search"}"#)
}

fn ensure_image_tool(body: String, model: &str, view: &View<'_>, headers: &HeaderMap) -> String {
    const TOOL: &str = r#"{"type":"image_generation","output_format":"png"}"#;
    if is_lite(&body, headers)
        || model.ends_with("spark")
        || (view.credential.provider.eq_ignore_ascii_case("codex") && view.plan_type().eq_ignore_ascii_case("free"))
    {
        return body;
    }
    let tools = gjson::get(&body, "tools");
    if !tools.exists() || tools.kind() != Kind::Array {
        return set_raw(&body, "tools", &format!("[{TOOL}]"));
    }
    let present = tools.array().iter().any(|t| {
        let kind = t.get("type");
        let name = t.get("name");
        match kind.str() {
            "image_generation" => true,
            "function" => name.str() == "image_gen.imagegen",
            "namespace" => {
                name.str() == "image_gen"
                    && t.get("tools")
                        .array()
                        .iter()
                        .any(|n| n.get("type").str() == "function" && n.get("name").str() == "imagegen")
            }
            _ => false,
        }
    });
    if present {
        body
    } else {
        set_raw(&body, "tools.-1", TOOL)
    }
}

fn normalize_parallel_tool_calls(body: String, headers: &HeaderMap) -> String {
    if is_lite(&body, headers) {
        return set_bool_if_different(body, "parallel_tool_calls", false);
    }
    if !gjson::get(&body, "parallel_tool_calls").exists() {
        return body;
    }
    let tools = gjson::get(&body, "tools");
    if tools.kind() == Kind::Array && !tools.array().is_empty() {
        return body;
    }
    delete(&body, "parallel_tool_calls")
}

/// `sanitizeOpenAIResponsesReasoningEncryptedContentWithCompat`. Compat models
/// (third-party Responses upstreams) keep reasoning `content` and orphan ids; invalid
/// `encrypted_content` is dropped either way.
fn sanitize_reasoning(body: String, is_compat: bool) -> String {
    let input = gjson::get(&body, "input");
    if input.kind() != Kind::Array {
        return body;
    }
    let strip_orphan_ids = !gjson::get(&body, "store").bool();
    let items = input.array();
    let mut rebuilt: Option<Vec<String>> = None;
    for (index, item) in items.iter().enumerate() {
        let mut next = item.json().to_owned();
        let mut changed = false;
        if item.get("type").str().trim() == "reasoning" {
            let content = item.get("content");
            if !is_compat && content.kind() == Kind::Array && !content.array().is_empty() {
                let summary = item.get("summary");
                let empty_summary = !summary.exists()
                    || summary.kind() == Kind::Null
                    || (summary.kind() == Kind::Array && summary.array().is_empty());
                if empty_summary {
                    let parts: Vec<String> = content
                        .array()
                        .iter()
                        .filter(|p| p.get("type").str().trim() == "reasoning_text" && !p.get("text").str().is_empty())
                        .map(|p| set_str(r#"{"type":"summary_text"}"#, "text", p.get("text").str()))
                        .collect();
                    if !parts.is_empty() {
                        next = set_raw(&next, "summary", &format!("[{}]", parts.join(",")));
                    }
                }
                next = set_raw(&next, "content", "[]");
                changed = true;
            }
            let encrypted = item.get("encrypted_content");
            let has_id = item.get("id").exists();
            if !encrypted.exists() {
                if !is_compat && strip_orphan_ids && has_id {
                    next = delete(&next, "id");
                    changed = true;
                }
            } else {
                let valid = encrypted.kind() == Kind::String
                    && encrypted.str() == encrypted.str().trim()
                    && cpa_common::signature::is_valid_gpt_reasoning_signature(encrypted.str());
                if !valid {
                    next = delete(&next, "encrypted_content");
                    changed = true;
                    if !is_compat && strip_orphan_ids && has_id {
                        next = delete(&next, "id");
                    }
                }
            }
        }
        if changed && rebuilt.is_none() {
            rebuilt = Some(items[..index].iter().map(|i| i.json().to_owned()).collect());
        }
        if let Some(rebuilt) = rebuilt.as_mut() {
            rebuilt.push(if changed { next } else { item.json().to_owned() });
        }
    }
    match rebuilt {
        Some(items) => set_raw(&body, "input", &format!("[{}]", items.join(","))),
        None => body,
    }
}

/// `helps.NormalizeCodexToolSchemas` (codex_tool_schema.go): strip what strict upstream
/// validators reject, then collapse large pure-`const` unions into `enum`. Only the Codex
/// executors call it, so it lives here rather than in `cpa_common::payload`.
fn normalize_tool_schemas(body: String) -> String {
    let tools = gjson::get(&body, "tools");
    match normalize_tool_list(&tools) {
        Some(updated) => set_raw(&body, "tools", &updated),
        None => body,
    }
}

fn normalize_tool_list(tools: &gjson::Value<'_>) -> Option<String> {
    if tools.kind() != Kind::Array {
        return None;
    }
    let raw = tools.json();
    let mut out = String::new();
    let mut offset = 0;
    let mut changed = false;
    tools.each(|_, tool| {
        if let Some(updated) = normalize_tool(&tool) {
            let start = tool.json().as_ptr() as usize - raw.as_ptr() as usize;
            out.push_str(&raw[offset..start]);
            out.push_str(&updated);
            offset = start + tool.json().len();
            changed = true;
        }
        true
    });
    changed.then(|| {
        out.push_str(&raw[offset..]);
        out
    })
}

fn normalize_tool(tool: &gjson::Value<'_>) -> Option<String> {
    let kind = tool.get("type");
    match kind.str() {
        "namespace" => {
            let inner = normalize_tool_list(&tool.get("tools"))?;
            Some(set_raw(tool.json(), "tools", &inner))
        }
        "function" | "custom" => {
            let params = tool.get("parameters");
            if params.kind() != Kind::Object {
                return None;
            }
            let stripped = strip_incompatible(params.json());
            let mut changed = stripped.is_some();
            let mut raw = stripped.unwrap_or_else(|| params.json().to_owned());
            let params = gjson::parse(&raw);
            let mut updates = Vec::new();
            let properties = params.get("properties");
            if properties.kind() == Kind::Object {
                properties.each(|name, prop| {
                    if let Some(updated) = normalize_property(&prop) {
                        updates.push((name.str().to_owned(), updated));
                    }
                    true
                });
            }
            for (name, updated) in updates {
                let key = name.replace('\\', "\\\\").replace('.', "\\.").replace(':', "\\:");
                raw = set_raw(&raw, &format!("properties.{key}"), &updated);
                changed = true;
            }
            changed.then(|| set_raw(tool.json(), "parameters", &raw))
        }
        _ => None,
    }
}

/// `stripIncompatiblePatternsFromJSON`: drops `pattern`s (and `patternProperties` keys)
/// with unsupported regex escapes and integer numeric bounds outside int64 (57d005fe),
/// visiting only subschema locations. A changed schema is re-encoded as Go's Encoder
/// writes a `map[string]any` (sorted keys, no HTML escaping, literal numbers).
fn strip_incompatible(raw: &str) -> Option<String> {
    // Go's fast path: a JSON-escaped `\\p{` contains `\p{`; `\\0`; any `\u` escape;
    // the four bound keywords.
    if !["\\p{", "\\P{", "\\u", "\\\\0", "inimum", "aximum"]
        .iter()
        .any(|needle| raw.contains(needle))
    {
        return None;
    }
    let mut root = cpa_common::json::GoValue::parse(raw.as_bytes())?;
    if root == cpa_common::json::GoValue::Null || !strip_schema(&mut root) {
        return None;
    }
    String::from_utf8(root.marshal_no_html()).ok()
}

/// `codexNumericBoundKeywords`.
const NUMERIC_BOUNDS: [&str; 4] = ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"];

/// `stripIncompatiblePatterns`.
fn strip_schema(value: &mut cpa_common::json::GoValue) -> bool {
    use cpa_common::json::GoValue;
    let escape = |s: &str| cpa_common::schema::has_unsupported_unicode_property_escape(s.as_bytes());
    let mut changed = false;
    match value {
        GoValue::Object(map) => {
            if matches!(map.get("pattern"), Some(GoValue::String(p)) if escape(p)) {
                map.remove("pattern");
                changed = true;
            }
            for key in NUMERIC_BOUNDS {
                if matches!(map.get(key), Some(GoValue::Number(n)) if bound_out_of_int64(n)) {
                    map.remove(key);
                    changed = true;
                }
            }
            if let Some(GoValue::Object(patterns)) = map.get_mut("patternProperties") {
                patterns.retain(|key, sub| {
                    if escape(key) {
                        changed = true;
                        return false;
                    }
                    changed |= strip_schema(sub);
                    true
                });
            }
            for key in cpa_common::schema::SCHEMA_MAP_KEYWORDS {
                let key = std::str::from_utf8(key).unwrap_or_default();
                if key == "patternProperties" {
                    continue;
                }
                if let Some(GoValue::Object(sub)) = map.get_mut(key) {
                    for schema in sub.values_mut() {
                        changed |= strip_schema(schema);
                    }
                }
            }
            for key in cpa_common::schema::SCHEMA_VALUE_KEYWORDS {
                match map.get_mut(std::str::from_utf8(key).unwrap_or_default()) {
                    Some(sub @ GoValue::Object(_)) => changed |= strip_schema(sub),
                    Some(GoValue::Array(items)) => {
                        for item in items {
                            changed |= strip_schema(item);
                        }
                    }
                    _ => {}
                }
            }
        }
        GoValue::Array(items) => {
            for item in items {
                changed |= strip_schema(item);
            }
        }
        _ => {}
    }
    changed
}

/// `isNumericBoundOutOfInt64`: whether a JSON number is an integer outside int64. A
/// fraction or a negative effective exponent is not an integer and stays.
fn bound_out_of_int64(number: &str) -> bool {
    use std::num::IntErrorKind::{NegOverflow, PosOverflow};
    let overflows = |text: &str| matches!(text.parse::<i64>(), Err(e) if matches!(e.kind(), PosOverflow | NegOverflow));
    let raw = number.trim();
    if raw.is_empty() {
        return false;
    }
    if !raw.contains(['.', 'e', 'E']) {
        return overflows(raw);
    }
    let (negative, s) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    };
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(at) => {
            let (mantissa, exp) = (&s[..at], &s[at + 1..]);
            match exp.parse::<i64>() {
                Ok(exp) => (mantissa, exp),
                Err(e) if matches!(e.kind(), PosOverflow | NegOverflow) => {
                    return !exp.starts_with('-') && !mantissa.trim_matches(['0', '.']).is_empty();
                }
                Err(_) => return false,
            }
        }
        None => (s, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((_, frac)) if frac.contains('.') => return false,
        Some(parts) => parts,
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return false;
    }
    if !int_part.bytes().chain(frac_part.bytes()).all(|b| b.is_ascii_digit()) {
        return false;
    }
    let digits = format!("{int_part}{frac_part}");
    let significant = digits.trim_start_matches('0');
    if significant.is_empty() {
        return false;
    }
    let len = raw.len() as i64;
    if exp < -len {
        return false;
    }
    if exp > len + 20 {
        return true;
    }
    let trimmed = significant.trim_end_matches('0');
    let trailing_zeros = (significant.len() - trimmed.len()) as i64;
    let effective = exp - frac_part.len() as i64 + trailing_zeros;
    if effective < 0 {
        return false;
    }
    if effective > 19 || trimmed.len() as i64 + effective > 19 {
        return true;
    }
    let sign = if negative { "-" } else { "" };
    overflows(&format!("{sign}{trimmed}{}", "0".repeat(effective as usize)))
}

fn normalize_property(prop: &gjson::Value<'_>) -> Option<String> {
    if prop.kind() != Kind::Object {
        return None;
    }
    let name = match (prop.get("oneOf").exists(), prop.get("anyOf").exists()) {
        (true, false) => "oneOf",
        (false, true) => "anyOf",
        _ => return None,
    };
    let union = prop.get(name);
    let branches = union.array();
    if union.kind() != Kind::Array || branches.len() < 8 {
        return None;
    }
    let mut keys = Vec::new();
    let mut raws = Vec::new();
    for branch in &branches {
        if branch.kind() != Kind::Object {
            return None;
        }
        let mut only_const = true;
        branch.each(|k, _| {
            only_const &= matches!(k.str(), "const" | "description" | "title");
            true
        });
        let value = branch.get("const");
        if !only_const || !value.exists() {
            return None;
        }
        let key = canonical_key(&value)?;
        if keys.contains(&key) {
            return None;
        }
        keys.push(key);
        raws.push(value.json().to_owned());
    }
    let existing = prop.get("enum");
    if existing.exists() && existing.kind() == Kind::Array {
        let mut existing_keys = Vec::new();
        for v in existing.array() {
            existing_keys.push(canonical_key(&v)?);
        }
        let mut unique = existing_keys.clone();
        unique.sort();
        unique.dedup();
        let mut wanted = keys.clone();
        wanted.sort();
        let same = existing_keys.len() == keys.len() && unique.len() == existing_keys.len() && unique == wanted;
        return same.then(|| delete(prop.json(), name));
    }
    let with_enum = set_raw(prop.json(), "enum", &format!("[{}]", raws.join(",")));
    Some(delete(&with_enum, name))
}

/// `canonicalJSONValueKey`: numbers compare as exact rationals (`1` == `1.0` == `1e0`).
fn canonical_key(value: &gjson::Value<'_>) -> Option<String> {
    Some(match value.kind() {
        Kind::String => format!("s:{}", value.str()),
        Kind::Number => format!("n:{}", rational(value.json().trim())),
        Kind::True => "b:true".into(),
        Kind::False => "b:false".into(),
        Kind::Null => "null".into(),
        _ => return None,
    })
}

/// Exact decimal normal form of a JSON number: sign, significant digits, exponent.
fn rational(raw: &str) -> String {
    let (mantissa, exp) = match raw.find(['e', 'E']) {
        Some(i) => (&raw[..i], raw[i + 1..].parse::<i64>().unwrap_or(0)),
        None => (raw, 0),
    };
    let negative = mantissa.starts_with('-');
    let mantissa = mantissa.trim_start_matches(['-', '+']);
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let exp = exp - frac.len() as i64;
    let digits = digits.trim_start_matches('0');
    if digits.is_empty() {
        return "0".into();
    }
    let trimmed = digits.trim_end_matches('0');
    let exp = exp + (digits.len() - trimmed.len()) as i64;
    format!("{}{trimmed}e{exp}", if negative { "-" } else { "" })
}

/// `helps.SanitizeCodexInputItemIDs`: type prefixes, the 64-char limit, collision suffixes
/// and dropping over-long encrypted reasoning items.
pub(crate) fn sanitize_input_ids(body: String) -> String {
    const LIMIT: usize = 64;
    const OCCUPIED: u8 = 1;
    const PRESERVED: u8 = 2;
    let input = gjson::get(&body, "input");
    if input.kind() != Kind::Array {
        return body;
    }
    let items = input.array();
    let normalize = |item: &gjson::Value<'_>, id: &str| -> String {
        let prefix = match item.get("type").str() {
            "message" => "msg",
            "reasoning" => "rs",
            "function_call" => "fc",
            "custom_tool_call" => "ctc",
            "custom_tool_call_output" => "ctco",
            _ => return id.to_owned(),
        };
        if id.is_empty() || id.starts_with(prefix) {
            id.to_owned()
        } else {
            format!("{prefix}_{id}")
        }
    };
    let dropped = |item: &gjson::Value<'_>| {
        let id = item.get("id");
        let enc = item.get("encrypted_content");
        item.get("type").str() == "reasoning"
            && id.kind() == Kind::String
            && id.str().chars().count() > LIMIT
            && enc.kind() == Kind::String
            && !enc.str().is_empty()
    };
    let mut states: std::collections::HashMap<String, u8> = Default::default();
    for item in &items {
        let id = item.get("id");
        if dropped(item) || id.kind() != Kind::String {
            continue;
        }
        let normalized = normalize(item, id.str());
        let mut state = states.get(&normalized).copied().unwrap_or(0);
        if normalized == id.str() {
            state |= PRESERVED;
        }
        if normalized.chars().count() <= LIMIT {
            state |= OCCUPIED;
        }
        if state != 0 {
            states.insert(normalized, state);
        }
    }
    let hashed = |id: &str, attempt: usize| {
        let mut input = id.to_owned();
        if attempt > 0 {
            input.push('\0');
            input.push_str(&attempt.to_string());
        }
        let digest = Sha256::digest(input.as_bytes());
        let suffix = format!(
            "_{}",
            digest[..8].iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        let keep = (LIMIT - suffix.len()).min(id.chars().count());
        format!("{}{suffix}", id.chars().take(keep).collect::<String>())
    };
    let mut shortened: std::collections::HashMap<String, String> = Default::default();
    let mut collisions: std::collections::HashMap<String, String> = Default::default();
    let mut rebuilt = Vec::with_capacity(items.len());
    let mut changed = false;
    for item in &items {
        if dropped(item) {
            changed = true;
            continue;
        }
        let mut raw = item.json().to_owned();
        let id = item.get("id");
        if id.kind() == Kind::String {
            let original = id.str();
            let mut next = normalize(item, original);
            if next != original && states.get(&next).is_some_and(|s| s & PRESERVED != 0) {
                next = match collisions.get(&next) {
                    Some(c) => c.clone(),
                    None => {
                        let mut attempt = 0;
                        let candidate = loop {
                            let candidate = hashed(&next, attempt);
                            if states.get(&candidate).is_some_and(|s| s & OCCUPIED != 0) {
                                attempt += 1;
                                continue;
                            }
                            break candidate;
                        };
                        collisions.insert(next.clone(), candidate.clone());
                        *states.entry(candidate.clone()).or_default() |= OCCUPIED;
                        candidate
                    }
                };
            }
            if next.chars().count() > LIMIT {
                next = match shortened.get(&next) {
                    Some(s) => s.clone(),
                    None => {
                        let mut candidate = hashed(&next, 0);
                        let mut attempt = 1;
                        while states.get(&candidate).is_some_and(|s| s & OCCUPIED != 0) {
                            candidate = hashed(&next, attempt);
                            attempt += 1;
                        }
                        shortened.insert(next.clone(), candidate.clone());
                        *states.entry(candidate.clone()).or_default() |= OCCUPIED;
                        candidate
                    }
                };
            }
            if next != original {
                raw = set_str(&raw, "id", &next);
                changed = true;
            }
        }
        rebuilt.push(raw);
    }
    if changed {
        set_raw(&body, "input", &format!("[{}]", rebuilt.join(",")))
    } else {
        body
    }
}

/// `uuid.NewString()`: a random v4 UUID.
pub(crate) fn random_uuid() -> String {
    let mut bytes = [0u8; 16];
    let _ = getrandom::fill(&mut bytes);
    uuid::Builder::from_random_bytes(bytes).into_uuid().to_string()
}

/// UUIDv5(OID, "cli-proxy-api\0codex\0<kind>\0<id>") (`stableProviderSessionUUID`).
pub(crate) fn provider_session_uuid(kind: &str, id: &str) -> Option<String> {
    let id = id.trim();
    (!id.is_empty()).then(|| {
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api\0codex\0{kind}\0{id}").as_bytes(),
        )
        .to_string()
    })
}

/// `ProviderSessionUUID`: the long-lived execution session (the downstream WebSocket
/// connection) when there is one, else the derived session identity.
pub(crate) fn session_uuid(req: &ExecRequest, ws_session: Option<&str>) -> Option<String> {
    let execution = ws_session
        .or(req.execution_session.as_deref())
        .filter(|id| !id.trim().is_empty());
    match execution {
        Some(id) => provider_session_uuid("execution-session", id),
        None => req
            .derived_session
            .as_deref()
            .and_then(|id| provider_session_uuid("derived-session", id)),
    }
}

/// `helps.ClaudeCodePromptCache`: one deterministic key per Claude Code agent and model.
fn claude_code_prompt_cache(model: &str, payload: &[u8], headers: &HeaderMap) -> Option<String> {
    let model = model.trim();
    let scope = crate::replay::claude_code_session(payload, headers)?;
    (!model.is_empty()).then(|| {
        let identity = format!("cli-proxy-api:codex:claude-code\0{model}\0{scope}");
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string()
    })
}

/// `cacheHelper` / `applyCodexPromptCacheHeadersWithContext`: the prompt cache key from
/// the client body (Claude Code agent, Responses or Chat) or the session identity,
/// written into the body.
pub(crate) fn prompt_cache(
    req: &ExecRequest,
    body: String,
    ws_session: Option<&str>,
    websocket: bool,
) -> (String, Option<String>) {
    let original = String::from_utf8_lossy(&req.body);
    let client_key = gjson::get(&original, "prompt_cache_key");
    let mut id = match req.source_format {
        Format::Claude => {
            let mut model = gjson::get(&body, "model").str().trim().to_owned();
            if model.is_empty() {
                model = base_model(&req.model);
            }
            claude_code_prompt_cache(&model, &req.body, &req.headers).unwrap_or_default()
        }
        Format::OpenAIResponse if client_key.exists() => client_key.str().to_owned(),
        Format::OpenAI if client_key.exists() && !websocket => client_key.str().trim().to_owned(),
        _ => String::new(),
    };
    if id.is_empty() {
        id = session_uuid(req, ws_session).unwrap_or_default();
    }
    if id.is_empty() && !websocket && req.source_format == Format::OpenAI && !req.caller.principal.trim().is_empty() {
        id = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("cli-proxy-api:codex:prompt-cache:{}", req.caller.principal.trim()).as_bytes(),
        )
        .to_string();
    }
    if id.is_empty() {
        return (body, None);
    }
    (set_str_if_different(body, "prompt_cache_key", &id), Some(id))
}

fn set(headers: &mut HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::from_str(value)) {
        headers.insert(name, value);
    }
}

/// `misc.EnsureHeader`: the client's value wins, then what is already set.
fn ensure_from_client(headers: &mut HeaderMap, client: &HeaderMap, name: &str) {
    let value = header(client, name).trim();
    if !value.is_empty() {
        set(headers, name, value);
    }
}

/// `ensureHeaderWithConfigPrecedence`: existing, config, client, fallback.
fn ensure_config_first(headers: &mut HeaderMap, client: &HeaderMap, name: &str, config: &str, fallback: &str) {
    if !header(headers, name).trim().is_empty() {
        return;
    }
    for value in [config, header(client, name), fallback] {
        if !value.trim().is_empty() {
            set(headers, name, value.trim());
            return;
        }
    }
}

/// `util.ApplyCustomHeadersFromAttrs` (`cpa_common::headers`) with the view's explicit
/// `$CPA-SESSION-ID`.
pub(crate) fn custom_headers(view: &View<'_>, client: &HeaderMap) -> Vec<(String, String)> {
    cpa_common::headers::custom_headers(&view.credential.attributes, client, view.session.as_deref())
}

/// `applyModelHeaderOverrides`: models.json `config.override_header` for the model.
fn model_overrides(model: &str) -> Vec<(String, String)> {
    let Some(info) = cpa_core::registry::pinned().lookup(model) else {
        return Vec::new();
    };
    info.raw
        .get("config")
        .and_then(|c| c.get("override_header"))
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !k.trim().is_empty())
                .filter_map(|(k, v)| Some((k.trim().to_owned(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// HTTP upstream headers (`applyCodexHeadersFromSources`, routing hint, model overrides).
pub(crate) fn http_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    let mut h = source_headers(view, settings, client, cache_id, stream);
    routing_hint(&mut h, view, client, body, model);
    model_header_overrides(&mut h, model);
    h
}

/// Images API upstream headers: `applyCodexHeaders` (or `applyCodexDirectImageHeaders`,
/// whose caller drops the client's User-Agent) and `applyModelHeaderOverrides`, without
/// the routing hint.
pub(crate) fn image_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    model: &str,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    let mut h = source_headers(view, settings, client, cache_id, stream);
    model_header_overrides(&mut h, model);
    h
}

/// `cacheHelper`'s `Session-Id`, then `applyCodexHeadersFromSources`.
fn source_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    if view.siwc {
        return siwc_http_headers(view, settings, client, cache_id, stream);
    }
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session-id", id);
    }
    set(&mut h, "content-type", "application/json");
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    for name in [
        "x-codex-beta-features",
        "version",
        "x-codex-turn-metadata",
        "x-codex-turn-state",
        "x-client-request-id",
        "x-codex-window-id",
        "thread-id",
        "session-id",
        LITE_HEADER,
    ] {
        ensure_from_client(&mut h, client, name);
    }
    let default_ua = if view.api_key { "" } else { &settings.default_user_agent };
    ensure_config_first(&mut h, client, "user-agent", default_ua, USER_AGENT);
    set(
        &mut h,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    set(&mut h, "connection", "Keep-Alive");
    apply_identity(&mut h, view, settings, client);
    h
}

/// The Sign in with ChatGPT branch of `applyCodexHeadersFromSources`: the session as
/// `session_id` and the client request id, the SIWC User-Agent and operator headers; no
/// originator, account id, client Codex headers or cloaking.
fn siwc_http_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    cache_id: Option<&str>,
    stream: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    set(&mut h, "content-type", "application/json");
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    if let Some(id) = cache_id.map(str::trim).filter(|id| !id.is_empty()) {
        set(&mut h, "session_id", id);
        set(&mut h, "x-client-request-id", id);
    }
    set(&mut h, "user-agent", &settings.siwc_user_agent);
    set(
        &mut h,
        "accept",
        if stream {
            "text/event-stream"
        } else {
            "application/json"
        },
    );
    set(&mut h, "connection", "Keep-Alive");
    for (name, value) in custom_headers(view, client) {
        set(&mut h, &name, &value);
    }
    h
}

/// Originator, account header, operator headers and cloaking, shared by HTTP and WebSocket.
fn apply_identity(h: &mut HeaderMap, view: &View<'_>, settings: &Settings, client: &HeaderMap) {
    let originator = header(client, "originator").trim();
    if !originator.is_empty() {
        set(h, "originator", originator);
    } else if !view.api_key {
        set(h, "originator", ORIGINATOR);
    }
    if !view.api_key
        && let Some(account) = view.credential.str("account_id")
    {
        set(h, "chatgpt-account-id", account.trim());
    }
    for (name, value) in custom_headers(view, client) {
        set(h, &name, &value);
    }
    if !view.cloaking_disabled(settings) {
        set(h, "user-agent", USER_AGENT);
        set(h, "originator", ORIGINATOR);
    }
}

/// `applyCodexRoutingHint`: `model=<slug>[;tier=<service_tier>]` for OAuth credentials.
fn routing_hint(h: &mut HeaderMap, view: &View<'_>, client: &HeaderMap, body: &str, model: &str) {
    if view.api_key || view.siwc {
        return;
    }
    h.remove(ROUTING_HINT);
    let operator = custom_headers(view, client)
        .into_iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(ROUTING_HINT))
        .map(|(_, v)| v);
    if let Some(value) = operator.filter(|v| !v.trim().is_empty()) {
        set(h, ROUTING_HINT, value.trim());
        return;
    }
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    let mut hint = format!("model={model}");
    let tier = gjson::get(body, "service_tier");
    if tier.kind() == Kind::String && !tier.str().trim().is_empty() {
        hint.push_str(";tier=");
        hint.push_str(tier.str().trim());
    }
    set(h, ROUTING_HINT, &hint);
}

fn model_header_overrides(h: &mut HeaderMap, model: &str) {
    let overrides = model_overrides(model);
    if overrides.is_empty() {
        return;
    }
    for (k, v) in overrides {
        set(h, &k, &v);
    }
    let has_session = ["session-id", "session_id"]
        .iter()
        .any(|n| !header(h, n).trim().is_empty());
    if header(h, "user-agent").contains("Mac OS") && !has_session {
        set(h, "session_id", &random_uuid());
    }
}

/// WebSocket handshake headers (`applyCodexPromptCacheHeadersWithContext`,
/// `applyCodexWebsocketHeaders`, routing hint, model overrides).
pub(crate) fn ws_headers(
    view: &View<'_>,
    settings: &Settings,
    client: &HeaderMap,
    body: &str,
    model: &str,
    cache_id: Option<&str>,
    native: bool,
) -> HeaderMap {
    let mut h = HeaderMap::new();
    if let Some(id) = cache_id {
        set(&mut h, "session_id", id);
        set(&mut h, "conversation_id", id);
    }
    if !view.token.trim().is_empty() {
        set(&mut h, "authorization", &format!("Bearer {}", view.token));
    }
    if view.siwc {
        siwc_ws_headers(&mut h, view, settings, client);
        model_header_overrides(&mut h, model);
        return h;
    }
    let (default_ua, default_beta) = if view.api_key {
        ("", "")
    } else {
        (
            settings.default_user_agent.as_str(),
            settings.default_beta_features.as_str(),
        )
    };
    // ensureHeaderWithPriority: existing, client, config.
    for value in [header(client, "x-codex-beta-features"), default_beta] {
        if !value.trim().is_empty() {
            set(&mut h, "x-codex-beta-features", value.trim());
            break;
        }
    }
    for name in [
        "x-codex-turn-state",
        "x-codex-turn-metadata",
        "x-client-request-id",
        "x-responsesapi-include-timing-metrics",
        "version",
    ] {
        ensure_from_client(&mut h, client, name);
    }
    if native {
        ensure_from_client(&mut h, client, LITE_HEADER);
    }
    if view.api_key {
        let ua = header(client, "user-agent").trim();
        if !ua.is_empty() {
            set(&mut h, "user-agent", ua);
        }
    } else {
        ensure_config_first(&mut h, client, "user-agent", default_ua, USER_AGENT);
    }
    let mut beta = header(client, "openai-beta").trim().to_owned();
    if !beta.contains("responses_websockets=") {
        beta = WS_BETA.to_owned();
    }
    set(&mut h, "openai-beta", &beta);
    // ensureCodexWebsocketSessionHeader: existing, client, then a fresh id for Mac OS UAs.
    let mut session = header(&h, "session_id").trim().to_owned();
    if session.is_empty() {
        session = ["session-id", "session_id"]
            .iter()
            .map(|n| header(client, n).trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_owned();
    }
    if session.is_empty() && header(&h, "user-agent").contains("Mac OS") {
        session = random_uuid();
    }
    if !session.is_empty() {
        set(&mut h, "session_id", &session);
    }
    h.remove("session-id");
    if native && view.cloaking_disabled(settings) {
        h.remove("session_id");
        h.remove("conversation_id");
        for name in [
            "session-id",
            "session_id",
            "conversation_id",
            "thread-id",
            ROUTING_HINT,
            "x-codex-window-id",
        ] {
            for value in client.get_all(name) {
                h.append(HeaderName::from_static(name), value.clone());
            }
        }
    }
    apply_identity(&mut h, view, settings, client);
    routing_hint(&mut h, view, client, body, model);
    model_header_overrides(&mut h, model);
    h
}

/// The Sign in with ChatGPT branch of `applyCodexWebsocketHeaders`: no conversation
/// header, the session (prompt cache or client) also as the client request id, the
/// SIWC User-Agent, the client's WebSocket beta header when it names one, then operator
/// headers.
fn siwc_ws_headers(h: &mut HeaderMap, view: &View<'_>, settings: &Settings, client: &HeaderMap) {
    h.remove("conversation_id");
    let existing = header(h, "session_id").trim().to_owned();
    let session = if existing.is_empty() {
        ["session-id", "session_id"]
            .iter()
            .map(|n| header(client, n).trim())
            .find(|v| !v.is_empty())
            .unwrap_or_default()
            .to_owned()
    } else {
        existing
    };
    if session.is_empty() {
        ensure_from_client(h, client, "x-client-request-id");
    } else {
        set(h, "session_id", &session);
        set(h, "x-client-request-id", &session);
    }
    set(h, "user-agent", &settings.siwc_user_agent);
    let beta = header(client, "openai-beta").trim();
    let beta = if beta.contains("responses_websockets=") {
        beta
    } else {
        WS_BETA
    };
    set(h, "openai-beta", beta);
    for (name, value) in custom_headers(view, client) {
        set(h, &name, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go v8.0.35's `NormalizeCodexToolSchemas` (57d005fe bounds, patterns, re-encoding)
    /// and `NormalizeCodexToolIntegerTypes` (d7914afd paths and namespaces), byte for
    /// byte; see tests/reference/codex_tool_schema.
    #[test]
    fn tool_schema_passes_match_go() {
        let go: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/codex_tool_schema_go.json")).unwrap();
        let expected = |case: &serde_json::Value| case.get("out").unwrap_or(&case["in"]).as_str().unwrap().to_owned();
        for case in go["schemas"].as_array().unwrap() {
            let input = case["in"].as_str().unwrap().to_owned();
            assert_eq!(normalize_tool_schemas(input), expected(case), "{}", case["name"]);
        }
        for case in go["integers"].as_array().unwrap() {
            let mut headers = HeaderMap::new();
            if let Some(ua) = case["ua"].as_str() {
                headers.insert(http::header::USER_AGENT, ua.parse().unwrap());
            }
            let out = cpa_common::payload::normalize_codex_tool_integer_types(
                case["in"].as_str().unwrap().as_bytes(),
                &headers,
            );
            assert_eq!(
                String::from_utf8(out.clone()).unwrap(),
                expected(case),
                "{} {:?}",
                case["name"],
                case["ua"]
            );
            // Go's tests also require a second pass to leave the output alone.
            let again = cpa_common::payload::normalize_codex_tool_integer_types(&out, &headers);
            assert_eq!(again, out, "{} is not idempotent", case["name"]);
        }
    }

    /// Go v8.0.35's `ensureWebSearchToolForHistory` (e7d4c731), byte for byte; see
    /// tests/reference/codex_web_search_history.
    #[test]
    fn web_search_history_matches_go() {
        let go: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/codex_web_search_history_go.json")).unwrap();
        for case in go["cases"].as_array().unwrap() {
            let mut headers = HeaderMap::new();
            if let Some(value) = case["lite_header"].as_str() {
                // Raw bytes: a non-ASCII value such as U+00A0 reaches the Lite check as sent.
                headers.insert(LITE_HEADER, HeaderValue::from_bytes(value.as_bytes()).unwrap());
            }
            let input = case["in"].as_str().unwrap();
            let want = case.get("out").unwrap_or(&case["in"]).as_str().unwrap();
            assert_eq!(
                ensure_web_search_for_history(input.to_owned(), &headers),
                want,
                "{}",
                case["name"]
            );
        }
    }

    /// Go `TestHomeCodexCompatUsesSelectedCredentialOptions`: without a binding or config
    /// models, a Home credential's `credential_options` decide is-compat for the upstream
    /// name, its alias and its suffixed form; another credential and standalone mode do
    /// not see them.
    #[test]
    fn home_codex_is_compat_reads_the_credential_options() {
        let mut credential = Credential {
            id: "c1".into(),
            provider: "codex".into(),
            source: cpa_core::credential::Source::File("c1".into()),
            disabled: false,
            label: String::new(),
            attributes: Default::default(),
            metadata: serde_json::json!({"credential_options": {"models": [{"name": "upstream", "alias": "alias", "is-compat": true}]}})
                .as_object()
                .cloned()
                .unwrap(),
            revision: 0,
            next_refresh_after: None,
        };
        let settings = |c: &Credential| Settings {
            home: c
                .attributes
                .contains_key(cpa_core::config::credentials::HOME_PROVIDER)
                .then(|| c.clone()),
            ..Settings::default()
        };
        let request = |model: &str| ExecRequest {
            operation: cpa_core::exec::Operation::Generate,
            source_format: Format::Codex,
            response_format: Format::Codex,
            stream_payloads: false,
            requested_model: model.into(),
            model: model.into(),
            original_body: Default::default(),
            body: Default::default(),
            stream: false,
            alt: None,
            session: None,
            execution_session: None,
            derived_session: None,
            request_path: String::new(),
            headers: Default::default(),
            caller: cpa_core::exec::Caller {
                principal: String::new(),
                source: "",
                provider: String::new(),
            },
            resolved_model: None,
            usage: Default::default(),
        };
        credential
            .attributes
            .insert(cpa_core::config::credentials::HOME_PROVIDER.into(), "codex".into());
        for model in ["upstream", "alias", "upstream(high)"] {
            assert!(settings(&credential).is_compat(&request(model)), "{model}");
        }
        let mut other = credential.clone();
        other.metadata.clear();
        assert!(!settings(&other).is_compat(&request("upstream")));
        credential.attributes.clear();
        assert!(!settings(&credential).is_compat(&request("upstream")), "standalone");

        // Config models compare names and aliases with Go's EqualFold: İ does not fold to
        // i, while σ and ς fold together.
        let keyed = |name: &str| Settings {
            key_models: vec![serde_json::json!({"name": name, "is-compat": true})],
            ..Settings::default()
        };
        assert!(!keyed("mİ").is_compat(&request("mi")), "dotted İ is not i");
        assert!(keyed("ος").is_compat(&request("ΟΣ")), "final sigma folds");
    }

    #[test]
    fn durations_follow_go_parse_duration() {
        assert_eq!(go_duration("1m30s"), Some(Duration::from_secs(90)));
        assert_eq!(go_duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(go_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(go_duration("off"), None);
        assert_eq!(go_duration("soon"), None);
        assert_eq!(go_duration("+2s"), Some(Duration::from_secs(2)));
        assert_eq!(go_duration("3\u{3bc}s"), Some(Duration::from_micros(3)));
        assert_eq!(go_duration("-1s"), None);
        assert_eq!(go_duration("+15"), Some(Duration::from_secs(15)));
    }

    #[test]
    fn rationals_compare_like_big_rat() {
        assert_eq!(rational("8"), rational("8.0"));
        assert_eq!(rational("8"), rational("0.8e1"));
        assert_eq!(rational("-0.50"), rational("-5e-1"));
        assert_ne!(rational("8"), rational("80"));
        assert_eq!(rational("0.000"), "0");
    }

    /// `chatgpt-keep-alive` is off unless the config turns it on, in the v8 layout or the
    /// flat one (whose `codex:` section moves to `oauth.providers.codex`), and YAML 1.1
    /// spellings decode as Go decodes a bool field.
    #[test]
    fn chatgpt_keep_alive_is_off_unless_configured() {
        let cases = [
            ("", false),
            (
                "oauth:\n  providers:\n    codex:\n      disable-codex-cloaking: true\n",
                false,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: false\n",
                false,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: true\n",
                true,
            ),
            (
                "oauth:\n  providers:\n    codex:\n      chatgpt-keep-alive: yes\n",
                true,
            ),
            ("codex:\n  chatgpt-keep-alive: true\n", true),
        ];
        for (yaml, want) in cases {
            let cfg = Config::parse(yaml).unwrap();
            assert_eq!(chatgpt_keep_alive(&cfg), want, "{yaml:?}");
        }
    }
}
