//! Gemini routes under `/v1beta` (sdk/api/handlers/gemini/gemini_handlers.go,
//! interactions_handlers.go).

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{MatchedPath, OriginalUri, Path, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use cpa_core::exec::{Caller, ExecError, Operation};
use cpa_core::format::Format;
use serde_json::Value;

use crate::claude::alt;
use crate::dispatch::{self, Call, Done};
use crate::respond::{self, Writer};
use crate::{Runtime, errors, gojson};

#[allow(clippy::too_many_arguments)]
pub async fn action(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    Path(action): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let action = action.trim_start_matches('/');
    let parts: Vec<&str> = action.split(':').collect();
    if parts.len() != 2 {
        return respond::error_detail(404, &format!("{} not found.", uri.path()), "invalid_request_error");
    }
    let body = body.unwrap_or_default();
    let alt = alt(uri.query().unwrap_or_default());
    let (operation, stream) = match parts[1] {
        "generateContent" => (Operation::Generate, false),
        "streamGenerateContent" => (Operation::Generate, true),
        "countTokens" => (Operation::CountTokens, false),
        // Go answers any other method with an empty 200.
        _ => return ().into_response(),
    };
    let call = Call {
        entry: Format::Gemini,
        response: Format::Gemini,
        operation,
        model: parts[0].to_owned(),
        body,
        stream,
        alt: alt.clone(),
        headers,
        caller,
        forced_provider: None,
        selection_model: None,
        execution_session: None,
        request_path: dispatch::route_path(matched.as_ref(), &uri),
        peer: dispatch::peer(peer),
        turn: None,
        media: None,
    };
    let keepalive = respond::keepalive(&rt.config()).filter(|_| alt.is_none());
    dispatch::serve(&rt, call, move |result| async move {
        match result {
            Err(failure) => errors::openai(&failure),
            Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
            Ok(Done::Stream { first, rest, .. }) => {
                let raw = alt.is_some();
                let body = respond::stream(first, rest, GeminiSse { raw }, keepalive);
                if raw { body.into_response() } else { respond::sse(body) }
            }
        }
    })
    .await
}

/// Gemini streaming: SSE `data:` frames, or raw payloads when `alt` is set.
struct GeminiSse {
    raw: bool,
}

impl Writer for GeminiSse {
    /// The Gemini executor emits exact client bytes for the requested `alt`.
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        vec![event]
    }

    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        let status = crate::classify::response_status(error);
        let body = errors::openai_body(status, &crate::classify::error_text(error));
        vec![if self.raw {
            Bytes::from(body)
        } else {
            Bytes::from(format!("event: error\ndata: {body}\n\n"))
        }]
    }

    fn end(&mut self) -> Vec<Bytes> {
        Vec::new()
    }
}

/// The auth-selection model Go uses for Interactions agents.
const AGENT_SELECTION_MODEL: &str = "gemini-2.5-flash";

pub async fn interactions(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let mut body = match body {
        Ok(body) => body,
        Err(rejection) => return respond::error_detail(400, &rejection.body_text(), "invalid_request_error"),
    };
    let invalid = |m: &str| respond::error_detail(400, m, "invalid_request_error");
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(&body) else {
        if serde_json::from_slice::<Value>(&body).is_ok() {
            return invalid("request requires exactly one of model or agent");
        }
        return invalid("invalid JSON body");
    };
    let model = gojson::trim(&gojson::gjson_string(root.get("model"))).to_owned();
    let agent = gojson::trim(&gojson::gjson_string(root.get("agent"))).to_owned();
    if model.is_empty() == agent.is_empty() {
        return invalid("request requires exactly one of model or agent");
    }
    let stream = match root.get("stream") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return invalid("stream must be a boolean"),
    };
    let (target, forced, selection) = if agent.is_empty() {
        let normalized = model
            .strip_prefix("models/")
            .filter(|m| !m.is_empty())
            .unwrap_or(&model)
            .to_owned();
        if normalized != model
            && let Some(updated) = cpa_common::json::try_set_str(&body, "model", &normalized).ok()
        {
            body = Bytes::from(updated);
        }
        (normalized, None, None)
    } else {
        (
            agent,
            Some("gemini-interactions".to_owned()),
            Some(AGENT_SELECTION_MODEL.to_owned()),
        )
    };
    let call = Call {
        entry: Format::Interactions,
        response: Format::Interactions,
        operation: Operation::Generate,
        model: target,
        body,
        stream,
        alt: alt(uri.query().unwrap_or_default()),
        headers,
        caller,
        forced_provider: forced,
        selection_model: selection,
        execution_session: None,
        request_path: dispatch::route_path(matched.as_ref(), &uri),
        peer: dispatch::peer(peer),
        turn: None,
        media: None,
    };
    let keepalive = respond::keepalive(&rt.config());
    dispatch::serve(&rt, call, move |result| async move {
        match result {
            Err(failure) => errors::openai(&failure),
            Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
            Ok(Done::Stream { first, rest, .. }) => {
                respond::sse(respond::stream(first, rest, InteractionsSse, keepalive))
            }
        }
    })
    .await
}

struct InteractionsSse;

impl Writer for InteractionsSse {
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        if event.is_empty() {
            return Vec::new();
        }
        let trimmed = event.trim_ascii();
        let mut out = Vec::with_capacity(event.len() + 8);
        if !trimmed.starts_with(b"event:") && !trimmed.starts_with(b"data:") {
            out.extend_from_slice(b"data: ");
        }
        out.extend_from_slice(&event);
        if !event.ends_with(b"\n\n") {
            out.extend_from_slice(b"\n\n");
        }
        vec![Bytes::from(out)]
    }

    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        let status = crate::classify::response_status(error);
        let body = errors::openai_body(status, &crate::classify::error_text(error));
        vec![Bytes::from(format!("event: error\ndata: {body}\n\n"))]
    }

    fn end(&mut self) -> Vec<Bytes> {
        Vec::new()
    }
}
