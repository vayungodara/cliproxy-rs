//! `/v1/images/generations` and `/v1/images/edits`
//! (sdk/api/handlers/openai/openai_images_handlers.go).
//!
//! Three model families: xAI image models get an xAI-shaped request and an Images-API
//! response (streams are emulated from the finished result), OpenAI-compatible image
//! models are forwarded with the model set (responses normalized, streams raw), and
//! gpt-image models are routed to whichever provider serves them and returned raw.
// ponytail: Go's Responses image_generation tool path after the three families is
// unreachable (rejectUnsupportedImagesModel admits only those families) and is not
// ported. Pre-result stream keep-alives and the non-streaming keep-alive are not ported
// either; both are off by default. Routed calls run with WithDisallowFreeAuth.

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{MatchedPath, OriginalUri, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use cpa_common::gostr::GoStr;
use cpa_common::json::{self as gj, Kind};
use cpa_core::exec::{Caller, ExecError, Operation};
use cpa_core::format::Format;
use cpa_exec::openai_compat_multipart::{self as multipart, Form, MediaType};

use crate::claude::read_failed;
use crate::dispatch::{self, Call, Done, Failure, Media, MediaKind, SseError};
use crate::respond::{self, Writer};
use crate::{Runtime, errors};

const DEFAULT_TOOL_MODEL: &str = "gpt-image-2";
const CODEX_TOOL_MODELS: [&str; 5] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
];
const XAI_DEFAULT_MODEL: &str = "grok-imagine-image";
const XAI_QUALITY_MODEL: &str = "grok-imagine-image-quality";
const XAI_20_MODEL: &str = "grok-imagine-image-2.0";
const XAI_DEFAULT_ASPECT_RATIO: &str = "1:1";
const XAI_DEFAULT_RESOLUTION: &str = "1k";
const GENERATIONS_PATH: &str = "/v1/images/generations";
const EDITS_PATH: &str = "/v1/images/edits";

// --- models -----------------------------------------------------------------------------

/// `imagesModelParts`: `prefix/base` split at the last slash (a trailing slash keeps the
/// whole model as base).
pub(crate) fn model_parts(model: &str) -> (&str, &str) {
    let model = model.trim();
    match model.rfind('/') {
        Some(i) if i < model.len() - 1 => (model[..i].trim(), model[i + 1..].trim()),
        _ => ("", model),
    }
}

fn model_base(model: &str) -> String {
    model_parts(model).1.trim().go_lower()
}

/// `isXAIImagesModel`.
fn is_xai_model(model: &str) -> bool {
    let (prefix, base) = model_parts(model);
    let base = base.trim().go_lower();
    if ![XAI_DEFAULT_MODEL, XAI_QUALITY_MODEL, XAI_20_MODEL].contains(&base.as_str()) {
        return false;
    }
    matches!(prefix.trim().go_lower().as_str(), "" | "xai" | "x-ai" | "grok")
}

/// `isCodexImagesToolModel`.
fn is_codex_tool_model(model: &str) -> bool {
    CODEX_TOOL_MODELS.contains(&model_base(model).as_str())
}

/// `isOpenAICompatImagesModel`: a registered model of type `openai-image`.
fn is_compat_model(model: &str) -> bool {
    let model = model.trim();
    !model.is_empty() && cpa_core::registry::lookup_model(model, None).is_some_and(|info| info.kind == "openai-image")
}

pub(crate) fn bad_request(message: &str) -> Response {
    respond::error_detail(400, message, "invalid_request_error")
}

/// `rejectUnsupportedImagesModel`.
fn unsupported(model: &str) -> Option<Response> {
    if is_codex_tool_model(model) || is_xai_model(model) || is_compat_model(model) {
        return None;
    }
    let [a, b, c, d, e] = CODEX_TOOL_MODELS;
    Some(bad_request(&format!(
        "Model {model} is not supported on {GENERATIONS_PATH} or {EDITS_PATH}. Use {a}, {b}, {c}, {d}, {e}, {XAI_DEFAULT_MODEL}, {XAI_QUALITY_MODEL}, {XAI_20_MODEL}, or a configured openai-compatibility image model."
    )))
}

// --- request builders ---------------------------------------------------------------------

/// `normalizeImagesResponseFormat`.
fn response_format(raw: &str) -> &'static str {
    if raw.trim().go_eq_fold("url") {
        "url"
    } else {
        "b64_json"
    }
}

