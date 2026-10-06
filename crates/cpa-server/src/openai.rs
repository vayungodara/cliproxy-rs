//! OpenAI routes: Chat Completions, legacy Completions, Responses and compact
//! (sdk/api/handlers/openai/openai_handlers.go, openai_responses_handlers.go).

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{MatchedPath, OriginalUri, State};
use axum::http::HeaderMap;
use axum::response::Response;
use cpa_core::exec::{Caller, ExecError, Operation};
use cpa_core::format::Format;
use serde_json::Value;

use crate::claude::{alt, peek, read_failed};
use crate::dispatch::{self, Call, Done};
use crate::gojson::{self, Obj};
use crate::respond::{self, Writer};
use crate::{Runtime, errors};

struct Request {
    caller: Caller,
    peer: Option<std::net::SocketAddr>,
    query: String,
    headers: HeaderMap,
    path: String,
}

fn call(req: Request, entry: Format, model: String, body: Bytes, stream: bool, alt: Option<String>) -> Call {
    Call {
        entry,
        response: entry,
        operation: Operation::Generate,
        model,
        body,
        stream,
        alt,
        headers: req.headers,
        caller: req.caller,
        forced_provider: None,
        selection_model: None,
        execution_session: None,
        request_path: req.path,
        peer: req.peer,
        turn: None,
        media: None,
    }
}

/// Go `handlers.ReadRequestBody`: `Content-Encoding` is decoded (zstd, applied last
/// first; identity is a no-op) before the JSON is read. A body that fails to decode
/// but is valid JSON is used as sent; otherwise the handler answers 400.
// ponytail: decode errors carry the zstd crate's reason, not klauspost/compress's, and
// the decoded size is bounded by MAX_REQUEST_BYTES (Go reads it unbounded).
fn read_body(headers: &HeaderMap, raw: Bytes) -> Result<Bytes, Box<Response>> {
    let encoding = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
        .unwrap_or_default();
    if encoding.is_empty() || encoding.eq_ignore_ascii_case("identity") {
        return Ok(raw);
    }
    let decode = || -> Result<Bytes, String> {
        let mut body = raw.clone();
        for part in encoding.split(',').rev() {
            match part.trim().to_ascii_lowercase().as_str() {
                "" | "identity" => {}
                "zstd" => body = decode_zstd(&body)?,
                other => return Err(format!("unsupported request content encoding: {other}")),
            }
        }
        Ok(body)
    };
    match decode() {
        Ok(body) => Ok(body),
        Err(_) if serde_json::from_slice::<serde::de::IgnoredAny>(&raw).is_ok() => Ok(raw),
        Err(e) => Err(Box::new(respond::error_detail(
            400,
            &format!("Invalid request: {e}"),
            "invalid_request_error",
        ))),
    }
}

fn decode_zstd(raw: &[u8]) -> Result<Bytes, String> {
    use std::io::Read;
    let fail = |e: std::io::Error| format!("failed to decode zstd request body: {e}");
    let decoder =
        zstd::stream::read::Decoder::new(raw).map_err(|e| format!("failed to create zstd request decoder: {e}"))?;
    let mut out = Vec::new();
    decoder
        .take(crate::MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(fail)?;
    if out.len() > crate::MAX_REQUEST_BYTES {
        return Err(fail(std::io::Error::other("decoded body too large")));
    }
    Ok(Bytes::from(out))
}

pub async fn chat_completions(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut body = match body {
        Ok(body) => match read_body(&headers, body) {
            Ok(body) => body,
            Err(response) => return *response,
        },
        Err(rejection) => return read_failed(&rejection),
    };
    let fields = peek(&body);
    let mut stream = fields.get("stream") == Some(&Value::Bool(true));
    // Go converts Responses-shaped payloads sent to this route into Chat Completions.
    if responses_shaped(&body)
        && let Some(pair) = cpa_translate::pair(Format::OpenAIResponse, Format::OpenAI)
    {
        let model = gojson::gjson_string(fields.get("model"));
        let ctx = cpa_translate::RequestCtx { model: &model, stream };
        if let Ok(converted) = (pair.request)(&ctx, &body) {
            body = Bytes::from(converted);
            stream = peek(&body)
                .get("stream")
                .is_some_and(|v| v.as_bool() == Some(true) || truthy(v));
        }
    }
    let model = gojson::gjson_string(peek(&body).get("model"));
    let query = uri.query().unwrap_or_default().to_owned();
    let path = dispatch::route_path(matched.as_ref(), &uri);
    let req = Request {
        caller,
        peer: dispatch::peer(peer),
        query,
        headers,
        path,
    };
    let alt = alt(&req.query);
    let keepalive = respond::keepalive(&rt.config());
    dispatch::serve(
        &rt,
        call(req, Format::OpenAI, model, body, stream, alt),
        move |result| async move {
            match result {
                Err(failure) => errors::openai(&failure),
                Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
                Ok(Done::Stream { first, rest, .. }) => {
                    respond::sse(respond::stream(first, rest, ChatSse { completions: false }, keepalive))
                }
            }
        },
    )
    .await
}

/// gjson `Bool()` on a non-boolean value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => matches!(s.as_str(), "1" | "t" | "T" | "TRUE" | "true" | "True"),
        _ => false,
    }
}

