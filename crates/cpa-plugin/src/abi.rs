//! `sdk/pluginabi`: ABI and schema versions, RPC method names and the JSON envelope.

use bytes::Bytes;

use crate::gojson::{self, DecodeError, GoJson, Node, ObjWriter};

/// Native C ABI shape (`cliproxy_plugin_init` and the two function tables).
pub const ABI_VERSION: u32 = 1;
/// RPC JSON contract version the host speaks at `plugin.register`.
///
/// 2 adds request lifecycle completion and termination; 3 omits `OriginalRequest` and
/// `RequestBody` on payload stream chunks; 4 adds WebSocket response observation; 5 omits
/// `HistoryChunks` on payload stream chunks; 6 keeps plugin management JSON responses
/// raw instead of HTML-escaping their strings.
pub const SCHEMA_VERSION: u32 = 6;
pub const SCHEMA_STREAM_CHUNK_OMIT_REQUEST_BODY: u32 = 3;
pub const SCHEMA_WEBSOCKET_RESPONSE_OBSERVER: u32 = 4;
pub const SCHEMA_STREAM_CHUNK_OMIT_HISTORY: u32 = 5;
pub const SCHEMA_RAW_MANAGEMENT_RESPONSE: u32 = 6;

pub mod method {
    pub const PLUGIN_REGISTER: &str = "plugin.register";
    pub const PLUGIN_QUIESCE: &str = "plugin.quiesce";
    pub const PLUGIN_RECONFIGURE: &str = "plugin.reconfigure";
    pub const PLUGIN_SHUTDOWN: &str = "plugin.shutdown";

    pub const MODEL_REGISTER: &str = "model.register";
    pub const MODEL_STATIC: &str = "model.static";
    pub const MODEL_FOR_AUTH: &str = "model.for_auth";

    pub const AUTH_IDENTIFIER: &str = "auth.identifier";
    pub const AUTH_PARSE: &str = "auth.parse";
    pub const AUTH_LOGIN_START: &str = "auth.login.start";
    pub const AUTH_LOGIN_POLL: &str = "auth.login.poll";
    pub const AUTH_REFRESH: &str = "auth.refresh";

    pub const FRONTEND_AUTH_IDENTIFIER: &str = "frontend_auth.identifier";
    pub const FRONTEND_AUTH_AUTHENTICATE: &str = "frontend_auth.authenticate";

    pub const SCHEDULER_PICK: &str = "scheduler.pick";
    pub const MODEL_ROUTE: &str = "model.route";

    pub const EXECUTOR_IDENTIFIER: &str = "executor.identifier";
    pub const EXECUTOR_EXECUTE: &str = "executor.execute";
    pub const EXECUTOR_EXECUTE_STREAM: &str = "executor.execute_stream";
    pub const EXECUTOR_COUNT_TOKENS: &str = "executor.count_tokens";
    pub const EXECUTOR_HTTP_REQUEST: &str = "executor.http_request";

    pub const REQUEST_TRANSLATE: &str = "request.translate";
    pub const REQUEST_NORMALIZE: &str = "request.normalize";
    pub const REQUEST_INTERCEPT_BEFORE: &str = "request.intercept_before";
    pub const REQUEST_INTERCEPT_AFTER: &str = "request.intercept_after";
    pub const REQUEST_COMPLETE: &str = "request.complete";

    pub const RESPONSE_TRANSLATE: &str = "response.translate";
    pub const RESPONSE_NORMALIZE_BEFORE: &str = "response.normalize_before";
    pub const RESPONSE_NORMALIZE_AFTER: &str = "response.normalize_after";
    pub const RESPONSE_INTERCEPT_AFTER: &str = "response.intercept_after";
    pub const RESPONSE_INTERCEPT_STREAM_CHUNK: &str = "response.intercept_stream_chunk";

    pub const WEBSOCKET_RESPONSE_EVENT: &str = "websocket.response_event";

    pub const THINKING_IDENTIFIER: &str = "thinking.identifier";
    pub const THINKING_APPLY: &str = "thinking.apply";

    pub const USAGE_HANDLE: &str = "usage.handle";

    pub const COMMAND_LINE_REGISTER: &str = "command_line.register";
    pub const COMMAND_LINE_EXECUTE: &str = "command_line.execute";

    pub const MANAGEMENT_REGISTER: &str = "management.register";
    pub const MANAGEMENT_HANDLE: &str = "management.handle";

    pub const QUOTA_IDENTIFIER: &str = "quota.identifier";
    pub const QUOTA_DESCRIBE: &str = "quota.describe";
    pub const QUOTA_FETCH: &str = "quota.fetch";
    pub const QUOTA_RESET: &str = "quota.reset";

    pub const HOST_HTTP_DO: &str = "host.http.do";
    pub const HOST_HTTP_DO_STREAM: &str = "host.http.do_stream";
    pub const HOST_HTTP_OPERATION_OPEN: &str = "host.http.operation_open";
    pub const HOST_HTTP_CANCEL: &str = "host.http.cancel";
    pub const HOST_HTTP_STREAM_READ: &str = "host.http.stream_read";
    pub const HOST_HTTP_STREAM_CLOSE: &str = "host.http.stream_close";
    pub const HOST_MODEL_EXECUTE: &str = "host.model.execute";
    pub const HOST_MODEL_EXECUTE_STREAM: &str = "host.model.execute_stream";
    pub const HOST_MODEL_STREAM_READ: &str = "host.model.stream_read";
    pub const HOST_MODEL_STREAM_CLOSE: &str = "host.model.stream_close";
    pub const HOST_STREAM_EMIT: &str = "host.stream.emit";
    pub const HOST_STREAM_CLOSE: &str = "host.stream.close";
    pub const HOST_LOG: &str = "host.log";
    pub const HOST_AUTH_LIST: &str = "host.auth.list";
    pub const HOST_AUTH_GET: &str = "host.auth.get";
    pub const HOST_AUTH_GET_RUNTIME: &str = "host.auth.get_runtime";
    pub const HOST_AUTH_SAVE: &str = "host.auth.save";
    pub const HOST_AFFINITY_LOOKUP: &str = "host.affinity.lookup";
}

