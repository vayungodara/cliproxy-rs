//! `--claude-login`: Go's browser OAuth flow (sdk/auth/claude.go, sdk/auth/manager.go,
//! internal/auth/claude/{oauth_server,html_templates,filename}.go).
//!
//! The callback server answers like Go's `OAuthServer` (302 to `/success`, Go's error
//! texts, the success page). The first callback ends the wait, as in Go; a wrong state
//! fails the login. After 15 seconds the CLI also accepts a pasted callback URL. The
//! credential is written under Go's file name, merged with an existing file of that
//! name and with a matching legacy file, which is then removed.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use cpa_core::credential::MetadataPatch;
use cpa_core::exec::{ExecError, FailureScope};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::oauth::{OAuth, authorize_url, pkce, random_hex};
use crate::tls::Transport;

/// `ClaudeAuthenticator.CallbackPort`.
pub const DEFAULT_CALLBACK_PORT: u16 = 54545;
const SUCCESS_HTML: &str = include_str!("claude_login/success.html");
const SETUP_NOTICE_HTML: &str = include_str!("claude_login/setup_notice.html");
/// Error type prefix the CLI maps to exit code 13, like Go's `ErrPortInUse`.
pub const PORT_IN_USE: &str = "port_in_use";
const MANUAL_PROMPT: &str = "Paste the Claude callback URL (or press Enter to keep waiting): ";

pub struct LoginOptions {
    pub no_browser: bool,
    pub callback_port: u16,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            no_browser: false,
            callback_port: DEFAULT_CALLBACK_PORT,
        }
    }
}

/// One line of user input, read when the manual prompt opens.
pub(crate) type Prompt = Box<dyn FnOnce() -> Pin<Box<dyn Future<Output = std::io::Result<String>> + Send>> + Send>;

/// How the login reaches the user; tests replace every part.
pub(crate) struct Interaction {
    pub manual_delay: Duration,
    pub prompt: Option<Prompt>,
    /// Receives the authorization URL (printing, opening a browser).
    pub show_url: Box<dyn FnOnce(&str) + Send>,
}

/// The CLI flow, through the production OAuth transport.
pub async fn login(auth_dir: &Path, options: &LoginOptions) -> Result<PathBuf, ExecError> {
    let port = options.callback_port;
    let no_browser = options.no_browser;
    let interaction = Interaction {
        manual_delay: Duration::from_secs(15),
        prompt: Some(Box::new(|| {
            Box::pin(async {
                print!("{MANUAL_PROMPT}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                // misc.AsyncPrompt: a detached reader, so an unanswered prompt never
                // keeps the process alive (Tokio waits for blocking-pool tasks).
                let (tx, rx) = tokio::sync::oneshot::channel();
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let _ = tx.send(std::io::stdin().read_line(&mut line).map(|_| line));
                });
                rx.await.map_err(std::io::Error::other)?
            })
        })),
        show_url: Box::new(move |url| show_url(url, port, no_browser)),
    };
    let oauth = OAuth::with_transport(Arc::new(Transport::new(crate::proxy::Hooks::default())));
    let path = login_with(auth_dir, bind(port).await?, oauth, interaction).await?;
    println!("Authentication saved to {}", path.display());
    println!("Claude authentication successful!");
    Ok(path)
}

/// A login started from the management API (Go `RequestAnthropicToken`): the
/// authorization URL and state to hand out; the PKCE verifier stays here for the
/// exchange. The callback reaches management, not a local server.
pub struct ManagedLogin {
    pub url: String,
    pub state: String,
    verifier: String,
}

impl ManagedLogin {
    pub fn start() -> Result<Self, ExecError> {
        let (verifier, challenge) = pkce()?;
        let state = random_hex(16)?;
        Ok(Self {
            url: authorize_url(&state, &challenge),
            state,
            verifier,
        })
    }

    /// Exchanges the callback code (Go drops anything after `#`) through the production
    /// OAuth transport and `proxy` (Go's Claude auth service uses `requests.proxy-url`).
    pub async fn exchange(&self, code: &str, proxy: &crate::proxy::Proxy) -> Result<MetadataPatch, ExecError> {
        let oauth = OAuth::with_transport(Arc::new(Transport::new(crate::proxy::Hooks::default()))).via(proxy);
        self.exchange_with(&oauth, code).await
    }