/// `canonicalXAIImagesModel`.
fn canonical_xai_model(model: &str) -> &'static str {
    match model_base(model).as_str() {
        XAI_QUALITY_MODEL => XAI_QUALITY_MODEL,
        XAI_20_MODEL => XAI_20_MODEL,
        _ => XAI_DEFAULT_MODEL,
    }
}

/// `xaiImagesAspectRatio`.
fn aspect_ratio(raw: &str, fallback: &str) -> String {
    match raw.trim().go_lower().as_str() {
        "1:1" | "square" => "1:1",
        "16:9" | "landscape" => "16:9",
        "9:16" | "portrait" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "4:3" => "4:3",
        "3:4" => "3:4",
        "3:2" => "3:2",
        "2:3" => "2:3",
        _ => fallback,
    }
    .to_owned()
}

/// `xaiImagesAspectRatioFromSize`.
fn aspect_ratio_from_size(size: &str, fallback: &str) -> String {
    match size.trim().go_lower().as_str() {
        "1024x1024" | "2048x2048" | "1:1" => "1:1",
        "1792x1024" | "16:9" => "16:9",
        "1024x1792" | "9:16" => "9:16",
        "9:20" => "9:20",
        "20:9" => "20:9",
        "1536x1024" | "3:2" => "3:2",
        "1024x1536" | "2:3" => "2:3",
        _ => fallback,
    }
    .to_owned()
}

/// `xaiImagesResolution`.
fn resolution(raw: &str, size: &str, fallback: &str) -> String {
    let lowered = raw.trim().go_lower();
    if lowered == "1k" || lowered == "2k" {
        return lowered;
    }
    if size.trim().go_lower().contains("2048") {
        return "2k".into();
    }
    fallback.to_owned()
}

/// `xaiImagesRef`.
fn xai_ref(url: &str) -> Vec<u8> {
    let mut out = br#"{"type":"image_url","url":""}"#.to_vec();
    gj::set_str(&mut out, "url", url.trim());
    out
}

/// The xAI request options shared by generations and edits.
struct XaiOptions {
    aspect_ratio: String,
    resolution: String,
    quality: String,
    n: i64,
}

/// `buildXAIImagesBaseRequest`.
fn xai_base_request(model: &str, prompt: &str, format: &str, o: &XaiOptions) -> Vec<u8> {
    let mut req = b"{}".to_vec();
    gj::set_str(&mut req, "model", canonical_xai_model(model));
    gj::set_str(&mut req, "prompt", prompt.trim());
    gj::set_str(&mut req, "response_format", response_format(format));
    if !o.aspect_ratio.is_empty() {
        gj::set_str(&mut req, "aspect_ratio", &o.aspect_ratio);
    }
    if !o.resolution.is_empty() {
        gj::set_str(&mut req, "resolution", &o.resolution);
    }
    let quality = o.quality.trim();
    if !quality.is_empty() {
        gj::set_str(&mut req, "quality", quality);
    }
    if o.n > 0 {
        gj::set_int(&mut req, "n", o.n);
    }
    req
}

pub(crate) fn text(body: &[u8], path: &str) -> String {
    gj::get(body, path).str().into_owned()
}

/// gjson `Int()` of a JSON number, else 0.
fn number(body: &[u8], path: &str) -> i64 {
    let v = gj::get(body, path);
    if v.kind == Kind::Number { v.int() } else { 0 }
}

/// `xaiImagesEditOptionsFromJSON`; generations default the ratio and resolution.
fn xai_json_options(body: &[u8], defaults: bool) -> XaiOptions {
    let size = text(body, "size");
    let size = size.trim();
    let mut ratio = aspect_ratio_from_size(size, &aspect_ratio(&text(body, "aspect_ratio"), ""));
    if defaults && ratio.is_empty() {
        ratio = XAI_DEFAULT_ASPECT_RATIO.into();
    }
    let fallback = if defaults { XAI_DEFAULT_RESOLUTION } else { "" };
    XaiOptions {
        aspect_ratio: ratio,
        resolution: resolution(&text(body, "resolution"), size, fallback),
        quality: text(body, "quality").trim().to_owned(),
        n: number(body, "n"),
    }
}

