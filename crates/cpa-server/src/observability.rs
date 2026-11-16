//! HTTP access logging, following internal/logging/gin_logger.go (v8.0.35).

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, HttpBody};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use cpa_core::config::TrustedProxies;

use crate::Runtime;

/// Go keeps the full UUID in request context; only filenames and log lines shorten it.
#[derive(Clone)]
pub struct RequestId(pub String);

tokio::task_local! {
    static REQUEST_ID: String;
    static CLOCK: Arc<Clock>;
}

/// Request-local identity for dispatch code without an HTTP extractor. Captured
/// before dispatch spawns work; never stored in a process-global mutable slot.
pub(crate) fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok().filter(|id| !id.is_empty())
}

/// When an inference request arrived, when its body was in and when its response first
/// went out: the usage record's timings (`crate::usage_record::Timings`) count from
/// `accepted`. One per HTTP inference request, shared by its request body, its response
/// body and the records of its attempts. Cost: one allocation per inference request.
#[derive(Debug)]
pub struct Clock {
    accepted: Instant,
    body: std::sync::OnceLock<(Instant, u64)>,
    response: std::sync::OnceLock<Instant>,
}

impl Default for Clock {
    /// A request that did not arrive over HTTP (a WebSocket turn, an internal call)
    /// counts from now and reports neither body.
    fn default() -> Self {
        Self {
            accepted: Instant::now(),
            body: std::sync::OnceLock::new(),
            response: std::sync::OnceLock::new(),
        }
    }
}

impl Clock {
    /// The server accepted the request: its head was read and routing began.
    pub fn accepted(&self) -> Instant {
        self.accepted
    }

    /// The request body was read to its end, and its size as received.
    pub fn body_received(&self) -> Option<(Instant, u64)> {
        self.body.get().copied()
    }

    /// The first response bytes were handed to the connection for the client. A
    /// keep-alive the server sent while waiting counts.
    pub fn response_started(&self) -> Option<Instant> {
        self.response.get().copied()
    }
}

/// The clock of the HTTP request being handled, or a new one outside such a request.
pub(crate) fn current_clock() -> Arc<Clock> {
    CLOCK.try_with(Arc::clone).unwrap_or_default()
}

/// An inference request's body, counted as it is read; its end goes to the [`Clock`].
/// Cost: one box per request and an addition per chunk.
struct Received {
    body: Body,
    bytes: u64,
    clock: Option<Arc<Clock>>,
}

impl Received {
    fn finish(&mut self) {
        if let Some(clock) = self.clock.take() {
            let _ = clock.body.set((Instant::now(), self.bytes));
        }
    }
}

