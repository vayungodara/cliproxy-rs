//! `/v1/videos*` (xAI-native passthrough) and `/openai/v1/videos*` (OpenAI Videos API over
//! xAI) (sdk/api/handlers/openai/openai_videos_handlers.go).
//!
//! A created video is bound to the credential that created it (and its routing model) for
//! `multimedia.video-result-auth-cache-ttl`, so polling and downloads reach the same xAI
//! account.
// ponytail: the non-streaming keep-alive (`requests.nonstream-keepalive-interval`) and
// upstream header passthrough are not applied, like the other Rust routes.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{MatchedPath, OriginalUri, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Kind};
use cpa_core::exec::{Caller, Operation};
use cpa_core::format::Format;
use futures_util::StreamExt;

use crate::claude::read_failed;
use crate::dispatch::{self, Call, Done, Failure, Media, MediaKind};
use crate::images::{bad_request, gateway_error, model_parts, multipart_form, text};
use crate::{Runtime, errors, respond};

const OPENAI_VIDEOS_PATH: &str = "/openai/v1/videos";
const XAI_GENERATIONS_API: &str = "/v1/videos/generations";
const XAI_EDITS_API: &str = "/v1/videos/edits";
const XAI_EXTENSIONS_API: &str = "/v1/videos/extensions";
const SORA_MODEL: &str = "sora-2";
const XAI_MODEL: &str = "grok-imagine-video";
const XAI_15_MODEL: &str = "grok-imagine-video-1.5";
const XAI_15_PREVIEW: &str = "grok-imagine-video-1.5-preview";
const DEFAULT_SECONDS: &str = "4";
const DEFAULT_SIZE: &str = "720x1280";
const DEFAULT_RESOLUTION: &str = "720p";
const MAX_REFERENCES: usize = 7;
const DEFAULT_BINDING_TTL: Duration = Duration::from_secs(3 * 3600);

// --- credential bindings ------------------------------------------------------------------

struct Binding {
    auth: String,
    model: String,
    expires: Instant,
}

/// Go's process-wide `videoAuthBindings`.
static BINDINGS: LazyLock<Mutex<HashMap<String, Binding>>> = LazyLock::new(Mutex::default);

fn bindings() -> std::sync::MutexGuard<'static, HashMap<String, Binding>> {
    BINDINGS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `setWithModel`: expired entries are swept on every write.
fn bind(video: &str, auth: &str, model: &str, ttl: Duration) {
    let (video, auth) = (video.trim(), auth.trim());
    if video.is_empty() || auth.is_empty() {
        return;
    }
    let now = Instant::now();
    let mut map = bindings();
    map.retain(|_, b| now <= b.expires);
    map.insert(
        video.to_owned(),
        Binding {
            auth: auth.to_owned(),
            model: model.trim().to_owned(),
            expires: now + ttl,
        },
    );
}

/// `getBinding`: the credential and model, unless expired.
fn binding(video: &str) -> Option<(String, String)> {
    let video = video.trim();
    if video.is_empty() {
        return None;
    }
    let now = Instant::now();
    let mut map = bindings();
    match map.get(video) {
        Some(b) if now > b.expires => {
            map.remove(video);
            None
        }
        Some(b) => Some((b.auth.clone(), b.model.clone())),
        None => None,
    }
}

/// `videoAuthBindingTTL`: `multimedia.video-result-auth-cache-ttl` when it parses as a
/// positive Go duration, else three hours.
fn binding_ttl(rt: &Runtime) -> Duration {
    rt.config()
        .document
        .get("multimedia")
        .and_then(|m| m.get("video-result-auth-cache-ttl"))
        .and_then(serde_yaml_ng::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(cpa_core::config::parse_duration)
        .filter(|n| *n > 0)
        .map_or(DEFAULT_BINDING_TTL, |n| Duration::from_nanos(n as u64))
}

// --- models -------------------------------------------------------------------------------

fn base(model: &str) -> String {
    model_parts(model).1.trim().go_lower()
}

/// `isXAIVideosModel`.
fn is_xai(model: &str) -> bool {
    let (prefix, b) = model_parts(model);
    let b = b.trim().go_lower();
    if b != XAI_MODEL && b != XAI_15_MODEL && b != XAI_15_PREVIEW {
        return false;
    }
    matches!(prefix.trim().go_lower().as_str(), "" | "xai" | "x-ai" | "grok")
}

/// `isSoraVideosModel`.
fn is_sora(model: &str) -> bool {
    let b = base(model);
    b == SORA_MODEL || b.starts_with("sora-2-")
}

/// `canonicalXAIVideosModel` (also `responseVideosModel`).
fn canonical(model: &str) -> &'static str {
    if is_sora(model) {
        return XAI_MODEL;
    }
    match base(model).as_str() {
        XAI_15_MODEL | XAI_15_PREVIEW => XAI_15_MODEL,
        _ => XAI_MODEL,
    }
}