/// `buildXAIImagesEditRequest`: one image as `image`, several as `images`.
fn xai_edit_request(model: &str, prompt: &str, images: &[String], format: &str, o: &XaiOptions) -> Vec<u8> {
    let mut req = xai_base_request(model, prompt, format, o);
    let images: Vec<&str> = images.iter().map(|i| i.trim()).filter(|i| !i.is_empty()).collect();
    if images.len() == 1 {
        gj::set_raw(&mut req, "image", xai_ref(images[0]));
        return req;
    }
    for image in images {
        gj::set_raw(&mut req, "images.-1", xai_ref(image));
    }
    req
}

/// `collectXAIImagesFromJSON`.
fn xai_images_from_json(body: &[u8]) -> Vec<String> {
    let mut images = Vec::new();
    let mut push = |url: String| {
        let url = url.trim();
        if !url.is_empty() {
            images.push(url.to_owned());
        }
    };
    let image = gj::get(body, "image");
    if image.exists() {
        if image.kind == Kind::String {
            push(image.str().into_owned());
        } else if image.kind == Kind::Json {
            push(image.get("image_url.url").str().into_owned());
            let url = image.get("image_url");
            if url.kind == Kind::String {
                push(url.str().into_owned());
            }
            push(image.get("url").str().into_owned());
        }
    }
    let list = gj::get(body, "images");
    if list.is_array() {
        for img in list.array() {
            if img.kind == Kind::String {
                push(img.str().into_owned());
                continue;
            }
            push(img.get("image_url.url").str().into_owned());
            let url = img.get("image_url");
            if url.kind == Kind::String {
                push(url.str().into_owned());
            }
            push(img.get("url").str().into_owned());
        }
    }
    images
}

/// `mimeTypeFromOutputFormat`.
fn mime_from_output_format(format: &str) -> String {
    if format.is_empty() {
        return "image/png".into();
    }
    if format.contains('/') {
        return format.to_owned();
    }
    match format.trim().go_lower().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
    .into()
}

/// `multipartFileToDataURL`.
fn data_url(part: &multipart::Part) -> String {
    let mut media = part.header("Content-Type").unwrap_or_default().trim().to_owned();
    if media.is_empty() {
        media = detect_content_type(&part.body).to_owned();
    }
    use base64::Engine;
    format!(
        "data:{media};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&part.body)
    )
}

/// `buildOpenAICompatImagesJSONRequest`.
fn compat_json_request(body: &[u8], model: &str, stream: bool) -> Vec<u8> {
    let mut out = body.to_vec();
    let model = model.trim();
    if !model.is_empty() {
        gj::set_str(&mut out, "model", model);
    }
    if stream {
        gj::set_bool(&mut out, "stream", true);
    } else {
        gj::delete(&mut out, "stream");
    }
    out
}

/// `parseIntField`.
fn parse_int(raw: &str, fallback: i64) -> i64 {
    let raw = raw.trim();
    if raw.is_empty() {
        return fallback;
    }
    parse_go_int(raw).unwrap_or(fallback)
}

/// `strconv.ParseInt(s, 10, 64)`.
fn parse_go_int(s: &str) -> Option<i64> {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `parseBoolField`.
fn parse_bool(raw: &str, fallback: bool) -> bool {
    match raw.go_lower().trim() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => fallback,
    }
}

// --- responses ----------------------------------------------------------------------------

struct XaiImage {
    b64_json: String,
    url: String,
    revised_prompt: String,
    mime_type: String,
}

/// What `extractXAIImagesResponse` returns.
struct XaiResult {
    images: Vec<XaiImage>,
    created: i64,
    usage: Option<Vec<u8>>,
}

/// `extractXAIImagesResponse`.
fn extract_xai(payload: &[u8]) -> Result<XaiResult, String> {
    if !gj::std_valid(payload) {
        return Err("upstream returned invalid image response JSON".into());
    }
    let mut created = gj::get(payload, "created").int();
    if created <= 0 {
        created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
    }
    let mut results = Vec::new();
    let data = gj::get(payload, "data");
    if data.is_array() {
        for item in data.array() {
            let field = |k: &str| item.get(k).str().trim().to_owned();
            let mut image = XaiImage {
                b64_json: field("b64_json"),
                url: field("url"),
                revised_prompt: field("revised_prompt"),
                mime_type: field("mime_type"),
            };
            if image.mime_type.is_empty() {
                image.mime_type = mime_from_output_format(&field("output_format"));
            }
            if image.b64_json.is_empty() && image.url.is_empty() {
                continue;
            }
            results.push(image);
        }
    }
    if results.is_empty() {
        return Err("upstream did not return image output".into());
    }
    let usage = gj::get(payload, "usage");
    let usage = (usage.exists() && usage.is_object()).then(|| usage.raw().to_vec());
    Ok(XaiResult {
        images: results,
        created,
        usage,
    })
}

