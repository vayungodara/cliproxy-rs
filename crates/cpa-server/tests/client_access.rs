//! Who may use a keyless proxy, and which management requests count towards a ban,
//! through the binary's composition (crates/cliproxy/src/main.rs without Home and
//! observability). Each request's peer comes from `X-Test-Peer`, as the listener records
//! it, and defaults to loopback. Go v8.0.35 is open with no client keys, grants CORS to
//! every origin and counts missing management keys (docs/DIFFERENCES-FROM-GO.md).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management};

const LOCAL: &str = "127.0.0.1:40000";
const REMOTE: &str = "203.0.113.5:40000";
const EVIL: &str = "https://evil.example";
const KEY: &str = "fake-management";
const WRONG: &str = "Bearer fake-wrong";
const FROM_MACHINE: &str = "only clients on this machine";
const FROM_WEBSITE: &str = "requests from other websites are refused";
const MANAGEMENT_REFUSED: &str = r#"{"error":"management requests from another website are refused"}"#;

struct Server {
    base: String,
    dir: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Reply {
    status: u16,
    headers: wreq::header::HeaderMap,
    body: String,
}

impl Reply {
    fn allow_origin(&self) -> Option<&str> {
        self.headers
            .get("access-control-allow-origin")
            .map(|v| v.to_str().unwrap())
    }
}

async fn peer(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    let peer: SocketAddr = req
        .headers()
        .get("x-test-peer")
        .map_or(LOCAL, |v| v.to_str().unwrap())
        .parse()
        .unwrap();
    req.extensions_mut().insert(axum::extract::ConnectInfo(peer));
    next.run(req).await
}

/// A server whose config ends with `access` (YAML), a private auth-dir and the
/// management key [`KEY`].
async fn serve(name: &str, access: &str) -> Server {
    let dir = std::env::temp_dir().join(format!("cpa-client-access-{name}-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("auth")).unwrap();
    let path = dir.join("config.yaml");
    let hash = bcrypt::hash(KEY, 4).unwrap();
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nserver:\n  port: 0\nmanagement:\n  secret-key: '{hash}'\noauth:\n  auth-dir: {}\n{access}",
            dir.join("auth").display()
        ),
    )
    .unwrap();
    let rt = Arc::new(cpa_server::testing::runtime(
        Config::load(&path).unwrap(),
        vec![],
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let options = management::Options {
        management_password: Some(String::new()),
        log_dir: Some(dir.join("logs")),
        ..Default::default()
    };
    let state = Management::with_options(rt.clone(), path, options);
    let app = cpa_server::app(rt.clone(), management::router(state.clone()));
    let app = cpa_server::safe_mode::router(&rt, app);
    let app = app.layer(axum::middleware::from_fn_with_state(rt.clone(), management::cors));
    let app = cpa_server::request_logging::router(&state, app);
    let app = cpa_server::body_limit::layer(&rt, app).layer(axum::middleware::from_fn(peer));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server { base, dir, task }
}

async fn send(server: &Server, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = wreq::Client::new().request(method.parse().unwrap(), format!("{}{path}", server.base));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    Reply {
        status,
        headers,
        body: res.text().await.unwrap(),
    }
}

async fn models(server: &Server, headers: &[(&str, &str)]) -> Reply {
    send(server, "GET", "/v1/models", headers).await
}

#[tokio::test]
async fn keyless_proxy_refuses_other_machines() {
    let s = serve("remote", "").await;
    for peer in [
        REMOTE,
        "192.168.1.20:5000",
        "[2001:db8::1]:5000",
        "[::ffff:203.0.113.5]:5000",
    ] {
        let r = models(&s, &[("x-test-peer", peer)]).await;
        assert_eq!(r.status, 403, "{peer}: {}", r.body);
        assert!(r.body.contains(FROM_MACHINE), "{peer}: {}", r.body);
    }
}

#[tokio::test]
async fn keyless_proxy_serves_this_machine() {
    let s = serve("loopback", "").await;
    for peer in [LOCAL, "127.0.0.2:5000", "[::1]:5000", "[::ffff:127.0.0.1]:5000"] {
        let r = models(&s, &[("x-test-peer", peer)]).await;
        assert_eq!(r.status, 200, "{peer}: {}", r.body);
        // No Origin, no CORS grant needed.
        assert_eq!(r.allow_origin(), None, "{peer}");
    }
    // The browser of this machine, from a loopback page that sends no Sec-Fetch-Site.
    let r = models(&s, &[("origin", "http://localhost:3000")]).await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.allow_origin(), Some("http://localhost:3000"));
}

#[tokio::test]
async fn keyless_proxy_refuses_other_websites() {
    let s = serve("cross-origin", "").await;
    for headers in [
        &[("origin", EVIL)][..],
        &[("origin", "null")],
        &[("origin", "http://localhost.evil.example")],
        &[("sec-fetch-site", "cross-site")],
        &[("sec-fetch-site", "same-site"), ("origin", "http://localhost:3000")],
    ] {
        let r = models(&s, headers).await;
        assert_eq!(r.status, 403, "{headers:?}: {}", r.body);
        assert!(r.body.contains(FROM_WEBSITE), "{headers:?}: {}", r.body);
        assert_eq!(r.allow_origin(), None, "{headers:?}");
    }
    // The preflight still ends in 204, but grants nothing to another site.
    let r = send(&s, "OPTIONS", "/v1/chat/completions", &[("origin", EVIL)]).await;
    assert_eq!(r.status, 204);
    assert_eq!(r.allow_origin(), None);
}

#[tokio::test]
async fn keyless_proxy_treats_a_relaying_loopback_peer_as_remote() {
    let s = serve("forwarded", "").await;
    for header in ["x-forwarded-for", "x-real-ip", "cf-connecting-ip"] {
        let r = models(&s, &[(header, "198.51.100.7")]).await;
        assert_eq!(r.status, 403, "{header}: {}", r.body);
        assert!(r.body.contains(FROM_MACHINE), "{header}: {}", r.body);
    }
    let r = models(&s, &[("forwarded", "for=198.51.100.7")]).await;
    assert_eq!(r.status, 403, "{}", r.body);
}

#[tokio::test]
async fn allow_open_access_restores_the_open_proxy() {
    let s = serve("open", "access:\n  allow-open-access: true\n").await;
    for headers in [
        &[("x-test-peer", REMOTE)][..],
        &[("x-forwarded-for", "198.51.100.7")],
        &[("origin", EVIL), ("sec-fetch-site", "cross-site")],
    ] {
        let r = models(&s, headers).await;
        assert_eq!(r.status, 200, "{headers:?}: {}", r.body);
        assert_eq!(r.allow_origin(), Some("*"), "{headers:?}");
    }
    let r = send(&s, "OPTIONS", "/v1/chat/completions", &[("origin", EVIL)]).await;
    assert_eq!((r.status, r.allow_origin()), (204, Some("*")));
}

#[tokio::test]
async fn client_keys_behave_as_before() {
    let s = serve("keys", "access:\n  api-keys: [fake-client]\n").await;
    let r = models(&s, &[("x-test-peer", REMOTE)]).await;
    assert_eq!((r.status, r.body.as_str()), (401, r#"{"error":"Missing API key"}"#));
    assert_eq!(r.allow_origin(), Some("*"));
    for headers in [
        &[("x-test-peer", REMOTE), ("authorization", "Bearer fake-client")][..],
        &[
            ("x-forwarded-for", "198.51.100.7"),
            ("authorization", "Bearer fake-client"),
        ],
        &[
            ("origin", EVIL),
            ("sec-fetch-site", "cross-site"),
            ("authorization", "Bearer fake-client"),
        ],
    ] {
        let r = models(&s, headers).await;
        assert_eq!(r.status, 200, "{headers:?}: {}", r.body);
        assert_eq!(r.allow_origin(), Some("*"), "{headers:?}");
    }
    let r = send(&s, "OPTIONS", "/v1/chat/completions", &[("origin", EVIL)]).await;
    assert_eq!((r.status, r.allow_origin()), (204, Some("*")));
}

const CONFIG: &str = "/v8/management/config";

#[tokio::test]
async fn cross_origin_management_requests_are_refused_and_never_ban() {
    let s = serve("management-cross-origin", "").await;
    for i in 0..6 {
        let headers: &[(&str, &str)] = if i % 2 == 0 {
            &[("origin", EVIL), ("authorization", WRONG)]
        } else {
            &[("sec-fetch-site", "cross-site"), ("authorization", WRONG)]
        };
        let r = send(&s, "GET", CONFIG, headers).await;
        assert_eq!((r.status, r.body.as_str()), (403, MANAGEMENT_REFUSED), "{headers:?}");
        assert_eq!(r.allow_origin(), None);
    }
    // Even a right key from another site is refused.
    let right = format!("Bearer {KEY}");
    let r = send(
        &s,
        "GET",
        CONFIG,
        &[("origin", EVIL), ("authorization", right.as_str())],
    )
    .await;
    assert_eq!((r.status, r.body.as_str()), (403, MANAGEMENT_REFUSED));
    let r = send(&s, "OPTIONS", CONFIG, &[("origin", EVIL)]).await;
    assert_eq!((r.status, r.allow_origin()), (204, None));
    // Not banned, and the dashboard on the server's own origin still works.
    let own = s.base.clone();
    let r = send(
        &s,
        "GET",
        CONFIG,
        &[
            ("origin", own.as_str()),
            ("sec-fetch-site", "same-origin"),
            ("authorization", right.as_str()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.allow_origin(), None, "management grants no CORS");
    // Behind a reverse proxy that rewrites Host, the browser's Sec-Fetch-Site decides.
    let r = send(
        &s,
        "GET",
        CONFIG,
        &[
            ("origin", "https://dash.example"),
            ("sec-fetch-site", "same-origin"),
            ("authorization", right.as_str()),
        ],
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn keyless_management_requests_never_ban() {
    let s = serve("management-keyless", "").await;
    for _ in 0..6 {
        let r = send(&s, "GET", CONFIG, &[]).await;
        assert_eq!(
            (r.status, r.body.as_str()),
            (401, r#"{"error":"missing management key"}"#)
        );
    }
    let right = format!("Bearer {KEY}");
    let r = send(&s, "GET", CONFIG, &[("authorization", right.as_str())]).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

#[tokio::test]
async fn wrong_management_keys_from_the_own_origin_still_ban() {
    let s = serve("management-ban", "").await;
    let own = s.base.clone();
    for _ in 0..5 {
        let r = send(&s, "GET", CONFIG, &[("origin", own.as_str()), ("authorization", WRONG)]).await;
        assert_eq!(
            (r.status, r.body.as_str()),
            (401, r#"{"error":"invalid management key"}"#)
        );
    }
    let right = format!("Bearer {KEY}");
    let r = send(
        &s,
        "GET",
        CONFIG,
        &[("origin", own.as_str()), ("authorization", right.as_str())],
    )
    .await;
    assert_eq!(r.status, 403, "{}", r.body);
    assert!(
        r.body.contains("IP banned due to too many failed attempts"),
        "{}",
        r.body
    );
}
