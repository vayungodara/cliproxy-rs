//! Request, response, stream-chunk, lifecycle and WebSocket hooks
//! (internal/pluginhost/adapters_interceptors.go).
//!
//! Interceptors run in snapshot order (priority, then ID). Each sees the previous one's
//! headers and body; a failing interceptor is skipped. `skip` is the plugin that started
//! a nested host execution (Go's `*Except` variants), which must not intercept itself.

use bytes::Bytes;

use crate::abi::{self, method};
use crate::api::{
    RequestCompletion, RequestInterceptRequest, RequestInterceptResponse, ResponseInterceptRequest,
    ResponseInterceptResponse, STREAM_CHUNK_HEADER_INIT_INDEX, StreamChunkInterceptRequest,
    StreamChunkInterceptResponse, WebSocketResponseEvent,
};
use crate::callbacks::RequestScope;
use crate::gojson::Header;
use crate::host::{Host, Record};
use crate::rpc::Empty;

/// Go `coreexecutor.RequestPathMetadataKey`.
pub const REQUEST_PATH_METADATA_KEY: &str = "request_path";

/// `http.Header.Del` then `Add` for every update, after deleting `clear`
/// (Go `mergeHeaders`). Keys are canonicalized the way `http.Header` methods do; keys
/// already in `current` keep their spelling.
pub fn merge_headers(current: &Header, updates: &Header, clear: &[String]) -> Header {
    let mut out = current.clone();
    for key in clear {
        out.remove(&cpa_exec::proxy::canonical_header(key));
    }
    for (key, values) in updates {
        let key = cpa_exec::proxy::canonical_header(key);
        out.remove(&key);
        if !values.is_empty() {
            out.entry(key).or_default().extend(values.iter().cloned());
        }
    }
    out
}

impl Host {
    /// Records with a hook, except `skip`; each call rechecks [`Host::live`].
    fn interceptor_records(&self, skip: &str, has: impl Fn(&Record) -> bool) -> Vec<Record> {
        let skip = skip.trim();
        self.active_records()
            .into_iter()
            .filter(|r| has(r) && r.id != skip)
            .collect()
    }

    /// Go `InterceptRequestBeforeAuth[Except]`.
    pub async fn intercept_request_before_auth(
        &self,
        req: RequestInterceptRequest,
        skip: &str,
        scope: &RequestScope,
    ) -> RequestInterceptResponse {
        self.intercept_request(req, method::REQUEST_INTERCEPT_BEFORE, skip, scope)
            .await
    }

    /// Go `InterceptRequestAfterAuth[Except]`.
    pub async fn intercept_request_after_auth(
        &self,
        req: RequestInterceptRequest,
        skip: &str,
        scope: &RequestScope,
    ) -> RequestInterceptResponse {
        self.intercept_request(req, method::REQUEST_INTERCEPT_AFTER, skip, scope)
            .await
    }

