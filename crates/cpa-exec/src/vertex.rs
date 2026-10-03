//! Vertex AI (internal/runtime/executor/gemini_vertex_executor.go).
//!
//! Two credential kinds share the Gemini request pipeline:
//! - Imported service-account files (`type: vertex`, `-vertex-import`): every request
//!   first exchanges a signed JWT for an access token (`vertex_auth`), then calls
//!   `https://<location>-aiplatform.googleapis.com` (or the global host) under
//!   `/v1/projects/<project>/locations/<location>/publishers/google/models/<model>`.
//! - API keys (`api-keys.vertex`, or a file credential's `access_token`): `x-goog-api-key`
//!   at `<base-url>/v1/publishers/google/models/<model>`, base URL used as configured.
//!
//! Unlike the Gemini API-key executor, Vertex translates without the is-compat variants,
//! does not cap `maxOutputTokens`, strips Responses tool-call IDs, sends Imagen models to
//! `:predict`, and hands every scanned stream line to the translator unfiltered.

use std::collections::BTreeMap;

use bytes::Bytes;
use cpa_common::json::{self as gj, GoValue};
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use serde_json::{Map, Value};

use crate::gemini::{self as g, Emit, LineState, Output};
use crate::gemini_payload as payload;
use crate::proxy::{self, GoClients, GoHeaders, Hooks, Proxy};
use crate::vertex_auth;

pub const PROVIDER: &str = "vertex";
const API_VERSION: &str = "v1";
const DEFAULT_API_KEY_BASE: &str = "https://aiplatform.googleapis.com";
const DEFAULT_LOCATION: &str = "us-central1";

pub struct VertexExecutor {
    clients: GoClients,
    /// Unix seconds for the JWT `iat` (tests pin it to Go's recorded clock).
    now: fn() -> i64,
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Default for VertexExecutor {
    fn default() -> Self {
        Self::with_hooks(Hooks::default(), system_now)
    }
}

/// How one request authenticates.
enum Auth {
    /// `x-goog-api-key` at `base` (empty: the global endpoint).
    ApiKey { key: String, base: String },
    ServiceAccount {
        project: String,
        location: String,
        account: Map<String, Value>,
    },
}

/// A Go error without a status code: the handler answers it with 500.
fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Credential, message)
}

/// `vertexAPICreds`: the `api_key` and `base_url` attributes as stored, else the file's
/// `access_token`.
fn api_creds(credential: &Credential) -> (String, String) {
    let attr = |k: &str| credential.attributes.get(k).cloned().unwrap_or_default();
    let mut key = attr("api_key");
    if key.is_empty() {
        key = credential.str("access_token").unwrap_or_default().to_owned();
    }
    (key, attr("base_url"))
}

/// `vertexCreds`: project (`project_id`, then `project`), location (default
/// us-central1) and the normalized service account.
fn service_account(credential: &Credential) -> Result<Auth, ExecError> {
    let text = |k: &str| credential.str(k).map(str::trim).unwrap_or_default().to_owned();
    let mut project = text("project_id");
    if project.is_empty() {
        project = text("project");
    }
    if project.is_empty() {
        return Err(plain_error("vertex executor: missing project_id in credentials"));
    }
    let mut location = text("location");
    if location.is_empty() {
        location = DEFAULT_LOCATION.into();
    }
    let Some(Value::Object(account)) = credential.metadata.get("service_account") else {
        return Err(plain_error("vertex executor: missing service_account in credentials"));
    };
    let account =
        vertex_auth::normalize_service_account(account).map_err(|e| plain_error(format!("vertex executor: {e}")))?;
    Ok(Auth::ServiceAccount {
        project,
        location,
        account,
    })
}

fn auth_for(credential: &Credential) -> Result<Auth, ExecError> {
    let (key, base) = api_creds(credential);
    if key.is_empty() {
        return service_account(credential);
    }
    Ok(Auth::ApiKey { key, base })
}

/// `vertexBaseURL`.
fn regional_base(location: &str) -> String {
    match location.trim() {
        "" => format!("https://{DEFAULT_LOCATION}-aiplatform.googleapis.com"),
        "global" => DEFAULT_API_KEY_BASE.into(),
        loc => format!("https://{loc}-aiplatform.googleapis.com"),
    }
}

fn endpoint(auth: &Auth, model: &str, action: &str) -> String {
    match auth {
        Auth::ApiKey { base, .. } => {
            let base = if base.is_empty() { DEFAULT_API_KEY_BASE } else { base };
            format!("{base}/{API_VERSION}/publishers/google/models/{model}:{action}")
        }
        Auth::ServiceAccount { project, location, .. } => format!(
            "{}/{API_VERSION}/projects/{project}/locations/{location}/publishers/google/models/{model}:{action}",
            regional_base(location)
        ),
    }
}

