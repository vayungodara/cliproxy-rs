//! HTTP plumbing shared by the Kimi, Meta and Devin executors: Go net/http header order,
//! per-credential custom headers, proxy-aware clients and Go's credential expiry rules.
//!
//! ponytail: provider-neutral (M4-0029 proxies, M4-0030 custom headers, M4-0027 refresh
//! timing); hoist when the shared executor helpers land. TLS is wreq's default profile,
//! not Go crypto/tls; these upstreams are not known to fingerprint ClientHello.

use std::collections::HashMap;
use std::sync::Mutex;

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, FailureScope};
use http::HeaderMap;
use serde_json::Value;

/// Go `runtime.GOOS`.
pub(crate) fn go_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// Go `runtime.GOARCH`.
pub(crate) fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" => "ppc64",
        other => other,
    }
}

/// Go `os.Hostname()`, or `None` when it fails.
pub(crate) fn hostname() -> Option<String> {
    #[cfg(target_os = "linux")]
    if let Ok(name) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let name = name.trim_end_matches('\n');
        if !name.is_empty() {
            return Some(name.to_owned());
        }
    }
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes into the provided buffer.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

/// Version reported where Go sends `buildinfo.Version`.
pub(crate) const BUILD_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Go `textproto.CanonicalMIMEHeaderKey`.
pub(crate) fn canonical_header(name: &str) -> String {
    let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
    if !name.bytes().all(token) {
        return name.to_owned();
    }
    let mut upper = true;
    name.bytes()
        .map(|b| {
            let c = if upper {
                b.to_ascii_uppercase()
            } else {
                b.to_ascii_lowercase()
            };
            upper = b == b'-';
            c as char
        })
        .collect()
}

/// Headers in the order Go's HTTP/1.1 client writes them: Host, User-Agent, Content-Length,
/// the remaining request headers sorted by canonical name, then the transport's
/// Accept-Encoding.
#[derive(Clone)]
pub(crate) struct GoHeaders {
    headers: Vec<(String, String)>,
}

impl GoHeaders {
    pub(crate) fn new() -> Self {
        Self { headers: Vec::new() }
    }