/// Go `shouldTreatAsResponsesFormat`.
fn responses_shaped(body: &[u8]) -> bool {
    #[derive(serde::Deserialize, Default)]
    struct Keys {
        messages: Option<serde::de::IgnoredAny>,
        input: Option<serde::de::IgnoredAny>,
        instructions: Option<serde::de::IgnoredAny>,
    }
    let keys: Keys = serde_json::from_slice(body).unwrap_or_default();
    keys.messages.is_none() && (keys.input.is_some() || keys.instructions.is_some())
}

pub async fn completions(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => match read_body(&headers, body) {
            Ok(body) => body,
            Err(response) => return *response,
        },
        Err(rejection) => return read_failed(&rejection),
    };
    let root: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let stream = root.get("stream") == Some(&Value::Bool(true));
    let chat = completions_to_chat(&root);
    let model = gojson::gjson_string(root.get("model"));
    let req = Request {
        caller,
        peer: dispatch::peer(peer),
        query: String::new(),
        headers,
        path: dispatch::route_path(matched.as_ref(), &original.0),
    };
    let keepalive = respond::keepalive(&rt.config());
    dispatch::serve(
        &rt,
        call(req, Format::OpenAI, model, Bytes::from(chat), stream, None),
        move |result| async move {
            match result {
                Err(failure) => errors::openai(&failure),
                Ok(Done::Buffered { body, .. }) => {
                    let chat: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    respond::json(200, "application/json", chat_to_completions(&chat))
                }
                Ok(Done::Stream { first, rest, .. }) => {
                    respond::sse(respond::stream(first, rest, ChatSse { completions: true }, keepalive))
                }
            }
        },
    )
    .await
}

/// strconv.FormatFloat(f, 'f', -1, 64).
fn go_float(v: &Value) -> String {
    let f = match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => 0.0,
    };
    let s = format!("{f}");
    s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
}

/// gjson `Int()`.
fn go_int(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
        Value::Bool(b) => i64::from(*b),
        _ => 0,
    }
}

/// Go `convertCompletionsRequestToChatCompletions`.
fn completions_to_chat(root: &Value) -> String {
    let mut prompt = gojson::gjson_string(root.get("prompt"));
    if prompt.is_empty() {
        prompt = "Complete this:".into();
    }
    let model = gojson::gjson_string(root.get("model"));
    let message = Obj::new()
        .str("role", "user")
        .raw("content", &gojson::sjson_string(&prompt))
        .finish();
    let mut out = Obj::new()
        .raw("model", &gojson::sjson_string(&model))
        .raw("messages", &format!("[{message}]"));
    let get = |k: &str| root.get(k);
    if let Some(v) = get("max_tokens") {
        out = out.raw("max_tokens", &go_int(v).to_string());
    }
    for key in ["temperature", "top_p", "frequency_penalty", "presence_penalty"] {
        if let Some(v) = get(key) {
            out = out.raw(key, &go_float(v));
        }
    }
    if let Some(v) = get("stop") {
        out = out.raw("stop", &v.to_string());
    }
    for key in ["stream", "logprobs"] {
        if let Some(v) = get(key) {
            out = out.raw(key, &truthy(v).to_string());
        }
    }
    if let Some(v) = get("top_logprobs") {
        out = out.raw("top_logprobs", &go_int(v).to_string());
    }
    if let Some(v) = get("echo") {
        out = out.raw("echo", &truthy(v).to_string());
    }
    out.finish()
}