/// `routingXAIVideosModel`: the preview alias keeps its own routing name.
fn routing(model: &str) -> &'static str {
    if is_sora(model) {
        return XAI_MODEL;
    }
    match base(model).as_str() {
        XAI_15_MODEL => XAI_15_MODEL,
        XAI_15_PREVIEW => XAI_15_PREVIEW,
        _ => XAI_MODEL,
    }
}

// --- request building ---------------------------------------------------------------------

/// The OpenAI-shaped create request, as `xaiVideoCreateMetadata` remembers it.
struct CreateMeta {
    model: &'static str,
    routing: &'static str,
    prompt: String,
    seconds: String,
    size: String,
    created_at: i64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// `strconv.ParseInt(s, 10, 64)`.
fn parse_int(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `normalizeXAIVideosSeconds`: an integer clamped to 1..=15.
fn seconds(raw: &str) -> Result<i64, String> {
    let mut s = raw.trim();
    if s.is_empty() {
        s = DEFAULT_SECONDS;
    }
    let duration = parse_int(s).ok_or("seconds must be an integer")?;
    Ok(duration.clamp(1, 15))
}

/// `xaiVideosSizeOptions`.
fn size_options(raw: &str) -> Result<(String, &'static str), String> {
    let mut size = raw.trim();
    if size.is_empty() {
        size = DEFAULT_SIZE;
    }
    match size {
        "720x1280" | "1024x1792" => Ok((size.to_owned(), "9:16")),
        "1280x720" | "1792x1024" => Ok((size.to_owned(), "16:9")),
        _ => Err("size must be one of 720x1280, 1280x720, 1024x1792, or 1792x1024".into()),
    }
}

/// `xaiVideosAspectRatio`.
fn aspect_ratio(raw: &str) -> &'static str {
    match raw.trim().go_lower().as_str() {
        "1:1" | "square" => "1:1",
        "16:9" | "landscape" => "16:9",
        "9:16" | "portrait" => "9:16",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => "",
    }
}

/// `xaiVideosResolution`.
fn resolution(raw: &str) -> &'static str {
    match raw.trim().go_lower().as_str() {
        "480p" => "480p",
        "720p" => "720p",
        _ => "",
    }
}

