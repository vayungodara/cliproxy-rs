//! Codex Images API (codex_openai_images.go): `/v1/images/generations` and
//! `/v1/images/edits` on Codex credentials.
//!
//! A gpt-image-* model goes straight to the backend's own `/images/*` endpoint
//! ("direct"). Any other model becomes a Responses request that forces the
//! `image_generation` tool on the images main model (`multimedia.gpt-image-2-base-model`
//! when it names a `gpt-` model, else gpt-5.4-mini). The tool calls of
//! `response.completed` become an Images API response, or `<prefix>.partial_image` and
//! `<prefix>.completed` SSE frames when streaming.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, ResponseBody};
use cpa_core::format::Format;
use futures_util::StreamExt;
use gjson::Kind;

use crate::codex::{CodexExecutor, events, explicit_session, report_request};
use crate::codex_capture::Wire;
use crate::codex_json::{delete, set_bool_if_different, set_raw, set_str, set_str_if_different};
use crate::codex_request::{self as request, Settings, View};
use crate::codex_response::{self as response, OutputItems};
use crate::openai_compat::{plain_err, prepare_images_payload, status_err};
use crate::openai_compat_multipart::{self as multipart, Form, MediaType, Part};

const GENERATIONS: &str = "/v1/images/generations";
const EDITS: &str = "/v1/images/edits";
/// `codexOpenAIImagesMainModel`.
const MAIN_MODEL: &str = "gpt-5.4-mini";
/// `codexDefaultImageToolModel`.
const DEFAULT_TOOL_MODEL: &str = "gpt-image-2";
/// Models the backend serves on its own Images endpoints (`codexIsDirectOpenAIImageModel`).
const DIRECT_MODELS: [&str; 5] = [
    "gpt-image-1.5",
    "gpt-image-2",
    "gpt-image-2.5-flare",
    "gpt-image-2.5-sunburst",
    "gpt-image-2.5",
];
/// The Images API's source format name in payload rules and thinking.
const SOURCE: &str = "openai-image";

/// `codexOpenAIImagePreparedRequest`.
struct Prepared {
    body: String,
    url_format: bool,
    /// `image_generation` or `image_edit`: the SSE event name prefix.
    prefix: &'static str,
}

/// One completed `image_generation_call` (`codexImageCallResult`).
#[derive(Default, Clone)]
struct ImageCall {
    result: String,
    revised_prompt: String,
    output_format: String,
    size: String,
    background: String,
    quality: String,
}

impl CodexExecutor {
    /// The Images API on a Codex credential (`executeOpenAIImage`,
    /// `executeOpenAIImageStream`). `request_path` is the inbound route; `req.stream`
    /// asks for SSE.
    pub async fn images(
        &self,
        credential: &Credential,
        req: ExecRequest,
        request_path: &str,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let view = View::for_request(credential, cfg).with_session(explicit_session(&req));
        let settings = Settings::scoped(cfg, &view);
        let path = request_path.trim();
        match (direct_model(&req), direct_endpoint(path)) {
            (Some(model), Some(endpoint)) => self.direct(&view, &settings, req, path, endpoint, &model).await,
            _ => self.tool(&view, &settings, req, path).await,
        }
    }

