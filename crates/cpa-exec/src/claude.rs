//! Claude executor: Anthropic Messages and count_tokens with OAuth tokens or API keys.
//!
//! The request pipeline follows claude_executor_execute.go / _stream.go / _tokens.go
//! in order: client detection, session identity, translation, cloaking, diagnostics,
//! cache policy, betas, MCP aliases, credential identity, CCH signing, headers, and
//! the native transport. Responses restore aliases and record billing continuity.
//!
//! ponytail: not ported here, each with its owner noted in docs/reviews:
//! payload rules (M4-0031) and thinking-suffix application inside the executor,
//! thinking-signature validation (M1-0020; tool-use signature fields are stripped),
//! Kimi/Vertex delegated providers, Home KV identity, usage reporting and request logs.

mod alias;
mod betas;
mod cloak;
mod detect;
mod headers;
mod identity;
mod profile;
mod session;
mod settings;
mod signals;
mod signing;
mod stream;

use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use cpa_core::config::Config;
use cpa_core::credential::{Credential, MetadataPatch};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::oauth::{self, OAuth};
use crate::rawjson;
use crate::tls::{self, Proxy, Transport};
use crate::upstream::{decoded_response, transport_error};
use crate::{quota, tokens, translate};
use settings::Settings;

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Advertised in `CLIProxyAPI/<version>` when a caller-owned request has no User-Agent.
pub(crate) const PROXY_VERSION: &str = env!("CARGO_PKG_VERSION");

pub use tls::Hooks;

/// The fields of a `"type": "claude"` credential this executor needs.
pub struct ClaudeView<'a> {
    pub access_token: &'a str,
    pub email: &'a str,
}

impl<'a> ClaudeView<'a> {
    pub fn new(credential: &'a Credential) -> Result<Self, ExecError> {
        let access_token = credential
            .attributes
            .get("api_key")
            .map(String::as_str)
            .filter(|t| !t.is_empty())
            .or_else(|| credential.str("access_token"))
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ExecError::local(401, FailureScope::Credential, "claude credential has no access_token"))?;
        Ok(Self {
            access_token,
            email: credential.str("email").unwrap_or_default(),
        })
    }
}

enum Clients {
    /// Production or harness: native/generic/OAuth profiles per effective proxy.
    Transport(Arc<Transport>),
    /// Unit tests against plain HTTP mocks.
    Fixed(wreq::Client),
}

pub struct ClaudeExecutor {
    clients: Clients,
    base_url: String,
    oauth: OAuth,
}

impl ClaudeExecutor {
    /// Production executor: native wire profile with the process-global transport.
    pub fn new(base_url: impl Into<String>) -> wreq::Result<Self> {
        Ok(Self::with_transport(
            Arc::new(Transport::new(Hooks::default())),
            base_url,
        ))
    }

    /// The production profile with test trust roots and dial overrides (harness).
    /// OAuth refresh uses the same transport, so it cannot escape the hooks.
    pub fn with_hooks(hooks: Hooks, base_url: impl Into<String>) -> Self {
        Self::with_transport(Arc::new(Transport::new(hooks)), base_url)
    }

    fn with_transport(transport: Arc<Transport>, base_url: impl Into<String>) -> Self {
        Self {
            oauth: OAuth::with_transport(transport.clone()),
            clients: Clients::Transport(transport),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// A caller-built client for every request (plain mocks); not the native profile.
    pub fn with_client(client: wreq::Client, base_url: impl Into<String>) -> Self {
        Self {
            oauth: OAuth::new(client.clone()),
            clients: Clients::Fixed(client),
            base_url: base_url.into().trim_end_matches('/').to_owned(),
        }
    }

    /// Overrides the OAuth service (local token/profile mocks).
    pub fn with_oauth(mut self, oauth: OAuth) -> Self {
        self.oauth = oauth;
        self
    }

    pub fn needs_prepare(&self, credential: &Credential, _cfg: &Config) -> bool {
        oauth::needs_prepare(credential)
    }

    pub async fn prepare(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.oauth.prepare(credential, &proxy_for(credential, cfg)).await
    }

    pub fn needs_refresh(&self, credential: &Credential, _cfg: &Config) -> bool {
        oauth::needs_refresh(credential)
    }

    pub async fn refresh(&self, credential: &Credential, cfg: &Config) -> Result<MetadataPatch, ExecError> {
        self.oauth
            .refresh_credential(credential, &proxy_for(credential, cfg))
            .await
    }

    fn client(&self, proxy: &Proxy, first_party: bool) -> Result<wreq::Client, ExecError> {
        match &self.clients {
            Clients::Fixed(client) => Ok(client.clone()),
            Clients::Transport(t) => {
                let clients = t
                    .clients(proxy)
                    .map_err(|_| ExecError::local(502, FailureScope::Transport, "upstream request failed"))?;
                Ok(if first_party {
                    clients.native.clone()
                } else {
                    clients.generic.clone()
                })
            }
        }
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        ClaudeView::new(credential)?;
        if req.alt.as_deref() == Some("responses/compact") {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "/responses/compact not supported",
            ));
        }
        if req.response_format != Format::Claude && cpa_translate::pair(req.response_format, Format::Claude).is_none() {
            return Err(ExecError::local(
                501,
                FailureScope::Request,
                "Claude response translation pair is not registered",
            ));
        }
        let ctx = Ctx::new(self, credential, &req, cfg);
        match req.operation {
            Operation::Generate => self.generate(ctx, req).await,
            Operation::CountTokens => self.count_tokens(ctx, req).await,
        }
    }