/// The image of one result in the client's response format.
fn image_fields(out: &mut Vec<u8>, image: &XaiImage, format: &str) {
    if format == "url" {
        if !image.url.is_empty() {
            gj::set_str(out, "url", &image.url);
        } else {
            let url = format!(
                "data:{};base64,{}",
                mime_from_output_format(&image.mime_type),
                image.b64_json
            );
            gj::set_str(out, "url", url);
        }
    } else if !image.b64_json.is_empty() {
        gj::set_str(out, "b64_json", &image.b64_json);
    } else {
        gj::set_str(out, "url", &image.url);
    }
}

/// `buildImagesAPIResponseFromXAI`.
fn images_api_response(payload: &[u8], format: &str) -> Result<Vec<u8>, String> {
    let XaiResult {
        images: results,
        created,
        usage,
    } = extract_xai(payload)?;
    let mut out = br#"{"created":0,"data":[]}"#.to_vec();
    gj::set_int(&mut out, "created", created);
    let format = response_format(format);
    for image in &results {
        let mut item = b"{}".to_vec();
        image_fields(&mut item, image, format);
        if !image.revised_prompt.is_empty() {
            gj::set_str(&mut item, "revised_prompt", &image.revised_prompt);
        }
        gj::set_raw(&mut out, "data.-1", item);
    }
    if let Some(usage) = usage.filter(|u| gj::std_valid(u)) {
        gj::set_raw(&mut out, "usage", usage);
    }
    Ok(out)
}

/// The emulated stream of a finished xAI result: one `<prefix>.completed` event per image.
fn completed_events(payload: &[u8], format: &str, prefix: &str) -> Result<Vec<u8>, String> {
    let XaiResult {
        images: results, usage, ..
    } = extract_xai(payload)?;
    let event = format!("{prefix}.completed");
    let format = response_format(format);
    let mut out = Vec::new();
    for image in &results {
        let mut data = br#"{"type":""}"#.to_vec();
        gj::set_str(&mut data, "type", &event);
        image_fields(&mut data, image, format);
        if let Some(usage) = usage.as_ref().filter(|u| gj::std_valid(u)) {
            gj::set_raw(&mut data, "usage", usage);
        }
        if !event.trim().is_empty() {
            out.extend_from_slice(format!("event: {event}\n").as_bytes());
        }
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(&data);
        out.extend_from_slice(b"\n\n");
    }
    Ok(out)
}

// --- execution ----------------------------------------------------------------------------

/// What a route parsed, ready for one of the three execution paths.
struct Prepared {
    rt: Arc<Runtime>,
    caller: Caller,
    peer: Option<std::net::SocketAddr>,
    headers: HeaderMap,
    path: String,
}

impl Prepared {
    /// `stream` is the upstream call's mode; `sse` whether the client gets events.
    /// `routed`: a gpt-image model routed to its provider, which Go runs with
    /// `WithDisallowFreeAuth`.
    fn call(&self, model: &str, body: Vec<u8>, stream: bool, sse: bool, routed: bool) -> Call {
        Call {
            entry: Format::OpenAI,
            response: Format::OpenAI,
            operation: Operation::Generate,
            model: model.trim().to_owned(),
            body: Bytes::from(body),
            stream,
            alt: None,
            headers: self.headers.clone(),
            caller: self.caller.clone(),
            forced_provider: None,
            selection_model: None,
            execution_session: None,
            request_path: self.path.clone(),
            peer: self.peer,
            turn: None,
            media: Some(Arc::new(Media {
                kind: MediaKind::Images,
                pinned: None,
                on_selected: None,
                sse,
                disallow_free: routed,
            })),
        }
    }