    /// `executeDirectOpenAIImage` / `executeDirectOpenAIImageStream`: the request (JSON, or
    /// a multipart edit as JSON) goes to the backend's Images endpoint; the response
    /// passes through.
    async fn direct(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        path: &str,
        endpoint: &str,
        model: &str,
    ) -> Result<ExecResponse, ExecError> {
        let content_type = request::header(&req.headers, "content-type").to_owned();
        let (body, content_type) = if path.ends_with(EDITS) {
            direct_edit_payload(&req.body, model, &content_type, req.stream)?
        } else {
            prepare_images_payload(&req.body, model, &content_type, req.stream)?
        };
        if req.usage.enabled() {
            req.usage.request(Format::OpenAI, &body);
        }
        let cache = request::session_uuid(&req, None);
        let body = json_body(body, cache.as_deref());
        // `applyCodexDirectImageHeaders`: the client's User-Agent is not forwarded.
        let mut client = req.headers.clone();
        client.remove(http::header::USER_AGENT);
        let mut headers = request::image_headers(view, settings, &client, model, cache.as_deref(), req.stream);
        if !content_type.is_empty()
            && let Ok(value) = http::HeaderValue::from_str(&content_type)
        {
            headers.insert(http::header::CONTENT_TYPE, value);
        }
        let url = format!("{}{endpoint}", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        wire.request(&url, &headers, &body);
        let upstream = match self.post(view, &url, &headers, body).await {
            Ok(upstream) => upstream,
            Err(error) => {
                wire.exec_error(&error);
                return Err(error);
            }
        };
        wire.metadata(upstream.status, &upstream.headers);
        self.quota().observe(&view.credential.id, model, &upstream.headers);
        let status = upstream.status;
        let mut headers = upstream.headers;
        headers.remove(http::header::CONTENT_ENCODING);
        headers.remove(http::header::CONTENT_LENGTH);
        if !(200..300).contains(&status) {
            // Both direct modes record and return a read error of the error body.
            let body = match crate::proxy::read_all(upstream.body, crate::proxy::MAX_ERROR_BODY, false).await {
                Ok(body) => body,
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            };
            wire.chunk(&body);
            return Err(response::status_error(
                status,
                &body,
                headers,
                settings.model_level_cooling,
            ));
        }
        if !req.stream {
            let data = match crate::proxy::read_all(upstream.body, usize::MAX, false).await {
                Ok(data) => data,
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            };
            wire.chunk(&data);
            if req.usage.enabled() {
                req.usage.response_body(Format::OpenAI, &data);
            }
            return Ok(ExecResponse {
                status,
                headers,
                body: ResponseBody::Buffered(data),
            });
        }
        let usage = req.usage.clone();
        let stream = upstream.body.inspect(move |chunk| {
            // Go records each body read as it is.
            match chunk {
                Ok(chunk) => wire.chunk(chunk),
                Err(error) => wire.exec_error(error),
            }
            if let Ok(chunk) = chunk
                && usage.enabled()
            {
                // Go splits each read on newlines (`ObserveOpenAIStream`).
                for line in chunk.split(|b| *b == b'\n') {
                    let line = line.trim_ascii();
                    let data = line.strip_prefix(b"data:").map_or(line, <[u8]>::trim_ascii);
                    if !data.is_empty() {
                        usage.response_line(Format::OpenAI, data);
                    }
                }
            }
        });
        Ok(ExecResponse {
            status,
            headers,
            body: ResponseBody::Stream(stream.boxed()),
        })
    }

    /// The Responses path: a forced `image_generation` tool call on the main model.
    async fn tool(
        &self,
        view: &View<'_>,
        settings: &Settings,
        req: ExecRequest,
        path: &str,
    ) -> Result<ExecResponse, ExecError> {
        let prepared = prepare(&req, path)?;
        let main_model = main_model(&settings.image_base_model);
        let body = tool_body(&prepared.body, &req, settings, path, main_model)?;
        report_request(&req, Format::Codex, &body);
        let cache = request::session_uuid(&req, None);
        let body = match &cache {
            Some(id) => set_str_if_different(body, "prompt_cache_key", id),
            None => body,
        };
        let body = request::sanitize_input_ids(body);
        let headers = request::image_headers(view, settings, &req.headers, main_model, cache.as_deref(), true);
        let url = format!("{}/responses", view.base_url);
        let wire = Wire::new(req.capture(), view.credential);
        let res = self
            .send(
                view,
                settings,
                url,
                headers,
                body,
                &Default::default(),
                &wire,
                crate::codex::ErrorBody::Strict,
            )
            .await?;
        let upstream = events(res.body);
        // ponytail: usage lands on the images main model's record; Go also publishes the
        // completion's `tool_usage.image_gen` under the tool model, which `UsageSink`
        // cannot express yet (see `Processor::reporting`).
        if req.stream {
            return Ok(ExecResponse {
                status: res.status,
                headers: res.headers,
                body: ResponseBody::Stream(frames(upstream, prepared, req.usage.clone(), wire)),
            });
        }
        // Go reads the whole body (`io.ReadAll`) and records it before parsing.
        let mut upstream = upstream;
        let mut read = Vec::new();
        while let Some(event) = upstream.next().await {
            match event {
                Ok(event) => read.push(event),
                Err(error) => {
                    wire.exec_error(&error);
                    return Err(error);
                }
            }
        }
        if wire.enabled() {
            wire.chunk(&read.concat());
        }
        let mut items = OutputItems::default();
        for event in read {
            for data in data_lines(&event) {
                if req.usage.enabled() {
                    req.usage.response_line(Format::Codex, data.as_bytes());
                }
                match gjson::get(data, "type").str() {
                    "response.output_item.done" => items.collect(data),
                    "response.completed" => {
                        let (calls, created, usage) = extract(data, &items);
                        if calls.is_empty() {
                            return Err(status_err(502, "upstream did not return image output"));
                        }
                        return Ok(ExecResponse {
                            status: res.status,
                            headers: res.headers,
                            body: ResponseBody::Buffered(Bytes::from(images_response(
                                &calls,
                                created,
                                usage.as_deref(),
                                prepared.url_format,
                            ))),
                        });
                    }
                    _ => {}
                }
            }
        }
        Err(status_err(504, "stream error: stream disconnected before completion"))
    }
}

/// The stream path's frames (`executeOpenAIImageStream`): partial images as they come,
/// then one completed frame per image. A stream that ends without
/// `response.completed` ends quietly, as in Go.
fn frames(upstream: ExecStream, prepared: Prepared, usage: cpa_core::exec::UsageSink, wire: Wire) -> ExecStream {
    struct State {
        upstream: ExecStream,
        prepared: Prepared,
        usage: cpa_core::exec::UsageSink,
        wire: Wire,
        items: OutputItems,
        out: VecDeque<Result<Bytes, ExecError>>,
        done: bool,
    }
    let state = State {
        upstream,
        prepared,
        usage,
        wire,
        items: OutputItems::default(),
        out: VecDeque::new(),
        done: false,
    };
    futures_util::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.out.pop_front() {
                return Some((item, st));
            }
            if st.done {
                return None;
            }
            let event = match st.upstream.next().await {
                None => return None,
                Some(Err(error)) => {
                    st.done = true;
                    st.wire.exec_error(&error);
                    st.out.push_back(Err(error));
                    continue;
                }
                Some(Ok(event)) => event,
            };
            for raw in event.split(|b| *b == b'\n') {
                let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
                // Go records each scanned line before reading it, and none after the end.
                st.wire.chunk(raw);
                let Some(data) = std::str::from_utf8(raw)
                    .ok()
                    .and_then(|line| line.strip_prefix("data:"))
                    .map(str::trim)
                else {
                    continue;
                };
                if st.usage.enabled() {
                    st.usage.response_line(Format::Codex, data.as_bytes());
                }
                match gjson::get(data, "type").str() {
                    "response.output_item.done" => st.items.collect(data),
                    "response.image_generation_call.partial_image" => {
                        if let Some(frame) = partial_frame(data, &st.prepared) {
                            st.out.push_back(Ok(Bytes::from(frame)));
                        }
                    }
                    "response.completed" => {
                        st.done = true;
                        let (calls, _, usage) = extract(data, &st.items);
                        if calls.is_empty() {
                            st.out
                                .push_back(Err(status_err(502, "upstream did not return image output")));
                        }
                        for call in &calls {
                            let frame = completed_frame(call, usage.as_deref(), &st.prepared);
                            st.out.push_back(Ok(Bytes::from(frame)));
                        }
                        break;
                    }
                    _ => {}
                }
            }
        }
    })
    .boxed()
}