/// `http.NewRequestWithContext`'s URL parse. Go builds the request before the token
/// exchange, so a URL it rejects fails without contacting the token endpoint.
fn check_url(url: &str) -> Result<(), ExecError> {
    crate::xai_url::parse(url).map_err(plain_error)?;
    // The shared client's own URL check (`proxy::send`), before the token exchange too.
    url::Url::parse(url).map_err(|_| ExecError::local(500, FailureScope::Request, "invalid upstream URL"))?;
    Ok(())
}

/// gjson's `String()` as Go's `json.Marshal` writes it back: each invalid UTF-8 byte
/// becomes U+FFFD.
fn go_string(value: &gj::Res<'_>) -> String {
    let bytes = value.bytes();
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        out.extend(std::iter::repeat_n('\u{FFFD}', chunk.invalid().len()));
    }
    out
}

/// `isImagenModel`.
fn is_imagen(model: &str) -> bool {
    model.to_lowercase().contains("imagen")
}

/// `convertToImagenRequest`: the prompt from `contents.0.parts.0.text`, the first
/// non-empty `messages[].content`, or `prompt`, plus the optional Imagen parameters.
fn imagen_request(body: &[u8]) -> Result<Vec<u8>, ExecError> {
    let mut prompt = String::new();
    let text = gj::get(body, "contents.0.parts.0.text");
    if text.exists() {
        prompt = go_string(&text);
    }
    if prompt.is_empty() {
        let contents = gj::get(body, "messages.#.content");
        if contents.exists()
            && contents.is_array()
            && let Some(first) = contents.array().iter().map(go_string).find(|m| !m.is_empty())
        {
            prompt = first;
        }
    }
    if prompt.is_empty() {
        let direct = gj::get(body, "prompt");
        if direct.exists() {
            prompt = go_string(&direct);
        }
    }
    if prompt.is_empty() {
        return Err(plain_error("imagen: no prompt found in request"));
    }
    let mut instance = BTreeMap::new();
    instance.insert("prompt".to_owned(), GoValue::String(prompt));
    let mut parameters = BTreeMap::new();
    parameters.insert("sampleCount".to_owned(), GoValue::Number("1".into()));
    let aspect = gj::get(body, "aspectRatio");
    if aspect.exists() {
        parameters.insert("aspectRatio".to_owned(), GoValue::String(go_string(&aspect)));
    }
    let count = gj::get(body, "sampleCount");
    if count.exists() {
        parameters.insert("sampleCount".to_owned(), GoValue::Number(count.int().to_string()));
    }
    let negative = gj::get(body, "negativePrompt");
    if negative.exists() {
        instance.insert("negativePrompt".to_owned(), GoValue::String(go_string(&negative)));
    }
    let mut request = BTreeMap::new();
    request.insert("instances".to_owned(), GoValue::Array(vec![GoValue::Object(instance)]));
    request.insert("parameters".to_owned(), GoValue::Object(parameters));
    Ok(GoValue::Object(request).marshal())
}

