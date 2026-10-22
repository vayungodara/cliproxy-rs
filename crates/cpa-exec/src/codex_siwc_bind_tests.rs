use super::*;
use crate::codex_testkit::{Mock, json};
use serde_json::Value;

fn golden() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/codex_siwc_bind_go.json")).unwrap()
}

fn temp_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!("cpa-siwc-bind-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join("auth")).unwrap();
    std::fs::create_dir_all(root.join("out")).unwrap();
    plain_path(std::fs::canonicalize(root).unwrap())
}

/// The Go recorder's file layout: `->target` is a symlink, `/` a directory. `false`
/// when the case needs symlinks this platform cannot make.
fn write_files(base: &Path, files: &Value, expand: &dyn Fn(&str) -> String) -> bool {
    for (name, content) in files.as_object().into_iter().flatten() {
        let (path, content) = (base.join(name), content.as_str().unwrap());
        if let Some(target) = content.strip_prefix("->") {
            #[cfg(unix)]
            std::os::unix::fs::symlink(expand(target), &path).unwrap();
            #[cfg(not(unix))]
            {
                let _ = (target, expand);
                return false;
            }
        } else if content == "/" {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
        }
    }
    true
}

/// Go's message, with a wrapped cause's OS text cut (`cause`), against ours.
fn assert_error(case: &Value, got: &str, expand: &dyn Fn(&str) -> String) {
    let want = expand(
        case["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{case}: unexpected error {got}")),
    );
    if case["cause"] == Value::Bool(true) {
        assert!(
            got.starts_with(&format!("{want}: ")),
            "{}: {got} vs {want}",
            case["name"]
        );
    } else {
        assert_eq!(got, want, "{}", case["name"]);
    }
}