    /// [`ManagedLogin::exchange`] through `oauth` (tests use local endpoints).
    pub async fn exchange_with(&self, oauth: &OAuth, code: &str) -> Result<MetadataPatch, ExecError> {
        let code = code.split('#').next().unwrap_or_default();
        oauth.exchange(code, &self.state, &self.verifier).await
    }

    /// Writes an exchanged login as the CLI login does (Go file name, merged with an
    /// existing file and a matching legacy file). Blocking.
    pub fn save(auth_dir: &Path, patch: MetadataPatch) -> Result<PathBuf, ExecError> {
        write_login(auth_dir, patch)
    }
}

fn show_url(url: &str, port: u16, no_browser: bool) {
    if !no_browser {
        println!("Opening browser for Claude authentication");
        if open_browser(url) {
            println!("Waiting for Claude authentication callback...");
            return;
        }
        tracing::warn!("No browser available; please open the URL manually");
    }
    print_ssh_tunnel_instructions(port);
    println!("Visit the following URL to continue authentication:\n{url}");
    println!("Waiting for Claude authentication callback...");
}

/// `browser.OpenURL`: announces the URL, then starts the platform opener.
fn open_browser(url: &str) -> bool {
    println!("Attempting to open URL in browser: {url}");
    let command = match std::env::consts::OS {
        "macos" => "open",
        "windows" => "explorer",
        _ => "xdg-open",
    };
    std::process::Command::new(command)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// `util.PrintSSHTunnelInstructions`.
// ponytail: Go asks four public IP services for the address first; this prints the
// outbound interface address (no packet is sent), Go's second choice, without
// announcing the login to third parties.
pub(crate) fn print_ssh_tunnel_instructions(port: u16) {
    let ip = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").and_then(|()| s.local_addr()))
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into());
    let border = "=".repeat(80);
    println!("To authenticate from a remote machine, an SSH tunnel may be required.");
    println!("{border}");
    println!("  Run one of the following commands on your local machine (NOT the server):");
    println!();
    println!("  # Standard SSH command (assumes SSH port 22):");
    println!("  ssh -L {port}:127.0.0.1:{port} root@{ip} -p 22");
    println!();
    println!("  # If using an SSH key (assumes SSH port 22):");
    println!("  ssh -i <path_to_your_key> -L {port}:127.0.0.1:{port} root@{ip} -p 22");
    println!();
    println!("  NOTE: If your server's SSH port is not 22, please modify the '-p 22' part accordingly.");
    println!("{border}");
}

fn login_error(message: impl Into<String>) -> ExecError {
    ExecError::local(400, FailureScope::Request, message)
}

/// `ClaudeAuthenticator.Login` + `Manager.Login` persistence, serving the callback on
/// `listener` (from [`bind`]).
pub(crate) async fn login_with(
    auth_dir: &Path,
    listener: tokio::net::TcpListener,
    oauth: OAuth,
    interaction: Interaction,
) -> Result<PathBuf, ExecError> {
    let (verifier, challenge) = pkce()?;
    let state = random_hex(16)?;
    let (results, mut callbacks) = mpsc::channel(1);
    let server = tokio::spawn(serve(listener, results));
    (interaction.show_url)(&authorize_url(&state, &challenge));
    let outcome = wait(&mut callbacks, interaction.manual_delay, interaction.prompt).await;
    let result = async {
        let callback = outcome?;
        // OAuthError.Error and AuthenticationError.Error texts.
        if !callback.error.is_empty() {
            return Err(login_error(if callback.description.is_empty() {
                format!("OAuth error: {}", callback.error)
            } else {
                format!("OAuth error {}: {}", callback.error, callback.description)
            }));
        }
        if callback.state != state {
            return Err(login_error(
                "invalid_state: OAuth state parameter is invalid (caused by: state mismatch)",
            ));
        }
        let patch = oauth.exchange(&callback.code, &state, &verifier).await?;
        let directory = auth_dir.to_owned();
        tokio::task::spawn_blocking(move || write_login(&directory, patch))
            .await
            .map_err(|_| login_error("credential publication failed"))?
    }
    .await;
    // Go stops the server when Login returns; give the browser's /success a moment.
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.abort();
    result
}

