//! Anthropic Messages routes (sdk/api/handlers/claude/code_handlers.go).

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

use crate::dispatch::{self, Call, Done};
use crate::respond::{self, Writer};
use crate::{Runtime, errors, gojson};

const DD_PREFIX: &str = "claude-fable-5-dd-";

/// Go `ResolveClaudeModelIDPrefix`: `claude-fable-5-dd-<reversed>` names the reversed
/// model, which the Anthropic catalog advertises for non-Claude IDs.
pub fn resolve_dd(id: &str) -> String {
    let (base, suffix) = match id.rfind('(') {
        Some(i) if id.ends_with(')') => (&id[..i], Some(&id[i + 1..id.len() - 1])),
        _ => (id, None),
    };
    let Some(encoded) = base.strip_prefix(DD_PREFIX).filter(|e| !e.is_empty()) else {
        return id.to_owned();
    };
    let resolved: String = encoded.chars().rev().collect();
    match suffix {
        Some(s) => format!("{resolved}({s})"),
        None => resolved,
    }
}

/// Go `EnsureClaudeModelIDPrefix`.
pub fn ensure_dd(id: &str) -> String {
    if id.is_empty() || id.starts_with("claude-") {
        id.to_owned()
    } else {
        format!("{DD_PREFIX}{}", id.chars().rev().collect::<String>())
    }
}

pub async fn messages(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(
        rt,
        caller,
        dispatch::peer(peer),
        &uri,
        matched.as_ref(),
        headers,
        body,
        Operation::Generate,
    )
    .await
}

pub async fn count_tokens(
    State(rt): State<Arc<Runtime>>,
    Extension(caller): Extension<Caller>,
    peer: dispatch::Peer,
    matched: Option<MatchedPath>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    handle(
        rt,
        caller,
        dispatch::peer(peer),
        &uri,
        matched.as_ref(),
        headers,
        body,
        Operation::CountTokens,
    )
    .await
}

/// Go `GetAlt`: `alt` when present (even empty), else `$alt`; `sse` and the empty
/// string both mean none, as Go's `""`.
pub fn alt(query: &str) -> Option<String> {
    let value = crate::access::query_get(query, "alt").or_else(|| crate::access::query_get(query, "$alt"))?;
    let value = String::from_utf8_lossy(&value).into_owned();
    (!value.is_empty() && value != "sse").then_some(value)
}

/// Reads the top-level fields a route needs without materializing the rest.
pub fn peek(body: &[u8]) -> serde_json::Map<String, Value> {
    #[derive(serde::Deserialize, Default)]
    struct Peek {
        #[serde(default)]
        model: Option<Value>,
        #[serde(default)]
        stream: Option<Value>,
    }
    let peek: Peek = serde_json::from_slice(body).unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(m) = peek.model {
        out.insert("model".into(), m);
    }
    if let Some(s) = peek.stream {
        out.insert("stream".into(), s);
    }
    out
}

/// gin `GetRawData` failure: 400 `Invalid request: ...`.
pub fn read_failed(rejection: &BytesRejection) -> Response {
    respond::error_detail(
        400,
        &format!("Invalid request: {}", rejection.body_text()),
        "invalid_request_error",
    )
}

#[allow(clippy::too_many_arguments)]
async fn handle(
    rt: Arc<Runtime>,
    caller: Caller,
    peer: Option<std::net::SocketAddr>,
    uri: &axum::http::Uri,
    matched: Option<&MatchedPath>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
    operation: Operation,
) -> Response {
    // Latency marks for examples/claude_latency.rs; free unless a subscriber enables them.
    tracing::trace!(target: "cpa_latency", stage = "handler");
    let mut body = match body {
        Ok(body) => body,
        Err(rejection) => return read_failed(&rejection),
    };
    let fields = peek(&body);
    let raw_model = gojson::gjson_string(fields.get("model"));
    let resolved = resolve_dd(&raw_model);
    if resolved != raw_model
        && let Some(updated) = cpa_common::json::try_set_str(&body, "model", &resolved).ok()
    {
        body = Bytes::from(updated);
    }
    // Go: anything but a missing or literal-false `stream` takes the streaming path.
    let stream = operation == Operation::Generate && !matches!(fields.get("stream"), None | Some(Value::Bool(false)));
    let call = Call {
        entry: Format::Claude,
        response: Format::Claude,
        operation,
        model: resolved,
        body,
        stream,
        // Go passes no alt to the streaming path.
        alt: if stream {
            None
        } else {
            alt(uri.query().unwrap_or_default())
        },
        headers,
        caller,
        forced_provider: None,
        selection_model: None,
        execution_session: None,
        request_path: dispatch::route_path(matched, uri),
        peer,
        turn: None,
        media: None,
    };
    let keepalive = respond::keepalive(&rt.config());
    dispatch::serve(&rt, call, move |result| async move {
        match result {
            Err(failure) => errors::claude(&failure),
            Ok(Done::Buffered { body, .. }) => respond::json(200, "application/json", body),
            Ok(Done::Stream { first, rest, .. }) => respond::sse(respond::stream(first, rest, ClaudeSse, keepalive)),
        }
    })
    .await
}

struct ClaudeSse;

impl Writer for ClaudeSse {
    fn chunk(&mut self, event: Bytes) -> Vec<Bytes> {
        vec![event]
    }

    fn error(&mut self, error: &ExecError) -> Vec<Bytes> {
        let status = crate::classify::response_status(error);
        let body = errors::claude_body(status, &crate::classify::error_text(error));
        vec![Bytes::from(format!("event: error\ndata: {body}\n\n"))]
    }

    fn end(&mut self) -> Vec<Bytes> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `GetAlt` goldens (tests/reference/server/main.go `alts`): an empty `alt`
    /// stops the `$alt` fallback and means none.
    #[test]
    fn alt_matches_go() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/server_go.json")).unwrap();
        let cases = fixture["alt"].as_array().unwrap();
        assert_eq!(cases.len(), 13);
        for case in cases {
            let want = case["out"].as_str().unwrap();
            let got = alt(case["in"].as_str().unwrap());
            assert_eq!(got.as_deref().unwrap_or(""), want, "query {}", case["in"]);
            assert_eq!(got.is_some(), !want.is_empty());
        }
    }

    #[test]
    fn dd_model_ids_round_trip_like_go() {
        assert_eq!(ensure_dd("gpt-5.5"), "claude-fable-5-dd-5.5-tpg");
        assert_eq!(ensure_dd("claude-opus-5"), "claude-opus-5");
        assert_eq!(resolve_dd("claude-fable-5-dd-5.5-tpg(high)"), "gpt-5.5(high)");
        assert_eq!(resolve_dd("claude-fable-5-dd-"), "claude-fable-5-dd-");
        assert_eq!(resolve_dd("claude-opus-5"), "claude-opus-5");
    }

    #[test]
    fn peek_fields_are_independent() {
        let fields = peek(br#"{"model":" claude-opus-5 ","stream":"yes","x":[1]}"#);
        assert_eq!(gojson::gjson_string(fields.get("model")), " claude-opus-5 ");
        assert_eq!(fields.get("stream"), Some(&Value::String("yes".into())));
        assert!(peek(b"not json").is_empty());
    }
}
