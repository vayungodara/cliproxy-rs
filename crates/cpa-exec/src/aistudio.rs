//! AI Studio over the `/v1/ws` relay (internal/runtime/executor/aistudio_executor.go).
//!
//! A browser running the AI Studio bridge connects to `/v1/ws`; while it stays
//! connected the server holds a runtime-only `aistudio` credential named after the
//! session. Requests take the Gemini pipeline (without `maxOutputTokens`, response MIME
//! type and schema, and with upper-case thinking levels) and travel to the browser as
//! relay messages. Every translated chunk and body is re-encoded the way Go's
//! `ensureColonSpacedJSON` writes JSON.

use cpa_core::gostr::GoStr;
use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use cpa_common::json::{self as gj, GoValue};
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{
    CaptureEvent, CaptureSink, ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody,
};
use cpa_core::format::Format;
use cpa_translate::stream::StreamOptions;
use futures_util::StreamExt;

use crate::gemini::{self as g, Emit, Output};
use crate::gemini_payload as payload;
use crate::payload_barrier::Barrier;
use crate::wsrelay::{self, HttpRequest, Relay, StreamEvent};

pub const PROVIDER: &str = "aistudio";

/// Requests through the relay its sessions are attached to.
#[derive(Default)]
pub struct AiStudioExecutor {
    pub relay: Arc<Relay>,
}

/// A Go error without a status code: the handler answers it with 500.
fn plain_error(message: impl Into<String>) -> ExecError {
    ExecError::local(500, FailureScope::Credential, message)
}

/// Go's `statusErr{code: status}`; a status outside HTTP's range is answered as 500.
fn status_error(status: i64, body: Vec<u8>) -> ExecError {
    g::status_err(u16::try_from(status).ok().filter(|s| *s >= 100).unwrap_or(500), body)
}

fn header_map(headers: &std::collections::BTreeMap<String, Vec<String>>) -> http::HeaderMap {
    let mut out = http::HeaderMap::new();
    for (name, values) in headers {
        let Ok(name) = http::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        for value in values {
            if let Ok(value) = http::HeaderValue::from_bytes(value.as_bytes()) {
                out.append(name.clone(), value);
            }
        }
    }
    out
}

/// `ensureColonSpacedJSON`: a JSON value re-encoded as `json.MarshalIndent` writes it
/// (keys sorted, float64 numbers), then with every line break and the indentation after
/// it removed outside strings, so `": "` keeps its space. Anything else is unchanged.
pub(crate) fn ensure_colon_spaced_json(payload: &[u8]) -> Vec<u8> {
    let trimmed = cpa_common::gostr::trim_space(payload);
    if trimmed.is_empty() || !gj::std_valid(trimmed) {
        return payload.to_vec();
    }
    let Some(value) = GoValue::parse_f64(trimmed) else {
        return payload.to_vec();
    };
    let indented = value.encode_indented();
    let mut out = Vec::with_capacity(indented.len());
    let (mut in_string, mut skip_space) = (false, false);
    for (i, &c) in indented.iter().enumerate() {
        if c == b'"' {
            // Escaped only after an odd run of backslashes.
            let backslashes = indented[..i].iter().rev().take_while(|b| **b == b'\\').count();
            if backslashes % 2 == 0 {
                in_string = !in_string;
            }
        }
        if !in_string {
            if c == b'\n' || c == b'\r' {
                skip_space = true;
                continue;
            }
            if skip_space {
                if c == b' ' || c == b'\t' {
                    continue;
                }
                skip_space = false;
            }
        }
        out.push(c);
    }
    out
}

/// `normalizeAIStudioThinkingLevel`: AI Studio accepts only upper-case levels.
fn normalize_thinking_level(body: Vec<u8>) -> Vec<u8> {
    const PATH: &str = "generationConfig.thinkingConfig.thinkingLevel";
    let level = gj::get(&body, PATH);
    if level.kind != gj::Kind::String {
        return body;
    }
    let current = level.str().into_owned();
    let upper = current.go_upper();
    if !matches!(upper.as_str(), "MINIMAL" | "LOW" | "MEDIUM" | "HIGH") || upper == current {
        return body;
    }
    gj::try_set_str(&body, PATH, &upper).unwrap_or(body)
}

