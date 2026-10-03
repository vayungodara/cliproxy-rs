//! Go's optional `GET /keep-alive` (sdk/api `WithKeepAliveEndpoint`,
//! internal/api/server_keepalive.go). Embedders mount it; the Go binary never does, so
//! the binary here does not either. Each request is a heartbeat; when none arrives for
//! `timeout`, the callback runs once. Dropping the router stops the watcher, as Go's
//! `Server.Stop` does.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

struct KeepAlive {
    heartbeat: tokio::sync::Notify,
    /// Go `localPassword` (`WithLocalManagementPassword`); empty accepts everyone.
    local_password: String,
}

/// Go `enableKeepAlive`: the route plus its idle watcher. A zero `timeout` enables
/// nothing. Call inside a Tokio runtime.
pub fn routes(timeout: Duration, local_password: &str, on_timeout: impl FnOnce() + Send + 'static) -> Router {
    if timeout.is_zero() {
        return Router::new();
    }
    let state = Arc::new(KeepAlive {
        heartbeat: tokio::sync::Notify::new(),
        local_password: local_password.to_owned(),
    });
    tokio::spawn(watch(state.clone(), timeout, on_timeout));
    // gin registers GET only: HEAD and other methods are NoRoute 404s.
    let not_found = || async { StatusCode::NOT_FOUND };
    Router::new()
        .route("/keep-alive", axum::routing::get(handle).head(not_found))
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

/// Go `handleKeepAlive`.
async fn handle(axum::extract::State(state): axum::extract::State<Arc<KeepAlive>>, headers: HeaderMap) -> Response {
    if !state.local_password.is_empty() {
        let header = |name: &str| {
            headers
                .get(name)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
                .unwrap_or_default()
        };
        let mut provided = header("authorization");
        if let Some((scheme, token)) = provided.split_once(' ')
            && scheme.eq_ignore_ascii_case("bearer")
        {
            provided = token.to_owned();
        }
        if provided.is_empty() {
            provided = header("x-local-password");
        }
        let matches: bool = subtle::ConstantTimeEq::ct_eq(provided.as_bytes(), state.local_password.as_bytes()).into();
        if !matches {
            return crate::respond::gin_json(401, r#"{"error":"invalid password"}"#.into());
        }
    }
    state.heartbeat.notify_one();
    crate::respond::gin_json(200, r#"{"status":"ok"}"#.into())
}

/// Go `watchKeepAlive`: each heartbeat restarts the timer. The router holds the only
/// other references, so a lone reference at timeout means it was dropped (Go's stop).
async fn watch(state: Arc<KeepAlive>, timeout: Duration, on_timeout: impl FnOnce()) {
    loop {
        tokio::select! {
            () = state.heartbeat.notified() => {}
            () = tokio::time::sleep(timeout) => {
                if Arc::strong_count(&state) > 1 {
                    tracing::warn!("keep-alive endpoint idle for {}, shutting down", go_duration(timeout));
                    on_timeout();
                }
                return;
            }
        }
    }
}

/// Go `time.Duration.String`.
fn go_duration(d: Duration) -> String {
    let nanos = d.as_nanos();
    let trim = |whole: u128, frac: u128, width: usize| {
        let frac = format!("{frac:0width$}");
        let frac = frac.trim_end_matches('0');
        if frac.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{frac}")
        }
    };
    match nanos {
        0 => "0s".into(),
        n if n < 1_000 => format!("{n}ns"),
        n if n < 1_000_000 => format!("{}µs", trim(n / 1_000, n % 1_000, 3)),
        n if n < 1_000_000_000 => format!("{}ms", trim(n / 1_000_000, n % 1_000_000, 6)),
        n => {
            let secs = n / 1_000_000_000;
            let seconds = trim(secs % 60, n % 1_000_000_000, 9);
            match (secs / 3600, secs / 60 % 60) {
                (0, 0) => format!("{seconds}s"),
                (0, m) => format!("{m}m{seconds}s"),
                (h, m) => format!("{h}h{m}m{seconds}s"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn get(app: &Router, headers: &[(&str, &str)]) -> (u16, String) {
        let mut req = axum::http::Request::get("/keep-alive");
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = tower_service::Service::call(&mut app.clone(), req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status().as_u16();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Go `handleKeepAlive` and `watchKeepAlive`, on Tokio's paused clock.
    #[tokio::test(start_paused = true)]
    async fn heartbeats_hold_off_the_timeout() {
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = fired.clone();
        let app = routes(Duration::from_secs(10), "secret", move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        let advance = |secs| tokio::time::advance(Duration::from_secs(secs));

        assert_eq!(get(&app, &[]).await, (401, r#"{"error":"invalid password"}"#.into()));
        assert_eq!(get(&app, &[("authorization", "Basic secret")]).await.0, 401);
        advance(9).await;
        assert_eq!(
            get(&app, &[("authorization", "bearer secret")]).await,
            (200, r#"{"status":"ok"}"#.into())
        );
        tokio::task::yield_now().await;
        advance(9).await;
        assert_eq!(get(&app, &[("x-local-password", " secret ")]).await.0, 200);
        tokio::task::yield_now().await;
        advance(9).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(Ordering::SeqCst), 0, "each heartbeat restarts the timer");
        advance(2).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_router_stops_the_watcher() {
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = fired.clone();
        let app = routes(Duration::from_secs(5), "", move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(get(&app, &[]).await.0, 200, "no local password accepts everyone");
        drop(app);
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert_eq!(fired.load(Ordering::SeqCst), 0);
        assert!(!routes(Duration::ZERO, "", || {}).has_routes());
    }

    #[test]
    fn go_duration_strings() {
        for (d, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(1500), "1.5s"),
            (Duration::from_secs(90), "1m30s"),
            (Duration::from_secs(3600), "1h0m0s"),
            (Duration::from_millis(250), "250ms"),
        ] {
            assert_eq!(go_duration(d), want);
        }
    }
}