#[test]
fn siwc_bind_matches_go() {
    let go = golden();
    for case in go["resolve"].as_array().unwrap() {
        let root = temp_root();
        let (dir, out) = (root.join("auth"), root.join("out"));
        // The golden's `/` is Go's separator on Linux; Windows paths use `\`.
        let expand = |s: &str| {
            let s = s
                .replace("$DIR", &dir.to_string_lossy())
                .replace("$OUT", &out.to_string_lossy());
            if cfg!(windows) { s.replace('/', "\\") } else { s }
        };
        if write_files(&out, &case["outside"], &expand) && write_files(&dir, &case["files"], &expand) {
            let auth_dir = expand(case["auth_dir"].as_str().unwrap());
            match resolve(Path::new(&auth_dir), &expand(case["target"].as_str().unwrap())) {
                Ok(path) => assert_eq!(
                    path,
                    PathBuf::from(expand(
                        case["result"].as_str().unwrap_or_else(|| panic!("{case}: {path:?}"))
                    )),
                    "{}",
                    case["name"]
                ),
                Err(e) => assert_error(case, &e, &expand),
            }
        }
        let _ = std::fs::remove_dir_all(root);
    }

    for case in go["device"].as_array().unwrap() {
        let root = temp_root();
        let dir = root.join("auth");
        write_files(&dir, &case["files"], &|s| s.to_owned());
        match device_id(&dir.join("c.json")) {
            Ok(id) if case["device"] == "generated" => {
                assert!(
                    crate::codex_siwc::is_valid_uuid(&id) && id == id.go_lower(),
                    "{case}: {id}"
                );
                assert!(!case["files"]["c.json"].as_str().unwrap().go_lower().contains(&id));
            }
            Ok(id) => assert_eq!(id, case["device"].as_str().unwrap(), "{}", case["name"]),
            Err(e) => assert_error(case, &e, &|s| s.to_owned()),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    let tokens = Tokens {
        access_token: "siwc-at".into(),
        refresh_token: "siwc-rt".into(),
        id_token: "siwc-id".into(),
        client_id: "issued-client".into(),
        scope: "openid chatgpt.tokens.use.direct".into(),
        scopes: vec!["openid".into(), "chatgpt.tokens.use.direct".into()],
        expired: "2026-10-22T18:00:00Z".into(),
        last_refresh: "2026-10-22T17:00:00Z".into(),
        device_id: "e61bbe28-07ef-466d-8e5d-a344f94ab305".into(),
    };
    for case in go["save"].as_array().unwrap() {
        let root = temp_root();
        let dir = root.join("auth");
        write_files(&dir, &case["files"], &|s| s.to_owned());
        let path = dir.join(case["target"].as_str().filter(|t| !t.is_empty()).unwrap_or("c.json"));
        match save(&path, &tokens) {
            Ok(()) => assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                case["result"].as_str().unwrap_or_else(|| panic!("{case}: saved")),
                "{}",
                case["name"]
            ),
            Err(e) => assert_error(case, &e, &|s| s.to_owned()),
        }
        // Go's lock file (`canonicalLockPath` + `.lock`) stays beside the credential.
        let mut listing: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        listing.sort();
        let mut want: Vec<String> = serde_json::from_value(case["listing"].clone()).unwrap();
        // Win32 path normalisation drops a name's trailing spaces and dots; Go, which
        // prefixes `\\?\` only to long paths, creates the same trimmed file there.
        if cfg!(windows) {
            want = want.iter().map(|n| n.trim_end_matches([' ', '.']).to_owned()).collect();
            want.sort();
        }
        assert_eq!(listing, want, "{}", case["name"]);
        let _ = std::fs::remove_dir_all(root);
    }

    for case in go["callback"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let s = |k: &str| case[k].as_str().unwrap_or_default().to_owned();
        match parse_callback_input(input) {
            Ok(Some(c)) => assert_eq!(
                (c.code, c.state, c.client_id, c.error, c.description),
                (s("code"), s("state"), s("client_id"), s("error"), s("desc")),
                "{input}"
            ),
            Ok(None) => panic!("{input}: nothing parsed"),
            Err(message) => assert_eq!(message, s("err"), "{input}"),
        }
    }
}

/// The whole command against a loopback token endpoint: the stored device ID reaches
/// the authorization URL, the callback's issued client ID the exchange, and the file
/// keeps its outer tokens beside the new `siwc` partition. A callback with another
/// state saves nothing.
#[tokio::test]
async fn bind_saves_the_issued_client_tokens() {
    let mock = Mock::start().await;
    let token_path = "/api/accounts/oauth/token";
    let grant = r#"{"access_token":"siwc-at","refresh_token":"siwc-rt","id_token":"it","scope":"openid chatgpt.tokens.use.direct","expires_in":3600}"#;
    mock.script(token_path, vec![json(200, grant)]);
    let root = temp_root();
    let dir = root.join("auth");
    let original = r#"{"type":"codex","access_token":"at","refresh_token":"rt","siwc":{"device_id":"E61BBE28-07EF-466D-8E5D-A344F94AB305"}}"#;
    std::fs::write(dir.join("c.json"), original).unwrap();
    let mut cfg = Config::parse("").unwrap();
    cfg.auth_dir = dir.clone();

    let run = |callback: &'static str| {
        let (cfg, url) = (cfg.clone(), format!("{}{token_path}", mock.url));
        async move {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            let interaction = Interaction {
                manual_delay: Duration::from_secs(3600),
                prompt: None,
                show_url: Box::new(move |url| {
                    let _ = tx.send(url.to_owned());
                }),
            };
            let oauth = CodexOAuth::new(wreq::Client::new()).with_siwc_token_url(&url);
            let task = tokio::spawn(async move { bind_with(&cfg, " c.json ", port, oauth, interaction).await });
            let auth_url = url::Url::parse(&rx.await.unwrap()).unwrap();
            let query = |k: &str| {
                auth_url
                    .query_pairs()
                    .find(|(key, _)| key == k)
                    .map(|(_, v)| v.into_owned())
                    .unwrap()
            };
            assert_eq!(
                query("ext_agent_host_id"),
                "urn:uuid:e61bbe28-07ef-466d-8e5d-a344f94ab305"
            );
            assert_eq!(query("redirect_uri"), format!("http://127.0.0.1:{port}/auth/callback"));
            let callback = callback.replace("{state}", &query("state"));
            let _ = wreq::Client::new()
                .get(format!("http://127.0.0.1:{port}/auth/callback?{callback}"))
                .send()
                .await
                .unwrap();
            (port, task.await.unwrap())
        }
    };

    let (_, mismatch) = run("code=the-code&state=other&client_id=oaiapp_issued").await;
    assert_eq!(
        mismatch,
        Err(BindError::Print(
            "Sign in with ChatGPT authentication failed: state mismatch".into()
        ))
    );
    assert!(mock.take().is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("c.json")).unwrap(), original);

    let (port, bound) = run("code=the-code&state={state}&client_id=%20oaiapp_issued%20").await;
    let real = plain_path(std::fs::canonicalize(dir.join("c.json")).unwrap());
    assert_eq!(bound, Ok(real.clone()));
    let sent = mock.take();
    assert_eq!(sent.len(), 1);
    let form: BTreeMap<String, String> = url::form_urlencoded::parse(&sent[0].body).into_owned().collect();
    assert_eq!(form["client_id"], "oaiapp_issued");
    assert_eq!(form["code"], "the-code");
    assert_eq!(form["redirect_uri"], format!("http://127.0.0.1:{port}/auth/callback"));
    let saved: Value = serde_json::from_slice(&std::fs::read(&real).unwrap()).unwrap();
    assert_eq!(
        (&saved["access_token"], &saved["refresh_token"]),
        (&serde_json::json!("at"), &serde_json::json!("rt"))
    );
    assert_eq!(saved["use_siwc"], serde_json::json!(true));
    assert_eq!(saved["siwc"]["access_token"], "siwc-at");
    assert_eq!(saved["siwc"]["client_id"], "oaiapp_issued");
    assert_eq!(saved["siwc"]["device_id"], "e61bbe28-07ef-466d-8e5d-a344f94ab305");
    let _ = std::fs::remove_dir_all(root);
}

/// Windows spells canonical paths the way Go's `EvalSymlinks` does.
#[cfg(windows)]
#[test]
fn verbatim_paths_lose_their_prefix() {
    for (path, want) in [
        (r"\\?\C:\Users\a\c.json", r"C:\Users\a\c.json"),
        (r"\\?\UNC\server\share\auth\c.json", r"\\server\share\auth\c.json"),
        (r"\\?\GLOBALROOT\Device\x", r"\\?\GLOBALROOT\Device\x"),
        (r"C:\Users\a\c.json", r"C:\Users\a\c.json"),
    ] {
        assert_eq!(plain_path(PathBuf::from(path)), PathBuf::from(want), "{path}");
    }
    // The temporary root is canonical and plain, so resolve() answers without the prefix.
    let root = temp_root();
    assert!(!root.to_string_lossy().starts_with(r"\\?\"), "{root:?}");
    let _ = std::fs::remove_dir_all(root);
}