/// `pluginabi.Error`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvelopeError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    /// HTTP status to surface to the client; 0 means 500.
    pub http_status: i64,
}

crate::go_struct! {
    /// `pluginabi.Error` on the wire.
    pub struct WireError("pluginabi.Error") {
        "code" => code: String,
        "message" => message: String,
        "retryable" omitempty => retryable: bool,
        "http_status" omitempty => http_status: i64,
    }
}

/// `pluginabi.Envelope` with the result kept as a parsed document (Go keeps it as a
/// `json.RawMessage` and decodes it into the target type afterwards).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Envelope {
    pub ok: bool,
    pub result: Option<Node>,
    pub error: Option<EnvelopeError>,
}

impl Envelope {
    /// Parses a plugin response envelope member by member, as `json.Unmarshal` does:
    /// keys match case-insensitively, later members decode over earlier ones, `null`
    /// leaves `ok` alone and clears `error`.
    pub fn parse(raw: &[u8]) -> Result<Self, DecodeError> {
        let node = gojson::parse(raw)?;
        let Node::Object(members) = &node else {
            if node.is_null() {
                return Ok(Self::default());
            }
            return Err(gojson::type_error(&node, "pluginabi.Envelope"));
        };
        let mut out = Envelope::default();
        let mut wire: Option<WireError> = None;
        let mut first_err = None;
        for (key, value) in members {
            let result = if key.eq_ignore_ascii_case("ok") {
                out.ok
                    .decode_into(value)
                    .map_err(|e| gojson::in_field(e, "Envelope", "ok"))
            } else if key.eq_ignore_ascii_case("result") {
                out.result = Some(value.clone());
                Ok(())
            } else if key.eq_ignore_ascii_case("error") {
                wire.decode_into(value)
                    .map_err(|e| gojson::in_field(e, "Envelope", "error"))
            } else {
                Ok(())
            };
            if let Err(e) = result {
                first_err.get_or_insert(e);
            }
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        out.error = wire.map(|wire| EnvelopeError {
            code: wire.code,
            message: wire.message,
            retryable: wire.retryable,
            http_status: wire.http_status,
        });
        Ok(out)
    }

    /// Go `isPluginErrorEnvelope`.
    pub fn is_error(raw: &[u8]) -> bool {
        Self::parse(raw).is_ok_and(|e| !e.ok && e.error.is_some())
    }
}

/// `marshalRPCResult`: `{"ok":true,"result":<json>}`.
pub fn ok_envelope<T: GoJson>(result: &T) -> Bytes {
    ok_envelope_raw(&gojson::to_vec(result))
}

/// `marshalRPCEnvelope` around an already encoded result.
pub fn ok_envelope_raw(result: &[u8]) -> Bytes {
    let mut out = Vec::with_capacity(result.len() + 24);
    let mut w = ObjWriter::begin(&mut out);
    w.field("ok", &true, false);
    w.field(
        "result",
        &gojson::RawJson(Bytes::copy_from_slice(if result.is_empty() { b"{}" } else { result })),
        true,
    );
    w.end();
    Bytes::from(out)
}

/// `marshalRPCError` / `pluginabi.NewErrorEnvelope`.
pub fn error_envelope(code: &str, message: &str, http_status: u16) -> Bytes {
    let mut out = Vec::new();
    let mut w = ObjWriter::begin(&mut out);
    w.field("ok", &false, false);
    let error = WireError {
        code: code.into(),
        message: message.into(),
        retryable: false,
        http_status: i64::from(http_status),
    };
    w.field("error", &error, false);
    w.end();
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_match_go_bytes() {
        assert_eq!(&ok_envelope_raw(b"{}")[..], br#"{"ok":true,"result":{}}"#);
        assert_eq!(
            &error_envelope("host_call_failed", "a<b", 0)[..],
            br#"{"ok":false,"error":{"code":"host_call_failed","message":"a\u003cb"}}"#
        );
        assert_eq!(
            &error_envelope("x", "y", 429)[..],
            br#"{"ok":false,"error":{"code":"x","message":"y","http_status":429}}"#
        );
        let parsed = Envelope::parse(br#"{"OK":false,"Error":{"Code":"c","Message":"m","HTTP_STATUS":401}}"#).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.error.unwrap().http_status, 401);
        // Go: a later null leaves the bool alone; an overflowing number in the raw result
        // is only a problem once the result is decoded.
        assert!(
            Envelope::parse(br#"{"ok":true,"ok":null,"result":{"x":1e400}}"#)
                .unwrap()
                .ok
        );
        let merged = Envelope::parse(br#"{"error":{"code":"a"},"error":{"message":"b"}}"#).unwrap();
        let error = merged.error.unwrap();
        assert_eq!((error.code.as_str(), error.message.as_str()), ("a", "b"));
    }
}