/// `xaiVideosInputImageURL`.
fn input_image(body: &[u8]) -> Result<String, String> {
    let reference = gj::get(body, "input_reference");
    if reference.exists() {
        let url = reference.get("image_url").str().trim().to_owned();
        let file = reference.get("file_id").str().trim().to_owned();
        if !url.is_empty() && !file.is_empty() {
            return Err("input_reference must provide exactly one of image_url or file_id".into());
        }
        if !file.is_empty() {
            return Err(
                "input_reference.file_id is not supported for xAI video generation; use input_reference.image_url"
                    .into(),
            );
        }
        if !url.is_empty() {
            return Ok(url);
        }
    }
    let image = gj::get(body, "image");
    if image.exists() {
        if image.kind == Kind::String {
            return Ok(image.str().trim().to_owned());
        }
        for path in ["url", "image_url.url"] {
            let value = image.get(path).str().trim().to_owned();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }
    Ok(text(body, "image_url").trim().to_owned())
}

/// `collectXAIVideoReferenceImages`.
fn reference_images(body: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |value: &str| {
        let value = value.trim();
        if !value.is_empty() {
            out.push(value.to_owned());
        }
    };
    for path in ["reference_images", "reference_image_urls"] {
        let list = gj::get(body, path);
        if !list.is_array() {
            continue;
        }
        for item in list.array() {
            if item.kind == Kind::String {
                push(&item.str());
                continue;
            }
            let url = item.get("url").str().into_owned();
            if !url.is_empty() {
                push(&url);
                continue;
            }
            let url = item.get("image_url.url").str().into_owned();
            if !url.is_empty() {
                push(&url);
            }
        }
    }
    out
}

/// `buildXAIVideosCreateRequest`.
fn create_request(body: &[u8], model: &str) -> Result<(Vec<u8>, CreateMeta), String> {
    let prompt = text(body, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return Err("prompt is required".into());
    }
    let duration = seconds(&text(body, "seconds"))?;
    let (size, mut ratio) = size_options(&text(body, "size"))?;
    let mut res = DEFAULT_RESOLUTION;
    let value = aspect_ratio(&text(body, "aspect_ratio"));
    if !value.is_empty() {
        ratio = value;
    }
    let value = resolution(&text(body, "resolution"));
    if !value.is_empty() {
        res = value;
    }
    let image = input_image(body)?;
    let references = reference_images(body);
    if references.len() > MAX_REFERENCES {
        return Err(format!(
            "reference_images supports at most {MAX_REFERENCES} images on xAI"
        ));
    }
    if !image.is_empty() && !references.is_empty() {
        return Err("image and reference_images cannot be combined on xAI".into());
    }
    let mut req = b"{}".to_vec();
    gj::set_str(&mut req, "model", canonical(model));
    gj::set_str(&mut req, "prompt", &prompt);
    gj::set_raw(&mut req, "duration", duration.to_string());
    gj::set_str(&mut req, "aspect_ratio", ratio);
    gj::set_str(&mut req, "resolution", res);
    if !image.is_empty() {
        gj::set_str(&mut req, "image.url", &image);
    }
    for reference in &references {
        gj::set_str(&mut req, "reference_images.-1.url", reference);
    }
    let meta = CreateMeta {
        model: canonical(model),
        routing: routing(model),
        prompt,
        seconds: duration.to_string(),
        size,
        created_at: now_unix(),
    };
    Ok((req, meta))
}

/// gin `c.ContentType()`: the media type before any space or `;`, trimmed and lowered by
/// the handler.
fn content_type(headers: &HeaderMap) -> String {
    let raw = headers
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let end = raw.find([' ', ';']).unwrap_or(raw.len());
    raw[..end].trim().go_lower()
}

/// `videosCreateRequestFromForm` over gin's `c.PostForm` (multipart values, or the
/// urlencoded body; a form that fails to parse has no values).
fn create_request_from_form(headers: &HeaderMap, body: &[u8]) -> Vec<u8> {
    let raw_type = headers
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let form = multipart_form(&raw_type, body).ok();
    let encoded = String::from_utf8_lossy(body).into_owned();
    let urlencoded = content_type(headers) == "application/x-www-form-urlencoded";
    let post_form = |key: &str| -> String {
        match &form {
            Some(form) => form.value(key),
            None if urlencoded => crate::access::query_get(&encoded, key)
                .map(|v| String::from_utf8_lossy(&v).into_owned())
                .unwrap_or_default(),
            None => String::new(),
        }
    };
    let first = |keys: &[&str]| {
        keys.iter()
            .map(|k| post_form(k))
            .find(|v| !v.trim().is_empty())
            .unwrap_or_default()
    };
    let mut out = b"{}".to_vec();
    for field in ["model", "prompt", "seconds", "size", "aspect_ratio", "resolution"] {
        let value = post_form(field).trim().to_owned();
        if !value.is_empty() {
            gj::set_str(&mut out, field, value);
        }
    }
    let value = first(&["input_reference[image_url]", "input_reference.image_url", "image_url"]);
    if !value.trim().is_empty() {
        gj::set_str(&mut out, "input_reference.image_url", value.trim());
    }
    let value = first(&["input_reference[file_id]", "input_reference.file_id", "file_id"]);
    if !value.trim().is_empty() {
        gj::set_str(&mut out, "input_reference.file_id", value.trim());
    }
    let refs = post_form("reference_image_urls");
    for reference in refs.trim().split(',') {
        let reference = reference.trim();
        if !reference.is_empty() {
            gj::set_str(&mut out, "reference_image_urls.-1", reference);
        }
    }
    out
}

// --- responses ----------------------------------------------------------------------------

/// `openAIVideoStatus`.
fn status(raw: &str) -> &'static str {
    match raw.trim().go_lower().as_str() {
        "queued" | "pending" => "queued",
        "in_progress" | "processing" | "running" => "in_progress",
        "completed" | "done" | "succeeded" | "success" => "completed",
        "failed" | "error" | "expired" | "cancelled" | "canceled" => "failed",
        _ => "",
    }
}

