//! The TUI against the real management router (in process, loopback only): the
//! password gate, lazy tab loading, and writes through the v0 routes.
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use cpa_exec::Executors;
use cpa_exec::claude::ClaudeExecutor;
use cpa_server::management::{self, Management, Options};

use super::app::{self, App};
use super::{Cmd, i18n};

const LOCAL_PASSWORD: &str = "fake-local-password";
/// bcrypt of `fake-secret-key`. Go and this port both refuse every management request,
/// the local password included, while no secret key is configured.
const SECRET_HASH: &str = "$2a$04$ZwmGqAwK/vcnBgQOZtSBE.RoTJoA.tmp0hCnv0bzqXssaSXsf63E6";

/// Starts the management router on 127.0.0.1 with the TUI's local password.
async fn serve(dir: &std::path::Path) -> String {
    let auth_dir = dir.join("auth");
    std::fs::create_dir_all(&auth_dir).unwrap();
    std::fs::write(
        auth_dir.join("claude-fake.json"),
        r#"{"type":"claude","email":"user@example.invalid","access_token":"fake-token"}"#,
    )
    .unwrap();
    let path = dir.join("config.yaml");
    std::fs::write(
        &path,
        format!(
            "config-version: 8\nmanagement:\n  secret-key: '{SECRET_HASH}'\noauth:\n  auth-dir: {}\naccess:\n  api-keys: [fake-key-a]\nobservability:\n  logs:\n    logging-to-file: false\n",
            auth_dir.display()
        ),
    )
    .unwrap();
    let config = Config::load(&path).unwrap();
    let credentials = cpa_core::config::credentials::load(&config);
    let rt = Arc::new(cpa_server::testing::runtime(
        config,
        credentials,
        Executors {
            claude: ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        },
    ));
    let state = Management::with_options(
        rt,
        path,
        Options {
            local_password: LOCAL_PASSWORD.into(),
            management_password: Some(String::new()),
            log_dir: Some(dir.join("logs")),
            ..Options::default()
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = management::router(state).into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// Runs commands and feeds their messages back, as the TUI loop does. A command that
/// does not finish within a second (the two-second logs poll timer) is dropped.
async fn settle(app: &mut App, cmds: Vec<Cmd>) {
    let mut queue: VecDeque<Cmd> = cmds.into();
    let mut steps = 0;
    while let Some(c) = queue.pop_front() {
        steps += 1;
        assert!(steps < 100, "runaway command chain");
        if let Ok(msg) = tokio::time::timeout(Duration::from_secs(1), c).await {
            queue.extend(app.message(msg).cmds);
        }
    }
}

async fn keys(app: &mut App, keys: &[&str]) {
    for key in keys {
        let step = app.key(key);
        assert!(!step.quit, "{key} quit");
        settle(app, step.cmds).await;
    }
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        keys(app, &[&c.to_string()]).await;
    }
}

fn joined(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn tui_drives_the_management_api() {
    // The locale is process-wide: hold the lock for the whole run.
    let _locale = i18n::LOCALE_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    i18n::set_chinese(false);
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(drive());
}

async fn drive() {
    let dir = std::env::temp_dir().join(format!("cpa-tui-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let base = serve(&dir).await;
    let mut app = App::new(&base, "", None);
    app.resize(100, 40);
    assert!(app.init().is_empty(), "the gate fetches nothing");

    // Go's gate: an empty password, then a wrong one, then the right one.
    keys(&mut app, &["enter"]).await;
    assert_eq!(app.auth_error, "password is required");
    type_text(&mut app, "wrong").await;
    keys(&mut app, &["enter"]).await;
    assert!(
        app.auth_error.starts_with("Connection failed: HTTP 401: "),
        "{}",
        app.auth_error
    );
    assert!(!app.authenticated);
    keys(&mut app, &["ctrl+u"]).await;
    type_text(&mut app, LOCAL_PASSWORD).await;
    keys(&mut app, &["enter"]).await;
    assert!(app.authenticated, "{}", app.auth_error);
    // logging-to-file is off: no logs tab, and only the dashboard has loaded.
    assert_eq!(app.tabs.len(), 5);
    assert_eq!(app.initialized, [true, false, false, false, false, false]);
    let dashboard = joined(&app.dashboard.vp.text());
    assert!(dashboard.contains(&format!("● Connected  {base}")), "{dashboard}");
    assert!(
        dashboard.contains("🔑 1") && dashboard.contains("Auth Files (1 active)"),
        "{dashboard}"
    );

    // Config: loaded on first visit; Enter on Debug writes PUT /debug.
    keys(&mut app, &["tab"]).await;
    assert_eq!(app.active, app::CONFIG);
    let debug = app.config.fields.iter().position(|f| f.path == "debug").unwrap();
    assert_eq!(app.config.fields[debug].value, "false");
    for _ in 0..debug {
        keys(&mut app, &["down"]).await;
    }
    keys(&mut app, &["enter"]).await;
    assert_eq!(app.config.fields[debug].value, "true");
    assert!(joined(&app.config.vp.text()).contains("✓ Updated successfully"));
    // Turning logging-to-file on brings the logs tab back.
    let logging = app
        .config
        .fields
        .iter()
        .position(|f| f.path == "logging-to-file")
        .unwrap();
    for _ in debug..logging {
        keys(&mut app, &["down"]).await;
    }
    keys(&mut app, &["enter"]).await;
    assert!(app.logs_enabled);
    assert_eq!(app.tabs.len(), 6);
    let saved = std::fs::read_to_string(dir.join("config.yaml")).unwrap();
    assert!(
        saved.contains("debug: true") && saved.contains("logging-to-file: true"),
        "{saved}"
    );

    // API keys: add (PATCH old: null), then delete the first.
    keys(&mut app, &["tab", "tab"]).await;
    assert_eq!(app.active, app::API_KEYS);
    assert_eq!(app.keys.keys, ["fake-key-a"]);
    // `q`, `L` and Tab are text while the field has focus.
    keys(&mut app, &["a"]).await;
    type_text(&mut app, "fake-key-qL").await;
    keys(&mut app, &["tab", "enter"]).await;
    assert_eq!(app.keys.keys, ["fake-key-a", "fake-key-qL"]);
    assert_eq!(app.active, app::API_KEYS);
    // Adding a key already present changes nothing.
    keys(&mut app, &["a"]).await;
    type_text(&mut app, "fake-key-a").await;
    keys(&mut app, &["enter"]).await;
    assert_eq!(app.keys.keys, ["fake-key-a", "fake-key-qL"]);
    keys(&mut app, &["d", "y"]).await;
    assert_eq!(app.keys.keys, ["fake-key-qL"]);

    // Auth files: disable the file through PATCH /auth-files/status.
    keys(&mut app, &["shift+tab"]).await;
    assert_eq!(app.active, app::AUTH_FILES);
    assert_eq!(app.auth.files.len(), 1);
    keys(&mut app, &["e"]).await;
    assert_eq!(app.auth.files[0].get("disabled"), Some(&serde_json::Value::Bool(true)));
    // As in Go, the refreshed list clears the "✓ Disabled ..." line at once.
    let auth = joined(&app.auth.vp.text());
    assert!(
        auth.contains("▸ ○ claude-fake.json") && auth.contains(" disabled"),
        "{auth}"
    );

    // OAuth: this server has no Antigravity sign-in, so the menu says so and Enter
    // starts nothing.
    keys(&mut app, &["tab", "tab"]).await;
    assert_eq!(app.active, app::OAUTH);
    let oauth = joined(&app.oauth.vp.text());
    assert!(oauth.contains("Antigravity (not supported by this server)"), "{oauth}");
    assert!(oauth.contains(" Codex (OpenAI) \n"), "{oauth}");
    keys(&mut app, &["down", "down", "enter"]).await;
    assert_eq!(app.oauth.state, super::oauth::State::Idle);
    assert!(joined(&app.oauth.vp.text()).contains("✗ Antigravity: not supported by this server"));
    keys(&mut app, &["shift+tab", "shift+tab"]).await;

    // `L` switches every tab to Chinese; `q` quits outside the logs tab.
    keys(&mut app, &["L"]).await;
    assert_eq!(app.tabs[0], "仪表盘");
    assert!(joined(&app.auth.vp.text()).contains("认证文件"));
    i18n::set_chinese(false);
    assert!(app.key("q").quit);
    let _ = std::fs::remove_dir_all(&dir);
}