    async fn generate(&self, ctx: Ctx<'_>, req: ExecRequest) -> Result<ExecResponse, ExecError> {
        let upstream_stream = req.stream || req.response_format != Format::Claude;
        let translated = translate::request(&req)?;
        let prepared = ctx.prepare_messages(&req, &translated, upstream_stream)?;
        let response = self.send(&ctx, &prepared, "/v1/messages").await?;
        let reverse = prepared.reverse.clone();
        let continuity = prepared.continuity.clone();
        let request_id = header_value(&response.headers, "request-id");
        let body = match response.body {
            ResponseBody::Stream(raw) if upstream_stream && req.stream && req.operation == Operation::Generate => {
                let done: stream::OnComplete = Box::new(move |message_id| {
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &message_id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                });
                ResponseBody::Stream(stream::relay(raw, reverse, done))
            }
            ResponseBody::Stream(raw) => {
                let data = collect(raw).await?;
                if upstream_stream {
                    stream::validate_buffered(&data)?;
                    let id = stream::buffered_message_id(&data);
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                    let mut out = Vec::with_capacity(data.len());
                    for (i, line) in data.split(|b| *b == b'\n').enumerate() {
                        if i > 0 {
                            out.push(b'\n');
                        }
                        out.extend(stream::restore_line(line, &reverse).map_err(|m| {
                            ExecError::local(
                                502,
                                FailureScope::Request,
                                format!("restore Claude OAuth tool name from streaming response: {m}"),
                            )
                        })?);
                    }
                    ResponseBody::Buffered(Bytes::from(out))
                } else {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    let id = rawjson::string(&text, "id").trim().to_owned();
                    session::commit(
                        &continuity.key,
                        continuity.sequence,
                        &id,
                        &request_id,
                        &continuity.prompt_id,
                    );
                    let restored = alias::restore_response(&text, &reverse).map_err(|m| {
                        ExecError::local(
                            502,
                            FailureScope::Request,
                            format!("restore Claude OAuth tool name from response: {m}"),
                        )
                    })?;
                    ResponseBody::Buffered(Bytes::from(restored))
                }
            }
            ResponseBody::Buffered(_) => unreachable!("send returns raw streams"),
        };
        let response = ExecResponse {
            status: response.status,
            headers: response.headers,
            body,
        };
        if req.response_format == Format::Claude {
            return Ok(response);
        }
        // Translated clients: framed events through the registered stream translator.
        let response = match response.body {
            ResponseBody::Buffered(_) => response,
            ResponseBody::Stream(events) => ExecResponse {
                body: ResponseBody::Stream(events),
                ..response
            },
        };
        translate::response(req, translated, response).await
    }

    async fn count_tokens(&self, ctx: Ctx<'_>, req: ExecRequest) -> Result<ExecResponse, ExecError> {
        let translated = translate::request(&req)?;
        if ctx.api_key.trim().is_empty() || !ctx.first_party {
            let body = sanitize_for_upstream(&String::from_utf8_lossy(&translated), &ctx.base_model);
            let response = ExecResponse {
                status: 200,
                headers: Default::default(),
                body: ResponseBody::Buffered(tokens::count(body.as_bytes())?),
            };
            return translate::response(req, translated, response).await;
        }
        let prepared = ctx.prepare_count(&req, &translated)?;
        let response = self.send(&ctx, &prepared, "/v1/messages/count_tokens").await?;
        let body = match response.body {
            ResponseBody::Stream(raw) => collect(raw).await?,
            ResponseBody::Buffered(b) => b,
        };
        let response = ExecResponse {
            status: response.status,
            headers: response.headers,
            body: ResponseBody::Buffered(body),
        };
        translate::response(req, translated, response).await
    }

    async fn send(&self, ctx: &Ctx<'_>, prepared: &Prepared, path: &str) -> Result<RawResponse, ExecError> {
        let url = format!("{}{path}?beta=true", ctx.base_url);
        let client = self.client(&ctx.proxy, ctx.first_party)?;
        let mut order = wreq::header::OrigHeaderMap::new();
        for name in &prepared.order {
            order.insert(name.clone());
        }
        let mut request = client
            .post(&url)
            .redirect(wreq::redirect::Policy::none())
            .orig_headers(order)
            .default_headers(false);
        for (name, value) in &prepared.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let res = request
            .body(prepared.body.clone())
            .send()
            .await
            .map_err(transport_error)?;
        let fast = ctx.first_party && prepared.fast;
        let (status, headers, body) = decoded_response(res).await?;
        if !(200..300).contains(&status) {
            let data = crate::upstream::read_bounded(body, crate::upstream::MAX_ERROR_BODY).await?;
            if fast {
                return Err(fast_direct_error(status, headers, data));
            }
            let mut error = ExecError {
                status,
                scope: crate::upstream::scope_for(status),
                retry_after: None,
                headers: Box::new(headers),
                body: if data.is_empty() {
                    Bytes::from(format!("status {status}"))
                } else {
                    data
                },
                direct: false,
            };
            error = quota::classify(error, ctx.settings.model_level_cooling);
            return Err(error);
        }
        Ok(RawResponse {
            status,
            headers,
            body: ResponseBody::Stream(body),
        })
    }
}

struct RawResponse {
    status: u16,
    headers: http::HeaderMap,
    body: ResponseBody,
}

fn header_value(headers: &http::HeaderMap, name: &str) -> String {
    detect::header(headers, name).to_owned()
}

async fn collect(mut raw: cpa_core::exec::ExecStream) -> Result<Bytes, ExecError> {
    let mut out = BytesMut::new();
    while let Some(chunk) = raw.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out.freeze())
}

/// `newClaudeFastDirectResponseError`: fast-mode refusals reach the client as sent.
fn fast_direct_error(status: u16, headers: http::HeaderMap, body: Bytes) -> ExecError {
    let mut kept = http::HeaderMap::new();
    if let Some(ct) = headers.get(http::header::CONTENT_TYPE) {
        kept.insert(http::header::CONTENT_TYPE, ct.clone());
    }
    ExecError {
        status,
        scope: FailureScope::Request,
        retry_after: None,
        headers: Box::new(kept),
        body,
        direct: true,
    }
}