/// `buildEndpoint`.
fn endpoint(model: &str, action: &str, alt: &str) -> String {
    let base = format!("{}/{}/models/{model}:{action}", g::DEFAULT_BASE_URL, g::API_VERSION);
    if action == "streamGenerateContent" {
        return if alt.is_empty() {
            format!("{base}?alt=sse")
        } else {
            format!("{base}?$alt={}", cpa_common::gostr::query_escape(alt))
        };
    }
    if !alt.is_empty() && action != "countTokens" {
        return format!("{base}?$alt={}", cpa_common::gostr::query_escape(alt));
    }
    base
}

/// `translateRequest`: the Gemini body and the action it is sent to.
fn translate(req: &ExecRequest, cfg: &Config, count: bool) -> Result<(Vec<u8>, &'static str), ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let (from, to) = (req.source_format, Format::Gemini);
    let stream = req.stream && !count;
    let original = g::original_request(req);
    let original_translated = g::translate(req, cfg, to, &base_model, original, stream, false);
    let body = g::translate(req, cfg, to, &base_model, &req.body, stream, false)?;
    let original_translated = crate::translate::baseline_or_working(original_translated, &body);
    let body = cpa_common::thinking::apply_thinking_with_source_payload(
        &body,
        &req.body,
        original,
        &req.model,
        from.as_str(),
        to.as_str(),
        PROVIDER,
        cpa_translate::pair(from, to).is_some(),
    )
    .map_err(|e| ExecError::local(e.status(), FailureScope::Request, e.message))?;
    let mut body = payload::fix_image_aspect_ratio(&base_model, body);
    for path in [
        "generationConfig.maxOutputTokens",
        "generationConfig.responseMimeType",
        "generationConfig.responseJsonSchema",
    ] {
        body = payload::delete(body, path);
    }
    let action = match (count, stream) {
        (true, _) => "countTokens",
        (false, true) => "streamGenerateContent",
        (false, false) => "generateContent",
    };
    body = payload::delete(body, "session_id");
    body = payload::ensure_leading_user_content(body, "contents");
    if !count {
        body = payload::ensure_trailing_user_content(body, "contents");
    }
    body = normalize_thinking_level(body);
    if count {
        for path in ["generationConfig", "tools", "safetySettings"] {
            body = payload::delete(body, path);
        }
    }
    let body = Barrier {
        executor: "",
        model: &base_model,
        protocol: to.as_str(),
        root: "",
        original: &original_translated,
    }
    .apply(cfg, req, body);
    Ok((body, action))
}

/// The relayed request: JSON content type and, except for countTokens, the
/// credential's custom headers (`Header.Set`, so a Host entry is relayed as a header).
fn http_request(url: String, body: Bytes, custom: Vec<(String, String)>) -> HttpRequest {
    let mut headers = vec![("Content-Type".to_owned(), vec!["application/json".to_owned()])];
    for (name, value) in custom {
        let name = crate::proxy::canonical_header(&name);
        headers.retain(|(n, _)| *n != name);
        headers.push((name, vec![value]));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    HttpRequest {
        method: "POST".into(),
        url,
        headers,
        body,
    }
}

/// A relayed header map as Go's `http.Header` pairs.
fn header_pairs<'a>(headers: impl IntoIterator<Item = (&'a String, &'a Vec<String>)>) -> Vec<(String, String)> {
    headers
        .into_iter()
        .flat_map(|(name, values)| values.iter().map(move |v| (name.clone(), v.clone())))
        .collect()
}

/// `RecordAPIRequest` for the relayed request.
fn capture_request(capture: &CaptureSink, credential: &Credential, relayed: &HttpRequest) {
    if capture.enabled() {
        let headers = header_pairs(relayed.headers.iter().map(|(n, v)| (n, v)));
        g::capture_request(capture, credential, PROVIDER, &relayed.url, &headers, &relayed.body);
    }
}

/// `RecordAPIResponseMetadata` for a relayed status and headers.
fn capture_metadata(capture: &CaptureSink, status: i64, headers: &std::collections::BTreeMap<String, Vec<String>>) {
    if capture.enabled() {
        let pairs = header_pairs(headers);
        capture.record(CaptureEvent::ResponseMetadata(
            u16::try_from(status).unwrap_or(0),
            &pairs,
        ));
    }
}