    /// `http.Header.Set`.
    pub(crate) fn set(&mut self, name: &str, value: impl Into<String>) {
        let name = canonical_header(name);
        let value = value.into();
        match self.headers.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => self.headers.push((name, value)),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        let name = canonical_header(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    pub(crate) fn take(&mut self, name: &str) -> Option<String> {
        let name = canonical_header(name);
        let pos = self.headers.iter().position(|(n, _)| *n == name)?;
        Some(self.headers.remove(pos).1)
    }

    /// Applies the headers with Go's wire order and spelling. Returns whether the transport
    /// asked for gzip itself (no explicit Accept-Encoding or Range), which is the only case
    /// Go decodes transparently.
    pub(crate) fn apply(mut self, builder: wreq::RequestBuilder) -> (wreq::RequestBuilder, bool) {
        let auto_gzip = self.get("Accept-Encoding").is_none() && self.get("Range").is_none();
        // A custom Host header becomes the request Host (util.applyCustomHeaders).
        let host = self.take("Host").filter(|h| !h.is_empty());
        self.take("Content-Length");
        let mut order = wreq::header::OrigHeaderMap::new();
        order.insert("Host");
        order.insert("User-Agent");
        order.insert("Content-Length");
        let mut rest: Vec<&String> = self
            .headers
            .iter()
            .map(|(n, _)| n)
            .filter(|n| *n != "User-Agent")
            .collect();
        rest.sort();
        for name in rest {
            order.insert(name.clone());
        }
        if auto_gzip {
            order.insert("Accept-Encoding");
            self.headers.push(("Accept-Encoding".into(), "gzip".into()));
        }
        let mut builder = builder.orig_headers(order);
        if let Some(host) = host {
            builder = builder.header("Host", host);
        }
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }
        (builder, auto_gzip)
    }
}

/// An upstream response with Go net/http semantics: the body is decoded only for gzip the
/// transport requested itself, and is never framed or buffered here.
pub(crate) struct Upstream {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>,
}

/// Largest error body kept from an upstream rejection.
// ponytail: Go reads error bodies without a bound; 16 MiB keeps a hostile upstream from
// exhausting a small VPS while covering any real provider error.
pub(crate) const MAX_ERROR_BODY: usize = 16 << 20;

fn body_error(_: std::io::Error) -> ExecError {
    ExecError::local(502, FailureScope::Transport, "upstream request failed")
}

/// POSTs `body` the way Go's `http.Client.Do` does, following redirects itself: up to ten
/// hops, 301/302/303 turn a POST into a body-less GET, 307/308 resend the body, the initial
/// headers are copied to every hop (credentials only within the original domain), and the
/// previous URL becomes `Referer`. `timeout` bounds the whole exchange (auth clients only).
pub(crate) async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: String,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    use futures_util::StreamExt;
    let initial =
        url::Url::parse(url).map_err(|_| ExecError::local(500, FailureScope::Request, "invalid upstream URL"))?;
    let explicit_host = headers.get("Host").map(str::to_owned);
    let explicit_referer = headers.get("Referer").map(str::to_owned);
    let mut current = initial.clone();
    let mut method = wreq::Method::POST;
    let mut include_body = true;
    let mut strip_sensitive = false;
    let mut hop_headers = headers.clone();
    let mut hops = 0;
    let response = loop {
        let mut builder = client.request(method.clone(), current.as_str());
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let (builder, auto_gzip) = hop_headers.clone().apply(builder);
        let builder = if include_body {
            builder.body(body.clone())
        } else {
            builder
        };
        let response = builder.send().await.map_err(crate::upstream::transport_error)?;
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get(http::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        if !matches!(status, 301 | 302 | 303 | 307 | 308) || location.is_empty() {
            break (response, auto_gzip);
        }
        if hops == 10 {
            return Err(ExecError::local(
                500,
                FailureScope::Transport,
                "stopped after 10 redirects",
            ));
        }
        hops += 1;
        let next = current.join(&location).map_err(|_| {
            ExecError::local(
                500,
                FailureScope::Transport,
                format!("failed to parse Location header {location:?}"),
            )
        })?;
        if (301..=303).contains(&status) {
            include_body = false;
            if method != wreq::Method::GET && method != wreq::Method::HEAD {
                method = wreq::Method::GET;
            }
        }
        if !strip_sensitive && initial.host_str() != next.host_str() {
            let (sub, parent) = (
                next.host_str().unwrap_or_default(),
                initial.host_str().unwrap_or_default(),
            );
            let subdomain = !sub.contains([':', '%'])
                && sub.len() > parent.len()
                && sub.ends_with(parent)
                && sub.as_bytes()[sub.len() - parent.len() - 1] == b'.';
            strip_sensitive = sub != parent && !subdomain;
        }
        hop_headers = headers.clone();
        if strip_sensitive {
            for name in [
                "Authorization",
                "Www-Authenticate",
                "Cookie",
                "Cookie2",
                "Proxy-Authorization",
                "Proxy-Authenticate",
            ] {
                hop_headers.take(name);
            }
        }
        if !include_body {
            for name in [
                "Content-Encoding",
                "Content-Language",
                "Content-Location",
                "Content-Type",
            ] {
                hop_headers.take(name);
            }
        }
        // A custom Host survives only a relative Location.
        hop_headers.take("Host");
        if let Some(host) = &explicit_host
            && url::Url::parse(&location).is_err()
        {
            hop_headers.set("Host", host.clone());
        }
        hop_headers.take("Referer");
        if !(current.scheme() == "https" && next.scheme() == "http") {
            let referer = explicit_referer.clone().unwrap_or_else(|| {
                let mut last = current.clone();
                let _ = last.set_username("");
                let _ = last.set_password(None);
                last.to_string()
            });
            hop_headers.set("Referer", referer);
        }
        current = next;
    };
    let (response, auto_gzip) = response;
    let status = response.status().as_u16();
    let mut headers = response.headers().clone();
    let gzip = auto_gzip
        && headers
            .get(http::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("gzip"));
    let stream = response.bytes_stream().map(|r| r.map_err(std::io::Error::other));
    let body = if gzip {
        headers.remove(http::header::CONTENT_ENCODING);
        headers.remove(http::header::CONTENT_LENGTH);
        let reader = tokio::io::BufReader::new(tokio_util::io::StreamReader::new(stream));
        let mut decoder = async_compression::tokio::bufread::GzipDecoder::new(reader);
        decoder.multiple_members(true);
        tokio_util::io::ReaderStream::new(decoder)
            .map(|r| r.map_err(body_error))
            .boxed()
    } else {
        stream.map(|r| r.map_err(body_error)).boxed()
    };
    Ok(Upstream { status, headers, body })
}

/// `io.ReadAll` up to `limit` bytes; with `lossy`, a read error keeps what arrived (Go's
/// error path ignores it).
pub(crate) async fn read_all(
    mut body: futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>,
    limit: usize,
    lossy: bool,
) -> Result<Bytes, ExecError> {
    use futures_util::StreamExt;
    let mut out = bytes::BytesMut::new();
    while out.len() < limit {
        match body.next().await {
            Some(Ok(chunk)) => out.extend_from_slice(&chunk[..chunk.len().min(limit - out.len())]),
            Some(Err(_)) if lossy => break,
            Some(Err(error)) => return Err(error),
            None => break,
        }
    }
    Ok(out.freeze())
}

/// `statusErr{code, msg: body}` with the scheduler hints the shared adapter derives.
pub(crate) async fn status_error(upstream: Upstream) -> ExecError {
    let body = read_all(upstream.body, MAX_ERROR_BODY, true).await.unwrap_or_default();
    let now = std::time::SystemTime::now();
    let retry_after = upstream
        .headers
        .get(http::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|raw| crate::quota::retry_after(raw, now))
        .and_then(|deadline| deadline.duration_since(now).ok());
    // Same status-only classification as upstream::scope_for.
    let scope = match upstream.status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    };
    ExecError {
        status: upstream.status,
        scope,
        body,
        headers: Box::new(upstream.headers),
        retry_after,
        direct: false,
    }
}