fn completion_head(root: &Value) -> Obj {
    Obj::new()
        .raw("id", &gojson::sjson_string(&gojson::gjson_string(root.get("id"))))
        .str("object", "text_completion")
        .raw("created", &root.get("created").map_or(0, go_int).to_string())
        .raw("model", &gojson::sjson_string(&gojson::gjson_string(root.get("model"))))
}

/// Go `convertChatCompletionsResponseToCompletions`.
fn chat_to_completions(root: &Value) -> String {
    let mut choices = Vec::new();
    for choice in root.get("choices").and_then(Value::as_array).into_iter().flatten() {
        let mut c = serde_json::Map::new();
        c.insert("index".into(), choice.get("index").map_or(0, go_int).into());
        let source = choice.get("message").or_else(|| choice.get("delta"));
        if let Some(content) = source.and_then(|m| m.get("content")) {
            c.insert("text".into(), gojson::gjson_string(Some(content)).into());
        }
        if let Some(reason) = choice.get("finish_reason") {
            c.insert("finish_reason".into(), gojson::gjson_string(Some(reason)).into());
        }
        if let Some(logprobs) = choice.get("logprobs") {
            c.insert("logprobs".into(), logprobs.clone());
        }
        choices.push(Value::Object(c));
    }
    let choices = if choices.is_empty() {
        "[]".into()
    } else {
        gojson::sorted(&Value::Array(choices))
    };
    let mut out = completion_head(root).raw("choices", &choices);
    if let Some(usage) = root.get("usage") {
        out = out.raw("usage", &usage.to_string());
    }
    out.finish()
}

/// Go `convertChatCompletionsStreamChunkToCompletions`; `None` drops the chunk.
fn chat_chunk_to_completions(root: &Value) -> Option<String> {
    let chat_choices = root.get("choices").and_then(Value::as_array);
    let has_content = chat_choices.into_iter().flatten().any(|choice| {
        let text = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .map(|c| gojson::gjson_string(Some(c)));
        let reason = choice.get("finish_reason").map(|r| gojson::gjson_string(Some(r)));
        text.is_some_and(|t| !t.is_empty()) || reason.is_some_and(|r| !r.is_empty() && r != "null")
    });
    if !has_content && root.get("usage").is_none() {
        return None;
    }
    let mut choices = Vec::new();
    for choice in chat_choices.into_iter().flatten() {
        let mut c = serde_json::Map::new();
        c.insert("index".into(), choice.get("index").map_or(0, go_int).into());
        let text = choice
            .get("delta")
            .and_then(|d| d.get("content"))
            .map(|c| gojson::gjson_string(Some(c)))
            .unwrap_or_default();
        c.insert("text".into(), text.into());
        if let Some(reason) = choice
            .get("finish_reason")
            .map(|r| gojson::gjson_string(Some(r)))
            .filter(|r| r != "null")
        {
            c.insert("finish_reason".into(), reason.into());
        }
        if let Some(logprobs) = choice.get("logprobs") {
            c.insert("logprobs".into(), logprobs.clone());
        }
        choices.push(Value::Object(c));
    }
    let choices = if choices.is_empty() {
        "[]".into()
    } else {
        gojson::sorted(&Value::Array(choices))
    };
    let mut out = completion_head(root).raw("choices", &choices);
    if let Some(usage) = root.get("usage") {
        out = out.raw("usage", &usage.to_string());
    }
    Some(out.finish())
}

/// Chat Completions SSE: `data:` frames, an error frame, then `data: [DONE]`.
struct ChatSse {
    completions: bool,
}

impl Writer for ChatSse {
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        let frame = respond::ensure_frame(event);
        if !self.completions {
            return vec![frame];
        }
        let Some(payload) = respond::data_payload(&frame) else {
            return Vec::new();
        };
        let Ok(root) = serde_json::from_slice::<Value>(&payload) else {
            return Vec::new();
        };
        chat_chunk_to_completions(&root)
            .map(|c| vec![Bytes::from(format!("data: {c}\n\n"))])
            .unwrap_or_default()
    }

    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        let status = crate::classify::response_status(error);
        let body = errors::openai_body(status, &crate::classify::error_text(error));
        vec![Bytes::from(format!("data: {body}\n\n"))]
    }

    fn end(&mut self) -> Vec<Bytes> {
        vec![Bytes::from_static(b"data: [DONE]\n\n")]
    }
}