/// The `data:` payloads of one SSE event, trimmed (Go reads `data:` lines).
fn data_lines(event: &[u8]) -> Vec<&str> {
    std::str::from_utf8(event)
        .unwrap_or_default()
        .split('\n')
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .collect()
}

/// `codexDirectOpenAIImageModel`: the payload's model, then the route's, when it is a
/// gpt-image-* model.
fn direct_model(req: &ExecRequest) -> Option<String> {
    let payload_model = gjson::get(&String::from_utf8_lossy(&req.body), "model")
        .str()
        .to_owned();
    [payload_model.as_str(), req.model.as_str()]
        .into_iter()
        .map(image_base_model)
        .find(|m| DIRECT_MODELS.contains(&m.as_str()))
}

/// `codexOpenAIImageBaseModel`: no thinking suffix, no `provider/` prefix, lower case.
fn image_base_model(model: &str) -> String {
    let base = request::base_model(model);
    let mut base = base.trim();
    if let Some(i) = base.rfind('/')
        && i < base.len() - 1
    {
        base = base[i + 1..].trim();
    }
    base.trim().to_lowercase()
}

/// `codexDirectOpenAIImageEndpoint`.
fn direct_endpoint(path: &str) -> Option<&'static str> {
    if path.ends_with(GENERATIONS) {
        Some("/images/generations")
    } else if path.ends_with(EDITS) {
        Some("/images/edits")
    } else {
        None
    }
}