/// `videoIDFromPayload`.
fn video_id(payload: &[u8]) -> String {
    let id = text(payload, "request_id").trim().to_owned();
    if id.is_empty() {
        text(payload, "id").trim().to_owned()
    } else {
        id
    }
}

/// `buildVideosCreateAPIResponseFromXAI`.
fn create_response(payload: &[u8], meta: &CreateMeta) -> Result<Vec<u8>, String> {
    let id = video_id(payload);
    if id.is_empty() {
        return Err("xAI video response did not include request_id".into());
    }
    let mut out = br#"{"object":"video","progress":0,"status":"queued"}"#.to_vec();
    gj::set_str(&mut out, "id", &id);
    gj::set_str(&mut out, "model", meta.model);
    gj::set_str(&mut out, "prompt", &meta.prompt);
    gj::set_str(&mut out, "seconds", &meta.seconds);
    gj::set_str(&mut out, "size", &meta.size);
    gj::set_int(&mut out, "created_at", meta.created_at);
    let mapped = status(&text(payload, "status"));
    if !mapped.is_empty() {
        gj::set_str(&mut out, "status", mapped);
    }
    let progress = gj::get(payload, "progress");
    if progress.exists() {
        gj::set_raw(&mut out, "progress", progress.raw());
    }
    Ok(out)
}

/// `buildVideosFailedAPIResponse`.
fn failed_response(model: &str, code: &str, message: &str) -> Vec<u8> {
    let model = if model.trim().is_empty() {
        XAI_MODEL
    } else {
        model.trim()
    };
    let mut out = br#"{"object":"video","status":"failed","progress":0}"#.to_vec();
    gj::set_str(&mut out, "id", format!("video_{}", uuid::Uuid::new_v4().simple()));
    gj::set_str(&mut out, "model", model);
    gj::set_str(&mut out, "error.code", code.trim());
    gj::set_str(&mut out, "error.message", message.trim());
    out
}

/// `writeVideosFailedError` (`c.Data` with `application/json`).
fn failed(status: u16, model: &str, message: &str) -> Response {
    respond::json(
        status,
        "application/json",
        failed_response(model, "invalid_request_error", message),
    )
}

/// `markOpenAIVideoFailed`.
fn mark_failed(out: &mut Vec<u8>) {
    if !gj::get(out, "status").exists() {
        gj::set_str(out, "status", "failed");
    }
    if !gj::get(out, "progress").exists() {
        gj::set_raw(out, "progress", "0");
    }
}

/// `setOpenAIVideoErrorFromXAI`.
fn set_error(out: &mut Vec<u8>, payload: &[u8]) {
    let error = gj::get(payload, "error");
    let code = text(payload, "code").trim().to_owned();
    if error.exists() {
        mark_failed(out);
        let (message, nested_code) = if error.kind == Kind::Json {
            if !gj::std_valid(error.raw()) {
                return;
            }
            (
                error.get("message").str().trim().to_owned(),
                error.get("code").str().trim().to_owned(),
            )
        } else {
            (error.str().trim().to_owned(), String::new())
        };
        if message.is_empty() {
            return;
        }
        let code = [code, nested_code]
            .into_iter()
            .find(|c| !c.is_empty())
            .unwrap_or_else(|| "video_generation_failed".into());
        gj::set_str(out, "error.code", code);
        gj::set_str(out, "error.message", message);
        return;
    }
    if !code.is_empty() {
        mark_failed(out);
        gj::set_str(out, "error.code", &code);
        gj::set_str(out, "error.message", &code);
    }
}

/// `buildVideosRetrieveAPIResponseFromXAI`.
fn retrieve_response(id: &str, payload: &[u8], fallback_model: &str) -> Vec<u8> {
    let mut out = br#"{"object":"video"}"#.to_vec();
    gj::set_str(&mut out, "id", id);
    let mut model = text(payload, "model").trim().to_owned();
    if model.is_empty() {
        model = canonical(fallback_model).to_owned();
    }
    gj::set_str(&mut out, "model", model);
    for field in [
        "created_at",
        "completed_at",
        "expires_at",
        "prompt",
        "remixed_from_video_id",
        "size",
    ] {
        let value = gj::get(payload, field);
        if value.exists() {
            gj::set_raw(&mut out, field, value.raw());
        }
    }
    let mapped = status(&text(payload, "status"));
    if !mapped.is_empty() {
        gj::set_str(&mut out, "status", mapped);
    }
    let progress = gj::get(payload, "progress");
    if progress.exists() {
        gj::set_raw(&mut out, "progress", progress.raw());
    }
    let seconds = gj::get(payload, "seconds");
    let duration = gj::get(payload, "video.duration");
    if seconds.exists() {
        gj::set_raw(&mut out, "seconds", seconds.raw());
    } else if duration.exists() {
        gj::set_str(&mut out, "seconds", &*duration.str());
    }
    let url = text(payload, "video.url").trim().to_owned();
    if !url.is_empty() {
        gj::set_str(&mut out, "video_url", url);
    }
    set_error(&mut out, payload);
    out
}