/// Go listens on `:port` (every interface, dual stack).
async fn bind(port: u16) -> Result<tokio::net::TcpListener, ExecError> {
    let in_use = || {
        login_error(format!(
            "{PORT_IN_USE}: OAuth callback port is already in use (caused by: port {port} is already in use)"
        ))
    };
    match tokio::net::TcpListener::bind(("::", port)).await {
        Ok(listener) => Ok(listener),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => Err(in_use()),
        Err(_) => tokio::net::TcpListener::bind(("0.0.0.0", port))
            .await
            .map_err(|_| in_use()),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Callback {
    pub code: String,
    pub state: String,
    pub error: String,
    pub description: String,
}

/// The login loop: the first callback, a pasted URL after `delay`, or the 5-minute
/// timeout. An empty paste keeps waiting without asking again.
async fn wait(
    callbacks: &mut mpsc::Receiver<Callback>,
    delay: Duration,
    prompt: Option<Prompt>,
) -> Result<Callback, ExecError> {
    let deadline = tokio::time::sleep(Duration::from_secs(300));
    tokio::pin!(deadline);
    let manual = async {
        let prompt = prompt?;
        tokio::time::sleep(delay).await;
        Some(prompt().await)
    };
    tokio::pin!(manual);
    let mut manual_done = false;
    loop {
        tokio::select! {
            biased;
            Some(callback) = callbacks.recv() => return Ok(callback),
            () = &mut deadline => return Err(login_error(
                "callback_timeout: Timeout waiting for OAuth callback (caused by: timeout waiting for OAuth callback)",
            )),
            input = &mut manual, if !manual_done => {
                manual_done = true;
                let Some(input) = input else { continue };
                let input = input.map_err(|e| login_error(e.to_string()))?;
                match parse_callback_input(&input) {
                    Ok(Some(callback)) => return Ok(callback),
                    Ok(None) => {}
                    Err(message) => return Err(login_error(message)),
                }
            }
        }
    }
}

/// `misc.ParseOAuthCallback`: `Ok(None)` for empty input.
pub(crate) fn parse_callback_input(input: &str) -> Result<Option<Callback>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_owned()
    } else if trimmed.starts_with('?') {
        format!("http://localhost{trimmed}")
    } else if trimmed.contains(['/', '?', '#', ':']) {
        format!("http://{trimmed}")
    } else if trimmed.contains('=') {
        format!("http://localhost/?{trimmed}")
    } else {
        return Err("invalid callback URL".into());
    };
    let url = url::Url::parse(&candidate).map_err(|e| format!("parse {candidate:?}: {e}"))?;
    let query = |pairs: url::form_urlencoded::Parse<'_>, key: &str| {
        pairs
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default()
    };
    let from_query = |key: &str| query(url.query_pairs(), key);
    let (mut code, mut state) = (from_query("code"), from_query("state"));
    let (mut error, mut description) = (from_query("error"), from_query("error_description"));
    if let Some(fragment) = url.fragment().filter(|f| !f.is_empty()) {
        let from_fragment = |key: &str| query(url::form_urlencoded::parse(fragment.as_bytes()), key);
        for (field, key) in [
            (&mut code, "code"),
            (&mut state, "state"),
            (&mut error, "error"),
            (&mut description, "error_description"),
        ] {
            if field.is_empty() {
                *field = from_fragment(key);
            }
        }
    }
    if !code.is_empty()
        && state.is_empty()
        && let Some((head, tail)) = code.split_once('#')
    {
        (code, state) = (head.to_owned(), tail.to_owned());
    }
    if error.is_empty() && !description.is_empty() {
        error = std::mem::take(&mut description);
    }
    if code.is_empty() && error.is_empty() {
        return Err("callback URL missing code".into());
    }
    Ok(Some(Callback {
        code,
        state,
        error,
        description,
    }))
}

/// Go's `OAuthServer` mux. One request per connection (`Connection: close`).
async fn serve(listener: tokio::net::TcpListener, results: mpsc::Sender<Callback>) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            continue;
        };
        let results = results.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), respond(socket, results)).await;
        });
    }
}