/// `bufio.Scanner` with `ScanLines`: one item per line without its `\n`, a trailing `\r`
/// dropped, the final unterminated line included, and Go's error once a line reaches
/// `max` bytes without a newline.
pub(crate) fn lines(
    body: futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>,
    max: usize,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>> {
    use futures_util::StreamExt;
    struct State {
        body: futures_util::stream::BoxStream<'static, Result<Bytes, ExecError>>,
        buf: bytes::BytesMut,
        scanned: usize,
        done: bool,
    }
    let drop_cr = |line: &[u8]| Bytes::copy_from_slice(line.strip_suffix(b"\r").unwrap_or(line));
    futures_util::stream::unfold(
        State {
            body,
            buf: bytes::BytesMut::new(),
            scanned: 0,
            done: false,
        },
        move |mut st| async move {
            loop {
                if let Some(pos) = st.buf[st.scanned..].iter().position(|b| *b == b'\n') {
                    let line = st.buf.split_to(st.scanned + pos + 1);
                    st.scanned = 0;
                    return Some((Ok(drop_cr(&line[..line.len() - 1])), st));
                }
                st.scanned = st.buf.len();
                if st.done {
                    if st.buf.is_empty() {
                        return None;
                    }
                    let line = st.buf.split();
                    st.scanned = 0;
                    return Some((Ok(drop_cr(&line)), st));
                }
                if st.buf.len() >= max {
                    st.done = true;
                    st.buf.clear();
                    st.scanned = 0;
                    return Some((
                        Err(ExecError::local(
                            500,
                            FailureScope::Request,
                            "bufio.Scanner: token too long",
                        )),
                        st,
                    ));
                }
                match st.body.next().await {
                    Some(Ok(chunk)) => st.buf.extend_from_slice(&chunk),
                    Some(Err(error)) => {
                        st.done = true;
                        st.buf.clear();
                        st.scanned = 0;
                        return Some((Err(error), st));
                    }
                    None => st.done = true,
                }
            }
        },
    )
    .boxed()
}

/// ponytail: adapter for `cpa_common::headers` (owner: server thread); swap for the shared
/// helper when it lands.
///
/// `util.ApplyCustomHeadersFromAttrs`: `header:<Name>` attributes and the file's `headers`
/// map (synthesized into attributes by Go). `$Name` copies an inbound header and
/// `$CPA-SESSION-ID` the request session; missing sources omit the header.
pub(crate) fn custom_headers(
    credential: &Credential,
    inbound: &HeaderMap,
    session: Option<&str>,
) -> Vec<(String, String)> {
    let mut configured: Vec<(String, String)> = Vec::new();
    if let Some(Value::Object(map)) = credential.metadata.get("headers") {
        for (name, value) in map {
            if let Some(value) = value.as_str() {
                configured.push((name.trim().to_owned(), value.trim().to_owned()));
            }
        }
    }
    for (key, value) in &credential.attributes {
        if let Some(name) = key.strip_prefix("header:") {
            configured.retain(|(n, _)| n != name.trim());
            configured.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    let session = session.map(str::trim).filter(|s| !s.is_empty());
    let mut out = Vec::new();
    for (name, value) in configured {
        if name.is_empty() || value.is_empty() {
            continue;
        }
        let upper = value.to_ascii_uppercase();
        let resolved = if value.starts_with('$') && value[1..].trim().eq_ignore_ascii_case("CPA-SESSION-ID") {
            match session {
                Some(id) => id.to_owned(),
                None => continue,
            }
        } else if upper.contains("$CPA-SESSION-ID") {
            let Some(id) = session else { continue };
            replace_session(&value, id)
        } else if let Some(var) = value.strip_prefix('$') {
            let var = var.trim();
            match inbound.get(var).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()) {
                Some(v) if !var.is_empty() => v.to_owned(),
                _ => continue,
            }
        } else {
            value
        };
        out.push((name, resolved));
    }
    out
}

fn replace_session(value: &str, session: &str) -> String {
    const TARGET: &str = "$CPA-SESSION-ID";
    let mut out = String::new();
    let mut rest = value;
    while let Some(pos) = rest.to_ascii_uppercase().find(TARGET) {
        out.push_str(&rest[..pos]);
        out.push_str(session);
        rest = &rest[pos + TARGET.len()..];
    }
    out.push_str(rest);
    out
}

/// ponytail: adapter for crates/cpa-exec/src/proxy.rs (owner: Claude thread), together
/// with [`Clients`]; build clients through the shared proxy module when it lands.
///
/// Effective proxy: credential `proxy_url`, then `requests.proxy-url` (helps.effectiveProxyURL).
pub(crate) fn proxy_url(credential: &Credential, cfg: &Config) -> String {
    let from_credential = credential
        .attributes
        .get("proxy_url")
        .map(String::as_str)
        .or_else(|| credential.str("proxy_url"))
        .map(str::trim)
        .unwrap_or_default();
    if !from_credential.is_empty() {
        return from_credential.to_owned();
    }
    cfg.document
        .get("requests")
        .and_then(|r| r.get("proxy-url"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or_default()
        .to_owned()
}

/// Clients keyed by effective proxy. An empty key is the injected default client.
pub(crate) struct Clients {
    default: wreq::Client,
    proxied: Mutex<HashMap<String, wreq::Client>>,
}

impl Clients {
    pub(crate) fn new(default: wreq::Client) -> Self {
        Self {
            default,
            proxied: Mutex::default(),
        }
    }

    /// Go proxyutil: empty inherits (environment proxies), `direct`/`none` bypass every
    /// proxy, http/https/socks5/socks5h URLs are used as-is; anything else falls back to
    /// the default transport like helps.NewProxyAwareHTTPClient.
    pub(crate) fn get(&self, proxy: &str) -> wreq::Client {
        let proxy = proxy.trim();
        if proxy.is_empty() {
            return self.default.clone();
        }
        let direct = proxy.eq_ignore_ascii_case("direct") || proxy.eq_ignore_ascii_case("none");
        let valid = direct
            || url::Url::parse(proxy).is_ok_and(|u| {
                matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h")
                    && u.host_str().is_some_and(|h| !h.is_empty())
            });
        if !valid {
            return self.default.clone();
        }
        let mut cache = self.proxied.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = cache.get(proxy) {
            return client.clone();
        }
        // Redirects are followed by `send`, which reproduces Go's per-hop header rules.
        let builder = wreq::Client::builder().redirect(wreq::redirect::Policy::none());
        let built = if direct {
            builder.no_proxy().build()
        } else {
            wreq::Proxy::all(proxy).and_then(|p| builder.proxy(p).build())
        };
        let Ok(client) = built else {
            return self.default.clone();
        };
        // ponytail: bounded by distinct configured proxies; clear rather than evict.
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(proxy.to_owned(), client.clone());
        client
    }
}

/// Redirects are followed by [`send`], not by wreq.
pub(crate) fn default_client() -> wreq::Client {
    wreq::Client::builder()
        .redirect(wreq::redirect::Policy::none())
        .build()
        .expect("default HTTP client")
}

/// Go `time.Time.Format(time.RFC3339)` in UTC.
pub(crate) fn rfc3339_utc(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Go `time.Now().Format(time.RFC3339)`: local zone, `Z` only at UTC.
pub(crate) fn rfc3339_local_now() -> String {
    chrono::Local::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn normalise_unix(raw: i64) -> Option<DateTime<Utc>> {
    if raw <= 0 {
        return None;
    }
    if raw > 1_000_000_000_000 {
        Utc.timestamp_millis_opt(raw).single()
    } else {
        Utc.timestamp_opt(raw, 0).single()
    }
}

/// Go `parseTimeValue`.
pub(crate) fn parse_time(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            if let Ok(t) = DateTime::parse_from_rfc3339(s) {
                return Some(t.with_timezone(&Utc));
            }
            for layout in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
                if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, layout) {
                    return Some(t.and_utc());
                }
            }
            s.parse::<i64>().ok().and_then(normalise_unix)
        }
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .and_then(normalise_unix),
        _ => None,
    }
}

fn jwt_exp(token: &str) -> Option<DateTime<Utc>> {
    use base64::Engine;
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&payload).ok()?;
    match claims.get("exp")? {
        Value::Number(n) => n.as_f64().filter(|v| *v > 0.0).and_then(|v| normalise_unix(v as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok().filter(|v| *v > 0).and_then(normalise_unix),
        _ => None,
    }
}

fn expiration_from_map(meta: &serde_json::Map<String, Value>) -> Option<DateTime<Utc>> {
    for key in ["expired", "expire", "expires_at", "expiresAt", "expiry", "expires"] {
        if let Some(t) = meta.get(key).and_then(parse_time) {
            return Some(t);
        }
    }
    let seconds = ["expires_in", "expiresIn"].iter().find_map(|k| {
        let v = meta.get(*k)?;
        let n = v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?;
        (n > 0).then_some(n)
    });
    if let Some(seconds) = seconds
        && let Some(issued) = ["timestamp", "issued_at", "issuedAt"]
            .iter()
            .find_map(|k| meta.get(*k).and_then(parse_time))
    {
        return Some(issued + chrono::Duration::seconds(seconds));
    }
    for nested in ["token", "Token"] {
        if let Some(Value::Object(inner)) = meta.get(nested)
            && let Some(t) = expiration_from_map(inner)
        {
            return Some(t);
        }
    }
    None
}

/// Go `Auth.ExpirationTime`: access-token JWT `exp`, then metadata expiry fields.
pub(crate) fn expiration(credential: &Credential) -> Option<DateTime<Utc>> {
    let token = credential
        .str("access_token")
        .filter(|s| !s.is_empty())
        .or_else(|| credential.str("accessToken"))
        .unwrap_or_default();
    jwt_exp(token).or_else(|| expiration_from_map(&credential.metadata))
}

fn last_refresh(credential: &Credential) -> Option<DateTime<Utc>> {
    ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
        .iter()
        .find_map(|k| credential.metadata.get(*k).and_then(parse_time))
        .or_else(|| {
            ["last_refresh", "lastRefresh", "last_refreshed_at", "lastRefreshedAt"]
                .iter()
                .find_map(|k| {
                    credential
                        .attributes
                        .get(*k)
                        .and_then(|v| parse_time(&Value::String(v.clone())))
                })
        })
}

fn preferred_interval(credential: &Credential) -> Option<chrono::Duration> {
    const KEYS: [&str; 4] = [
        "refresh_interval_seconds",
        "refreshIntervalSeconds",
        "refresh_interval",
        "refreshInterval",
    ];
    let seconds = KEYS.iter().find_map(|k| match credential.metadata.get(*k)? {
        Value::Number(n) => n.as_f64().filter(|v| *v > 0.0),
        Value::String(s) => parse_duration_string(s),
        _ => None,
    });
    let seconds = seconds.or_else(|| {
        KEYS.iter()
            .find_map(|k| {
                credential
                    .attributes
                    .get(*k)
                    .and_then(|s| parse_go_duration(s).or_else(|| s.trim().parse().ok()))
            })
            .filter(|v: &f64| *v > 0.0)
    })?;
    chrono::Duration::try_milliseconds((seconds * 1000.0) as i64)
}

/// Go `time.ParseDuration`, in seconds.
pub(crate) fn parse_go_duration(s: &str) -> Option<f64> {
    let mut rest = s;
    let negative = rest.starts_with('-');
    rest = rest.strip_prefix(['-', '+']).unwrap_or(rest);
    if rest == "0" {
        return Some(0.0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(rest.len());
        let number = &rest[..digits];
        if number.is_empty() || number == "." || number.matches('.').count() > 1 {
            return None;
        }
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        rest = &rest[unit_len..];
        total += number.parse::<f64>().ok()? * scale;
    }
    Some(if negative { -total } else { total })
}

/// `parseDurationString`: a Go duration, else plain seconds; non-positive is absent.
fn parse_duration_string(s: &str) -> Option<f64> {
    let s = s.trim();
    parse_go_duration(s)
        .filter(|v| *v > 0.0)
        .or_else(|| s.parse::<f64>().ok().filter(|v| *v > 0.0))
}

/// Go `shouldRefresh` timing for one credential (backoff and lifecycle gates belong to the
/// runtime). `lead` is the provider's SDK refresh lead; `None` means no scheduled refresh.
pub(crate) fn refresh_due(credential: &Credential, lead: Option<chrono::Duration>, now: DateTime<Utc>) -> bool {
    let expiry = expiration(credential);
    if let Some(interval) = preferred_interval(credential) {
        if let Some(expiry) = expiry
            && (expiry <= now || expiry - now <= interval)
        {
            return true;
        }
        return last_refresh(credential).is_none_or(|last| now - last >= interval);
    }
    let Some(lead) = lead else {
        return false;
    };
    if let Some(expiry) = expiry {
        return expiry - now <= lead;
    }
    last_refresh(credential).is_none_or(|last| now - last >= lead)
}

/// A control-plane failure that must not echo upstream bodies (they may contain tokens).
pub(crate) fn auth_error(status: u16, message: impl Into<String>) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(meta: Value) -> Credential {
        Credential::from_file(
            std::path::Path::new("/fake"),
            std::path::Path::new("/fake/a.json"),
            meta.as_object().unwrap().clone(),
        )
        .unwrap()
    }

    #[test]
    fn canonical_header_matches_go() {
        assert_eq!(canonical_header("x-msh-device-id"), "X-Msh-Device-Id");
        assert_eq!(canonical_header("content-TYPE"), "Content-Type");
        assert_eq!(canonical_header("bad header"), "bad header");
    }

    #[test]
    fn custom_headers_resolve_references_and_session() {
        let mut cred = credential(serde_json::json!({"type":"kimi","headers":{"X-Team":" blue ","X-Empty":""}}));
        cred.attributes.insert("header:X-Trace".into(), "$X-Request-Id".into());
        cred.attributes
            .insert("header:X-Session".into(), "s-$cpa-session-id-x".into());
        cred.attributes.insert("header:X-Missing".into(), "$Nope".into());
        let mut inbound = HeaderMap::new();
        inbound.insert("x-request-id", "abc".parse().unwrap());
        let mut got = custom_headers(&cred, &inbound, Some("sess"));
        got.sort();
        assert_eq!(
            got,
            [
                ("X-Session".to_owned(), "s-sess-x".to_owned()),
                ("X-Team".to_owned(), "blue".to_owned()),
                ("X-Trace".to_owned(), "abc".to_owned()),
            ]
        );
    }

    #[test]
    fn refresh_due_uses_jwt_exp_before_metadata_and_lead() {
        let now = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        let lead = Some(chrono::Duration::minutes(5));
        use base64::Engine;
        let exp = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"exp":1790000100}"#);
        let jwt = format!("h.{exp}.s");
        // JWT exp (100s away) wins over a far metadata expiry.
        let c = credential(serde_json::json!({"type":"kimi","access_token":jwt,"expired":"2099-01-01T00:00:00Z"}));
        assert!(refresh_due(&c, lead, now));
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque","expired":"2099-01-01T00:00:00Z"}));
        assert!(!refresh_due(&c, lead, now));
        // No expiry: last refresh decides; none recorded means refresh now.
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque"}));
        assert!(refresh_due(&c, lead, now));
        let c = credential(serde_json::json!({"type":"kimi","access_token":"opaque","last_refresh":1_789_999_900}));
        assert!(!refresh_due(&c, lead, now));
        assert!(!refresh_due(&c, None, now), "no lead means no scheduled refresh");
        // expires_in is relative to timestamp; values above 1e12 are milliseconds.
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","expires_in":200,"timestamp":1_789_999_900_000u64}),
        );
        assert!(refresh_due(&c, lead, now));
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","expires_in":900,"timestamp":1_789_999_900_000u64}),
        );
        assert!(!refresh_due(&c, lead, now));
        // refresh_interval accepts Go compound durations and plain seconds.
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","refresh_interval":"1h30m","last_refresh":1_790_000_000 - 1200}),
        );
        assert!(!refresh_due(&c, lead, now));
        let c = credential(
            serde_json::json!({"type":"kimi","access_token":"o","refresh_interval":"600","last_refresh":1_790_000_000 - 1200}),
        );
        assert!(refresh_due(&c, lead, now));
        assert_eq!(parse_go_duration("1h30m"), Some(5400.0));
        assert_eq!(parse_go_duration("1.5h2.5s"), Some(5402.5));
        assert_eq!(parse_go_duration("300ms"), Some(0.3));
        assert_eq!(parse_go_duration("10"), None);
        assert_eq!(parse_go_duration("h"), None);
    }

    #[tokio::test]
    async fn lines_follow_bufio_scanner() {
        use futures_util::StreamExt;
        let chunks = ["data: a\r\rdata: b", "\r\n\nfinal", ""];
        let body = futures_util::stream::iter(chunks.map(|c| Ok(Bytes::from(c)))).boxed();
        let got: Vec<_> = lines(body, 64).map(|r| r.unwrap()).collect().await;
        assert_eq!(got, ["data: a\r\rdata: b", "", "final"].map(Bytes::from));
        let long = futures_util::stream::iter([Ok(Bytes::from(vec![b'x'; 70]))]).boxed();
        let got: Vec<_> = lines(long, 64).collect().await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].as_ref().unwrap_err().body, "bufio.Scanner: token too long");
        let exact =
            futures_util::stream::iter([Ok(Bytes::from(vec![b'x'; 63])), Ok(Bytes::from_static(b"\n"))]).boxed();
        assert_eq!(
            lines(exact, 64).count().await,
            1,
            "63 bytes plus newline fit a 64-byte buffer"
        );
    }
}