/// `xaiVideoContentURLFromPayload`: an absolute http(s) URL with a host.
fn content_url(payload: &[u8]) -> Result<String, String> {
    let raw = text(payload, "video.url").trim().to_owned();
    if raw.is_empty() {
        return Err("xAI video response did not include video.url".into());
    }
    if !cpa_exec::xai::is_http_url(&raw) {
        return Err("xAI video response included invalid video.url".into());
    }
    Ok(raw)
}

// --- execution ----------------------------------------------------------------------------

struct Request {
    rt: Arc<Runtime>,
    caller: Caller,
    peer: Option<std::net::SocketAddr>,
    headers: HeaderMap,
    path: String,
}

/// One buffered execution of `body` on `model` (`ExecuteWithAuthManager` with the
/// `openai-video` handler type), pinned to `pinned` when set. `render` gets the payload or
/// the failure, and the last credential selected (`WithSelectedAuthIDCallback`).
async fn execute<F, Fut>(r: &Request, model: &str, body: Vec<u8>, pinned: Option<String>, render: F) -> Response
where
    F: FnOnce(Result<Bytes, Failure>, String) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Response> + Send,
{
    let selected: Arc<Mutex<String>> = Arc::default();
    let sink = selected.clone();
    let call = Call {
        entry: Format::OpenAI,
        response: Format::OpenAI,
        operation: Operation::Generate,
        model: model.trim().to_owned(),
        body: Bytes::from(body),
        stream: false,
        alt: None,
        headers: r.headers.clone(),
        caller: r.caller.clone(),
        forced_provider: None,
        selection_model: None,
        execution_session: None,
        request_path: r.path.clone(),
        peer: r.peer,
        turn: None,
        media: Some(Arc::new(Media {
            kind: MediaKind::Videos,
            pinned,
            on_selected: Some(Box::new(move |c| {
                *sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = c.id.clone();
            })),
            sse: false,
            disallow_free: false,
        })),
    };
    dispatch::serve(&r.rt, call, move |result| async move {
        let selected = selected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let payload = result.map(|done| match done {
            Done::Buffered { body, .. } => body,
            Done::Stream { .. } => unreachable!("a non-stream call is buffered"),
        });
        render(payload, selected).await
    })
    .await
}

fn json_body(body: impl Into<Body>) -> Response {
    respond::json(200, "application/json", body)
}

/// Go `contextWithVideoAuthBinding` + `modelWithVideoAuthBinding`.
fn pinned_and_model(video: &str, fallback: &str) -> (Option<String>, String) {
    match binding(video) {
        Some((auth, model)) => {
            let model = if model.trim().is_empty() {
                fallback.to_owned()
            } else {
                model
            };
            (Some(auth), model)
        }
        None => (None, fallback.to_owned()),
    }
}

/// `collectXAIVideosNative`: the upstream body as is, binding the created (or polled)
/// video to the serving credential.
async fn native(r: Request, body: Vec<u8>, model: &str, bind_created: bool) -> Response {
    let video = video_id(&body);
    let (pinned, model) = if bind_created {
        (None, model.to_owned())
    } else {
        pinned_and_model(&video, model)
    };
    let ttl = binding_ttl(&r.rt);
    let routed = routing(&model);
    execute(&r, &model, body, pinned, move |result, selected| async move {
        let payload = match result {
            Ok(payload) => payload,
            Err(failure) => return errors::openai(&failure),
        };
        let bound = if bind_created { video_id(&payload) } else { video };
        bind(&bound, &selected, routed, ttl);
        json_body(payload)
    })
    .await
}