async fn respond(mut socket: tokio::net::TcpStream, results: mpsc::Sender<Callback>) -> std::io::Result<()> {
    let mut head = Vec::new();
    let mut byte = [0];
    while head.len() < 8192 && !head.ends_with(b"\r\n\r\n") {
        if socket.read(&mut byte).await? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&head);
    let mut parts = line.lines().next().unwrap_or_default().split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    let (status, content_type, extra, body) = route(method, target, &results);
    let reason = match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "Method Not Allowed",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    response.push_str(&body);
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

/// Status, Content-Type, extra header lines and body, as Go's handlers write them.
fn route(method: &str, target: &str, results: &mpsc::Sender<Callback>) -> (u16, &'static str, String, String) {
    const TEXT: &str = "text/plain; charset=utf-8";
    const HTML: &str = "text/html; charset=utf-8";
    let nosniff = || "X-Content-Type-Options: nosniff\r\n".to_owned();
    let url = url::Url::parse(&format!("http://localhost{target}")).ok();
    let path = url.as_ref().map(url::Url::path).unwrap_or_default();
    let get = |key: &str| {
        url.as_ref()
            .and_then(|u| u.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned()))
            .unwrap_or_default()
    };
    // sendResult: the first result wins; later ones are dropped.
    let send = |callback: Callback| {
        let _ = results.try_send(callback);
    };
    match path {
        "/callback" if method != "GET" => (405, TEXT, nosniff(), "Method not allowed\n".into()),
        "/callback" => {
            let (code, state, error) = (get("code"), get("state"), get("error"));
            if !error.is_empty() {
                let body = format!("OAuth error: {error}\n");
                send(Callback {
                    error,
                    ..Default::default()
                });
                (400, TEXT, nosniff(), body)
            } else if code.is_empty() {
                send(Callback {
                    error: "no_code".into(),
                    ..Default::default()
                });
                (400, TEXT, nosniff(), "No authorization code received\n".into())
            } else if state.is_empty() {
                send(Callback {
                    error: "no_state".into(),
                    ..Default::default()
                });
                (400, TEXT, nosniff(), "No state parameter received\n".into())
            } else {
                send(Callback {
                    code,
                    state,
                    ..Default::default()
                });
                let body = if method == "GET" || method == "HEAD" {
                    "<a href=\"/success\">Found</a>.\n\n".into()
                } else {
                    String::new()
                };
                (302, HTML, "Location: /success\r\n".into(), body)
            }
        }
        "/success" => {
            let setup = get("setup_required") == "true";
            let mut platform = get("platform_url");
            if platform.is_empty() {
                platform = "https://console.anthropic.com/".into();
            }
            (200, HTML, String::new(), success_html(setup, &platform))
        }
        _ => (404, TEXT, nosniff(), "404 page not found\n".into()),
    }
}

/// `generateSuccessHTML`. The platform URL is HTML-escaped: Go inserts the query value
/// verbatim, which lets any local page inject markup into this origin. Ordinary URLs
/// render identically.
fn success_html(setup_required: bool, platform_url: &str) -> String {
    let escaped = platform_url
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&#34;")
        .replace('\'', "&#39;");
    let notice = if setup_required {
        SETUP_NOTICE_HTML.replace("{{PLATFORM_URL}}", &escaped)
    } else {
        String::new()
    };
    SUCCESS_HTML
        .replace("{{PLATFORM_URL}}", &escaped)
        .replacen("{{SETUP_NOTICE}}", &notice, 1)
}

/// `CredentialFileName`.
pub(crate) fn credential_file_name(email: &str, organization: &str, account: &str) -> String {
    let email = email.trim();
    let identity = Some(organization.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| account.trim());
    if identity.is_empty() {
        return format!("claude-{email}.json");
    }
    let digest = Sha256::digest(identity.as_bytes());
    let hash: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
    format!("claude-{hash}-{email}.json")
}

/// `IsAuthTokenPayloadKey`: never inherited from an existing file.
fn token_payload_key(key: &str) -> bool {
    matches!(
        key.trim().to_lowercase().as_str(),
        "access_token"
            | "refresh_token"
            | "id_token"
            | "session_id"
            | "expired"
            | "last_refresh"
            | "expires_in"
            | "timestamp"
            | "token_type"
            | "user_code"
            | "verification_uri"
            | "verification_uri_complete"
    )
}

/// `MergeExistingAuthMetadata`: fills keys the new credential lacks.
fn merge_existing(target: &mut BTreeMap<String, Value>, existing: &serde_json::Map<String, Value>) {
    for (key, value) in existing {
        if !token_payload_key(key) && !target.contains_key(key) {
            target.insert(key.clone(), value.clone());
        }
    }
}

fn read_json(path: &Path) -> Option<serde_json::Map<String, Value>> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn json_files(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            json_files(&path, out);
        } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("json")) {
            out.push(path);
        }
    }
}