pub async fn responses(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => match read_body(&headers, body) {
            Ok(body) => body,
            Err(response) => return *response,
        },
        Err(rejection) => return read_failed(&rejection),
    };
    // Go `prepareCodexMultiAgentV2Tools` then `prepareCodexOrphanDelegation`.
    let settings = cpa_common::codex_client::Settings::for_responses_handler(&rt.config());
    // The spawn_agent model list comes from the Codex client catalog.
    // Cost: one header check, then one `Once` check, per request.
    if cpa_common::codex_client::multi_agent_client(&headers, settings.optimize_multi_agent_v2) {
        crate::model_updater::codex_client_catalog_wanted(&rt);
    }
    let body = Bytes::from(cpa_common::codex_client::prepare_responses_request(
        &headers, &body, &settings,
    ));
    let fields = peek(&body);
    let stream = fields.get("stream") == Some(&Value::Bool(true));
    let model = gojson::gjson_string(fields.get("model"));
    let codex_client = codex_client(&headers);
    let req = Request {
        caller,
        peer: dispatch::peer(peer),
        query: String::new(),
        headers,
        path: dispatch::route_path(matched.as_ref(), &original.0),
    };
    let keepalive = respond::keepalive(&rt.config());
    dispatch::serve(
        &rt,
        call(req, Format::OpenAIResponse, model, body, stream, None),
        move |result| async move {
            match result {
                Err(failure) if stream => {
                    let mut failure = failure;
                    // Go sanitizes initial streaming errors unless they are direct responses.
                    if failure.direct().is_none() {
                        let status = failure.status();
                        let text = sanitize_error_text(status, &failure.text());
                        failure = dispatch::Failure::Exec(ExecError::local(
                            status,
                            cpa_core::exec::FailureScope::Request,
                            text,
                        ));
                    }
                    errors::openai(&failure)
                }
                Err(failure) => errors::openai(&failure),
                Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
                Ok(Done::Stream { first, mut rest, .. }) => {
                    // Go commits only once a data frame (or a terminal error) is ready.
                    let mut framer = ResponsesSse::new(codex_client);
                    let mut ready = first.map(|f| framer.chunk(f)).unwrap_or_default();
                    while framer.data_frames == 0 && framer.terminal_error.is_none() {
                        match futures_util::StreamExt::next(&mut rest).await {
                            Some(Ok(event)) => ready.extend(framer.chunk(event)),
                            Some(Err(error)) => {
                                let status = crate::classify::response_status(&error);
                                let text = sanitize_error_text(status, &crate::classify::error_text(&error));
                                let failure = dispatch::Failure::Exec(ExecError::local(
                                    status,
                                    cpa_core::exec::FailureScope::Request,
                                    text,
                                ));
                                return errors::openai(&failure);
                            }
                            None => {
                                let failure = dispatch::Failure::Exec(ExecError::local(
                                    502,
                                    cpa_core::exec::FailureScope::Request,
                                    "upstream stream closed before first payload",
                                ));
                                return errors::openai(&failure);
                            }
                        }
                    }
                    let finished = framer.stopped();
                    respond::sse(respond::resume(ready, finished, rest, framer, keepalive))
                }
            }
        },
    )
    .await
}

/// Go `isCodexResponsesClientRequest`.
fn codex_client(headers: &HeaderMap) -> bool {
    let get = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let ua = get("user-agent");
    if ua.starts_with("Codex Desktop/")
        || ua.starts_with("codex-tui/")
        || ua == "codex_cli_rs"
        || ua.starts_with("codex_cli_rs/")
        || ua.starts_with("codex_exec/")
    {
        return true;
    }
    let originator = get("originator").to_lowercase();
    matches!(originator.as_str(), "codex desktop" | "codex-tui" | "codex_cli_rs")
        || ["codex desktop/", "codex-tui/", "codex_cli_rs/"]
            .iter()
            .any(|p| originator.starts_with(p))
}

/// Go `responsesStreamErrorText`: sensitive values redacted, long text truncated, JSON
/// errors reduced to their `error` object.
pub(crate) fn sanitize_error_text(status: u16, text: &str) -> String {
    let trimmed = gojson::trim(text);
    let trimmed = if trimmed.is_empty() {
        gojson::status_text(status)
    } else {
        trimmed
    };
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(trimmed) else {
        return truncate(&redact(trimmed), 2048);
    };
    let error = root.get("error").filter(|e| e.is_object()).or_else(|| {
        root.get("response")
            .and_then(|r| r.get("error"))
            .filter(|e| e.is_object())
    });
    if let Some(error) = error {
        let mut out = serde_json::Map::new();
        out.insert("error".into(), sanitize_node(error));
        if let Some(seq) = root.get("sequence_number") {
            out.insert("sequence_number".into(), seq.clone());
        }
        return gojson::sorted(&Value::Object(out));
    }
    gojson::sorted(&sanitize_node(&Value::Object(root)))
}

