//! HTTP access logging, following internal/logging/gin_logger.go at 6fecc6e.

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
}

/// Request-local identity for dispatch code without an HTTP extractor. Captured
/// before dispatch spawns work; never stored in a process-global mutable slot.
pub(crate) fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok().filter(|id| !id.is_empty())
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
    let query = mask_query(request.uri().query().unwrap_or_default());
    let method = request.method().to_string();
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|p| p.0);
    let client = state
        .proxies
        .client_ip(peer, |name| request.headers().get(name).map(|v| v.as_bytes()));
    let id = if is_ai_path(&path) {
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
    let response = REQUEST_ID.scope(context_id, next.run(request)).await;
    let status = response.status().as_u16();
    if health && (200..300).contains(&status) {
        log.0 = None;
        return response;
    }
    if let Some(line) = &mut log.0 {
        line.status = status;
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(AccessBody { body, log }))
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
    format!("{status:3} | {latency:>13} | {client:>15} | {method:<7} \"{path}\"")
}

struct AccessBody {
    body: Body,
    log: AccessGuard,
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
pub(crate) fn mask_query(raw: &str) -> String {
    raw.split('&')
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            let decoded = crate::access::unescape(key).unwrap_or_else(|| key.as_bytes().to_owned());
            let lower = cpa_common::gostr::lower_bytes(cpa_core::config::go_trim_space(&decoded));
            let lower = lower.strip_suffix("[]").unwrap_or(&lower);
            if !(lower == "key"
                || ["api-key", "apikey", "api_key", "token", "secret"]
                    .iter()
                    .any(|word| lower.contains(word)))
            {
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