/// `NonStream` with Go's capture: the error, or the metadata and a non-empty body.
async fn non_stream_captured(
    relay: &Relay,
    capture: &CaptureSink,
    credential: &Credential,
    relayed: &HttpRequest,
) -> Result<wsrelay::HttpResponse, ExecError> {
    capture_request(capture, credential, relayed);
    let resp = relay.non_stream(&credential.id, relayed).await.map_err(|e| {
        capture.record(CaptureEvent::ResponseError(&e));
        plain_error(e)
    })?;
    capture_metadata(capture, resp.status, &resp.headers);
    if !resp.body.is_empty() {
        capture.record(CaptureEvent::ResponseChunk(&resp.body));
    }
    Ok(resp)
}

fn custom_headers(credential: &Credential, req: &ExecRequest) -> Vec<(String, String)> {
    let session = cpa_common::session::cpa_session_id(req.session.as_deref());
    cpa_common::headers::custom_headers(&credential.attributes, &req.headers, session.as_deref())
}

impl AiStudioExecutor {
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
        let base_model = parse_suffix(&req.model).model_name;
        let (body, action) = translate(&req, cfg, false)?;
        req.usage.request(Format::Gemini, &body);
        let url = endpoint(&base_model, action, req.alt.as_deref().unwrap_or_default());
        let body = Bytes::from(body);
        let relayed = http_request(url, body.clone(), custom_headers(credential, &req));
        if req.stream {
            return self.stream(credential, &req, relayed, body).await;
        }
        let resp = non_stream_captured(&self.relay, req.capture(), credential, &relayed).await?;
        if !(200..300).contains(&resp.status) {
            return Err(status_error(resp.status, resp.body));
        }
        req.usage.response_body(Format::Gemini, &resp.body);
        let out = g::translate_non_stream(&req, Format::Gemini, &body, &resp.body)?;
        g::claude_error(&req, &out)?;
        Ok(ExecResponse {
            status: 200,
            headers: header_map(&resp.headers),
            body: ResponseBody::Buffered(Bytes::from(ensure_colon_spaced_json(&out))),
        })
    }

    /// `ExecuteStream`: a first event with a non-2xx status is an error carrying every
    /// payload until the end; otherwise chunks are translated as they arrive.
    async fn stream(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        relayed: HttpRequest,
        body: Bytes,
    ) -> Result<ExecResponse, ExecError> {
        let capture = req.capture().clone();
        let fail = |message: String| {
            capture.record(CaptureEvent::ResponseError(&message));
            plain_error(message)
        };
        capture_request(&capture, credential, &relayed);
        let mut events = self.relay.stream(&credential.id, &relayed).await.map_err(fail)?;
        let first = events
            .recv()
            .await
            .ok_or_else(|| fail("wsrelay: stream closed before start".into()))?;
        if first.status > 0 && !(200..300).contains(&first.status) {
            capture_metadata(&capture, first.status, &first.headers);
            if !first.payload.is_empty() {
                capture.record(CaptureEvent::ResponseChunk(&first.payload));
            }
            let mut error_body = first.payload.clone();
            if first.kind != wsrelay::STREAM_END {
                while let Some(event) = events.recv().await {
                    if let Some(error) = &event.error {
                        capture.record(CaptureEvent::ResponseError(error));
                        if error_body.is_empty() {
                            error_body.extend_from_slice(error.as_bytes());
                        }
                        break;
                    }
                    if !event.payload.is_empty() {
                        capture.record(CaptureEvent::ResponseChunk(&event.payload));
                    }
                    error_body.extend_from_slice(&event.payload);
                    if event.kind == wsrelay::STREAM_END {
                        break;
                    }
                }
            }
            return Err(status_error(first.status, error_body));
        }
        let headers = header_map(&first.headers);
        let output = Output::with_options(
            req,
            Format::Gemini,
            &body,
            StreamOptions {
                whole_events: true,
                chunk: Some(ensure_colon_spaced_json),
            },
        )
        .with_claude_stream_errors();
        let state = Events {
            output,
            pending: VecDeque::new(),
            first: Some(first),
            events,
            ended: false,
            capture,
            metadata_logged: false,
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(item) = state.pending.pop_front() {
                    return Some((item, state));
                }
                if state.ended {
                    return None;
                }
                let event = match state.first.take() {
                    Some(first) => Some(first),
                    None => state.events.recv().await,
                };
                state.process(event);
            }
        });
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Stream(stream.boxed()),
        })
    }

    /// `CountTokens`: no custom headers and no alt; a missing or zero count is an error.
    async fn count_tokens(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        cfg: &Config,
    ) -> Result<ExecResponse, ExecError> {
        let base_model = parse_suffix(&req.model).model_name;
        let (body, _) = translate(req, cfg, true)?;
        let relayed = http_request(endpoint(&base_model, "countTokens", ""), body.into(), Vec::new());
        let resp = non_stream_captured(&self.relay, req.capture(), credential, &relayed).await?;
        if !(200..300).contains(&resp.status) {
            return Err(status_error(resp.status, resp.body));
        }
        let total = gj::get(&resp.body, "totalTokens").int();
        if total <= 0 {
            return Err(plain_error("wsrelay: totalTokens missing in response"));
        }
        let out = cpa_translate::translate_token_count(req.response_format, Format::Gemini, total, &resp.body);
        Ok(ExecResponse {
            status: 200,
            headers: http::HeaderMap::new(),
            body: ResponseBody::Buffered(Bytes::from(out)),
        })
    }
}