/// `FindMatchingLegacyCredential` over the auth directory.
fn legacy_credential(directory: &Path, target_name: &str, target: &BTreeMap<String, Value>) -> Option<PathBuf> {
    let field = |key: &str| {
        target
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let (email, organization, account) = (field("email"), field("organization_uuid"), field("account_uuid"));
    if email.is_empty()
        || (organization.is_empty() && account.is_empty())
        || !target_name.eq_ignore_ascii_case(&credential_file_name(&email, &organization, &account))
    {
        return None;
    }
    let legacy_name = credential_file_name(&email, "", "");
    let account_name = if !organization.is_empty() && !account.is_empty() {
        credential_file_name(&email, "", &account)
    } else {
        String::new()
    };
    let mut files = Vec::new();
    json_files(directory, &mut files);
    files.sort();
    files.into_iter().find(|path| {
        let base = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let email_legacy = base.eq_ignore_ascii_case(&legacy_name);
        let account_predecessor = !account_name.is_empty() && base.eq_ignore_ascii_case(&account_name);
        if !email_legacy && !account_predecessor {
            return false;
        }
        let Some(candidate) = read_json(path) else {
            return false;
        };
        let get = |key: &str| {
            candidate
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if !get("type").eq_ignore_ascii_case("claude") {
            return false;
        }
        let (c_org, c_account) = (get("organization_uuid"), get("account_uuid"));
        if !organization.is_empty() {
            (!c_org.is_empty() && c_org.eq_ignore_ascii_case(&organization))
                || (c_org.is_empty()
                    && account_predecessor
                    && !c_account.is_empty()
                    && c_account.eq_ignore_ascii_case(&account))
        } else {
            email_legacy && c_org.is_empty() && !c_account.is_empty() && c_account.eq_ignore_ascii_case(&account)
        }
    })
}

/// Go's `json.NewEncoder(f).Encode(map)`: sorted keys, HTML-escaped strings, newline.
fn go_encode(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::String(s) => cpa_common::json::marshal_str(out, s.as_bytes(), true),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                go_encode(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push(b'{');
            for (i, (key, item)) in sorted.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                cpa_common::json::marshal_str(out, key.as_bytes(), true);
                out.push(b':');
                go_encode(item, out);
            }
            out.push(b'}');
        }
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

/// Builds the credential like `CreateTokenStorage` + `Manager.Login` and publishes it
/// atomically (0600, which is stricter than Go's `os.Create`).
pub(crate) fn write_login(directory: &Path, patch: MetadataPatch) -> Result<PathBuf, ExecError> {
    let string = |key: &str| {
        patch
            .set
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let email = string("email");
    if email.is_empty() {
        return Err(login_error("claude token storage missing account information"));
    }
    if email.contains(['/', '\\']) {
        return Err(login_error("invalid credential email filename"));
    }
    let name = credential_file_name(&email, &string("organization_uuid"), &string("account_uuid"));
    let path = directory.join(&name);
    let mut metadata: BTreeMap<String, Value> = patch.set.clone().into_iter().collect();
    for key in &patch.remove {
        metadata.remove(key);
    }
    if let Some(existing) = read_json(&path) {
        merge_existing(&mut metadata, &existing);
    }
    let legacy = legacy_credential(directory, &name, &metadata);
    if let Some(existing) = legacy.as_ref().and_then(|p| read_json(p)) {
        merge_existing(&mut metadata, &existing);
    }
    let mut bytes = Vec::new();
    go_encode(&Value::Object(metadata.into_iter().collect()), &mut bytes);
    bytes.push(b'\n');
    publish(directory, &path, &bytes).map_err(|_| login_error("cannot publish Claude credential"))?;
    if let Some(legacy) = legacy.filter(|l| *l != path) {
        std::fs::remove_file(&legacy).map_err(|_| {
            login_error("cliproxy auth: canonical Claude credential saved but legacy credential cleanup failed")
        })?;
    }
    Ok(path)
}

fn publish(directory: &Path, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut directory_options = std::fs::DirBuilder::new();
    directory_options.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory_options.mode(0o700);
    }
    directory_options.create(directory)?;
    let temporary = directory.join(format!(
        ".claude-{}.tmp",
        random_hex(16).map_err(std::io::Error::other)?
    ));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(directory)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
#[path = "claude_login_tests.rs"]
mod tests;