    /// The request's Content-Type, as Go sets it before forwarding a rebuilt form.
    fn with_content_type(mut self, content_type: &str) -> Self {
        if let Ok(value) = HeaderValue::from_str(content_type) {
            self.headers.insert(header::CONTENT_TYPE, value);
        }
        self
    }
}

fn json_ok(body: impl Into<Body>) -> Response {
    respond::json(200, "application/json", body)
}

/// Finished events, written as a flushed stream (chunked, no Content-Length) like Go.
fn sse_events(events: impl Into<Bytes>) -> Response {
    let chunk = Ok::<_, std::convert::Infallible>(events.into());
    respond::sse(Body::from_stream(futures_util::stream::iter([chunk])))
}

/// `WriteErrorResponse` for a handler-side failure.
pub(crate) fn gateway_error(message: &str) -> Response {
    respond::json(502, "application/json", errors::openai_body(502, message))
}

/// `writeImagesStreamErrorEvent`.
fn stream_error_event(status: u16, text: &str) -> Bytes {
    let status = match status {
        s @ 400..=599 => s,
        _ => 500,
    };
    let text = crate::openai::sanitize_error_text(status, text);
    Bytes::from(format!(
        "event: error\ndata: {}\n\n",
        errors::openai_body(status, &text)
    ))
}

/// An error response that becomes an `error` event once the event stream committed.
fn stream_failed(mut response: Response, status: u16, text: &str) -> Response {
    response
        .extensions_mut()
        .insert(SseError(stream_error_event(status, text)));
    response
}

fn failed(failure: &Failure, stream: bool) -> Response {
    let response = errors::openai(failure);
    if !stream {
        return response;
    }
    stream_failed(response, failure.status(), &failure.text())
}

fn bad_gateway(message: &str, stream: bool) -> Response {
    let response = gateway_error(message);
    if !stream {
        return response;
    }
    stream_failed(response, 502, message)
}

/// Raw upstream bytes; a terminal error becomes an `error` event. A routed stream that
/// ends without data writes a single newline (`streamRoutedImages`); a compat one writes
/// nothing (`streamOpenAICompatImages`).
struct Raw {
    started: bool,
    routed: bool,
}

impl Writer for Raw {
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        self.started = true;
        vec![event]
    }
    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        let failure = Failure::Exec(error.clone());
        vec![stream_error_event(failure.status(), &failure.text())]
    }
    fn end(&mut self) -> Vec<Bytes> {
        if self.started || !self.routed {
            vec![]
        } else {
            vec![Bytes::from_static(b"\n")]
        }
    }
}

/// `collectRoutedImages` / `streamRoutedImages`, and the streaming half of the compat
/// path (`streamOpenAICompatImages`, `routed` false): the upstream body or event stream as
/// is.
async fn raw(p: Prepared, model: &str, body: Vec<u8>, stream: bool, routed: bool) -> Response {
    let keepalive = respond::keepalive(&p.rt.config());
    let call = p.call(model, body, stream, stream, routed);
    dispatch::serve(&p.rt, call, move |result| async move {
        match result {
            Err(failure) => failed(&failure, stream),
            Ok(Done::Buffered { body, .. }) if !stream => json_ok(body),
            Ok(Done::Buffered { body, .. }) => sse_events(body),
            Ok(Done::Stream { first, rest, .. }) => {
                let writer = Raw { started: false, routed };
                respond::sse(respond::stream(first, rest, writer, keepalive))
            }
        }
    })
    .await
}

/// `collectImagesWithModel` and `streamImagesWithModel`: one buffered execution whose
/// result is normalized to the Images API, or emitted as completed events.
async fn normalized(p: Prepared, model: &str, body: Vec<u8>, format: String, prefix: &str, stream: bool) -> Response {
    let call = p.call(model, body, false, stream, false);
    let prefix = prefix.to_owned();
    dispatch::serve(&p.rt, call, move |result| async move {
        let payload = match result {
            Err(failure) => return failed(&failure, stream),
            Ok(Done::Buffered { body, .. }) => body,
            Ok(Done::Stream { .. }) => unreachable!("a non-stream call is buffered"),
        };
        if !stream {
            return match images_api_response(&payload, &format) {
                Ok(out) => json_ok(out),
                Err(message) => gateway_error(&message),
            };
        }
        match completed_events(&payload, &format, &prefix) {
            Ok(out) => sse_events(out),
            Err(message) => bad_gateway(&message, true),
        }
    })
    .await
}

