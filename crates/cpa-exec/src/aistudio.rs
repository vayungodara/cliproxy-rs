//! AI Studio over the `/v1/ws` relay (internal/runtime/executor/aistudio_executor.go).
//!
//! A browser running the AI Studio bridge connects to `/v1/ws`; while it stays
//! connected the server holds a runtime-only `aistudio` credential named after the
//! session. Requests take the Gemini pipeline (without `maxOutputTokens`, response MIME
//! type and schema, and with upper-case thinking levels) and travel to the browser as
//! relay messages. Every translated chunk and body is re-encoded the way Go's
//! `ensureColonSpacedJSON` writes JSON.

use std::collections::VecDeque;
use std::sync::Arc;

use bytes::Bytes;
use cpa_common::json::{self as gj, GoValue};
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::stream::StreamOptions;
use futures_util::StreamExt;

use crate::gemini::{self as g, Emit, Output};
use crate::gemini_payload as payload;
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
    let upper = current.to_uppercase();
    if !matches!(upper.as_str(), "MINIMAL" | "LOW" | "MEDIUM" | "HIGH") || upper == current {
        return body;
    }
    gj::try_set_str(&body, PATH, &upper).unwrap_or(body)
}

/// Go's `url.QueryEscape`.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `buildEndpoint`.
fn endpoint(model: &str, action: &str, alt: &str) -> String {
    let base = format!("{}/{}/models/{model}:{action}", g::DEFAULT_BASE_URL, g::API_VERSION);
    if action == "streamGenerateContent" {
        return if alt.is_empty() {
            format!("{base}?alt=sse")
        } else {
            format!("{base}?$alt={}", query_escape(alt))
        };
    }
    if !alt.is_empty() && action != "countTokens" {
        return format!("{base}?$alt={}", query_escape(alt));
    }
    base
}

/// `translateRequest`: the Gemini body and the action it is sent to.
fn translate(req: &ExecRequest, cfg: &Config, count: bool) -> Result<(Vec<u8>, &'static str), ExecError> {
    let base_model = parse_suffix(&req.model).model_name;
    let (from, to) = (req.source_format, Format::Gemini);
    let stream = req.stream && !count;
    let original = g::original_request(req);
    let original_translated = g::translate(req, cfg, to, &base_model, original, stream, false)?;
    let body = g::translate(req, cfg, to, &base_model, &req.body, stream, false)?;
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
    let rules = cpa_common::payload::Rules::from_config(cfg);
    body = g::apply_payload_rules(&rules, &base_model, to.as_str(), body, &original_translated, req);
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
    Ok((normalize_thinking_level(body), action))
}

/// The relayed request: JSON content type and, except for countTokens, the
/// credential's custom headers (`Header.Set`, so a Host entry is relayed as a header).
fn http_request(url: String, body: &[u8], custom: Vec<(String, String)>) -> HttpRequest {
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
        body: body.to_vec(),
    }
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
        let relayed = http_request(url, &body, custom_headers(credential, &req));
        if req.stream {
            return self.stream(credential, &req, relayed, body).await;
        }
        let resp = self
            .relay
            .non_stream(&credential.id, &relayed)
            .await
            .map_err(plain_error)?;
        if !(200..300).contains(&resp.status) {
            return Err(status_error(resp.status, resp.body));
        }
        req.usage.response_body(Format::Gemini, &resp.body);
        let out = g::translate_non_stream(&req, Format::Gemini, &body, &resp.body)?;
        Ok(ExecResponse {
            status: 200,
            headers: header_map(&resp.headers),
            body: ResponseBody::Buffered(Bytes::from(ensure_colon_spaced_json(&out))),
        })
    }

    /// `ExecuteStream`: a first event with a non-200 status is an error carrying every
    /// payload until the end; otherwise chunks are translated as they arrive.
    async fn stream(
        &self,
        credential: &Credential,
        req: &ExecRequest,
        relayed: HttpRequest,
        body: Vec<u8>,
    ) -> Result<ExecResponse, ExecError> {
        let mut events = self.relay.stream(&credential.id, &relayed).await.map_err(plain_error)?;
        let first = events
            .recv()
            .await
            .ok_or_else(|| plain_error("wsrelay: stream closed before start"))?;
        if first.status > 0 && first.status != 200 {
            let mut error_body = first.payload.clone();
            if first.kind != wsrelay::STREAM_END {
                while let Some(event) = events.recv().await {
                    if let Some(error) = &event.error {
                        if error_body.is_empty() {
                            error_body.extend_from_slice(error.as_bytes());
                        }
                        break;
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
        );
        let state = Events {
            output,
            pending: VecDeque::new(),
            first: Some(first),
            events,
            ended: false,
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
        let (mut body, _) = translate(req, cfg, true)?;
        for path in ["generationConfig", "tools", "safetySettings"] {
            body = payload::delete(body, path);
        }
        let relayed = http_request(endpoint(&base_model, "countTokens", ""), &body, Vec::new());
        let resp = self
            .relay
            .non_stream(&credential.id, &relayed)
            .await
            .map_err(plain_error)?;
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
            self.fail(plain_error(format!("wsrelay: {error}")));
            return;
        }
        match event.kind.as_str() {
            wsrelay::STREAM_CHUNK if !event.payload.is_empty() => {
                let filtered = crate::gemini_stream::filter_sse_usage_metadata(&event.payload);
                self.output.usage.response_line(Format::Gemini, &filtered);
                let translated = self.output.translate(&filtered);
                self.emit(translated);
            }
            wsrelay::STREAM_END => {
                let end = self.output.end();
                self.emit(end);
                self.ended = true;
            }
            wsrelay::HTTP_RESPONSE => {
                let translated = self.output.translate(&event.payload);
                if self.emit(translated) {
                    return;
                }
                let end = self.output.end();
                if self.emit(end) {
                    return;
                }
                self.output.usage.response_body(Format::Gemini, &event.payload);
                self.ended = true;
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