/// Effective proxy: credential `proxy_url`, then `requests.proxy-url`.
fn proxy_for(credential: &Credential, cfg: &Config) -> Proxy {
    let own = credential
        .attributes
        .get("proxy_url")
        .map(|s| s.trim())
        .unwrap_or_default();
    if !own.is_empty() {
        return Proxy::parse(own);
    }
    let global = cfg
        .document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(serde_yaml_ng::Value::as_str)
        .unwrap_or_default();
    Proxy::parse(global)
}

/// Request facts shared by every stage.
struct Ctx<'a> {
    credential: &'a Credential,
    settings: Settings,
    api_key: String,
    base_url: String,
    first_party: bool,
    proxy: Proxy,
    base_model: String,
    /// Real Claude OAuth token (`sk-ant-oat`).
    oauth_token: bool,
    /// `fp.ProfileClaudeCodeCLI`: OAuth token or `fingerprint-profile: claude-code-cli`.
    cli_profile: bool,
    /// Bearer vs x-api-key on first-party (`claudeCredentialUsesOAuth`).
    bearer: bool,
    /// Local date for the currentDate reminder.
    // ponytail: only UTC and the process zone are resolvable without a tz database;
    // other IANA names fall back to local time, as Go does for unknown zones.
    today: String,
}

struct Prepared {
    body: String,
    headers: Vec<(String, String)>,
    order: Vec<String>,
    reverse: alias::Reverse,
    continuity: session::Continuity,
    fast: bool,
}

/// `thinking.ParseSuffix`: `model(level)` → `model`.
fn base_model(model: &str) -> String {
    match (model.rfind('('), model.ends_with(')')) {
        (Some(open), true) => model[..open].to_owned(),
        _ => model.to_owned(),
    }
}

/// `sanitizeClaudeMessagesForClaudeUpstreamWithDebug`: for Claude targets, tool-use
/// parts lose foreign signature/provenance fields and any modified message is
/// rebuilt the way Go joins kept parts; then empty web_search domain lists go.
fn sanitize_for_upstream(body: &str, base_model: &str) -> String {
    let mut body = body.to_owned();
    if base_model.to_lowercase().contains("claude") {
        body = sanitize_signatures(&body);
    }
    let tools = rawjson::get(&body, "tools").array().len();
    for t in 0..tools {
        if !gjson::get(&body, &format!("tools.{t}.type"))
            .str()
            .starts_with("web_search_")
        {
            continue;
        }
        for field in ["allowed_domains", "blocked_domains"] {
            let path = format!("tools.{t}.{field}");
            let v = gjson::get(&body, &path).json().to_owned();
            if gjson::parse(&v).kind() == gjson::Kind::Array && gjson::parse(&v).array().is_empty() {
                body = rawjson::delete(&body, &path);
            }
        }
    }
    body
}

/// `SanitizeClaudeMessagesForClaudeUpstream` (DropToolSignatures, DropEmptyMessages).
fn sanitize_signatures(body: &str) -> String {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut kept_messages = Vec::new();
    let mut modified = false;
    for message in messages.array() {
        let content = message.get("content");
        if content.kind() != gjson::Kind::Array {
            kept_messages.push(message.json().to_owned());
            continue;
        }
        let mut parts = Vec::new();
        let mut changed = false;
        for part in content.array() {
            match part.get("type").str() {
                "tool_use" => {
                    let (raw, stripped) = strip_tool_use_provenance(part.json());
                    changed |= stripped;
                    parts.push(raw);
                }
                "thinking" => match thinking_signature_decision(part.json()) {
                    Some(raw) => {
                        changed |= raw != part.json();
                        parts.push(raw);
                    }
                    None => changed = true,
                },
                _ => parts.push(part.json().to_owned()),
            }
        }
        if !changed {
            kept_messages.push(message.json().to_owned());
            continue;
        }
        modified = true;
        if parts.is_empty() {
            continue;
        }
        kept_messages.push(rawjson::set_raw(
            message.json(),
            "content",
            &format!("[{}]", parts.join(",")),
        ));
    }
    if !modified {
        return body.to_owned();
    }
    rawjson::set_raw(body, "messages", &format!("[{}]", kept_messages.join(",")))
}

fn strip_tool_use_provenance(raw: &str) -> (String, bool) {
    let mut raw = raw.to_owned();
    let mut changed = false;
    for path in [
        "signature",
        "thoughtSignature",
        "thought_signature",
        "extra_content.google.thought_signature",
        "model",
    ] {
        if gjson::get(&raw, path).exists() {
            raw = rawjson::delete(&raw, path);
            changed = true;
        }
    }
    for path in ["extra_content.google", "extra_content"] {
        let v = gjson::get(&raw, path).json().to_owned();
        let mut members = 0;
        gjson::parse(&v).each(|_, _| {
            members += 1;
            true
        });
        if gjson::parse(&v).kind() == gjson::Kind::Object && members == 0 {
            raw = rawjson::delete(&raw, path);
            changed = true;
        }
    }
    (raw, changed)
}

/// Keep, rewrite (`Some`) or drop (`None`) one thinking block for a Claude target.
// ponytail: adapter for cpa-common::signature (owner: Google thread). Until it lands,
// every thinking block is preserved as sent; foreign or invalid signatures are left
// for Anthropic to reject instead of being dropped or replaced locally.
fn thinking_signature_decision(raw: &str) -> Option<String> {
    Some(raw.to_owned())
}

