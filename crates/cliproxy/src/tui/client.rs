//! Go's management API client (internal/tui/client.go): the v0 routes, a 10 s
//! timeout, the Bearer key, Go's environment proxy rules and redirect limit, and Go's
//! error texts (`HTTP <code>: <body>`).
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value};

/// A JSON object as Go decodes it into `map[string]any`.
pub type Object = Map<String, Value>;

/// Go `NewClientWithBaseURL`'s base URL rules.
pub fn normalize_base_url(raw: &str) -> String {
    let base = raw.trim();
    if base.is_empty() {
        return "http://127.0.0.1:8317".into();
    }
    let lower = base.to_lowercase();
    let base = if lower.starts_with("http://") || lower.starts_with("https://") {
        base.to_owned()
    } else {
        format!("http://{base}")
    };
    base.trim_end_matches('/').to_owned()
}

/// Go `url.QueryEscape`.
pub fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Go `json.Marshal` of a `map[string]any`: keys sorted, `<`, `>` and `&` escaped.
pub fn marshal_map(pairs: Vec<(&str, Value)>) -> String {
    let mut pairs = pairs;
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    let map: Object = pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    Value::Object(map)
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

pub struct Client {
    base_url: String,
    secret: Mutex<String>,
    /// The server's last `X-CPA-VERSION`.
    version: Mutex<String>,
    http: wreq::Client,
}

impl Client {
    pub fn new(base_url: &str, secret: &str) -> Self {
        Client {
            base_url: normalize_base_url(base_url),
            secret: Mutex::new(secret.trim().to_owned()),
            version: Mutex::new(String::new()),
            http: cpa_exec::proxy::default_client(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The server answered as cliproxy-rs (its `X-CPA-VERSION`), not Go CLIProxyAPI.
    pub fn is_cliproxy_rs(&self) -> bool {
        self.version
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .starts_with("cliproxy-rs")
    }

    /// Go `SetSecretKey`.
    pub fn set_secret_key(&self, secret: &str) {
        *self.secret.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = secret.trim().to_owned();
    }

    /// Go `doRequest`: the status and the whole body, with `http.Client`'s rules: at
    /// most ten requests across redirects, 301-303 turn into a body-less GET, the
    /// Bearer key follows only to the same host or a subdomain, and the ten-second
    /// timeout covers the whole exchange. Errors read like Go's `url.Error`, with a
    /// password in the URL shown as `***`.
    // ponytail: the transport cause is the OS error text ("Connection refused (os error
    // 111)") where Go prints its dialer's ("dial tcp ...: connect: connection refused").
    async fn request(&self, method: wreq::Method, path: &str, body: Option<String>) -> Result<(u16, Vec<u8>), String> {
        let url = format!("{}{path}", self.base_url);
        let mut op = method.as_str().to_lowercase();
        op[..1].make_ascii_uppercase();
        // Go's url.Error names the request that failed (its password shown as ***).
        let fail_at = |at: &str, cause: &str| format!("{op} {}: {cause}", cpa_common::gostr::quote(at));
        let secret = self
            .secret
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let headers_seen = std::sync::atomic::AtomicBool::new(false);
        // The request in flight, for a timeout's message (Go names the current hop).
        let in_flight = std::sync::Mutex::new(redact_url(&url));
        let exchange = async {
            let initial = url::Url::parse(&url).map_err(|e| fail_at(&redact_url(&url), &e.to_string()))?;
            let (mut current, mut method, mut body) = (initial.clone(), method.clone(), body);
            let (mut with_auth, mut referer) = (!secret.is_empty(), None::<String>);
            let mut sent = 0;
            let res = loop {
                // wreq turns URL userinfo into Basic auth; Go does so only when no
                // Authorization header is set, so the Bearer key replaces it.
                let mut target = current.clone();
                if with_auth {
                    let _ = target.set_username("");
                    let _ = target.set_password(None);
                }
                let mut req = self
                    .http
                    .request(method.clone(), target.as_str())
                    .redirect(wreq::redirect::Policy::none());
                if with_auth {
                    req = req.header("Authorization", format!("Bearer {secret}"));
                }
                if let Some(body) = &body {
                    req = req.header("Content-Type", "application/json").body(body.clone());
                }
                if let Some(referer) = &referer {
                    req = req.header("Referer", referer.clone());
                }
                let shown = redact_url(current.as_str());
                *in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = shown.clone();
                let res = req.send().await.map_err(|e| fail_at(&shown, &cause(&e)))?;
                sent += 1;
                let location = res
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let status = res.status().as_u16();
                if !matches!(status, 301 | 302 | 303 | 307 | 308) || location.is_empty() {
                    break res;
                }
                let next = current.join(&location).map_err(|e| {
                    let quoted = cpa_common::gostr::quote(&location);
                    fail_at(&shown, &format!("failed to parse Location header {quoted}: {e}"))
                })?;
                if sent >= 10 {
                    // Go names the Location value here.
                    return Err(fail_at(&location, "stopped after 10 redirects"));
                }
                // Go builds the next request before draining, so a timeout from here on
                // names it.
                *in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = redact_url(next.as_str());
                // Go drains a small or unsized redirect body (2 KiB at most) before
                // following it.
                if res.content_length().is_none_or(|n| n <= 2048) {
                    use futures_util::StreamExt;
                    let mut body = res.bytes_stream();
                    let mut drained = 0;
                    while drained < 2048 {
                        match body.next().await {
                            Some(Ok(chunk)) => drained += chunk.len(),
                            _ => break,
                        }
                    }
                }
                if status <= 303 {
                    body = None;
                    if method != wreq::Method::GET && method != wreq::Method::HEAD {
                        method = wreq::Method::GET;
                    }
                }
                if with_auth && !same_site(&initial, &next) {
                    with_auth = false;
                }
                referer = (!(current.scheme() == "https" && next.scheme() == "http")).then(|| {
                    let mut last = current.clone();
                    let _ = last.set_username("");
                    let _ = last.set_password(None);
                    last.to_string()
                });
                current = next;
            };
            headers_seen.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(version) = res.headers().get("x-cpa-version").and_then(|v| v.to_str().ok()) {
                *self.version.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = version.to_owned();
            }
            let status = res.status().as_u16();
            let data = res.bytes().await.map_err(|e| cause(&e))?;
            Ok((status, data.to_vec()))
        };
        match tokio::time::timeout(Duration::from_secs(10), exchange).await {
            Ok(result) => result,
            Err(_) if headers_seen.load(std::sync::atomic::Ordering::Relaxed) => {
                Err("context deadline exceeded (Client.Timeout or context cancellation while reading body)".into())
            }
            Err(_) => {
                let at = in_flight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                Err(fail_at(
                    &at,
                    "context deadline exceeded (Client.Timeout exceeded while awaiting headers)",
                ))
            }
        }
    }

    /// Go `get`/`put`/`patch`: a status of 400 or more is `HTTP <code>: <body>`.
    async fn checked(&self, method: wreq::Method, path: &str, body: Option<String>) -> Result<Vec<u8>, String> {
        let (code, data) = self.request(method, path, body).await?;
        if code >= 400 {
            let text = String::from_utf8_lossy(cpa_common::gostr::trim_space(&data)).into_owned();
            return Err(format!("HTTP {code}: {text}"));
        }
        Ok(data)
    }

    /// Go `getJSON`.
    pub async fn get_json(&self, path: &str) -> Result<Object, String> {
        let data = self.checked(wreq::Method::GET, path, None).await?;
        match serde_json::from_slice::<Value>(&data).map_err(|e| json_error(&data, &e))? {
            Value::Object(o) => Ok(o),
            Value::Null => Ok(Object::new()),
            other => Err(type_error(&other, "map[string]interface {}")),
        }
    }

    /// Go `postJSON`: a failure is `HTTP <code>` only.
    async fn post_json(&self, path: &str, body: String) -> Result<(), String> {
        let (code, _) = self.request(wreq::Method::POST, path, Some(body)).await?;
        if code >= 400 {
            return Err(format!("HTTP {code}"));
        }
        Ok(())
    }

    /// A DELETE whose failure is Go's `delete failed (HTTP <code>)`.
    async fn delete(&self, path: &str, failure: &str) -> Result<(), String> {
        let (code, _) = self.request(wreq::Method::DELETE, path, None).await?;
        if code >= 400 {
            return Err(failure.replace("{code}", &code.to_string()));
        }
        Ok(())
    }

    pub async fn get_config(&self) -> Result<Object, String> {
        self.get_json("/v0/management/config").await
    }

    pub async fn get_auth_files(&self) -> Result<Vec<Object>, String> {
        extract_list(&self.get_json("/v0/management/auth-files").await?, "files")
    }

    pub async fn delete_auth_file(&self, name: &str) -> Result<(), String> {
        let path = format!("/v0/management/auth-files?name={}", query_escape(name));
        self.delete(&path, "delete failed (HTTP {code})").await
    }

    pub async fn toggle_auth_file(&self, name: &str, disabled: bool) -> Result<(), String> {
        let body = marshal_map(vec![("name", name.into()), ("disabled", disabled.into())]);
        self.checked(wreq::Method::PATCH, "/v0/management/auth-files/status", Some(body))
            .await
            .map(drop)
    }

    pub async fn patch_auth_file_fields(&self, name: &str, mut fields: Vec<(&str, Value)>) -> Result<(), String> {
        fields.push(("name", name.into()));
        let body = marshal_map(fields);
        self.checked(wreq::Method::PATCH, "/v0/management/auth-files/fields", Some(body))
            .await
            .map(drop)
    }

    pub async fn refresh_auth_file(&self, name: &str) -> Result<(), String> {
        let body = marshal_map(vec![("name", name.into())]);
        self.post_json("/v0/management/auth-files/refresh", body).await
    }

    /// Go `GetLogs`: new lines and the latest timestamp (never below `after`).
    pub async fn get_logs(&self, after: i64, limit: i64) -> Result<(Vec<String>, i64), String> {
        // url.Values.Encode sorts the keys.
        let mut query = Vec::new();
        if after > 0 {
            query.push(format!("after={after}"));
        }
        if limit > 0 {
            query.push(format!("limit={limit}"));
        }
        let mut path = "/v0/management/logs".to_owned();
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        let wrapper = self.get_json(&path).await?;
        let lines = strings(wrapper.get("lines"))?;
        let latest = match wrapper.get("latest-timestamp").and_then(Value::as_f64) {
            Some(f) => (f as i64).max(after),
            None => after,
        };
        Ok((lines, latest))
    }

    pub async fn get_api_keys(&self) -> Result<Vec<String>, String> {
        strings(self.get_json("/v0/management/api-keys").await?.get("api-keys"))
    }

    /// Go `AddAPIKey`. Go sends `{"old": null, "new": key}`, which Go's own
    /// `patchStringList` rejects (`400 missing fields`), so adding never worked there.
    /// `old` = `new` appends the key when it is absent and changes nothing when present.
    pub async fn add_api_key(&self, key: &str) -> Result<(), String> {
        let body = marshal_map(vec![("old", key.into()), ("new", key.into())]);
        self.checked(wreq::Method::PATCH, "/v0/management/api-keys", Some(body))
            .await
            .map(drop)
    }

    pub async fn edit_api_key(&self, index: usize, value: &str) -> Result<(), String> {
        let body = marshal_map(vec![("index", index.into()), ("value", value.into())]);
        self.checked(wreq::Method::PATCH, "/v0/management/api-keys", Some(body))
            .await
            .map(drop)
    }

    pub async fn delete_api_key(&self, index: usize) -> Result<(), String> {
        let path = format!("/v0/management/api-keys?index={index}");
        self.delete(&path, "delete failed (HTTP {code})").await
    }

    /// Go `getWrappedKeyList`.
    pub async fn get_key_list(&self, route: &str) -> Result<Vec<Object>, String> {
        extract_list(&self.get_json(&format!("/v0/management/{route}")).await?, route)
    }

    /// Go `GetAuthStatus`: the status and error strings.
    pub async fn get_auth_status(&self, state: &str) -> Result<(String, String), String> {
        let path = format!("/v0/management/get-auth-status?state={}", query_escape(state));
        let wrapper = self.get_json(&path).await?;
        let text = |k: &str| wrapper.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
        Ok((text("status"), text("error")))
    }

    /// Go `CancelAuthSession`.
    pub async fn cancel_auth_session(&self, state: &str) -> Result<(), String> {
        let state = state.trim();
        if state.is_empty() {
            return Ok(());
        }
        let path = format!("/v0/management/oauth-session?state={}", query_escape(state));
        self.delete(&path, "HTTP {code}").await
    }

    /// Go `PutBoolField`, `PutIntField` and `PutStringField`.
    pub async fn put_field(&self, path: &str, value: Value) -> Result<(), String> {
        let body = marshal_map(vec![("value", value)]);
        self.checked(wreq::Method::PUT, &format!("/v0/management/{path}"), Some(body))
            .await
            .map(drop)
    }

    /// The v0 OAuth callback submission (Go's `submitCallback`).
    pub async fn post_oauth_callback(&self, provider: &str, redirect_url: &str, state: &str) -> Result<(), String> {
        let body = marshal_map(vec![
            ("provider", provider.into()),
            ("redirect_url", redirect_url.into()),
            ("state", state.into()),
        ]);
        self.post_json("/v0/management/oauth-callback", body).await
    }
}

/// The innermost cause of a request error.
fn cause(e: &wreq::Error) -> String {
    let mut cause: &dyn std::error::Error = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    cause.to_string()
}

/// Go `stripPassword`: a password in the URL's userinfo becomes `***`.
fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("***"));
            u.to_string()
        }
        _ => raw.to_owned(),
    }
}