/// `VideosRetrieve` and `VideosContent`: polls `id` on its bound credential and model,
/// rebinding it to the credential that answered.
async fn poll<F, Fut>(r: &Request, id: &str, render: F) -> Response
where
    F: FnOnce(Bytes) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Response> + Send,
{
    let mut payload = b"{}".to_vec();
    gj::set_str(&mut payload, "request_id", id);
    let (pinned, model) = pinned_and_model(id, XAI_MODEL);
    let ttl = binding_ttl(&r.rt);
    let routed = routing(&model);
    let id = id.to_owned();
    execute(r, &model, payload, pinned, move |result, selected| async move {
        match result {
            Ok(payload) => {
                bind(&id, &selected, routed, ttl);
                render(payload).await
            }
            Err(failure) => errors::openai(&failure),
        }
    })
    .await
}

/// The HTTP client the video download uses: the bound credential's proxy, else the
/// global one (`videoContentHTTPClient`).
static DOWNLOADS: LazyLock<cpa_exec::proxy::GoClients> =
    LazyLock::new(|| cpa_exec::proxy::GoClients::new(Default::default()));

/// `writeVideoContentFromURL`.
async fn download(rt: &Runtime, video: &str, url: &str) -> Response {
    let cfg = rt.config();
    let credential = binding(video).and_then(|(auth, _)| rt.store().get(&auth));
    let proxy = match &credential {
        Some(c) => cpa_exec::proxy::Proxy::effective(c, &cfg),
        None => cpa_exec::proxy::Proxy::parse(
            cfg.document
                .get("requests")
                .and_then(|r| r.get("proxy-url"))
                .and_then(serde_yaml_ng::Value::as_str)
                .unwrap_or_default(),
        ),
    };
    let client = DOWNLOADS.get(&proxy);
    let headers = cpa_exec::proxy::GoHeaders::new();
    // `HTTPStatusFromErrorOr(err, http.StatusBadGateway)`: a failed download is a 502
    // unless the error carries its own status.
    let upstream = match cpa_exec::proxy::request(&client, wreq::Method::GET, url, headers, None, None).await {
        Ok(upstream) => upstream,
        Err(error) if error.scope == cpa_core::exec::FailureScope::Transport => {
            return gateway_error(&String::from_utf8_lossy(&error.body));
        }
        Err(error) => return errors::openai(&Failure::Exec(error)),
    };
    if !(200..300).contains(&upstream.status) {
        let status = upstream.status;
        let body = cpa_exec::proxy::read_all(upstream.body, usize::MAX, true)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body).trim().to_owned();
        let message = if text.is_empty() {
            let reason = StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("");
            format!("video content download failed: {status} {reason}")
        } else {
            format!("video content download failed: {text}")
        };
        let body = errors::openai_body(status, &message);
        return respond::json(status, "application/json", body);
    }
    let mut response = Response::new(Body::from_stream(
        upstream.body.map(|r| r.map_err(std::io::Error::other)),
    ));
    *response.status_mut() = StatusCode::from_u16(upstream.status).unwrap_or(StatusCode::OK);
    let out = response.headers_mut();
    for name in [
        "Content-Type",
        "Content-Length",
        "Content-Disposition",
        "Cache-Control",
        "ETag",
        "Last-Modified",
    ] {
        if let Some(value) = upstream.headers.get(name).filter(|v| !v.is_empty()) {
            out.insert(
                header::HeaderName::from_bytes(name.as_bytes()).expect("static name"),
                value.clone(),
            );
        }
    }
    if !out.contains_key(header::CONTENT_TYPE) {
        out.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    response
}

// --- routes -------------------------------------------------------------------------------

fn request(
    rt: Arc<Runtime>,
    caller: Caller,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    uri: &axum::http::Uri,
    headers: HeaderMap,
) -> Request {
    Request {
        rt,
        caller,
        peer: dispatch::peer(peer),
        headers,
        path: dispatch::route_path(matched.as_ref(), uri),
    }
}