fn sensitive_key(key: &str) -> bool {
    let k = key.trim().to_lowercase().replace('-', "_");
    if ["tokens", "token_count", "token_limit", "token_usage"]
        .iter()
        .any(|s| k.contains(s))
    {
        return false;
    }
    matches!(
        k.as_str(),
        "authorization"
            | "secret"
            | "password"
            | "passwd"
            | "api_key"
            | "apikey"
            | "token"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "api_token"
            | "client_secret"
            | "client_key"
    ) || ["_secret", "_password", "_api_key", "_token"]
        .iter()
        .any(|s| k.ends_with(s))
}

fn sanitize_node(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(truncate(&redact(s), 2048)),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        if sensitive_key(k) {
                            "[REDACTED]".into()
                        } else {
                            sanitize_node(v)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(sanitize_node).collect()),
        other => other.clone(),
    }
}

fn redact(text: &str) -> String {
    static VALUE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)((?:"?(?:api[_-]?key|access[_-]?token|token|authorization|secret)"?)\s*[=:]\s*"?)([^\s"&,;}]+)"#,
        )
        .unwrap()
    });
    static BEARER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=-]+").unwrap());
    let text = VALUE.replace_all(text, "${1}[REDACTED]");
    BEARER.replace_all(&text, "Bearer [REDACTED]").into_owned()
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.to_owned()
    } else {
        text.chars().take(limit).collect::<String>() + "…"
    }
}

fn stream_error_class(status: u16) -> (&'static str, &'static str) {
    match status {
        401 => ("invalid_api_key", "invalid_request_error"),
        403 => ("insufficient_quota", "invalid_request_error"),
        429 => ("rate_limit_exceeded", "invalid_request_error"),
        404 => ("model_not_found", "invalid_request_error"),
        408 => ("request_timeout", "server_error"),
        500.. => ("internal_server_error", "server_error"),
        400.. => ("invalid_request_error", "invalid_request_error"),
        _ => ("unknown_error", "invalid_request_error"),
    }
}

/// Go `openAIResponsesStreamErrorDetail`.
fn stream_error_detail(status: u16, text: &str) -> Value {
    let (mut code, kind) = stream_error_class(status);
    let mut message = gojson::trim(text).to_owned();
    if message.is_empty() {
        message = gojson::status_text(status).into();
    }
    let payload = serde_json::from_str::<Value>(gojson::trim(text))
        .ok()
        .filter(Value::is_object);
    let code_owned;
    if let Some(p) = &payload {
        if let Some(e) = p.get("error").filter(|e| e.is_object()) {
            return e.clone();
        }
        if let Some(e) = p.get("response").and_then(|r| r.get("error")).filter(|e| e.is_object()) {
            return e.clone();
        }
        if let Some(m) = p
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.trim().is_empty())
        {
            message = m.trim().to_owned();
        }
        if let Some(c) = p.get("code").filter(|c| !c.is_null()) {
            code_owned = gojson::trim(&gojson::gjson_string(Some(c))).to_owned();
            code = &code_owned;
        }
    }
    let mut detail = serde_json::json!({"type": kind, "code": code, "message": message, "param": null});
    if let Some(p) = &payload {
        if let Some(t) = p
            .get("type")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty() && *t != "error")
        {
            detail["type"] = t.into();
        }
        if let Some(param) = p.get("param") {
            detail["param"] = param.clone();
        }
    }
    detail
}

/// gjson `Get(text, "sequence_number").Int()`, when present.
fn sequence_from(text: &str) -> Option<i64> {
    let seq = cpa_common::json::get(text.as_bytes(), "sequence_number");
    seq.exists().then(|| seq.int())
}