/// Image generation turned off for every route (`disable-image-generation: true`).
fn disabled(rt: &Runtime) -> bool {
    cpa_common::payload::Rules::of(&rt.config()).image_generation == cpa_common::payload::ImageGeneration::All
}

fn not_found() -> Response {
    axum::http::StatusCode::NOT_FOUND.into_response()
}

// --- routes -------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub async fn generations(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if disabled(&rt) {
        return not_found();
    }
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return read_failed(&rejection),
    };
    if !gj::std_valid(&body) {
        return bad_request("Invalid request: body must be valid JSON");
    }
    let mut model = text(&body, "model").trim().to_owned();
    if model.is_empty() {
        model = DEFAULT_TOOL_MODEL.into();
    }
    if let Some(rejected) = unsupported(&model) {
        return rejected;
    }
    let prompt = text(&body, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return bad_request("Invalid request: prompt is required");
    }
    let mut format = text(&body, "response_format").trim().to_owned();
    if format.is_empty() {
        format = "b64_json".into();
    }
    let stream = gj::get(&body, "stream").bool();
    let p = Prepared {
        rt,
        caller,
        peer: dispatch::peer(peer),
        headers,
        path: dispatch::route_path(matched.as_ref(), &uri),
    };
    if is_codex_tool_model(&model) {
        let req = compat_json_request(&body, &model, stream);
        return raw(p, &model, req, stream, true).await;
    }
    if is_xai_model(&model) {
        let req = xai_base_request(&model, &prompt, &format, &xai_json_options(&body, true));
        let routing = text(&req, "model");
        return normalized(p, &routing, req, format, "image_generation", stream).await;
    }
    // isOpenAICompatImagesModel (the only family left after `unsupported`).
    let req = compat_json_request(&body, &model, stream);
    if stream {
        return raw(p, &model, req, true, false).await;
    }
    normalized(p, &model, req, format, "image_generation", false).await
}