/// Go `shouldCopyHeaderOnRedirect`: the same host name, or a subdomain of it.
fn same_site(initial: &url::Url, next: &url::Url) -> bool {
    let (parent, host) = (
        initial.host_str().unwrap_or_default(),
        next.host_str().unwrap_or_default(),
    );
    host == parent
        || (!host.contains([':', '%'])
            && host.len() > parent.len()
            && host.ends_with(parent)
            && host.as_bytes()[host.len() - parent.len() - 1] == b'.')
}

/// The JSON kind encoding/json names in an `UnmarshalTypeError`.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// encoding/json's `UnmarshalTypeError` text.
fn type_error(v: &Value, target: &str) -> String {
    format!("json: cannot unmarshal {} into Go value of type {target}", kind(v))
}

/// encoding/json's `quoteChar`: the byte as a rune, `strconv`-quoted in single quotes.
fn quote_char(b: u8) -> String {
    match b {
        b'\'' => "'\\''".into(),
        b'"' => "'\"'".into(),
        b => {
            let q = cpa_common::gostr::quote(char::from(b).to_string());
            format!("'{}'", &q[1..q.len() - 1])
        }
    }
}

/// What Go's scanner says when the input ends inside a top-level literal or number
/// (`scanner.eof` feeds it a space); `None` where it says "unexpected end of JSON input".
fn eof_error(value: &[u8]) -> Option<String> {
    for word in ["true", "false", "null"] {
        if !value.is_empty() && value.len() < word.len() && word.as_bytes().starts_with(value) {
            let expecting = word.as_bytes()[value.len()] as char;
            return Some(format!(
                "invalid character ' ' in literal {word} (expecting '{expecting}')"
            ));
        }
    }
    let numeric = value
        .iter()
        .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'));
    let context = match (value.last()?, value.len()) {
        (b'-', 1) => "in numeric literal",
        (b'.', _) => "after decimal point in numeric literal",
        (b'e' | b'E' | b'+' | b'-', _) => "in exponent of numeric literal",
        _ => return None,
    };
    numeric.then(|| format!("invalid character ' ' {context}"))
}