/// Go `BuildOpenAIResponsesStreamErrorChunk` / `...FailedChunk` as an SSE frame.
fn stream_error_frame(codex: bool, status: u16, text: &str, seq: i64, lead: bool) -> Bytes {
    let seq = sequence_from(text).unwrap_or(seq).max(0);
    let detail = gojson::sorted(&stream_error_detail(status, text));
    let lead = if lead { "\n" } else { "" };
    if codex {
        let response = Obj::new().str("status", "failed").raw("error", &detail).finish();
        let chunk = Obj::new()
            .str("type", "response.failed")
            .raw("sequence_number", &seq.to_string())
            .raw("response", &response)
            .finish();
        Bytes::from(format!("{lead}event: response.failed\ndata: {chunk}\n\n"))
    } else {
        let chunk = Obj::new()
            .str("type", "error")
            .raw("error", &detail)
            .raw("sequence_number", &seq.to_string())
            .finish();
        Bytes::from(format!("{lead}event: error\ndata: {chunk}\n\n"))
    }
}

/// Go `responsesSSEFramer` over complete frames.
struct ResponsesSse {
    codex: bool,
    data_frames: i64,
    last_event: String,
    terminal_event: String,
    terminal_error: Option<(u16, String)>,
    output: std::collections::BTreeMap<i64, String>,
    unindexed: Vec<String>,
}

impl ResponsesSse {
    fn new(codex: bool) -> Self {
        Self {
            codex,
            data_frames: 0,
            last_event: String::new(),
            terminal_event: String::new(),
            terminal_error: None,
            output: Default::default(),
            unindexed: Vec::new(),
        }
    }

    fn private(&self, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || matches!(name, "response.failed" | "response.error" | "error") {
            return false;
        }
        name.starts_with("responsesapi.")
            || if self.codex {
                name == "codex.rate_limits"
            } else {
                name.starts_with("codex.")
            }
    }

    /// Go `repairFrame`: drops private events, rewrites error payloads, records output
    /// items and restores them into an empty `response.completed` output.
    fn repair(&mut self, frame: Bytes) -> Option<Bytes> {
        use cpa_common::json;
        let event = respond::event_name(&frame);
        if !event.is_empty() && self.private(&event) {
            return None;
        }
        let Some(payload) = respond::data_payload(&frame).filter(|p| !p.is_empty()) else {
            return Some(frame);
        };
        if payload == b"[DONE]" {
            self.data_frames += 1;
            return Some(frame);
        }
        if !json::valid(&payload) {
            return Some(frame);
        }
        let kind = json::get(&payload, "type").str().into_owned();
        if self.private(&event) || self.private(&kind) {
            return None;
        }
        self.data_frames += 1;
        let has_error = ["error", "response.error"].iter().any(|p| {
            let r = json::get(&payload, p);
            r.exists() && r.kind != json::Kind::Null
        }) || (json::get(&payload, "code").exists() && json::get(&payload, "message").exists());
        let error_event = |t: &str| matches!(t, "response.failed" | "response.error" | "error");
        let terminal = |t: &str| {
            matches!(
                t,
                "response.completed"
                    | "response.incomplete"
                    | "response.failed"
                    | "response.done"
                    | "response.error"
                    | "error"
            )
        };
        if error_event(&kind) || has_error {
            if !kind.is_empty() {
                self.last_event = kind.clone();
            }
            return Some(self.error_payload(&payload));
        }
        let event_type = if terminal(&event) || kind.is_empty() {
            event.clone()
        } else {
            kind.clone()
        };
        if !event_type.is_empty() {
            self.last_event = event_type.clone();
        }
        if error_event(&event_type) {
            return Some(self.error_payload(&payload));
        }
        if terminal(&event_type) {
            self.terminal_event = event_type.clone();
        }
        match event_type.as_str() {
            "response.output_item.done" => {
                let item = json::get(&payload, "item");
                if item.is_object() && !item.get("type").str().is_empty() {
                    let raw = String::from_utf8_lossy(item.raw()).into_owned();
                    let index = json::get(&payload, "output_index");
                    if index.exists() {
                        self.output.insert(index.int(), raw);
                    } else {
                        self.unindexed.push(raw);
                    }
                }
            }
            "response.completed" if !self.output.is_empty() || !self.unindexed.is_empty() => {
                let output = json::get(&payload, "response.output");
                if output.exists() && (!output.is_array() || !output.array().is_empty()) {
                    return Some(frame);
                }
                let items: Vec<&str> = self
                    .output
                    .values()
                    .map(String::as_str)
                    .chain(self.unindexed.iter().map(String::as_str))
                    .collect();
                let Ok(repaired) = json::try_set_raw(&payload, "response.output", format!("[{}]", items.join(",")))
                else {
                    return Some(frame);
                };
                if repaired != payload {
                    return Some(frame_with_data(&frame, &repaired));
                }
            }
            _ => {}
        }
        Some(frame)
    }