/// `convertImagenToGeminiResponse`: predictions with image bytes become `inlineData`
/// parts of one Gemini candidate. `responseId` is `imagen-<UnixNano>` like Go's.
fn imagen_response(data: &[u8], model: &str) -> Vec<u8> {
    let predictions = gj::get(data, "predictions");
    if !predictions.exists() || !predictions.is_array() {
        return data.to_vec();
    }
    let string = |s: &str| GoValue::String(s.to_owned());
    let mut parts = Vec::new();
    for prediction in predictions.array() {
        let image = go_string(&prediction.get("bytesBase64Encoded"));
        let mut mime = go_string(&prediction.get("mimeType"));
        if mime.is_empty() {
            mime = "image/png".into();
        }
        if image.is_empty() {
            continue;
        }
        let inline = BTreeMap::from([
            ("mimeType".to_owned(), string(&mime)),
            ("data".to_owned(), string(&image)),
        ]);
        parts.push(GoValue::Object(BTreeMap::from([(
            "inlineData".to_owned(),
            GoValue::Object(inline),
        )])));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let zero = || GoValue::Number("0".into());
    let content = BTreeMap::from([
        ("parts".to_owned(), GoValue::Array(parts)),
        ("role".to_owned(), string("model")),
    ]);
    let candidate = BTreeMap::from([
        ("content".to_owned(), GoValue::Object(content)),
        ("finishReason".to_owned(), string("STOP")),
    ]);
    let usage = BTreeMap::from([
        ("promptTokenCount".to_owned(), zero()),
        ("candidatesTokenCount".to_owned(), zero()),
        ("totalTokenCount".to_owned(), zero()),
    ]);
    GoValue::Object(BTreeMap::from([
        (
            "candidates".to_owned(),
            GoValue::Array(vec![GoValue::Object(candidate)]),
        ),
        ("responseId".to_owned(), GoValue::String(format!("imagen-{nanos}"))),
        ("modelVersion".to_owned(), string(model)),
        ("usageMetadata".to_owned(), GoValue::Object(usage)),
    ]))
    .marshal()
}

/// `helps.StripVertexOpenAIResponsesToolCallIDs`: Vertex rejects the Responses call IDs
/// the translator keeps on `functionCall` and `functionResponse` parts.
fn strip_tool_call_ids(body: Vec<u8>, source: Format) -> Vec<u8> {
    if source != Format::OpenAIResponse {
        return body;
    }
    let contents = gj::get(&body, "contents");
    if !contents.is_array() {
        return body;
    }
    let items = contents.array();
    let has_ids = items.iter().any(|c| {
        let parts = c.get("parts");
        parts.is_array()
            && parts
                .array()
                .iter()
                .any(|p| p.get("functionCall.id").exists() || p.get("functionResponse.id").exists())
    });
    if !has_ids {
        return body;
    }
    let mut changed = false;
    let mut out_items: Vec<Vec<u8>> = Vec::with_capacity(items.len());
    for content in &items {
        let parts = content.get("parts");
        if !parts.is_array() {
            out_items.push(content.raw().to_vec());
            continue;
        }
        let mut parts_changed = false;
        let mut part_items = Vec::new();
        for part in parts.array() {
            let mut raw = part.raw().to_vec();
            for path in ["functionCall.id", "functionResponse.id"] {
                if part.get(path).exists()
                    && let Ok(updated) = gj::try_delete(&raw, path)
                {
                    raw = updated;
                    parts_changed = true;
                }
            }
            part_items.push(raw);
        }
        let mut raw = content.raw().to_vec();
        if parts_changed && let Ok(updated) = gj::try_set_raw(&raw, "parts", gj::join(&part_items)) {
            raw = updated;
            changed = true;
        }
        out_items.push(raw);
    }
    if !changed {
        return body;
    }
    gj::try_set_raw(&body, "contents", gj::join(&out_items)).unwrap_or(body)
}

/// Vertex hands every scanned line, prefixes and blank lines included, to the
/// translator; then the tool-input finalization and `[DONE]`.
struct RawLines(Output);

impl LineState for RawLines {
    fn line(&mut self, line: &[u8]) -> Emit {
        // Go's reporter sees each raw line (model and usage, unfiltered).
        self.0.usage.response_line(Format::Gemini, line);
        self.0.translate(&g::line_event(line))
    }

    fn end(&mut self) -> Emit {
        self.0.end_with_done()
    }

    fn flush(&mut self) -> Vec<Bytes> {
        self.0.flush_frames()
    }
}

impl VertexExecutor {
    /// Tests pass trust and resolve hooks and a fixed clock.
    pub fn with_hooks(hooks: Hooks, now: fn() -> i64) -> Self {
        Self {
            clients: GoClients::new(hooks),
            now,
        }
    }

    fn client(&self, credential: &Credential, cfg: &Config) -> wreq::Client {
        self.clients.get(&Proxy::effective(credential, cfg))
    }

    pub async fn execute(
        &self,
        credential: &Credential,
        req: ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        if req.operation == Operation::CountTokens {
            return self.count_tokens(credential, &req, cfg).await;
        }
        if req.alt.as_deref() == Some(g::COMPACT_ALT) {
            return Err(g::status_err(501, "/responses/compact not supported"));
        }
        let auth = auth_for(credential)?;
        self.generate(credential, &req, cfg, &auth).await
    }

    /// Content-Type, the credential (API key, or a fresh access token) and the custom
    /// headers.
    async fn headers(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
        auth: &Auth,
    ) -> Result<GoHeaders, ExecError> {
        let mut headers = GoHeaders::new();
        headers.set("Content-Type", "application/json");
        match auth {
            Auth::ApiKey { key, .. } => headers.set("x-goog-api-key", key.clone()),
            Auth::ServiceAccount { account, .. } => {
                // Token exchange uses the credential's proxy, like the model request.
                let token = vertex_auth::access_token(&self.client(credential, cfg), account, (self.now)())
                    .await
                    .map_err(|error| {
                        tracing::error!(%error, "vertex executor: access token error");
                        g::status_err(500, "internal server error")
                    })?;
                if !token.is_empty() {
                    headers.set("Authorization", format!("Bearer {token}"));
                }
            }
        }
        g::set_custom_headers(&mut headers, credential, req);
        Ok(headers)
    }

    /// The translated body through Go's Vertex request edits, before the boundary turns.
    fn prepared_body(&self, req: &ExecRequest, cfg: &Config, base_model: &str) -> Result<Vec<u8>, ExecError> {
        let (from, to) = (req.source_format, Format::Gemini);
        let resolved = g::resolved(req);
        let original = g::translate(req, cfg, to, base_model, g::original_request(req), req.stream, false)?;
        let body = g::translate(req, cfg, to, base_model, &req.body, req.stream, false)?;
        let mut body = g::apply_thinking(req, body, from, to, PROVIDER, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(base_model, body);
        let rules = cpa_common::payload::Rules::from_config(cfg);
        body = g::apply_payload_rules(&rules, base_model, to.as_str(), body, &original, req);
        body = payload::set_str_if_different(body, "model", base_model);
        body = strip_tool_call_ids(body, from);
        Ok(cpa_common::signature::sanitize_gemini_request_thought_signatures(
            &body, "contents",
        ))
    }

    async fn generate(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
        auth: &Auth,
    ) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let imagen = is_imagen(&base_model);
        // Only the service-account Execute path speaks Imagen's own request format.
        let imagen_wire = imagen && !req.stream && matches!(auth, Auth::ServiceAccount { .. });
        let mut body = if imagen_wire {
            imagen_request(&req.body)?
        } else {
            self.prepared_body(req, cfg, &base_model)?
        };
        body = payload::ensure_leading_user_content(body, "contents");
        body = payload::ensure_trailing_user_content(body, "contents");
        let action = match (imagen, req.stream) {
            (true, _) => "predict",
            (false, true) => "streamGenerateContent",
            (false, false) => "generateContent",
        };
        let mut url = endpoint(auth, &base_model, action);
        match (req.stream, g::alt(req)) {
            (true, _) if imagen => {}
            (true, None) => url.push_str("?alt=sse"),
            (_, Some(alt)) => url.push_str(&format!("?$alt={alt}")),
            (false, None) => {}
        }
        body = payload::delete(body, "session_id");
        req.usage.request(Format::Gemini, &body);
        check_url(&url)?;
        let headers = self.headers(credential, req, cfg, auth).await?;
        let upstream = proxy::send(&self.client(credential, cfg), &url, headers, body.clone(), None).await?;
        if !(200..300).contains(&upstream.status) {
            return Err(g::error_from(upstream).await);
        }
        let response_headers = upstream.headers.clone();
        if !req.stream {
            let mut data = proxy::read_all(upstream.body, usize::MAX, false).await?.to_vec();
            // ponytail: Go observes the response model on the raw body but parses usage
            // after the Imagen conversion, whose usageMetadata is all zeros; Imagen
            // predict responses carry no usageMetadata, so the raw body gives the same.
            req.usage.response_body(Format::Gemini, &data);
            if imagen_wire {
                data = imagen_response(&data, &base_model);
            }
            let out = g::translate_non_stream(req, Format::Gemini, &body, &data)?;
            return Ok(ExecResponse {
                status: 200,
                headers: response_headers,
                body: ResponseBody::Buffered(out),
            });
        }
        let output = Output::new(req, Format::Gemini, &body);
        let stream = g::drive(proxy::lines(upstream.body, g::MAX_LINE), RawLines(output));
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Stream(stream),
        })
    }

    /// `countTokensWith*`: the translated request without tools, generation and safety
    /// settings, a leading user turn only.
    async fn count_tokens(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let auth = auth_for(credential)?;
        let base_model = parse_suffix(&req.model).model_name;
        let (from, to) = (req.source_format, Format::Gemini);
        let resolved = g::resolved(req);
        let body = g::translate(req, cfg, to, &base_model, &req.body, false, false)?;
        let mut body = g::apply_thinking(req, body, from, to, PROVIDER, resolved.as_ref())?;
        body = payload::fix_image_aspect_ratio(&base_model, body);
        body = gj::try_set_str(&body, "model", &base_model).unwrap_or(body);
        body = strip_tool_call_ids(body, from);
        for path in ["tools", "generationConfig", "safetySettings"] {
            body = payload::delete(body, path);
        }
        body = cpa_common::signature::sanitize_gemini_request_thought_signatures(&body, "contents");
        body = payload::ensure_leading_user_content(body, "contents");
        let url = endpoint(&auth, &base_model, "countTokens");
        check_url(&url)?;
        let headers = self.headers(credential, req, cfg, &auth).await?;
        let upstream = proxy::send(&self.client(credential, cfg), &url, headers, body, None).await?;
        if !(200..300).contains(&upstream.status) {
            return Err(g::error_from(upstream).await);
        }
        let response_headers = upstream.headers.clone();
        let data = proxy::read_all(upstream.body, usize::MAX, false).await?;
        let count = gj::get(&data, "totalTokens").int();
        let out = cpa_translate::translate_token_count(req.response_format, to, count, &data);
        Ok(ExecResponse {
            status: 200,
            headers: response_headers,
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// `type` values the import flag writes and the executor serves.
pub fn handles(provider: &str) -> bool {
    provider == PROVIDER
}

#[cfg(test)]
#[path = "vertex_tests.rs"]
mod tests;