/// `resolveGPTImage2BaseModel`.
fn main_model(configured: &str) -> &str {
    if !configured.is_empty() && configured.to_lowercase().starts_with("gpt-") {
        configured
    } else {
        MAIN_MODEL
    }
}

/// `cacheHelper` on a direct body: the prompt cache key and input-id sanitising apply to
/// JSON bodies only.
fn json_body(body: Bytes, cache: Option<&str>) -> Bytes {
    if !cpa_common::json::std_valid(&body) {
        return body;
    }
    let text = String::from_utf8_lossy(&body).into_owned();
    let text = match cache {
        Some(id) => set_str_if_different(text, "prompt_cache_key", id),
        None => text,
    };
    Bytes::from(request::sanitize_input_ids(text))
}

/// `codexPrepareDirectOpenAIImageEditPayload`: JSON as for generations; a multipart edit
/// is rewritten to JSON with data URLs.
fn direct_edit_payload(
    payload: &[u8],
    model: &str,
    content_type: &str,
    stream: bool,
) -> Result<(Bytes, String), ExecError> {
    if cpa_common::json::std_valid(payload) {
        return prepare_images_payload(payload, model, content_type, stream);
    }
    let unsupported = || plain_err(format!("unsupported OpenAI image edit Content-Type {content_type:?}"));
    let MediaType::Ok(media, params) = multipart::parse_media_type(content_type.trim()) else {
        return Err(unsupported());
    };
    if !media.trim().starts_with("multipart/") {
        return Err(unsupported());
    }
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_err("multipart boundary is missing"));
    }
    let form =
        multipart::read_form(payload, boundary).map_err(|e| plain_err(format!("read multipart form failed: {e}")))?;
    Ok((
        Bytes::from(edit_form_to_json(&form, model, stream)),
        "application/json".into(),
    ))
}