impl HttpBody for Received {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        match &result {
            Poll::Ready(Some(Ok(frame))) => {
                self.bytes += frame.data_ref().map_or(0, |data| data.len() as u64);
                if self.body.is_end_stream() {
                    self.finish();
                }
            }
            Poll::Ready(None) => self.finish(),
            _ => {}
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

#[derive(Clone)]
struct Access {
    proxies: TrustedProxies,
}

/// Applies process access logging to the complete API, including management routes.
/// Trusted proxies are captured at startup, not changed by reload (Go gin.Engine).
pub fn router(rt: &Arc<Runtime>, app: axum::Router) -> axum::Router {
    app.layer(axum::middleware::from_fn_with_state(
        Access {
            proxies: TrustedProxies::new(&rt.config().trusted_proxies),
        },
        access_log,
    ))
}

pub(crate) fn is_ai_path(path: &str) -> bool {
    ["/v1", "/v1beta", "/openai/v1", "/backend-api/codex"]
        .iter()
        .any(|prefix| path == *prefix || path.strip_prefix(prefix).is_some_and(|tail| tail.starts_with('/')))
}

async fn access_log(State(state): State<Access>, mut request: Request, next: Next) -> Response {
    let start = Instant::now();
    let path = crate::management::percent_decode(request.uri().path());
    let query = logged_query(&path, request.uri().query().unwrap_or_default());
    let method = request.method().to_string();
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|p| p.0);
    let client = state
        .proxies
        .client_ip(peer, |name| request.headers().get(name).map(|v| v.as_bytes()));
    let inference = is_ai_path(&path);
    let id = if inference {
        let id = crate::dispatch::request_id();
        request.extensions_mut().insert(RequestId(id.clone()));
        id
    } else {
        "--------".to_owned()
    };
    let context_id = if id == "--------" { String::new() } else { id.clone() };
    let health = path == "/healthz" && matches!(method.as_str(), "GET" | "HEAD");
    let path = if query.is_empty() {
        path
    } else {
        format!("{path}?{query}")
    };
    let mut log = AccessGuard(Some(AccessLine {
        start,
        status: 200,
        client,
        method,
        path,
        id,
        health,
    }));
    let clock = inference.then(|| {
        Arc::new(Clock {
            accepted: start,
            ..Clock::default()
        })
    });
    let response = match &clock {
        Some(clock) => {
            let request = request.map(|body| {
                Body::new(Received {
                    body,
                    bytes: 0,
                    clock: Some(clock.clone()),
                })
            });
            REQUEST_ID
                .scope(context_id, CLOCK.scope(clock.clone(), next.run(request)))
                .await
        }
        None => REQUEST_ID.scope(context_id, next.run(request)).await,
    };
    let status = response.status().as_u16();
    if health && (200..300).contains(&status) {
        log.0 = None;
        return response;
    }
    if let Some(line) = &mut log.0 {
        line.status = status;
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(AccessBody { body, log, clock }))
}

struct AccessLine {
    start: Instant,
    status: u16,
    client: String,
    method: String,
    path: String,
    id: String,
    health: bool,
}

impl AccessLine {
    fn emit(self) {
        if self.health && (200..300).contains(&self.status) {
            return;
        }
        let line = access_line(
            self.status,
            self.start.elapsed(),
            &self.client,
            &self.method,
            &self.path,
        );
        match self.status {
            500.. => tracing::error!(request_id = self.id, "{line}"),
            400.. => tracing::warn!(request_id = self.id, "{line}"),
            _ => tracing::info!(request_id = self.id, "{line}"),
        }
    }
}

/// Go truncates milliseconds through one minute, whole seconds above it.
fn access_line(status: u16, latency: Duration, client: &str, method: &str, path: &str) -> String {
    let latency = if latency >= Duration::from_secs(60) {
        let seconds = latency.as_secs();
        let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
        if hours > 0 {
            format!("{hours}h{minutes}m{seconds}s")
        } else {
            format!("{minutes}m{seconds}s")
        }
    } else {
        let millis = latency.as_millis();
        if millis == 0 {
            "0s".to_owned()
        } else if millis < 1000 {
            format!("{millis}ms")
        } else {
            let fraction = format!("{:03}", millis % 1000);
            if millis.is_multiple_of(1000) {
                format!("{}s", millis / 1000)
            } else {
                format!("{}.{}s", millis / 1000, fraction.trim_end_matches('0'))
            }
        }
    };
    // Go v8.0.35 (`4337f1f0`) quotes the path with `%q`, so a decoded line break cannot
    // forge a second entry.
    let path = cpa_common::gostr::quote(path);
    format!("{status:3} | {latency:>13} | {client:>15} | {method:<7} {path}")
}

struct AccessBody {
    body: Body,
    log: AccessGuard,
    /// An inference request's clock, until its first response bytes go out.
    clock: Option<Arc<Clock>>,
}

struct AccessGuard(Option<AccessLine>);

impl AccessGuard {
    fn finish(&mut self) {
        if let Some(log) = self.0.take() {
            log.emit();
        }
    }
}

impl Drop for AccessGuard {
    fn drop(&mut self) {
        self.finish();
    }
}

impl HttpBody for AccessBody {
    type Data = bytes::Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let result = Pin::new(&mut self.body).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &result
            && frame.data_ref().is_some_and(|data| !data.is_empty())
            && let Some(clock) = self.clock.take()
        {
            let _ = clock.response.set(Instant::now());
        }
        if matches!(result, Poll::Ready(None | Some(Err(_)))) {
            self.log.finish();
        }
        result
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.body.size_hint()
    }
}