#[allow(clippy::too_many_arguments)]
pub async fn edits(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if disabled(&rt) {
        return not_found();
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .unwrap_or_default();
    let lowered = content_type.trim().go_lower();
    let json = lowered.starts_with("application/json");
    if !json && !lowered.starts_with("multipart/form-data") && !lowered.is_empty() {
        return bad_request(&format!(
            "Invalid request: unsupported Content-Type {}",
            cpa_common::gostr::quote(&lowered)
        ));
    }
    let body = match body {
        Ok(body) => body,
        Err(rejection) => return read_failed(&rejection),
    };
    let p = Prepared {
        rt,
        caller,
        peer: dispatch::peer(peer),
        headers,
        path: dispatch::route_path(matched.as_ref(), &uri),
    };
    if json {
        edits_json(p, &body).await
    } else {
        edits_multipart(p, &content_type, &body).await
    }
}

/// `http.Request.ParseMultipartForm` as gin's `c.MultipartForm` calls it.
pub(crate) fn multipart_form(content_type: &str, body: &[u8]) -> Result<Form, String> {
    const NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
    if content_type.is_empty() {
        return Err(NOT_MULTIPART.into());
    }
    let MediaType::Ok(media, params) = multipart::parse_media_type(content_type) else {
        return Err(NOT_MULTIPART.into());
    };
    if media != "multipart/form-data" {
        return Err(NOT_MULTIPART.into());
    }
    let Some(boundary) = params.get("boundary") else {
        return Err("no multipart boundary param in Content-Type".into());
    };
    multipart::read_form(body, boundary)
}

/// `imagesEditsFromMultipart`.
async fn edits_multipart(p: Prepared, content_type: &str, body: &[u8]) -> Response {
    let form = match multipart_form(content_type, body) {
        Ok(form) => form,
        Err(e) => return bad_request(&format!("Invalid request: {e}")),
    };
    let mut model = form.value("model").trim().to_owned();
    if model.is_empty() {
        model = DEFAULT_TOOL_MODEL.into();
    }
    if let Some(rejected) = unsupported(&model) {
        return rejected;
    }
    let prompt = form.value("prompt").trim().to_owned();
    if prompt.is_empty() {
        return bad_request("Invalid request: prompt is required");
    }
    let files = match form.files("image[]") {
        [] => form.files("image"),
        files => files,
    };
    if files.is_empty() {
        return bad_request("Invalid request: image is required");
    }
    let images: Vec<String> = files.iter().map(data_url).collect();
    let mut format = form.value("response_format").trim().to_owned();
    if format.is_empty() {
        format = "b64_json".into();
    }
    let stream = parse_bool(&form.value("stream"), false);
    if is_xai_model(&model) {
        let size = form.value("size");
        let options = XaiOptions {
            aspect_ratio: aspect_ratio_from_size(&size, &aspect_ratio(&form.value("aspect_ratio"), "")),
            resolution: resolution(&form.value("resolution"), &size, ""),
            quality: form.value("quality").trim().to_owned(),
            n: parse_int(&form.value("n"), 0),
        };
        let req = xai_edit_request(&model, &prompt, &images, &format, &options);
        let routing = text(&req, "model");
        return normalized(p, &routing, req, format, "image_edit", stream).await;
    }
    // Codex tool models and OpenAI-compatible image models forward the rebuilt form.
    let (req, content_type) = multipart::rewrite_images_form(&form, &model, stream, true);
    let p = p.with_content_type(&content_type);
    let routed = is_codex_tool_model(&model);
    if routed || stream {
        return raw(p, &model, req, stream, routed).await;
    }
    normalized(p, &model, req, format, "image_edit", false).await
}

/// `imagesEditsFromJSON`.
async fn edits_json(p: Prepared, body: &[u8]) -> Response {
    if !gj::std_valid(body) {
        return bad_request("Invalid request: body must be valid JSON");
    }
    let mut model = text(body, "model").trim().to_owned();
    if model.is_empty() {
        model = DEFAULT_TOOL_MODEL.into();
    }
    if let Some(rejected) = unsupported(&model) {
        return rejected;
    }
    let prompt = text(body, "prompt").trim().to_owned();
    if prompt.is_empty() {
        return bad_request("Invalid request: prompt is required");
    }
    let mut format = text(body, "response_format").trim().to_owned();
    if format.is_empty() {
        format = "b64_json".into();
    }
    let stream = gj::get(body, "stream").bool();
    if is_codex_tool_model(&model) {
        let req = compat_json_request(body, &model, stream);
        return raw(p, &model, req, stream, true).await;
    }
    if is_xai_model(&model) {
        let images = xai_images_from_json(body);
        if images.is_empty() {
            return bad_request("Invalid request: image is required");
        }
        let req = xai_edit_request(&model, &prompt, &images, &format, &xai_json_options(body, false));
        let routing = text(&req, "model");
        return normalized(p, &routing, req, format, "image_edit", stream).await;
    }
    let req = compat_json_request(body, &model, stream);
    if stream {
        return raw(p, &model, req, true, false).await;
    }
    normalized(p, &model, req, format, "image_edit", false).await
}

// --- net/http DetectContentType (sniff.go) ------------------------------------------------

/// Go `http.DetectContentType`.
pub(crate) fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(512)];
    let ws = |b: u8| matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ');
    let first = data.iter().take_while(|b| ws(**b)).count();
    const HTML: [&[u8]; 17] = [
        b"<!DOCTYPE HTML",
        b"<HTML",
        b"<HEAD",
        b"<SCRIPT",
        b"<IFRAME",
        b"<H1",
        b"<DIV",
        b"<FONT",
        b"<TABLE",
        b"<A",
        b"<STYLE",
        b"<TITLE",
        b"<B",
        b"<BODY",
        b"<BR",
        b"<P",
        b"<!--",
    ];
    for sig in HTML {
        let d = &data[first..];
        if d.len() > sig.len()
            && sig.iter().zip(d).all(|(&b, &db)| {
                let db = if b.is_ascii_uppercase() { db & 0xDF } else { db };
                b == db
            })
            && matches!(d[sig.len()], b' ' | b'>')
        {
            return "text/html; charset=utf-8";
        }
    }
    let masked = |data: &[u8], mask: &[u8], pat: &[u8]| {
        data.len() >= pat.len() && pat.iter().zip(mask).zip(data).all(|((&p, &m), &d)| d & m == p)
    };
    if masked(&data[first..], b"\xFF\xFF\xFF\xFF\xFF", b"<?xml") {
        return "text/xml; charset=utf-8";
    }
    let exact: [(&[u8], &'static str); 2] = [
        (b"%PDF-", "application/pdf"),
        (b"%!PS-Adobe-", "application/postscript"),
    ];
    for (sig, ct) in exact {
        if data.starts_with(sig) {
            return ct;
        }
    }
    type Sig = (&'static [u8], &'static [u8], &'static str);
    let masked_sigs: [Sig; 3] = [
        (b"\xFF\xFF\x00\x00", b"\xFE\xFF\x00\x00", "text/plain; charset=utf-16be"),
        (b"\xFF\xFF\x00\x00", b"\xFF\xFE\x00\x00", "text/plain; charset=utf-16le"),
        (b"\xFF\xFF\xFF\x00", b"\xEF\xBB\xBF\x00", "text/plain; charset=utf-8"),
    ];
    for (mask, pat, ct) in masked_sigs {
        if masked(data, mask, pat) {
            return ct;
        }
    }
    let images: [(&[u8], &'static str); 5] = [
        (b"\x00\x00\x01\x00", "image/x-icon"),
        (b"\x00\x00\x02\x00", "image/x-icon"),
        (b"BM", "image/bmp"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
    ];
    for (sig, ct) in images {
        if data.starts_with(sig) {
            return ct;
        }
    }
    if masked(
        data,
        b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF\xFF\xFF",
        b"RIFF\x00\x00\x00\x00WEBPVP",
    ) {
        return "image/webp";
    }
    if data.starts_with(b"\x89PNG\x0D\x0A\x1A\x0A") {
        return "image/png";
    }
    if data.starts_with(b"\xFF\xD8\xFF") {
        return "image/jpeg";
    }
    let media: [Sig; 6] = [
        (
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"FORM\x00\x00\x00\x00AIFF",
            "audio/aiff",
        ),
        (b"\xFF\xFF\xFF", b"ID3", "audio/mpeg"),
        (b"\xFF\xFF\xFF\xFF\xFF", b"OggS\x00", "application/ogg"),
        (
            b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF",
            b"MThd\x00\x00\x00\x06",
            "audio/midi",
        ),
        (
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"RIFF\x00\x00\x00\x00AVI ",
            "video/avi",
        ),
        (
            b"\xFF\xFF\xFF\xFF\x00\x00\x00\x00\xFF\xFF\xFF\xFF",
            b"RIFF\x00\x00\x00\x00WAVE",
            "audio/wave",
        ),
    ];
    for (mask, pat, ct) in media {
        if masked(data, mask, pat) {
            return ct;
        }
    }
    if data.len() >= 12 {
        let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
        if data.len() >= size && size.is_multiple_of(4) && &data[4..8] == b"ftyp" {
            let mut st = 8;
            while st < size {
                if st != 12 && &data[st..st + 3] == b"mp4" {
                    return "video/mp4";
                }
                st += 4;
            }
        }
    }
    if data.starts_with(b"\x1A\x45\xDF\xA3") {
        return "video/webm";
    }
    let mut eot_mask = [0u8; 36];
    eot_mask[34] = 0xFF;
    eot_mask[35] = 0xFF;
    let mut eot_pat = [0u8; 36];
    eot_pat[34] = b'L';
    eot_pat[35] = b'P';
    if masked(data, &eot_mask, &eot_pat) {
        return "application/vnd.ms-fontobject";
    }
    let rest: [(&[u8], &'static str); 10] = [
        (b"\x00\x01\x00\x00", "font/ttf"),
        (b"OTTO", "font/otf"),
        (b"ttcf", "font/collection"),
        (b"wOFF", "font/woff"),
        (b"wOF2", "font/woff2"),
        (b"\x1F\x8B\x08", "application/x-gzip"),
        (b"PK\x03\x04", "application/zip"),
        (b"Rar!\x1A\x07\x00", "application/x-rar-compressed"),
        (b"Rar!\x1A\x07\x01\x00", "application/x-rar-compressed"),
        (b"\x00\x61\x73\x6D", "application/wasm"),
    ];
    for (sig, ct) in rest {
        if data.starts_with(sig) {
            return ct;
        }
    }
    let binary = data[first..]
        .iter()
        .any(|&b| b <= 0x08 || b == 0x0B || (0x0E..=0x1A).contains(&b) || (0x1C..=0x1F).contains(&b));
    if binary {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

#[cfg(test)]
#[path = "images_tests.rs"]
mod tests;