/// `codexRewriteOpenAIImageEditMultipartToJSON`.
// ponytail: Go ranges over its form map, so field order varies per run; this keeps the
// order fields arrived in, one of Go's orders.
fn edit_form_to_json(form: &Form, model: &str, stream: bool) -> String {
    let mut out = set_str("{}", "model", model);
    if stream {
        out = set_raw(&out, "stream", "true");
    }
    for (key, values) in &form.values {
        let key = key.trim();
        if key.is_empty() || key == "model" || key == "stream" || values.is_empty() {
            continue;
        }
        let path = match key {
            "mask[file_id]" => "mask.file_id",
            "mask[image_url]" => "mask.image_url",
            other => other,
        };
        let raw = if let [value] = values.as_slice() {
            form_json_value(path, value)
        } else {
            let items: Vec<String> = values.iter().map(|v| form_json_value(key, v)).collect();
            format!("[{}]", items.join(","))
        };
        out = set_raw(&out, path, &raw);
    }
    if let Some(mask) = form.files("mask").first() {
        out = set_str(&out, "mask.image_url", &data_url(mask));
    }
    let files = image_files(form);
    let existing = gjson::get(&out, "images");
    if !existing.exists() || existing.kind() == Kind::Array {
        let mut items: Vec<String> = existing.array().iter().map(|v| v.json().to_owned()).collect();
        for file in files {
            items.push(set_str(r#"{"image_url":""}"#, "image_url", &data_url(file)));
        }
        if !files.is_empty() {
            out = set_raw(&out, "images", &format!("[{}]", items.join(",")));
        }
    } else {
        for file in files {
            out = set_str(&out, "images.-1.image_url", &data_url(file));
        }
    }
    out
}

/// `codexOpenAIImageEditFormJSONValue`: integers for the count fields, strings otherwise.
fn form_json_value(key: &str, value: &[u8]) -> String {
    let text = String::from_utf8_lossy(value);
    let text = text.trim();
    if matches!(
        key.trim().to_lowercase().as_str(),
        "n" | "output_compression" | "partial_images"
    ) && let Ok(n) = go_parse_int(text)
    {
        return n.to_string();
    }
    String::from_utf8_lossy(&cpa_common::json::quote(text)).into_owned()
}

/// `strconv.ParseInt(s, 10, 64)`.
fn go_parse_int(s: &str) -> Result<i64, std::num::ParseIntError> {
    s.parse::<i64>()
}

/// `codexMultipartImageFiles`: `image[]` files, else `image` files.
fn image_files(form: &Form) -> &[Part] {
    match form.files("image[]") {
        [] => form.files("image"),
        files => files,
    }
}

/// `codexMultipartFileToDataURL`: the part's Content-Type, else the sniffed type.
fn data_url(part: &Part) -> String {
    let media = part
        .header("Content-Type")
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| detect_content_type(&part.body));
    format!(
        "data:{media};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&part.body)
    )
}

/// `http.DetectContentType` for uploaded images.
// ponytail: Go's image, BOM and text-versus-binary rules only; its HTML, XML, PDF, audio,
// video, archive and font signatures are not ported (image uploads do not carry them).
fn detect_content_type(data: &[u8]) -> &'static str {
    let data = &data[..data.len().min(512)];
    const EXACT: [(&[u8], &str); 8] = [
        (b"\xFE\xFF", "text/plain; charset=utf-16be"),
        (b"\xFF\xFE", "text/plain; charset=utf-16le"),
        (b"\xEF\xBB\xBF", "text/plain; charset=utf-8"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"\x89PNG\x0D\x0A\x1A\x0A", "image/png"),
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"BM", "image/bmp"),
    ];
    for (signature, media) in EXACT {
        if data.starts_with(signature) {
            return media;
        }
    }
    if data.len() >= 14 && &data[..4] == b"RIFF" && &data[8..14] == b"WEBPVP" {
        return "image/webp";
    }
    if data.starts_with(b"\x00\x00\x01\x00") {
        return "image/x-icon";
    }
    let binary = |b: &u8| matches!(*b, 0x00..=0x08 | 0x0B | 0x0E..=0x1A | 0x1C..=0x1F);
    if data.iter().any(binary) {
        "application/octet-stream"
    } else {
        "text/plain; charset=utf-8"
    }
}

/// `codexPrepareOpenAIImageRequest`.
fn prepare(req: &ExecRequest, path: &str) -> Result<Prepared, ExecError> {
    if path.ends_with(GENERATIONS) {
        return generation_json(&req.body, &req.model);
    }
    if !path.ends_with(EDITS) {
        return Err(plain_err(format!("unsupported OpenAI image endpoint path {path:?}")));
    }
    let content_type = request::header(&req.headers, "content-type").trim();
    let media = match multipart::parse_media_type(content_type) {
        MediaType::Ok(media, _) | MediaType::BadParams(media) => media,
        MediaType::Invalid => String::new(),
    };
    if media.starts_with("multipart/") {
        return edit_multipart(&req.body, &req.model, content_type);
    }
    edit_json(&req.body, &req.model)
}