/// encoding/json's syntax error text for the common cases.
// ponytail: an error inside a document keeps serde's wording.
fn json_error(data: &[u8], e: &serde_json::Error) -> String {
    let space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    let start = data.iter().position(|b| !space(b)).unwrap_or(data.len());
    let value = &data[start..];
    let Some(&first) = value.first() else {
        return "unexpected end of JSON input".into();
    };
    if !matches!(first, b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n') {
        return format!("invalid character {} looking for beginning of value", quote_char(first));
    }
    if e.is_eof() {
        let end = value.iter().rposition(|b| !space(b)).map_or(0, |i| i + 1);
        return eof_error(&value[..end]).unwrap_or_else(|| "unexpected end of JSON input".into());
    }
    if e.to_string().starts_with("trailing characters") {
        let mut stream = serde_json::Deserializer::from_slice(data).into_iter::<Value>();
        let _ = stream.next();
        if let Some(&b) = data[stream.byte_offset()..].iter().find(|b| !space(b)) {
            return format!("invalid character {} after top-level value", quote_char(b));
        }
    }
    e.to_string()
}

/// Go `extractList`: a missing or null member is no list; elements must be objects
/// (a null element is an empty map).
fn extract_list(wrapper: &Object, key: &str) -> Result<Vec<Object>, String> {
    match wrapper.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Object(o) => Ok(o.clone()),
                Value::Null => Ok(Object::new()),
                other => Err(type_error(other, "map[string]interface {}")),
            })
            .collect(),
        Some(other) => Err(type_error(other, "[]map[string]interface {}")),
    }
}