/// Go util.HideAPIKey operates on bytes, not Unicode code points.
///
/// Deliberate difference: Go returns keys of one or two bytes unchanged, which
/// writes a short client key to the log verbatim. Those become `...` here.
pub(crate) fn hide_key(value: &[u8]) -> Vec<u8> {
    let keep = match value.len() {
        9.. => 4,
        5.. => 2,
        3.. => 1,
        0 => return Vec::new(),
        _ => return b"...".to_vec(),
    };
    [&value[..keep], b"...", &value[value.len() - keep..]].concat()
}

/// Go util.MaskSensitiveQuery preserves raw names, order, malformed escapes and
/// non-sensitive values. It does not parse/re-encode the whole query.
/// The query an access line shows. Go v8.0.35 `d50a1995`: a v8 OAuth query carries
/// the callback state, code and issued client ID, so none of it is logged.
fn logged_query(path: &str, raw: &str) -> String {
    if path.starts_with("/v8/management/oauth/") {
        return String::new();
    }
    mask_query(raw)
}

pub(crate) fn mask_query(raw: &str) -> String {
    mask_query_by(raw, go_sensitive_name)
}

/// Go's sensitive query names. v8.0.35 adds `value`: deleting a client key passes it
/// as `?value=`.
fn go_sensitive_name(lower: &[u8]) -> bool {
    lower == b"key"
        || lower == b"value"
        || [&b"api-key"[..], b"apikey", b"api_key", b"token", b"secret"]
            .iter()
            .any(|word| lower.windows(word.len()).any(|w| w == *word))
}

/// [`mask_query`] for a configured upstream URL, which a provider may sign or key
/// with other names. Deliberate difference: Go masks upstream queries like client
/// queries; this also masks names containing `key`, `token`, `secret`, `auth`,
/// `password` or `sig` (which covers `signature`).
pub(crate) fn mask_upstream_query(raw: &str) -> String {
    mask_query_by(raw, |lower| {
        go_sensitive_name(lower)
            || [&b"key"[..], b"token", b"secret", b"auth", b"password", b"sig"]
                .iter()
                .any(|word| lower.windows(word.len()).any(|w| w == *word))
    })
}