    /// Go `repairErrorPayload`.
    fn error_payload(&mut self, payload: &[u8]) -> Bytes {
        use cpa_common::json;
        let status = [
            "status",
            "status_code",
            "error.status",
            "error.status_code",
            "response.error.status",
            "response.error.status_code",
        ]
        .iter()
        .map(|p| json::get(payload, p).int())
        .find(|s| (400..=599).contains(s))
        .unwrap_or(502) as u16;
        let text = sanitize_error_text(status, &String::from_utf8_lossy(payload));
        self.terminal_error = Some((status, text.clone()));
        self.terminal_event = if self.codex {
            "response.failed".into()
        } else {
            "error".into()
        };
        let own = json::get(payload, "sequence_number");
        let seq = if own.exists() {
            own.int()
        } else {
            sequence_from(&text).unwrap_or((self.data_frames - 1).max(0))
        };
        stream_error_frame(self.codex, status, &text, seq, false)
    }
}

/// Go `responsesSSEFrameWithData`: the frame's non-data lines, then the payload's lines
/// as `data:` lines.
fn frame_with_data(frame: &[u8], payload: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(frame.len() + payload.len());
    for line in frame.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let trimmed = line.trim_ascii();
        if trimmed.is_empty() || trimmed.starts_with(b"data:") {
            continue;
        }
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    for line in payload.split(|&b| b == b'\n') {
        out.extend_from_slice(b"data: ");
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out.push(b'\n');
    Bytes::from(out)
}

impl Writer for ResponsesSse {
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        if !self.terminal_event.is_empty() {
            return Vec::new();
        }
        self.repair(respond::ensure_frame(event)).into_iter().collect()
    }

    fn stopped(&self) -> bool {
        self.terminal_error.is_some()
    }

    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        if !self.terminal_event.is_empty() {
            return Vec::new();
        }
        let status = crate::classify::response_status(error);
        let text = sanitize_error_text(status, &crate::classify::error_text(error));
        vec![stream_error_frame(self.codex, status, &text, self.data_frames, true)]
    }

    fn end(&mut self) -> Vec<Bytes> {
        if self.terminal_error.is_some() || !self.terminal_event.is_empty() {
            return vec![Bytes::from_static(b"\n")];
        }
        let last = if self.last_event.is_empty() {
            "none"
        } else {
            &self.last_event
        };
        let text = format!("upstream stream closed before a terminal event (last event: {last})");
        vec![stream_error_frame(self.codex, 502, &text, self.data_frames, true)]
    }
}