/// A JSON `[]string` as Go decodes it (null elements are empty strings).
fn strings(v: Option<&Value>) -> Result<Vec<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => Ok(s.clone()),
                Value::Null => Ok(String::new()),
                other => Err(type_error(other, "string")),
            })
            .collect(),
        Some(other) => Err(type_error(other, "[]string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values: Go 1.26 json.Unmarshal into map[string]any.
    #[test]
    fn json_errors_read_like_go() {
        let err = |b: &[u8]| {
            let e = serde_json::from_slice::<Value>(b).unwrap_err();
            json_error(b, &e)
        };
        assert_eq!(err(b""), "unexpected end of JSON input");
        assert_eq!(err(b" {\"a\":"), "unexpected end of JSON input");
        assert_eq!(err(b"<html>"), "invalid character '<' looking for beginning of value");
        assert_eq!(err(b"{} x"), "invalid character 'x' after top-level value");
        assert_eq!(err(b"'a'"), "invalid character '\\'' looking for beginning of value");
        assert_eq!(err(b"tru"), "invalid character ' ' in literal true (expecting 'e')");
        assert_eq!(err(b"n"), "invalid character ' ' in literal null (expecting 'u')");
        assert_eq!(err(b"-"), "invalid character ' ' in numeric literal");
        assert_eq!(
            err(b"1."),
            "invalid character ' ' after decimal point in numeric literal"
        );
        assert_eq!(err(b"1e+"), "invalid character ' ' in exponent of numeric literal");
        assert_eq!(err(b"[1,"), "unexpected end of JSON input");
        assert_eq!(
            err("é".as_bytes()),
            "invalid character 'Ã' looking for beginning of value"
        );
        assert_eq!(err("{} é".as_bytes()), "invalid character 'Ã' after top-level value");
        assert_eq!(
            type_error(&serde_json::json!([1]), "map[string]interface {}"),
            "json: cannot unmarshal array into Go value of type map[string]interface {}"
        );
    }

    /// A refused connection reads like Go's url.Error, and a URL password never shows.
    #[tokio::test]
    async fn request_errors_hide_url_passwords() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let client = Client::new(&format!("http://user:fake-url-password@127.0.0.1:{port}"), "k");
        let err = client.get_config().await.unwrap_err();
        assert!(
            err.starts_with(&format!(
                "Get \"http://user:***@127.0.0.1:{port}/v0/management/config\": "
            )),
            "{err}"
        );
        assert!(!err.contains("fake-url-password"));
    }

    /// A one-shot HTTP server answering every request with `reply`; returns its port
    /// and the requests it saw.
    async fn record(reply: String) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                log.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        (port, seen)
    }

    // Go TestClient_RemoteServerInteraction and TestNewAppWithBaseURL.
    #[tokio::test]
    async fn remote_get_config_sends_path_and_bearer() {
        let (port, seen) =
            record("HTTP/1.1 200 OK\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"status\":\"ok\"}".into())
                .await;
        let client = Client::new(&format!("http://127.0.0.1:{port}"), "remote-secret-key");
        let cfg = client.get_config().await.unwrap();
        assert_eq!(cfg.get("status"), Some(&Value::from("ok")));
        let request = seen.lock().unwrap()[0].clone();
        assert!(
            request.starts_with("GET /v0/management/config HTTP/1.1\r\n"),
            "{request}"
        );
        assert!(
            request
                .to_lowercase()
                .contains("authorization: bearer remote-secret-key\r\n"),
            "{request}"
        );
        let app = super::super::app::App::new("https://proxy.example.com", "secret", None);
        assert_eq!(app.base_url(), "https://proxy.example.com");
        // Go TestNewClient_BackwardsCompatibility: the standalone client's local URL.
        assert_eq!(
            normalize_base_url(&format!("http://127.0.0.1:{}", 8317)),
            "http://127.0.0.1:8317"
        );
    }

    /// URL userinfo never becomes a second Authorization header next to the Bearer key
    /// (Go sets Basic auth only when Authorization is unset).
    #[tokio::test]
    async fn bearer_replaces_url_basic_auth() {
        let (port, seen) = record("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()).await;
        let client = Client::new(&format!("http://u:fake-url-password@127.0.0.1:{port}"), "fake-bearer");
        client.get_config().await.unwrap();
        let request = seen.lock().unwrap()[0].to_lowercase();
        assert_eq!(request.matches("authorization:").count(), 1, "{request}");
        assert!(request.contains("authorization: bearer fake-bearer"));
    }

    /// Errors name the request that failed: the redirect target, or the Location value
    /// after ten redirects.
    #[tokio::test]
    async fn redirect_errors_name_the_failing_hop() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = closed.local_addr().unwrap().port();
        drop(closed);
        let (port, _) = record(format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{dead}/gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ))
        .await;
        let err = Client::new(&format!("http://127.0.0.1:{port}"), "k")
            .get_config()
            .await
            .unwrap_err();
        assert!(
            err.starts_with(&format!("Get \"http://127.0.0.1:{dead}/gone\": ")),
            "{err}"
        );
        let (port, seen) =
            record("HTTP/1.1 302 Found\r\nLocation: /loop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())
                .await;
        let err = Client::new(&format!("http://127.0.0.1:{port}"), "k")
            .get_config()
            .await
            .unwrap_err();
        assert_eq!(err, "Get \"/loop\": stopped after 10 redirects");
        assert_eq!(seen.lock().unwrap().len(), 10);
    }

    /// Go's redirect rules: a PUT that meets a 302 becomes a body-less GET, and the
    /// Bearer key follows to the same host on another port but not to another host.
    #[tokio::test]
    async fn redirects_follow_go_http_client() {
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let serve = |reply: fn(u16) -> String| {
            let seen = seen.clone();
            async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = listener.local_addr().unwrap().port();
                tokio::spawn(async move {
                    while let Ok((mut sock, _)) = listener.accept().await {
                        let mut buf = vec![0; 4096];
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        seen.lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                        let _ = sock.write_all(reply(port).as_bytes()).await;
                    }
                });
                port
            }
        };
        let target = serve(|_| "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".into()).await;
        static TARGET: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
        TARGET.set(target).unwrap();
        let start = serve(|_| {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{}/moved\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                TARGET.get().unwrap()
            )
        })
        .await;
        let client = Client::new(&format!("http://127.0.0.1:{start}"), "fake-bearer");
        client.put_field("debug", Value::Bool(true)).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen[0].starts_with("PUT /v0/management/debug ") && seen[0].contains("{\"value\":true}"),
            "{}",
            seen[0]
        );
        assert!(seen[1].starts_with("GET /moved "), "{}", seen[1]);
        assert!(!seen[1].contains("value"), "the body is dropped: {}", seen[1]);
        assert!(
            seen[1].to_lowercase().contains("authorization: bearer fake-bearer"),
            "same host keeps the key"
        );
        assert!(
            seen[1]
                .to_lowercase()
                .contains(&format!("referer: http://127.0.0.1:{start}/v0/management/debug"))
        );
    }

    // Expected values: Go TestNewClientWithBaseURL and net/url.
    #[test]
    fn base_urls_follow_go() {
        for (input, want) in [
            ("http://192.168.1.100:8317", "http://192.168.1.100:8317"),
            ("https://proxy.example.com/", "https://proxy.example.com"),
            ("HTTPS://proxy.example.com/", "HTTPS://proxy.example.com"),
            ("proxy.example.com:9000", "http://proxy.example.com:9000"),
            ("https://proxy.example.com/prefix/", "https://proxy.example.com/prefix"),
            ("", "http://127.0.0.1:8317"),
            ("  ", "http://127.0.0.1:8317"),
        ] {
            assert_eq!(normalize_base_url(input), want, "{input:?}");
        }
        assert_eq!(query_escape("a b/ü~.json"), "a+b%2F%C3%BC~.json");
        assert_eq!(
            redact_url("http://user:fake-url-password@127.0.0.1:1/v0/management/config"),
            "http://user:***@127.0.0.1:1/v0/management/config"
        );
        let host = |s: &str| url::Url::parse(s).unwrap();
        assert!(same_site(&host("http://a.example:1/x"), &host("http://a.example:2/y")));
        assert!(same_site(
            &host("http://example.invalid/"),
            &host("https://sub.example.invalid/")
        ));
        assert!(!same_site(
            &host("http://example.invalid/"),
            &host("http://badexample.invalid/")
        ));
        assert_eq!(
            marshal_map(vec![("old", Value::Null), ("new", "k<&>".into())]),
            r#"{"new":"k\u003c\u0026\u003e","old":null}"#
        );
    }
}