fn mask_query_by(raw: &str, sensitive: impl Fn(&[u8]) -> bool) -> String {
    raw.split('&')
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let decoded = crate::access::unescape(key).unwrap_or_else(|| key.as_bytes().to_owned());
            let lower = cpa_common::gostr::lower_bytes(cpa_core::config::go_trim_space(&decoded));
            let lower = lower.strip_suffix("[]").unwrap_or(&lower).as_bytes();
            if !sensitive(lower) {
                return part.to_owned();
            }
            let value = crate::access::unescape(value).unwrap_or_else(|| value.as_bytes().to_owned());
            let value = hide_key(cpa_core::config::go_trim_space(&value));
            let escaped: String = value
                .iter()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (*b as char).to_string(),
                    b' ' => "+".to_owned(),
                    _ => format!("%{b:02X}"),
                })
                .collect();
            format!("{key}={escaped}")
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use tower_service::Service;
    use tracing::instrument::WithSubscriber;

    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn subscriber(capture: &Capture) -> impl tracing::Subscriber + Send + Sync + use<> {
        let capture = capture.clone();
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || capture.clone())
            .finish()
    }

    fn runtime() -> Arc<Runtime> {
        Arc::new(crate::testing::runtime(
            cpa_core::config::Config::parse("server: {trusted-proxies: [127.0.0.1]}\n").unwrap(),
            Vec::new(),
            cpa_exec::Executors {
                claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
                codex: Default::default(),
                devices: Default::default(),
                openai: Default::default(),
                google: Default::default(),
            },
        ))
    }

    #[tokio::test]
    async fn health_status_levels_and_stream_completion_follow_go() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/observe_go.json")).unwrap();
        let rt = runtime();
        for case in fixture["health"].as_array().unwrap() {
            let status = StatusCode::from_u16(case["status"].as_u64().unwrap() as u16).unwrap();
            let app = axum::Router::new().fallback(move || async move { status });
            let mut app = router(&rt, app);
            let request = Request::builder()
                .method(case["method"].as_str().unwrap())
                .uri("/healthz")
                .body(Body::empty())
                .unwrap();
            let capture = Capture::default();
            async {
                let response = app.call(request).await.unwrap();
                assert_eq!(response.status(), status);
                drop(response);
            }
            .with_subscriber(subscriber(&capture))
            .await;
            let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
            let level = case["level"].as_str().unwrap();
            if level.is_empty() {
                assert!(text.is_empty(), "{text}");
            } else {
                assert!(
                    text.contains(match level {
                        "warning" => "WARN",
                        "error" => "ERROR",
                        _ => "INFO",
                    }),
                    "{text}"
                );
            }
        }
        let capture = Capture::default();
        async {
            let app = axum::Router::new().fallback(|axum::Extension(id): axum::Extension<RequestId>| async move {
                let trace = crate::dispatch::Trace::with_request_id(current_request_id());
                assert_eq!(id.0, trace.request_id());
                // Pending forever: dropping the downstream stream must emit once.
                Body::from_stream(futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>())
                    .into_response()
            });
            let mut app = router(&rt, app);
            let mut request = Request::builder()
                .uri("/backend-api/codex/responses?key=abcdefghijk")
                .header("x-forwarded-for", "203.0.113.8")
                .body(Body::empty())
                .unwrap();
            request
                .extensions_mut()
                .insert(ConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()));
            let response = app.call(request).await.unwrap();
            assert!(capture.0.lock().unwrap().is_empty(), "stream has not completed");
            drop(response);
        }
        .with_subscriber(subscriber(&capture))
        .await;
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("203.0.113.8"), "{text}");
        assert!(
            text.contains("key=abcd...hijk") && !text.contains("abcdefghijk"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn cancellation_before_headers_and_body_end_log_exactly_once() {
        let rt = runtime();
        let health_capture = Capture::default();
        async {
            let app = axum::Router::new().fallback(std::future::pending::<StatusCode>);
            let mut app = router(&rt, app);
            let request = Request::builder().uri("/healthz").body(Body::empty()).unwrap();
            let mut pending = Box::pin(app.call(request));
            assert!(futures_util::poll!(&mut pending).is_pending());
            drop(pending);
        }
        .with_subscriber(subscriber(&health_capture))
        .await;
        assert!(health_capture.0.lock().unwrap().is_empty());
        let capture = Capture::default();
        async {
            let app = axum::Router::new().fallback(std::future::pending::<StatusCode>);
            let mut app = router(&rt, app);
            let request = Request::builder().uri("/v1/responses").body(Body::empty()).unwrap();
            let mut pending = Box::pin(app.call(request));
            assert!(futures_util::poll!(&mut pending).is_pending());
            assert!(capture.0.lock().unwrap().is_empty());
            drop(pending);
        }
        .with_subscriber(subscriber(&capture))
        .await;
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("200 |") && text.contains("request_id="), "{text}");

        for fail in [false, true] {
            let capture = Capture::default();
            async {
                let app = axum::Router::new().fallback(move || async move {
                    let result = if fail {
                        Err(std::io::Error::other("fixture"))
                    } else {
                        Ok(bytes::Bytes::from_static(b"ok"))
                    };
                    Body::from_stream(futures_util::stream::iter([result]))
                });
                let mut app = router(&rt, app);
                let response = app
                    .call(Request::builder().uri("/v1/responses").body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert!(capture.0.lock().unwrap().is_empty());
                let result = axum::body::to_bytes(response.into_body(), usize::MAX).await;
                assert_eq!(result.is_err(), fail);
            }
            .with_subscriber(subscriber(&capture))
            .await;
            assert_eq!(
                String::from_utf8(capture.0.lock().unwrap().clone())
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn real_dispatch_header_access_and_usage_share_identity() {
        let config = cpa_core::config::Config::parse("claude-api-key:\n  - api-key: fake-observe-key\n    base-url: http://127.0.0.1:9\n    models: [{name: claude-sonnet-4-6}]\nobservability: {usage: {usage-statistics-enabled: true}}\n").unwrap();
        let credentials = cpa_core::config::credentials::from_config(&config);
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:9").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let rt = Arc::new(crate::testing::runtime(config, credentials, executors));
        rt.usage_queue().configure(true, &rt.config());
        let capture = Capture::default();
        let id = async {
            let mut app = router(&rt, crate::router(rt.clone()));
            let request = Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"claude-sonnet-4-6","max_tokens":1,"messages":[]}"#,
                ))
                .unwrap();
            let response = app.call(request).await.unwrap();
            let trace = response
                .headers()
                .get("x-cpa-trace-id")
                .expect("selected credential trace")
                .to_str()
                .unwrap()
                .to_owned();
            let id = trace.splitn(3, '-').nth(2).unwrap().to_owned();
            assert_eq!(id.as_bytes()[14], b'7');
            let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            id
        }
        .with_subscriber(subscriber(&capture))
        .await;
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains(&id), "{text}");
        let records = rt.usage_queue().pop_oldest(10);
        assert!(!records.is_empty());
        for record in records {
            let record: serde_json::Value = serde_json::from_slice(&record).unwrap();
            assert_eq!(record["request_id"], id);
            assert_eq!(record["trace_id"], id);
        }
    }

    #[test]
    fn go_access_and_redaction_goldens() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/observe_go.json")).unwrap();
        for case in fixture["query"].as_array().unwrap() {
            // Deliberate difference: Go leaves the two-byte key `ab` in the clear.
            let want = case["out"].as_str().unwrap().replacen("key=ab&", "key=...&", 1);
            assert_eq!(mask_query(case["in"].as_str().unwrap()), want);
        }
        // Go v8.0.35 `TestMaskSensitiveQueryMasksValue`.
        assert_eq!(
            mask_query("value=sk-secret-client-key&index=3"),
            "value=sk-s...-key&index=3"
        );
        // Go v8.0.35 `TestGinLogrusLoggerRedactsV8OAuthQueries`: no v8 OAuth query, so
        // no `?` either; other paths, the v0 callback included, keep the masked query.
        let oauth = "state=secret-state&code=secret-code&client_id=issued-secret&error=secret-error";
        for path in [
            "/v8/management/oauth/callback",
            "/v8/management/oauth/status",
            "/v8/management/oauth/session",
        ] {
            assert_eq!(logged_query(path, oauth), "");
        }
        assert_eq!(logged_query("/v0/management/oauth-callback", "state=s"), "state=s");
        assert_eq!(logged_query("/v8/management/oauthx", "state=s"), "state=s");
        for case in fixture["lines"].as_array().unwrap() {
            assert_eq!(
                access_line(
                    case["status"].as_u64().unwrap() as u16,
                    Duration::from_nanos(case["nanos"].as_u64().unwrap()),
                    case["client"].as_str().unwrap(),
                    case["method"].as_str().unwrap(),
                    case["path"].as_str().unwrap()
                ),
                case["out"]
            );
        }
        // Go v8.0.35 `TestGinLogrusLoggerEscapesEncodedNewlineInPath`.
        assert!(
            access_line(404, Duration::ZERO, "127.0.0.1", "GET", "/x\n200 forged")
                .ends_with(r#"GET     "/x\n200 forged""#)
        );
        // Go v8.0.35 `TestGinLogrusLoggerKeepsMaskedQueryReadable`: the masked query is
        // quoted with the path, its `...` and `&` left readable.
        let query = logged_query("/v1/models", "key=sk-0123456789cdef&limit=5");
        assert!(
            access_line(200, Duration::ZERO, "127.0.0.1", "GET", &format!("/v1/models?{query}"))
                .ends_with(r#"GET     "/v1/models?key=sk-0...cdef&limit=5""#)
        );
        for path in [
            "/v1",
            "/v1/images/edits",
            "/openai/v1/videos",
            "/backend-api/codex/responses",
        ] {
            assert!(is_ai_path(path));
        }
        for path in ["/v10", "/openai/v10", "/backend-api/codex-status", "/healthz"] {
            assert!(!is_ai_path(path));
        }
    }
}