/// The stream loop over relay events (Go's `processEvent`).
struct Events {
    output: Output,
    pending: VecDeque<Result<Bytes, ExecError>>,
    first: Option<StreamEvent>,
    events: tokio::sync::mpsc::Receiver<StreamEvent>,
    ended: bool,
    capture: CaptureSink,
    /// Go's `metadataLogged`: the first status-bearing event's metadata only.
    metadata_logged: bool,
}

impl Events {
    fn emit(&mut self, emit: Emit) -> bool {
        self.pending.extend(emit.out.into_iter().map(Ok));
        match emit.stop {
            Some(error) => {
                self.fail(error);
                true
            }
            None => false,
        }
    }

    /// A terminal error after the client side's pending frames.
    fn fail(&mut self, error: ExecError) {
        self.pending.extend(self.output.flush_frames().into_iter().map(Ok));
        self.pending.push_back(Err(error));
        self.ended = true;
    }

    fn process(&mut self, event: Option<StreamEvent>) {
        // The relay ends every stream with a terminal event; a vanished one ends it too.
        let Some(event) = event else {
            let end = self.output.end();
            self.emit(end);
            self.ended = true;
            return;
        };
        if let Some(error) = event.error {
            // A retained Claude error event wins over the relay error (Go 7f2f17d7).
            let mut retained = Emit::default();
            if self.output.stop_retained(&mut retained) {
                self.emit(retained);
                return;
            }
            self.capture.record(CaptureEvent::ResponseError(&error));
            self.fail(plain_error(format!("wsrelay: {error}")));
            return;
        }
        let status_event = matches!(event.kind.as_str(), wsrelay::STREAM_START | wsrelay::HTTP_RESPONSE);
        if status_event && !self.metadata_logged && event.status > 0 {
            capture_metadata(&self.capture, event.status, &event.headers);
            self.metadata_logged = true;
        }
        match event.kind.as_str() {
            wsrelay::STREAM_CHUNK if !event.payload.is_empty() => {
                self.capture.record(CaptureEvent::ResponseChunk(&event.payload));
                // Go 7f2f17d7 reads usage from the raw payload, before the filter.
                self.output.usage.response_line(Format::Gemini, &event.payload);
                let filtered = crate::gemini_stream::filter_sse_usage_metadata(&event.payload);
                let translated = self.output.translate(&filtered);
                self.emit(translated);
            }
            // Go 5ec31442 `finishStream`: the translator also sees `[DONE]`.
            wsrelay::STREAM_END => {
                let end = self.output.end_with_done();
                self.emit(end);
                self.ended = true;
            }
            wsrelay::HTTP_RESPONSE => {
                if !event.payload.is_empty() {
                    self.capture.record(CaptureEvent::ResponseChunk(&event.payload));
                }
                self.ended = true;
                // Go 5ec31442: a relayed non-2xx answer ends the stream with its status.
                if !(200..300).contains(&event.status) {
                    let error = status_error(event.status, event.payload);
                    self.capture
                        .record(CaptureEvent::ResponseError(&String::from_utf8_lossy(&error.body)));
                    self.fail(error);
                    return;
                }
                self.output.usage.response_body(Format::Gemini, &event.payload);
                let translated = self.output.translate(&event.payload);
                if self.emit(translated) {
                    return;
                }
                let mut retained = Emit::default();
                if self.output.stop_retained(&mut retained) {
                    self.emit(retained);
                    return;
                }
                let end = self.output.end_with_done();
                self.emit(end);
            }
            _ => {}
        }
    }
}

/// The provider key AI Studio sessions register under.
pub fn handles(provider: &str) -> bool {
    provider == PROVIDER
}

#[cfg(test)]
#[path = "aistudio_tests.rs"]
mod tests;