/// `extractAndRemoveBetas`.
fn extract_betas(body: &str) -> (Vec<String>, String) {
    let v = rawjson::get(body, "betas");
    if !v.exists() {
        return (Vec::new(), body.to_owned());
    }
    let betas = if v.kind() == gjson::Kind::Array {
        v.array()
            .iter()
            .map(|b| b.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .collect()
    } else {
        Some(v.str().trim().to_owned())
            .filter(|b| !b.is_empty())
            .into_iter()
            .collect()
    };
    (betas, rawjson::delete(body, "betas"))
}

impl<'a> Ctx<'a> {
    fn new(exec: &ClaudeExecutor, credential: &'a Credential, req: &ExecRequest, cfg: &Config) -> Self {
        let settings = Settings::for_credential(cfg, credential);
        let attr = |k: &str| credential.attributes.get(k).map(String::as_str).unwrap_or_default();
        let api_key = if attr("api_key").is_empty() {
            credential.str("access_token").unwrap_or_default().to_owned()
        } else {
            attr("api_key").to_owned()
        };
        let base_url = if attr("base_url").is_empty() {
            exec.base_url.clone()
        } else {
            attr("base_url").trim_end_matches('/').to_owned()
        };
        let oauth_token = api_key.contains("sk-ant-oat");
        let profile = {
            let own = attr("fingerprint_profile").trim().to_lowercase();
            let own = if own.is_empty() {
                ["fingerprint_profile", "fingerprint-profile"]
                    .iter()
                    .filter_map(|k| credential.str(k))
                    .map(|s| s.trim().to_lowercase())
                    .find(|s| !s.is_empty())
                    .unwrap_or_default()
            } else {
                own
            };
            if own.is_empty() {
                settings
                    .key_for(&api_key, attr("base_url"))
                    .map(|k| k.fingerprint_profile.trim().to_lowercase())
                    .unwrap_or_default()
            } else {
                own
            }
        };
        let api_key_kind = attr("auth_kind") == "apikey";
        let bearer = oauth_token || (!api_key_kind && attr("api_key").trim().is_empty());
        Self {
            credential,
            first_party: tokens::first_party(&base_url),
            proxy: proxy_for(credential, cfg),
            base_model: base_model(&req.model),
            cli_profile: oauth_token || profile == "claude-code-cli",
            oauth_token,
            bearer,
            settings,
            api_key,
            base_url,
            today: String::new(),
        }
        .with_today(credential)
    }

    /// `claudeCodeTimezone`: credential `timezone`, then header defaults, else local.
    fn with_today(mut self, credential: &Credential) -> Self {
        let own = credential
            .attributes
            .get("timezone")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let zone = own
            .or_else(|| {
                credential
                    .str("timezone")
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| self.settings.header_defaults.timezone.trim().to_owned());
        self.today = if matches!(zone.as_str(), "UTC" | "Etc/UTC") {
            chrono::Utc::now().format("%Y-%m-%d").to_string()
        } else {
            chrono::Local::now().format("%Y-%m-%d").to_string()
        };
        self
    }

    /// `resolveClaudeWirePolicy`: (cloak, strict, sensitive words, cache user id).
    fn wire_policy(&self, confirmed: bool) -> (bool, bool, Vec<String>, bool) {
        let attr = |k: &str| {
            let a = self.credential.attributes.get(k).map(|s| s.trim()).unwrap_or_default();
            if a.is_empty() {
                self.credential.str(k).unwrap_or_default().trim().to_owned()
            } else {
                a.to_owned()
            }
        };
        let attr_mode = attr("cloak_mode");
        let attr_strict = attr("cloak_strict_mode").eq_ignore_ascii_case("true");
        let attr_words: Vec<String> = Some(attr("cloak_sensitive_words"))
            .filter(|s| !s.is_empty())
            .map(|s| s.split(',').map(|w| w.trim().to_owned()).collect())
            .unwrap_or_default();
        let attr_cache = attr("cloak_cache_user_id").eq_ignore_ascii_case("true");
        let mut mode = if self.settings.disable_cloak_mode {
            "never".to_owned()
        } else {
            "auto".to_owned()
        };
        let (mut strict, mut words, mut cache) = (attr_strict, attr_words.clone(), attr_cache);
        if !attr_mode.is_empty() {
            mode = attr_mode.clone();
        }
        let base = self
            .credential
            .attributes
            .get("base_url")
            .map(String::as_str)
            .unwrap_or_default();
        let key_cloak = self.settings.key_for(&self.api_key, base).and_then(|k| k.cloak.clone());
        if let Some(c) = &key_cloak {
            if !c.mode.trim().is_empty() {
                mode = c.mode.trim().to_owned();
            }
            strict |= c.strict_mode;
            if !c.sensitive_words.is_empty() {
                words = c.sensitive_words.clone();
            }
            if let Some(v) = c.cache_user_id {
                cache = v;
            }
        }
        let configured =
            key_cloak.is_some() || !attr_mode.is_empty() || attr_strict || !attr_words.is_empty() || attr_cache;
        let mut cloak = (self.cli_profile || configured) && !confirmed;
        if !confirmed {
            match mode.trim().to_lowercase().as_str() {
                "always" => cloak = true,
                "never" => cloak = false,
                _ => {}
            }
        }
        (cloak, strict, words, cache)
    }

    fn derived_session(&self, req: &ExecRequest) -> String {
        let original = String::from_utf8_lossy(&req.original_body);
        if session::has_explicit_session(&req.headers, &original) {
            return String::new();
        }
        session::derive_id(
            req.source_format,
            &original,
            &session::caller_scope(&req.caller.principal),
        )
    }

    fn prepare_messages(
        &self,
        req: &ExecRequest,
        translated: &[u8],
        upstream_stream: bool,
    ) -> Result<Prepared, ExecError> {
        let original = String::from_utf8_lossy(&req.original_body).into_owned();
        let detection = detect::detect(&req.headers, &original, false, &self.settings);
        let confirmed = detection.confirmed;
        let derived = self.derived_session(req);
        let translated = String::from_utf8_lossy(translated).into_owned();
        let session_id = if self.cli_profile {
            session::agent_session_uuid(
                &session::Inputs {
                    headers: &req.headers,
                    original: &original,
                    translated: &String::from_utf8_lossy(&req.body),
                    derived: &derived,
                },
                confirmed,
            )
        } else {
            String::new()
        };
        let mut body = translated;
        if rawjson::string(&body, "model") != self.base_model || !rawjson::get(&body, "model").exists() {
            body = rawjson::set_str(&body, "model", &self.base_model);
        }
        if self.rebuild_mid_system() {
            body = rebuild_mid_system(&body);
        }
        let (cloak, strict, words, cache_user_id) = self.wire_policy(confirmed);
        let cch = signing::enabled(&self.api_key, self.cli_profile, self.first_party);
        let probe_before = signals::probe_or_helper(&body);
        let mut continuity = session::Continuity::default();
        let mut cloaked = false;
        if cloak {
            if !strict && let Some(message) = cloak::invalid_system_block(&body) {
                return Err(ExecError::local(400, FailureScope::Request, message));
            }
            let mut subagent = false;
            let (mut prev, mut prompt) = (String::new(), String::new());
            if !probe_before {
                subagent = signals::subagent(&req.headers, &body);
                let (existing_prev, existing_prompt) = signals::billing_tags(&body);
                (prev, prompt, continuity) =
                    self.continuity_tags(&req.headers, &body, &session_id, &existing_prev, &existing_prompt);
            }
            let version = profile::default_version(&self.settings);
            let workload = detect::header(&req.headers, "x-cpa-claude-workload").to_owned();
            body = cloak::install_system(
                &body,
                &cloak::SystemPlan {
                    strict,
                    billing: cloak::Billing {
                        signed: cch,
                        version: &version,
                        message: "",
                        entrypoint: "cli",
                        workload: &workload,
                        subagent,
                        prev_req: &prev,
                        prompt_id: &prompt,
                        human_turn: !probe_before && !subagent,
                    },
                    date: &self.today,
                },
            );
            let model = rawjson::string(&body, "model").trim().to_lowercase();
            if cloak::is_opus55(&model) && !probe_before && !rawjson::get(&body, "fallbacks").exists() {
                body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-4-8"}]"#);
            }
            if cloak::is_fable51(&model) && !probe_before && !rawjson::get(&body, "fallbacks").exists() {
                body = rawjson::set_raw(&body, "fallbacks", r#"[{"model":"claude-opus-5"}]"#);
            }
            if !probe_before {
                body = cloak_thinking_display(&body);
            }
            if probe_before || (subagent && !signals::subagent_requests_1h(&req.headers, &body)) {
                body = cloak::strip_ttl(&body);
            }
            if !self.cli_profile {
                let api_key = self.api_key.clone();
                let generate = move || cloak::fake_user_id(&session::cached_session_id(&api_key));
                body = inject_fake_user_id(&body, || {
                    if cache_user_id {
                        session::cached_user_id(&self.api_key, generate)
                    } else {
                        generate()
                    }
                });
            }
            cloaked = true;
        }
        let probe = probe_before || signals::probe_or_helper(&body);
        let eligible = cloaked && self.first_party;
        let caller_owned_cm = rawjson::get(&body, "context_management").exists();
        let mut injected_cm = false;
        let mut diagnostics = session::Continuity::default();
        let mut injected_diagnostics = false;
        if continuity.initialized {
            diagnostics = continuity.clone();
        }
        if eligible {
            if let Some(updated) = cloak::inject_context_management(&body) {
                body = updated;
                injected_cm = true;
            }
            if self.cli_profile && !probe {
                if !continuity.initialized {
                    let begun = session::begin(&self.continuity_identity(), &session_id, false, "");
                    diagnostics = begun;
                    diagnostics.initialized = !diagnostics.key.is_empty();
                }
                if diagnostics.initialized || !diagnostics.key.is_empty() {
                    body = inject_diagnostics(&body, &diagnostics.previous_message_id);
                    injected_diagnostics = true;
                }
            }
        }
        // ponytail: payload rules (cpa-common::payload, server thread) run here in Go and
        // may (de)classify a probe; without them only the post-cloak recheck remains.
        let probe = signals::probe_or_helper(&body);
        if probe {
            diagnostics = session::Continuity::default();
            if injected_diagnostics {
                body = rawjson::delete(&body, "diagnostics");
            }
            if cloaked {
                body = signals::strip_billing_tags(&body);
            }
        }
        body = self.ensure_max_tokens(&body);
        body = disable_thinking_if_tool_choice_forced(&body);
        body = cloak::reconcile_context_management(&body, eligible, caller_owned_cm, injected_cm);
        body = normalize_sampling(&body, confirmed);
        let cpa_owns_cache = !confirmed && (cloaked || cloak::count_cache_controls(&body) == 0);
        if cpa_owns_cache {
            body = cloak::ensure_cache_control(&body);
        }
        body = cloak::enforce_cache_limit(&body, 4);
        let subagent = signals::subagent(&req.headers, &body);
        let subagent_1h = subagent && signals::subagent_requests_1h(&req.headers, &body);
        if cpa_owns_cache && self.cli_profile && (!subagent || subagent_1h) && !probe {
            body = cloak::upgrade_ttl(&body, "1h");
        } else if probe || (subagent && !subagent_1h) {
            body = cloak::strip_ttl(&body);
        }
        body = cloak::normalize_ttl(&body);
        let stream_field = rawjson::get(&body, "stream");
        if !detection.helper_profile || stream_field.exists() || upstream_stream {
            let want = if upstream_stream { "true" } else { "false" };
            if stream_field.json() != want {
                body = rawjson::set_raw(&body, "stream", want);
            }
        }
        let (extra_betas, stripped) = extract_betas(&body);
        body = stripped;
        let mut reverse = alias::Reverse::new();
        if self.cli_profile && cloaked {
            let secret = Some(req.caller.principal.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(alias::DEFAULT_SECRET);
            (body, reverse) = alias::remap(&body, secret);
        }
        body = sanitize_for_upstream(&body, &self.base_model);
        if self.cli_profile {
            body = self.apply_identity(&body, &session_id)?;
        }
        if cloaked
            && !words.is_empty()
            && let Some(m) = cloak::SensitiveWords::new(&words)
        {
            body = m.obfuscate(&body);
        }
        if cch {
            let fallback = if !detection.helper_profile || rawjson::get(&body, "system").exists() {
                self.fallback_billing(&req.headers, &body, &detection.entrypoint, &diagnostics)
            } else {
                String::new()
            };
            body = signing::finalize(&body, &fallback)
                .map_err(|e| ExecError::local(500, FailureScope::Request, format!("finalize Claude CCH: {e}")))?;
        }
        validate_mid_system(&body, confirmed, self.first_party)?;
        let fast = betas::uses_fast_mode(&body, &betas::requested("", &[]));
        let (headers, order) = self.headers(
            req,
            &body,
            &extra_betas,
            upstream_stream,
            false,
            confirmed && !cloaked,
            detection.helper_profile,
            &session_id,
            cloaked,
        );
        let fast = fast
            || headers.iter().any(|(k, v)| {
                k.eq_ignore_ascii_case("anthropic-beta") && v.split(',').any(|b| b.trim() == betas::FAST_MODE)
            });
        Ok(Prepared {
            body,
            headers,
            order,
            reverse,
            continuity: diagnostics,
            fast,
        })
    }

    fn prepare_count(&self, req: &ExecRequest, translated: &[u8]) -> Result<Prepared, ExecError> {
        let original = String::from_utf8_lossy(&req.original_body).into_owned();
        let detection = detect::detect(&req.headers, &original, true, &self.settings);
        let confirmed = detection.confirmed;
        let session_id = if self.cli_profile {
            let derived = self.derived_session(req);
            session::agent_session_uuid(
                &session::Inputs {
                    headers: &req.headers,
                    original: &original,
                    translated: &String::from_utf8_lossy(&req.body),
                    derived: &derived,
                },
                confirmed,
            )
        } else {
            String::new()
        };
        let mut body = String::from_utf8_lossy(translated).into_owned();
        if rawjson::string(&body, "model") != self.base_model || !rawjson::get(&body, "model").exists() {
            body = rawjson::set_str(&body, "model", &self.base_model);
        }
        if self.rebuild_mid_system() {
            body = rebuild_mid_system(&body);
        }
        let (cloak, strict, words, _) = self.wire_policy(confirmed);
        if cloak {
            if !strict && let Some(message) = cloak::invalid_system_block(&body) {
                return Err(ExecError::local(400, FailureScope::Request, message));
            }
            body = cloak::relocate_system_for_count(&body, strict);
            if !words.is_empty()
                && let Some(m) = cloak::SensitiveWords::new(&words)
            {
                body = m.obfuscate(&body);
            }
        }
        body = cloak::enforce_cache_limit(&body, 4);
        body = cloak::normalize_ttl(&body);
        let (mut extra_betas, stripped) = extract_betas(&body);
        body = stripped;
        extra_betas.push(betas::TOKEN_COUNTING.into());
        if self.cli_profile && cloak {
            let secret = Some(req.caller.principal.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or(alias::DEFAULT_SECRET);
            body = alias::remap(&body, secret).0;
        }
        body = sanitize_for_upstream(&body, &self.base_model);
        if self.first_party || self.cli_profile {
            for field in ["metadata", "context_management", "diagnostics"] {
                body = rawjson::delete(&body, field);
            }
        }
        if self.cli_profile {
            body = strip_attribution_system(&body);
        }
        validate_mid_system(&body, confirmed, self.first_party)?;
        let (headers, order) = self.headers(
            req,
            &body,
            &extra_betas,
            false,
            true,
            confirmed && !cloak,
            false,
            &session_id,
            cloak,
        );
        Ok(Prepared {
            body,
            headers,
            order,
            reverse: Default::default(),
            continuity: Default::default(),
            fast: false,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn headers(
        &self,
        req: &ExecRequest,
        body: &str,
        extra_betas: &[String],
        stream: bool,
        count_tokens: bool,
        confirmed: bool,
        helper: bool,
        session_id: &str,
        cloak: bool,
    ) -> (Vec<(String, String)>, Vec<String>) {
        let original = String::from_utf8_lossy(&req.original_body);
        let derived = self.derived_session(req);
        let cpa_session = session::canonical(&req.headers, &original, &derived);
        let h = headers::build(&headers::Plan {
            api_key: &self.api_key,
            bearer: self.bearer,
            first_party: self.first_party,
            count_tokens,
            stream,
            extra_betas,
            body,
            incoming: &req.headers,
            confirmed,
            helper,
            cli_fingerprint: self.cli_profile || cloak,
            use_oauth_betas: self.cli_profile,
            session_id,
            settings: &self.settings,
            credential_id: &self.credential.id,
            attributes: &self.credential.attributes,
            cpa_session: &cpa_session,
        });
        headers::wire(h, self.first_party, count_tokens)
    }

    fn rebuild_mid_system(&self) -> bool {
        self.credential
            .attributes
            .get("rebuild_mid_system_message")
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
            || self
                .settings
                .key_for(
                    &self.api_key,
                    self.credential
                        .attributes
                        .get("base_url")
                        .map(String::as_str)
                        .unwrap_or_default(),
                )
                .is_some_and(|k| k.rebuild_mid_system_message)
    }

    /// `claudeDiagnosticsCredentialIdentity`.
    fn continuity_identity(&self) -> String {
        if !self.credential.id.trim().is_empty() {
            return format!("id:{}", self.credential.id.trim());
        }
        let account = identity::account_uuid(self.credential);
        if !account.is_empty() {
            return format!("account:{account}");
        }
        String::new()
    }

    /// `resolveClaudeContinuityTags`.
    fn continuity_tags(
        &self,
        headers: &http::HeaderMap,
        body: &str,
        session_id: &str,
        existing_prev: &str,
        existing_prompt: &str,
    ) -> (String, String, session::Continuity) {
        let session = if session_id.is_empty() {
            session::agent_session_uuid(
                &session::Inputs {
                    headers,
                    original: body,
                    translated: body,
                    derived: "",
                },
                false,
            )
        } else {
            session_id.to_owned()
        };
        // ponytail: execution-session metadata (websocket executions) is not carried
        // on ExecRequest; HTTP requests never have it in Go either.
        let has_execution_metadata = false;
        let new_turn = signals::new_prompt_turn(body);
        let begun = session::begin(&self.continuity_identity(), &session, new_turn, existing_prompt);
        if begun.key.is_empty() {
            return Default::default();
        }
        let prompt = if !existing_prompt.is_empty() {
            existing_prompt.to_owned()
        } else if !begun.previous_message_id.is_empty()
            && !begun.prompt_id.is_empty()
            && (has_execution_metadata || !new_turn)
        {
            begun.prompt_id.clone()
        } else if !has_execution_metadata {
            session::deterministic_prompt_id(&format!("cpa:prompt:{}", cloak::fingerprint_message(body)))
        } else {
            begun.prompt_id.clone()
        };
        let use_stored = has_execution_metadata || !existing_prev.is_empty();
        let prev = if use_stored && !begun.previous_request_id.is_empty() {
            begun.previous_request_id.clone()
        } else {
            existing_prev.to_owned()
        };
        let continuity = session::Continuity {
            key: begun.key,
            sequence: begun.sequence,
            previous_message_id: if use_stored {
                begun.previous_message_id
            } else {
                String::new()
            },
            previous_request_id: if use_stored {
                begun.previous_request_id
            } else {
                String::new()
            },
            prompt_id: prompt.clone(),
            initialized: true,
        };
        (prev, prompt, continuity)
    }

    /// `claudeCCHFallbackBillingHeader`.
    fn fallback_billing(
        &self,
        headers: &http::HeaderMap,
        body: &str,
        entrypoint: &str,
        continuity: &session::Continuity,
    ) -> String {
        let probe = signals::probe_or_helper(body);
        let (mut prev, mut prompt) = signals::billing_tags(body);
        if !probe {
            if prev.is_empty() {
                prev = continuity.previous_request_id.clone();
            }
            if prompt.is_empty() {
                prompt = continuity.prompt_id.clone();
            }
        }
        let message = cloak::fingerprint_message(body);
        cloak::billing_header(&cloak::Billing {
            signed: true,
            version: &profile::default_version(&self.settings),
            message: &message,
            entrypoint,
            workload: detect::header(headers, "x-cpa-claude-workload"),
            subagent: signals::subagent(headers, body),
            prev_req: &prev,
            prompt_id: &prompt,
            human_turn: false,
        })
    }

    fn apply_identity(&self, body: &str, session_id: &str) -> Result<String, ExecError> {
        let synthesize = self.cli_profile && !self.oauth_token;
        let (device, account) = identity::wire_identity(self.credential, &self.api_key, synthesize);
        identity::apply(body, &device, &account, session_id).map_err(|e| {
            let status = if e.contains("account UUID is empty") || e.contains("session ID is empty") {
                500
            } else {
                400
            };
            ExecError::local(status, FailureScope::Request, e)
        })
    }

    /// `ensureModelMaxTokens` against the pinned Claude catalog.
    fn ensure_max_tokens(&self, body: &str) -> String {
        if !gjson::valid(body) || rawjson::get(body, "max_tokens").exists() {
            return body.to_owned();
        }
        let Some(info) = cpa_core::registry::pinned()
            .channel("claude")
            .iter()
            .find(|m| m.id == self.base_model.trim())
        else {
            return body.to_owned();
        };
        let max = info
            .raw
            .get("max_completion_tokens")
            .and_then(serde_json::Value::as_i64)
            .filter(|n| *n > 0)
            .unwrap_or(1024);
        rawjson::set_raw(body, "max_tokens", &max.to_string())
    }
}

/// Fill `thinking.display: updates` for progress-display models with active thinking.
fn cloak_thinking_display(body: &str) -> String {
    if rawjson::get(body, "thinking.display").exists() || !betas::progress_display(&rawjson::string(body, "model")) {
        return body.to_owned();
    }
    match rawjson::string(body, "thinking.type").trim().to_lowercase().as_str() {
        "adaptive" | "enabled" => rawjson::set_str(body, "thinking.display", "updates"),
        _ => body.to_owned(),
    }
}

/// `injectFakeUserID`: a caller's valid Claude Code user ID is kept.
fn inject_fake_user_id(body: &str, user_id: impl FnOnce() -> String) -> String {
    if rawjson::get(body, "metadata").exists() {
        let existing = rawjson::string(body, "metadata.user_id");
        if !existing.is_empty() && detect::valid_user_id(&existing) {
            return body.to_owned();
        }
    }
    rawjson::set_str(body, "metadata.user_id", &user_id())
}

/// `injectClaudeDiagnosticsWithState`: after context_management when present.
fn inject_diagnostics(body: &str, previous_message_id: &str) -> String {
    let value = if previous_message_id.is_empty() {
        r#"{"previous_message_id":null}"#.to_owned()
    } else {
        format!(
            r#"{{"previous_message_id":{}}}"#,
            rawjson::js_string(previous_message_id)
        )
    };
    if rawjson::get(body, "diagnostics").exists() {
        return rawjson::set_raw(body, "diagnostics", &value);
    }
    let cm = rawjson::get(body, "context_management");
    if let Some(start) = rawjson::offset(body, &cm) {
        let end = start + cm.json().len();
        return format!("{},\"diagnostics\":{value}{}", &body[..end], &body[end..]);
    }
    rawjson::set_raw(body, "diagnostics", &value)
}

/// `disableThinkingIfToolChoiceForced`.
fn disable_thinking_if_tool_choice_forced(body: &str) -> String {
    if !matches!(rawjson::get(body, "tool_choice.type").str(), "any" | "tool") {
        return body.to_owned();
    }
    let mut body = rawjson::delete(body, "thinking");
    body = rawjson::delete(&body, "output_config.effort");
    let oc = rawjson::get(&body, "output_config");
    let mut members = 0;
    oc.each(|_, _| {
        members += 1;
        true
    });
    if oc.kind() == gjson::Kind::Object && members == 0 {
        body = rawjson::delete(&body, "output_config");
    }
    body
}

/// `normalizeClaudeSamplingForUpstream`.
fn normalize_sampling(body: &str, native: bool) -> String {
    let thinking = matches!(
        rawjson::string(body, "thinking.type").trim().to_lowercase().as_str(),
        "enabled" | "adaptive" | "auto"
    );
    let mut body = body.to_owned();
    if !native {
        body = rawjson::delete(&body, "temperature");
        body = rawjson::delete(&body, "top_p");
        if thinking {
            body = rawjson::delete(&body, "top_k");
        }
        return body;
    }
    if thinking {
        let t = rawjson::get(&body, "temperature");
        if t.exists() && t.f64() != 1.0 {
            body = rawjson::delete(&body, "temperature");
        }
        let p = rawjson::get(&body, "top_p");
        if p.exists() && p.f64() < 0.95 {
            body = rawjson::delete(&body, "top_p");
        }
        return rawjson::delete(&body, "top_k");
    }
    if rawjson::get(&body, "temperature").exists() && rawjson::get(&body, "top_p").exists() {
        body = rawjson::delete(&body, "top_p");
    }
    body
}

fn has_mid_system(body: &str) -> bool {
    rawjson::get(body, "messages")
        .array()
        .iter()
        .any(|m| m.get("role").str() == "system")
}

/// `validateClaudeMidSystemMessageModel`.
fn validate_mid_system(body: &str, confirmed: bool, first_party: bool) -> Result<(), ExecError> {
    if confirmed || !first_party || !cloak::legacy_system_reminder(body) || !has_mid_system(body) {
        return Ok(());
    }
    let model = rawjson::string(body, "model");
    let model = if model.is_empty() { "unknown".to_owned() } else { model };
    Err(ExecError::local(
        400,
        FailureScope::Request,
        format!(
            "invalid_request_error: role 'system' is not supported on this model. Model {model:?} predates mid-conversation system turns, so system instructions must stay in the top-level system field for it."
        ),
    ))
}

/// `rebuildMidSystemMessagesToTopLevel`: fold role=system turns into `system`.
fn rebuild_mid_system(body: &str) -> String {
    let messages = rawjson::get(body, "messages");
    if messages.kind() != gjson::Kind::Array {
        return body.to_owned();
    }
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    for m in messages.array() {
        if m.get("role").str().trim().eq_ignore_ascii_case("system") {
            moved.extend(system_text_parts(&m.get("content")));
        } else {
            kept.push(m.json().to_owned());
        }
    }
    if moved.is_empty() {
        return body.to_owned();
    }
    let mut parts = system_text_parts(&rawjson::get(body, "system"));
    parts.extend(moved);
    let body = rawjson::set_raw(body, "system", &format!("[{}]", parts.join(",")));
    rawjson::set_raw(&body, "messages", &format!("[{}]", kept.join(",")))
}

/// `claudeSystemTextParts`: strings become text blocks, text objects stay raw.
fn system_text_parts(content: &gjson::Value<'_>) -> Vec<String> {
    let block = |text: &str| rawjson::set_str(r#"{"type":"text","text":""}"#, "text", text);
    match content.kind() {
        gjson::Kind::String if !content.str().trim().is_empty() => vec![block(content.str())],
        gjson::Kind::Array => content
            .array()
            .iter()
            .filter_map(|item| match item.kind() {
                gjson::Kind::String if !item.str().trim().is_empty() => Some(block(item.str())),
                gjson::Kind::Object
                    if item.get("type").str() == "text" && !item.get("text").str().trim().is_empty() =>
                {
                    Some(item.json().to_owned())
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `util.StripClaudeCodeAttributionSystem`.
fn strip_attribution_system(body: &str) -> String {
    let system = rawjson::get(body, "system");
    let attribution = |t: &str| t.trim_start().starts_with(cloak::BILLING_PREFIX);
    match system.kind() {
        gjson::Kind::String if attribution(system.str()) => rawjson::delete(body, "system"),
        gjson::Kind::Array => {
            let blocks = system.array();
            let kept: Vec<String> = blocks
                .iter()
                .filter(|b| !(b.get("type").str() == "text" && attribution(b.get("text").str())))
                .map(|b| b.json().to_owned())
                .collect();
            if kept.len() == blocks.len() {
                body.to_owned()
            } else if kept.is_empty() {
                rawjson::delete(body, "system")
            } else {
                rawjson::set_raw(body, "system", &format!("[{}]", kept.join(",")))
            }
        }
        _ => body.to_owned(),
    }
}

#[cfg(test)]
mod tests;