    /// Go `interceptRequest`: the result carries the merged headers, the body only when
    /// an interceptor replaced it, the last path override, and the first termination.
    async fn intercept_request(
        &self,
        req: RequestInterceptRequest,
        method: &str,
        skip: &str,
        scope: &RequestScope,
    ) -> RequestInterceptResponse {
        let mut current = RequestInterceptResponse {
            headers: req.headers.clone(),
            ..Default::default()
        };
        let mut body = req.body.clone();
        let mut body_modified = false;
        for record in self.interceptor_records(skip, |r| r.plugin.caps.request_interceptor) {
            if !self.live(&record) {
                continue;
            }
            let mut next = req.clone();
            next.headers = current.headers.clone();
            next.body = body.clone();
            if !current.path.is_empty() {
                next.metadata
                    .insert(REQUEST_PATH_METADATA_KEY.into(), current.path.clone().into());
            }
            let resp: RequestInterceptResponse = match self.call_with_callback(&record, method, &next, scope).await {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!("pluginhost: request interceptor {} failed: {e}", record.id);
                    continue;
                }
            };
            current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
            if !resp.body.is_empty() {
                body = resp.body.clone();
                body_modified = true;
            }
            if !resp.path.trim().is_empty() {
                current.path = resp.path.trim().to_owned();
            }
            if resp.terminate {
                current.terminate = true;
                current.status_code = resp.status_code;
                current.response_headers = resp.response_headers;
                current.response_body = resp.response_body;
                break;
            }
        }
        if body_modified {
            current.body = body;
        }
        current
    }

    /// Go `CompleteRequest[Except]`: one asynchronous `request.complete` per lifecycle
    /// plugin; delivery never blocks the response.
    pub fn complete_request(&self, completion: RequestCompletion, skip: &str, scope: &RequestScope) {
        for record in self.interceptor_records(skip, |r| r.plugin.caps.request_lifecycle_plugin) {
            if !self.live(&record) {
                continue;
            }
            let host = self.clone();
            let completion = completion.clone();
            let scope = scope.clone();
            tokio::spawn(async move {
                let result: Result<Empty, _> = host
                    .call_with_callback(&record, method::REQUEST_COMPLETE, &completion, &scope)
                    .await;
                if let Err(e) = result {
                    tracing::warn!("pluginhost: request lifecycle plugin {} failed: {e}", record.id);
                }
            });
        }
    }

    /// Go `InterceptResponse[Except]`: successful non-streaming responses.
    pub async fn intercept_response(
        &self,
        req: ResponseInterceptRequest,
        skip: &str,
        scope: &RequestScope,
    ) -> ResponseInterceptResponse {
        let mut current = ResponseInterceptResponse {
            headers: req.response_headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };
        for record in self.interceptor_records(skip, |r| r.plugin.caps.response_interceptor) {
            if !self.live(&record) {
                continue;
            }
            let mut next = req.clone();
            next.response_headers = current.headers.clone();
            next.body = current.body.clone();
            match self
                .call_with_callback::<ResponseInterceptResponse, _>(
                    &record,
                    method::RESPONSE_INTERCEPT_AFTER,
                    &next,
                    scope,
                )
                .await
            {
                Ok(resp) => {
                    current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
                    if !resp.body.is_empty() {
                        current.body = resp.body;
                    }
                }
                Err(e) => tracing::warn!("pluginhost: response interceptor {} failed: {e}", record.id),
            }
        }
        current
    }

    /// Go `InterceptStreamChunk[Except]`. Payload chunks (`ChunkIndex >= 0`) omit request
    /// bodies for schema 3+ and history for schema 5+; the header-init call always
    /// carries them. A dropped chunk ends the chain.
    pub async fn intercept_stream_chunk(
        &self,
        req: StreamChunkInterceptRequest,
        skip: &str,
        scope: &RequestScope,
    ) -> StreamChunkInterceptResponse {
        let mut current = StreamChunkInterceptResponse {
            headers: req.response_headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };
        for record in self.interceptor_records(skip, |r| r.plugin.caps.stream_chunk_interceptor) {
            if current.drop_chunk {
                break;
            }
            if !self.live(&record) {
                continue;
            }
            let schema = record.plugin.schema_version;
            let payload = req.chunk_index != STREAM_CHUNK_HEADER_INIT_INDEX;
            let mut next = req.clone();
            next.response_headers = current.headers.clone();
            next.body = current.body.clone();
            if payload && schema >= abi::SCHEMA_STREAM_CHUNK_OMIT_REQUEST_BODY {
                next.original_request = Bytes::new();
                next.request_body = Bytes::new();
            }
            if payload && schema >= abi::SCHEMA_STREAM_CHUNK_OMIT_HISTORY {
                next.history_chunks = Vec::new();
            }
            match self
                .call_with_callback::<StreamChunkInterceptResponse, _>(
                    &record,
                    method::RESPONSE_INTERCEPT_STREAM_CHUNK,
                    &next,
                    scope,
                )
                .await
            {
                Ok(resp) => {
                    current.headers = merge_headers(&current.headers, &resp.headers, &resp.clear_headers);
                    if !resp.body.is_empty() {
                        current.body = resp.body;
                    }
                    current.drop_chunk |= resp.drop_chunk;
                }
                Err(e) => tracing::warn!("pluginhost: stream chunk interceptor {} failed: {e}", record.id),
            }
        }
        current
    }

    /// Go `ObserveWebSocketResponseEvent[Except]`: synchronous, in order, errors logged.
    pub async fn observe_websocket_response_event(
        &self,
        event: WebSocketResponseEvent,
        skip: &str,
        scope: &RequestScope,
    ) {
        for record in self.interceptor_records(skip, |r| r.plugin.caps.websocket_response_observer) {
            if !self.live(&record) {
                continue;
            }
            let result: Result<Empty, _> = self
                .call_with_callback(&record, method::WEBSOCKET_RESPONSE_EVENT, &event, scope)
                .await;
            if let Err(e) = result {
                tracing::warn!("pluginhost: websocket response observer {} failed: {e}", record.id);
            }
        }
    }

    fn any_active(&self, has: impl Fn(&Record) -> bool) -> bool {
        self.active_records().iter().any(|r| has(r) && !self.is_fused(&r.id))
    }

    /// Go `HasRequestInterceptors`.
    pub fn has_request_interceptors(&self) -> bool {
        self.any_active(|r| r.plugin.caps.request_interceptor)
    }

    /// Go `HasStreamInterceptors`.
    pub fn has_stream_interceptors(&self) -> bool {
        self.any_active(|r| r.plugin.caps.stream_chunk_interceptor)
    }

    /// Go `HasWebSocketResponseObservers`.
    pub fn has_websocket_response_observers(&self) -> bool {
        self.any_active(|r| r.plugin.caps.websocket_response_observer)
    }

    /// Go `HasResponseInterceptors` equivalent for the handler's fast path.
    pub fn has_response_interceptors(&self) -> bool {
        self.any_active(|r| r.plugin.caps.response_interceptor)
    }

    /// Go `HasRequestLifecyclePlugins` equivalent.
    pub fn has_request_lifecycle_plugins(&self) -> bool {
        self.any_active(|r| r.plugin.caps.request_lifecycle_plugin)
    }

    /// Go `StreamChunkPayloadIncludesRequestBody`: some stream interceptor speaks schema < 3.
    pub fn stream_chunk_payload_includes_request_body(&self) -> bool {
        self.any_active(|r| {
            r.plugin.caps.stream_chunk_interceptor
                && r.plugin.schema_version < abi::SCHEMA_STREAM_CHUNK_OMIT_REQUEST_BODY
        })
    }

    /// Go `StreamChunkPayloadIncludesHistory`: some stream interceptor speaks schema < 5.
    pub fn stream_chunk_payload_includes_history(&self) -> bool {
        self.any_active(|r| {
            r.plugin.caps.stream_chunk_interceptor && r.plugin.schema_version < abi::SCHEMA_STREAM_CHUNK_OMIT_HISTORY
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `mergeHeaders`: clears first, replaces per canonical key, keeps the rest.
    #[test]
    fn merge_headers_follows_http_header() {
        let current: Header = [
            ("X-Keep".to_owned(), vec!["1".to_owned()]),
            ("X-Clear".to_owned(), vec!["2".to_owned()]),
            ("X-Replace".to_owned(), vec!["3".to_owned()]),
        ]
        .into();
        let updates: Header = [
            ("x-replace".to_owned(), vec!["a".to_owned(), "b".to_owned()]),
            ("x-new".to_owned(), vec!["n".to_owned()]),
        ]
        .into();
        let merged = merge_headers(&current, &updates, &["x-clear".to_owned()]);
        let got: Vec<_> = merged.iter().map(|(k, v)| format!("{k}={}", v.join(","))).collect();
        assert_eq!(got, ["X-Keep=1", "X-New=n", "X-Replace=a,b"]);
    }
}