fn json_text(raw: &[u8]) -> Option<String> {
    cpa_common::json::std_valid(raw).then(|| String::from_utf8_lossy(raw).into_owned())
}

/// `codexPrepareOpenAIImageGenerationJSON`.
fn generation_json(raw: &[u8], route_model: &str) -> Result<Prepared, ExecError> {
    let raw = json_text(raw).ok_or_else(|| plain_err("invalid OpenAI image generation request JSON"))?;
    let prompt = gjson::get(&raw, "prompt").str().trim().to_owned();
    let tool = tool_json(
        &raw,
        route_model,
        "generate",
        &["size", "quality", "background", "output_format", "moderation"],
    );
    Ok(Prepared {
        body: responses_request(&prompt, &[], &tool),
        url_format: url_format(gjson::get(&raw, "response_format").str()),
        prefix: "image_generation",
    })
}

/// `codexPrepareOpenAIImageEditJSON`.
fn edit_json(raw: &[u8], route_model: &str) -> Result<Prepared, ExecError> {
    let raw = json_text(raw).ok_or_else(|| plain_err("invalid OpenAI image edit request JSON"))?;
    let prompt = gjson::get(&raw, "prompt").str().trim().to_owned();
    let images_value = gjson::get(&raw, "images");
    let images: Vec<String> = if images_value.kind() == Kind::Array {
        images_value
            .array()
            .iter()
            .map(|img| img.get("image_url").str().trim().to_owned())
            .filter(|url| !url.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    let mut tool = tool_json(
        &raw,
        route_model,
        "edit",
        &[
            "size",
            "quality",
            "background",
            "output_format",
            "input_fidelity",
            "moderation",
        ],
    );
    let mask = gjson::get(&raw, "mask.image_url").str().trim().to_owned();
    if !mask.is_empty() {
        tool = set_str(&tool, "input_image_mask.image_url", &mask);
    }
    Ok(Prepared {
        body: responses_request(&prompt, &images, &tool),
        url_format: url_format(gjson::get(&raw, "response_format").str()),
        prefix: "image_edit",
    })
}

/// `codexPrepareOpenAIImageEditMultipart`.
fn edit_multipart(body: &[u8], route_model: &str, content_type: &str) -> Result<Prepared, ExecError> {
    let params = match multipart::parse_media_type(content_type) {
        MediaType::Ok(_, params) => params,
        _ => {
            return Err(plain_err(
                "parse multipart content type failed: mime: invalid media parameter",
            ));
        }
    };
    let boundary = params.get("boundary").map(|b| b.trim()).unwrap_or_default();
    if boundary.is_empty() {
        return Err(plain_err("multipart boundary is required"));
    }
    let form =
        multipart::read_form(body, boundary).map_err(|e| plain_err(format!("parse multipart form failed: {e}")))?;
    let value = |key: &str| form.value(key).trim().to_owned();
    let mut tool = r#"{"type":"image_generation","action":"edit"}"#.to_owned();
    tool = set_str(&tool, "model", &tool_model(&value("model"), route_model));
    for field in [
        "size",
        "quality",
        "background",
        "output_format",
        "input_fidelity",
        "moderation",
    ] {
        let v = value(field);
        if !v.is_empty() {
            tool = set_str(&tool, field, &v);
        }
    }
    for field in ["output_compression", "partial_images"] {
        if let Ok(n) = go_parse_int(&value(field)) {
            tool = set_raw(&tool, field, &n.to_string());
        }
    }
    let images: Vec<String> = image_files(&form).iter().map(data_url).collect();
    if let Some(mask) = form.files("mask").first() {
        tool = set_str(&tool, "input_image_mask.image_url", &data_url(mask));
    }
    Ok(Prepared {
        body: responses_request(&value("prompt"), &images, &tool),
        url_format: url_format(&value("response_format")),
        prefix: "image_edit",
    })
}

/// `codexNormalizeImageResponseFormat`: `url`, else `b64_json`.
fn url_format(format: &str) -> bool {
    format.trim().eq_ignore_ascii_case("url")
}

/// `codexOpenAIImageToolModel`.
fn tool_model(request_model: &str, route_model: &str) -> String {
    [request_model.trim(), route_model.trim()]
        .into_iter()
        .find(|m| !m.is_empty())
        .unwrap_or(DEFAULT_TOOL_MODEL)
        .to_owned()
}

/// `codexBuildOpenAIImageTool`: string fields as Go's `String()` reads them, the count
/// fields only when numeric (truncated to integers).
fn tool_json(raw: &str, route_model: &str, action: &str, string_fields: &[&str]) -> String {
    let mut tool = set_str(r#"{"type":"image_generation","action":""}"#, "action", action);
    tool = set_str(&tool, "model", &tool_model(gjson::get(raw, "model").str(), route_model));
    for field in string_fields {
        let value = gjson::get(raw, field);
        let value = value.str().trim();
        if !value.is_empty() {
            tool = set_str(&tool, field, value);
        }
    }
    for field in ["output_compression", "partial_images"] {
        let value = gjson::get(raw, field);
        if value.kind() == Kind::Number {
            tool = set_raw(&tool, field, &value.i64().to_string());
        }
    }
    tool
}

/// `codexBuildImagesResponsesRequest`.
fn responses_request(prompt: &str, images: &[String], tool: &str) -> String {
    let mut req = r#"{"instructions":"","stream":true,"reasoning":{"effort":"medium","summary":"auto"},"parallel_tool_calls":true,"include":["reasoning.encrypted_content"],"model":"","store":false,"tool_choice":{"type":"image_generation"},"tools":[]}"#.to_owned();
    req = set_str(&req, "model", MAIN_MODEL);
    if !tool.is_empty() && gjson::valid(tool) {
        req = set_raw(&req, "tools", &format!("[{tool}]"));
    }
    let mut content = vec![set_str(r#"{"type":"input_text","text":""}"#, "text", prompt)];
    for image in images.iter().filter(|i| !i.trim().is_empty()) {
        content.push(set_str(r#"{"type":"input_image","image_url":""}"#, "image_url", image));
    }
    let input = format!(
        r#"[{{"type":"message","role":"user","content":[{}]}}]"#,
        content.join(",")
    );
    set_raw(&req, "input", &input)
}

/// `prepareCodexOpenAIImageBody`: thinking, payload rules, then the Codex request rules.
fn tool_body(
    body: &str,
    req: &ExecRequest,
    settings: &Settings,
    path: &str,
    main_model: &str,
) -> Result<String, ExecError> {
    let thought = cpa_common::thinking::apply_thinking_with_source_payload(
        body.as_bytes(),
        body.as_bytes(),
        body.as_bytes(),
        main_model,
        SOURCE,
        "codex",
        "codex",
        false,
    )
    .map_err(|e| ExecError::local(e.status(), cpa_core::exec::FailureScope::Request, e.message))?;
    let requested = if req.requested_model.trim().is_empty() {
        req.model.trim()
    } else {
        req.requested_model.trim()
    };
    let rules = cpa_common::payload::Request {
        target_executor: "",
        model: main_model,
        requested_model: requested,
        protocol: "codex",
        from_protocol: SOURCE,
        root: "",
        original: body.as_bytes(),
        request_path: path,
        headers: Some(&req.headers),
    };
    let out = cpa_common::payload::apply(&settings.payload, &rules, thought);
    let mut out = String::from_utf8_lossy(&out).into_owned();
    out = set_str_if_different(out, "model", main_model);
    out = set_bool_if_different(out, "stream", true);
    for key in [
        "previous_response_id",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
    ] {
        out = delete(&out, key);
    }
    let mut bytes = out.into_bytes();
    cpa_common::codex_client::normalize_codex_instructions(&mut bytes);
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `codexExtractImageResults`: image calls from the completed output, else from the
/// collected `output_item.done` items in index order; the creation time (now when
/// absent); and `tool_usage.image_gen`.
fn extract(completed: &str, items: &OutputItems) -> (Vec<ImageCall>, i64, Option<String>) {
    let mut created = gjson::get(completed, "response.created_at").i64();
    if created <= 0 {
        created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
    }
    let patched = items.patch(completed.to_owned());
    let calls = gjson::get(&patched, "response.output")
        .array()
        .iter()
        .filter(|item| item.get("type").str() == "image_generation_call")
        .filter_map(|item| {
            let field = |k: &str| item.get(k).str().trim().to_owned();
            let result = field("result");
            (!result.is_empty()).then(|| ImageCall {
                result,
                revised_prompt: field("revised_prompt"),
                output_format: field("output_format"),
                size: field("size"),
                background: field("background"),
                quality: field("quality"),
            })
        })
        .collect();
    let usage = gjson::get(completed, "response.tool_usage.image_gen");
    let usage = (usage.kind() == Kind::Object).then(|| usage.json().to_owned());
    (calls, created, usage)
}

/// `codexBuildImagesAPIResponse`.
fn images_response(calls: &[ImageCall], created: i64, usage: Option<&str>, url: bool) -> String {
    let mut out = set_raw(r#"{"created":0,"data":[]}"#, "created", &created.to_string());
    let first = calls.first().cloned().unwrap_or_default();
    for (key, value) in [
        ("background", &first.background),
        ("output_format", &first.output_format),
        ("quality", &first.quality),
        ("size", &first.size),
    ] {
        if !value.is_empty() {
            out = set_str(&out, key, value);
        }
    }
    if let Some(usage) = usage.filter(|u| gjson::valid(u)) {
        out = set_raw(&out, "usage", usage);
    }
    let items: Vec<String> = calls
        .iter()
        .map(|call| {
            let mut item = "{}".to_owned();
            if !call.revised_prompt.is_empty() {
                item = set_str(&item, "revised_prompt", &call.revised_prompt);
            }
            image_field(item, call, url)
        })
        .collect();
    set_raw(&out, "data", &format!("[{}]", items.join(",")))
}

/// `url` as a data URL, else `b64_json`.
fn image_field(json: String, call: &ImageCall, url: bool) -> String {
    if url {
        set_str(
            &json,
            "url",
            &format!("data:{};base64,{}", mime_type(&call.output_format), call.result),
        )
    } else {
        set_str(&json, "b64_json", &call.result)
    }
}

/// `codexBuildImagePartialFrame`.
fn partial_frame(payload: &str, prepared: &Prepared) -> Option<String> {
    let b64 = gjson::get(payload, "partial_image_b64").str().trim().to_owned();
    if b64.is_empty() {
        return None;
    }
    let call = ImageCall {
        result: b64,
        output_format: gjson::get(payload, "output_format").str().trim().to_owned(),
        ..Default::default()
    };
    let event = format!("{}.partial_image", prepared.prefix);
    let mut data = set_str(r#"{"type":"","partial_image_index":0}"#, "type", &event);
    data = set_raw(
        &data,
        "partial_image_index",
        &gjson::get(payload, "partial_image_index").i64().to_string(),
    );
    Some(sse_frame(&event, &image_field(data, &call, prepared.url_format)))
}

/// `codexBuildImageCompletedFrame`.
fn completed_frame(call: &ImageCall, usage: Option<&str>, prepared: &Prepared) -> String {
    let event = format!("{}.completed", prepared.prefix);
    let mut data = set_str(r#"{"type":""}"#, "type", &event);
    if let Some(usage) = usage.filter(|u| gjson::valid(u)) {
        data = set_raw(&data, "usage", usage);
    }
    sse_frame(&event, &image_field(data, call, prepared.url_format))
}

fn sse_frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// `codexMimeTypeFromOutputFormat`.
fn mime_type(output_format: &str) -> &'static str {
    match output_format.trim().to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}