/// `POST /openai/v1/videos` (`VideosCreate`).
#[allow(clippy::too_many_arguments)]
pub async fn create(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let raw = match content_type(&headers).as_str() {
        "multipart/form-data" | "application/x-www-form-urlencoded" => match &body {
            Ok(body) => Ok(create_request_from_form(&headers, body)),
            Err(rejection) => Err(rejection.body_text()),
        },
        _ => match &body {
            Ok(body) if gj::std_valid(body) => Ok(body.to_vec()),
            Ok(_) => Err("body must be valid JSON".to_owned()),
            Err(rejection) => Err(rejection.body_text()),
        },
    };
    let raw = match raw {
        Ok(raw) => raw,
        Err(e) => return failed(400, XAI_MODEL, &format!("Invalid request: {e}")),
    };
    let mut model = text(&raw, "model").trim().to_owned();
    if model.is_empty() {
        model = XAI_MODEL.into();
    }
    if !is_xai(&model) && !is_sora(&model) {
        let path = if uri.path().trim().is_empty() {
            OPENAI_VIDEOS_PATH
        } else {
            uri.path().trim()
        };
        return failed(
            400,
            &model,
            &format!("Model {model} is not supported on {path}. Use {SORA_MODEL}."),
        );
    }
    let (req, meta) = match create_request(&raw, &model) {
        Ok(built) => built,
        Err(e) => return failed(400, canonical(&model), &format!("Invalid request: {e}")),
    };
    let r = request(rt, caller, peer, matched, &uri, headers);
    let ttl = binding_ttl(&r.rt);
    let routed = meta.routing;
    execute(&r, routed, req, None, move |result, selected| async move {
        let payload = match result {
            Ok(payload) => payload,
            Err(failure) => return errors::openai(&failure),
        };
        let out = match create_response(&payload, &meta) {
            Ok(out) => out,
            Err(message) => return gateway_error(&message),
        };
        bind(&video_id(&out), &selected, routed, ttl);
        json_body(out)
    })
    .await
}

/// `POST /v1/videos`, `/v1/videos/generations`, `/edits`, `/extensions`
/// (`handleXAIVideosNativePost`).
#[allow(clippy::too_many_arguments)]
pub async fn native_post(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return read_failed(&rejection),
    };
    if !gj::std_valid(&body) {
        return bad_request("Invalid request: body must be valid JSON");
    }
    let mut model = text(&body, "model").trim().to_owned();
    if model.is_empty() {
        model = XAI_MODEL.into();
    }
    if !is_xai(&model) {
        return bad_request(&format!(
            "Model {model} is not supported on {XAI_GENERATIONS_API}, {XAI_EDITS_API}, or {XAI_EXTENSIONS_API}. Use {XAI_MODEL}."
        ));
    }
    let mut raw = body.to_vec();
    gj::set_str(&mut raw, "model", canonical(&model));
    let r = request(rt, caller, peer, matched, &uri, headers);
    native(r, raw, routing(&model), true).await
}

/// `GET /v1/videos/{request_id}` (`XAIVideosRetrieve`).
pub async fn native_retrieve(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = id.trim().to_owned();
    if id.is_empty() {
        return bad_request("Invalid request: request_id is required");
    }
    let mut payload = b"{}".to_vec();
    gj::set_str(&mut payload, "request_id", &id);
    let r = request(rt, caller, peer, matched, &uri, headers);
    native(r, payload, XAI_MODEL, false).await
}

/// `GET /openai/v1/videos/{video_id}` (`VideosRetrieve`).
pub async fn retrieve(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = id.trim().to_owned();
    if id.is_empty() {
        return bad_request("Invalid request: video_id is required");
    }
    let r = request(rt, caller, peer, matched, &uri, headers);
    let video = id.clone();
    poll(&r, &id, |payload| async move {
        json_body(retrieve_response(&video, &payload, SORA_MODEL))
    })
    .await
}

/// `GET /openai/v1/videos/{video_id}/content` (`VideosContent`): polls the job, then
/// streams the finished file from its `video.url`.
#[allow(clippy::too_many_arguments)]
pub async fn content(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let id = id.trim().to_owned();
    if id.is_empty() {
        return bad_request("Invalid request: video_id is required");
    }
    let query = uri.query().unwrap_or_default();
    let mut variant = crate::access::query_get(query, "variant")
        .map(|v| String::from_utf8_lossy(&v).trim().to_owned())
        .unwrap_or_default();
    if variant.is_empty() {
        variant = "video".into();
    }
    if variant != "video" {
        return bad_request(&format!(
            "Invalid request: variant {} is not available for xAI video downloads",
            cpa_common::gostr::quote(&variant)
        ));
    }
    let r = request(rt, caller, peer, matched, &uri, headers);
    let rt = r.rt.clone();
    let video = id.clone();
    poll(&r, &id, |payload| async move {
        match content_url(&payload) {
            Ok(url) => download(&rt, &video, &url).await,
            Err(message) => gateway_error(&message),
        }
    })
    .await
}

#[cfg(test)]
#[path = "videos_tests.rs"]
mod tests;