pub async fn compact(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    original: OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let body = match body {
        Ok(body) => match read_body(&headers, body) {
            Ok(body) => body,
            Err(response) => return *response,
        },
        Err(rejection) => return read_failed(&rejection),
    };
    // Go `prepareCodexOrphanDelegation` (compact skips the multi-agent tool step).
    let settings = cpa_common::codex_client::Settings::for_responses_handler(&rt.config());
    let mut body = Bytes::from(cpa_common::codex_client::rewrite_orphan_delegation_input(
        &headers,
        &body,
        settings.orphan_delegation,
    ));
    let fields = peek(&body);
    match fields.get("stream") {
        Some(Value::Bool(true)) => {
            return respond::error_detail(
                400,
                "Streaming not supported for compact responses",
                "invalid_request_error",
            );
        }
        Some(_) => {
            if let Ok(updated) = cpa_common::json::try_delete(&body, "stream") {
                body = Bytes::from(updated);
            }
        }
        None => {}
    }
    let model = gojson::gjson_string(fields.get("model"));
    let req = Request {
        caller,
        peer: dispatch::peer(peer),
        query: String::new(),
        headers,
        path: dispatch::route_path(matched.as_ref(), &original.0),
    };
    let call = call(
        req,
        Format::OpenAIResponse,
        model,
        body,
        false,
        Some("responses/compact".into()),
    );
    dispatch::serve(&rt, call, |result| async move {
        match result {
            Err(failure) => errors::openai(&failure),
            Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
            Ok(Done::Stream { .. }) => unreachable!("non-stream calls are buffered"),
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_request_conversion_matches_go() {
        let root: Value = serde_json::from_str(
            r#"{"model":"m","prompt":"hi <b>","max_tokens":5.7,"temperature":1,"top_p":0.25,"stop":["\n"],"stream":true,"echo":0}"#,
        )
        .unwrap();
        assert_eq!(
            completions_to_chat(&root),
            r#"{"model":"m","messages":[{"role":"user","content":"hi <b>"}],"max_tokens":5,"temperature":1,"top_p":0.25,"stop":["\n"],"stream":true,"echo":false}"#
        );
        let empty: Value = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert!(completions_to_chat(&empty).contains(r#""content":"Complete this:""#));
    }

    #[test]
    fn completions_response_conversion_matches_go() {
        let chat: Value = serde_json::from_str(
            r#"{"id":"c1","created":7,"model":"m","choices":[{"index":0,"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"total_tokens":3}}"#,
        )
        .unwrap();
        assert_eq!(
            chat_to_completions(&chat),
            r#"{"id":"c1","object":"text_completion","created":7,"model":"m","choices":[{"finish_reason":"stop","index":0,"text":"ok"}],"usage":{"total_tokens":3}}"#
        );
        let role_only: Value = serde_json::from_str(
            r#"{"id":"c","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert_eq!(chat_chunk_to_completions(&role_only), None);
        let delta: Value = serde_json::from_str(r#"{"id":"c","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"x"},"finish_reason":null}]}"#).unwrap();
        assert_eq!(
            chat_chunk_to_completions(&delta).unwrap(),
            r#"{"id":"c","object":"text_completion","created":1,"model":"m","choices":[{"finish_reason":"","index":0,"text":"x"}]}"#
        );
    }

    #[test]
    fn responses_framer_repairs_errors_and_requires_terminal_events() {
        let mut f = ResponsesSse::new(false);
        assert!(
            f.chunk(Bytes::from_static(b"event: codex.rate_limits\ndata: {}\n\n"))
                .is_empty()
        );
        let out = f.chunk(Bytes::from_static(
            b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        ));
        assert_eq!(out.len(), 1);
        let out = f.chunk(Bytes::from_static(
            b"data: {\"type\":\"error\",\"error\":{\"message\":\"bad\",\"code\":\"x\"},\"sequence_number\":4}\n\n",
        ));
        assert_eq!(
            String::from_utf8(out[0].to_vec()).unwrap(),
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"x\",\"message\":\"bad\"},\"sequence_number\":4}\n\n"
        );
        assert!(f.stopped());
        let mut g = ResponsesSse::new(true);
        g.chunk(Bytes::from_static(
            b"data: {\"type\":\"response.output_text.delta\"}\n\n",
        ));
        let end = String::from_utf8(g.end()[0].to_vec()).unwrap();
        assert!(end.starts_with("\nevent: response.failed\n"), "{end}");
        assert!(
            end.contains("upstream stream closed before a terminal event (last event: response.output_text.delta)")
        );
        let mut h = ResponsesSse::new(false);
        h.chunk(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\n",
        ));
        let done = h.chunk(Bytes::from_static(
            b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[]}}\n\n",
        ));
        assert_eq!(
            String::from_utf8(done[0].to_vec()).unwrap(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\"}]}}\n\n"
        );
        assert_eq!(h.end(), [Bytes::from_static(b"\n")]);

        // Go splices the recorded raw items with sjson: every other byte is kept.
        let mut k = ResponsesSse::new(false);
        k.chunk(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":\"1\",\"item\":{\"type\": \"message\", \"id\":\"m\\u00e9\"}}\n\n",
        ));
        k.chunk(Bytes::from_static(
            b"data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\"}}\n\n",
        ));
        let done = k.chunk(Bytes::from_static(
            b"event: response.completed\r\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\\u003c1\",\"usage\":{\"x\":1.50}},\"sequence_number\":7}\r\n\r\n",
        ));
        assert_eq!(
            String::from_utf8(done[0].to_vec()).unwrap(),
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\\u003c1\",\"usage\":{\"x\":1.50},\"output\":[{\"type\":\"reasoning\"},{\"type\": \"message\", \"id\":\"m\\u00e9\"}]},\"sequence_number\":7}\n\n"
        );
    }

    #[test]
    fn stream_error_sanitizing_redacts_and_reduces() {
        assert_eq!(
            sanitize_error_text(
                401,
                r#"{"error":{"message":"bad Bearer abc.def","api_key":"k"},"other":1}"#
            ),
            r#"{"error":{"api_key":"[REDACTED]","message":"bad Bearer [REDACTED]"}}"#
        );
        assert_eq!(sanitize_error_text(500, "token=abc rest"), "token=[REDACTED] rest");
    }
}
