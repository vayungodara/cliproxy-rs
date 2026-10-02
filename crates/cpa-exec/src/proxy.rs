//! Proxy-aware HTTP clients with Go net/http semantics, shared by every executor
//! (helps/proxy_helpers.go, sdk/proxyutil, net/http client behaviour; M4-0029).
//!
//! - [`Proxy`]: the effective proxy for a request (credential, then
//!   `requests.proxy-url`), parsed like `proxyutil.Parse`.
//! - [`GoClients`]: clients keyed by effective proxy in a bounded 64-entry LRU, with
//!   Go's standard-transport proxy rules (empty inherits environment proxies, `direct`
//!   and `none` bypass them, an unusable URL falls back to the default transport).
//! - [`send`] and [`GoHeaders`]: one exchange the way `http.Client.Do` performs it:
//!   HTTP/1.1 header line order, custom Host, manual redirects with Referer and
//!   credential stripping, and transparent gzip only when Go would have asked for it.
//! - [`lines`]: `bufio.Scanner` line semantics for event streams.
//!
//! Claude's first-party Node/OpenSSL profile lives in tls.rs; it uses [`Proxy`] and
//! [`Hooks`] from here so both kinds of client share proxy and test routing.
//!
//! ponytail: no request-scoped proxy override (Go `RequestProxyURL` / `opts.ProxyURL`);
//! ExecRequest carries none. Add a field to the execution envelope when a route needs it.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use cpa_core::config::Config;
use cpa_core::credential::Credential;
use cpa_core::exec::{ExecError, FailureScope};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use http::HeaderMap;
use wreq::tls::trust::CertStore;

/// Bounded like Go's executor transport caches.
pub(crate) const CACHE_CAPACITY: usize = 64;

/// Test-only routing: extra trust roots replace the default store, and resolve
/// overrides pin logical hosts to local addresses while URL, Host and SNI stay
/// unchanged. Production uses [`Hooks::default`].
#[derive(Clone, Default)]
pub struct Hooks {
    pub trust: Option<CertStore>,
    pub resolve: Vec<(String, SocketAddr)>,
}

impl Hooks {
    pub(crate) fn apply(&self, mut builder: wreq::ClientBuilder) -> wreq::ClientBuilder {
        if let Some(trust) = &self.trust {
            builder = builder.tls_cert_store(trust.clone());
        }
        for (host, addr) in &self.resolve {
            builder = builder.resolve(host.clone(), *addr);
        }
        builder
    }
}

/// Effective proxy for one request (`effectiveProxyURL` + `proxyutil.Parse`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Proxy {
    /// Nothing configured.
    Inherit,
    /// `direct` / `none`: bypass every proxy, including environment ones.
    Direct,
    /// http, https, socks5 or socks5h URL, as configured.
    Url(String),
    /// Unparseable or unsupported: Go logs it and behaves as if none were configured.
    Invalid,
}

impl Proxy {
    pub fn parse(raw: &str) -> Self {
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::Inherit;
        }
        if raw.eq_ignore_ascii_case("direct") || raw.eq_ignore_ascii_case("none") {
            return Self::Direct;
        }
        match url::Url::parse(raw) {
            Ok(u)
                if u.host_str().is_some_and(|h| !h.is_empty())
                    && matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h") =>
            {
                Self::Url(raw.into())
            }
            _ => Self::Invalid,
        }
    }

    /// Credential `proxy_url` (attribute, then file metadata), then `requests.proxy-url`.
    pub fn effective(credential: &Credential, cfg: &Config) -> Self {
        let own = credential
            .attributes
            .get("proxy_url")
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| credential.str("proxy_url"))
            .map(str::trim)
            .unwrap_or_default();
        if !own.is_empty() {
            return Self::parse(own);
        }
        let global = cfg
            .document
            .get("requests")
            .and_then(|r| r.get("proxy-url"))
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default();
        Self::parse(global)
    }

    /// Applies this proxy to a client. `inherit_env` mirrors Go's standard transport,
    /// which honours environment proxies when nothing is configured; the Claude uTLS
    /// transports dial directly instead.
    pub(crate) fn apply(&self, builder: wreq::ClientBuilder, inherit_env: bool) -> wreq::Result<wreq::ClientBuilder> {
        Ok(match self {
            Proxy::Url(url) => builder.proxy(wreq::Proxy::all(wreq_proxy_url(url).as_str())?),
            Proxy::Inherit | Proxy::Invalid if inherit_env => builder,
            _ => builder.no_proxy(),
        })
    }
}

/// Go's SOCKS5 dialer always sends the hostname to the proxy, so `socks5` resolves
/// remotely there; wreq does that only for `socks5h`.
fn wreq_proxy_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("socks5") => format!("socks5h://{rest}"),
        _ => url.to_owned(),
    }
}

/// `proxyutil.Redact`: a proxy URL safe for logs (no user info).
pub fn redact(raw: &str) -> String {
    match url::Url::parse(raw.trim()) {
        Ok(mut u) => {
            if !u.username().is_empty() || u.password().is_some() {
                let _ = u.set_username("xxxxx");
                let _ = u.set_password(None);
            }
            u.to_string()
        }
        Err(_) => "<invalid proxy url>".into(),
    }
}

/// Go standard-transport clients keyed by effective proxy, most recently used first.
pub struct GoClients {
    hooks: Hooks,
    /// Injected default (tests point it at local mocks); used for Inherit/Invalid.
    default: Option<wreq::Client>,
    cache: Mutex<Vec<(Proxy, wreq::Client)>>,
}

impl GoClients {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            hooks,
            default: None,
            cache: Mutex::default(),
        }
    }

    /// Every unproxied request uses `client` (local mock servers in tests).
    pub fn with_default(client: wreq::Client) -> Self {
        Self {
            hooks: Hooks::default(),
            default: Some(client),
            cache: Mutex::default(),
        }
    }

    pub fn get(&self, proxy: &Proxy) -> wreq::Client {
        if let (Some(default), Proxy::Inherit | Proxy::Invalid) = (&self.default, proxy) {
            return default.clone();
        }
        if matches!(proxy, Proxy::Invalid) {
            tracing::warn!("unusable proxy configuration; using the default transport");
        }
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = cache.iter().position(|(p, _)| p == proxy) {
            let entry = cache.remove(i);
            let client = entry.1.clone();
            cache.insert(0, entry);
            return client;
        }
        let built = proxy
            .apply(
                self.hooks
                    .apply(wreq::Client::builder().redirect(wreq::redirect::Policy::none())),
                true,
            )
            .and_then(wreq::ClientBuilder::build);
        let client = match built {
            Ok(client) => client,
            Err(_) => {
                tracing::warn!(proxy = %match proxy { Proxy::Url(u) => redact(u), _ => String::new() }, "proxy client failed; using the default transport");
                return self.default.clone().unwrap_or_else(default_client);
            }
        };
        cache.insert(0, (proxy.clone(), client.clone()));
        cache.truncate(CACHE_CAPACITY);
        client
    }
}

/// A plain client whose redirects are followed by [`send`], not by wreq.
pub fn default_client() -> wreq::Client {
    wreq::Client::builder()
        .redirect(wreq::redirect::Policy::none())
        .build()
        .expect("default HTTP client")
}

/// Go `textproto.CanonicalMIMEHeaderKey`.
pub fn canonical_header(name: &str) -> String {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    if !valid {
        return name.to_owned();
    }
    let mut upper = true;
    name.chars()
        .map(|c| {
            let out = if upper {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            };
            upper = c == '-';
            out
        })
        .collect()
}

/// Headers in the order Go's HTTP/1.1 client writes them: Host, User-Agent (Go's
/// default when unset), Content-Length, the remaining request headers sorted by key
/// with their values in order, then the transport's Accept-Encoding.
#[derive(Clone, Default)]
pub struct GoHeaders {
    headers: Vec<(String, String)>,
}

impl GoHeaders {
    pub fn new() -> Self {
        Self::default()
    }

    /// `http.Header.Set`.
    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        let name = canonical_header(name);
        self.headers.retain(|(n, _)| *n != name);
        self.headers.push((name, value.into()));
    }

    /// Appends a value under a key that keeps its exact spelling (a raw `http.Header`
    /// map key in Go, written as stored).
    pub fn add_raw(&mut self, name: &str, value: impl Into<String>) {
        self.headers.push((name.to_owned(), value.into()));
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        let name = canonical_header(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    /// Removes every value of `name` (`http.Header.Del`) and returns the first.
    pub fn take(&mut self, name: &str) -> Option<String> {
        let name = canonical_header(name);
        let mut first = None;
        self.headers.retain(|(n, v)| {
            if *n == name {
                first.get_or_insert_with(|| v.clone());
                false
            } else {
                true
            }
        });
        first
    }

    /// Applies the headers with their wire spelling. `order` is an exact header order
    /// (Claude's ordered request writer); `None` writes Go net/http's order. Returns
    /// whether the transport asked for gzip itself (no explicit Accept-Encoding or
    /// Range), which is the only case Go decodes transparently.
    pub fn apply(mut self, builder: wreq::RequestBuilder, order: Option<&[String]>) -> (wreq::RequestBuilder, bool) {
        let auto_gzip = self.get("Accept-Encoding").is_none() && self.get("Range").is_none();
        // A custom Host header becomes the request Host (util.applyCustomHeaders).
        let host = self.take("Host").filter(|h| !h.is_empty());
        self.take("Content-Length");
        if self.get("User-Agent").is_none() {
            // ponytail: Go says Go-http-client/2.0 on HTTP/2; this is the HTTP/1.1 value.
            self.headers.push(("User-Agent".into(), "Go-http-client/1.1".into()));
        }
        let mut wire = wreq::header::OrigHeaderMap::new();
        match order {
            Some(order) => {
                for name in order {
                    wire.insert(name.clone());
                }
            }
            None => {
                wire.insert("Host");
                wire.insert("User-Agent");
                wire.insert("Content-Length");
                let mut rest: Vec<&String> = self
                    .headers
                    .iter()
                    .map(|(n, _)| n)
                    .filter(|n| *n != "User-Agent")
                    .collect();
                rest.sort();
                rest.dedup();
                for name in rest {
                    wire.insert(name.clone());
                }
            }
        }
        if auto_gzip {
            wire.insert("Accept-Encoding");
            self.headers.push(("Accept-Encoding".into(), "gzip".into()));
        }
        let mut builder = builder.orig_headers(wire).default_headers(false);
        if let Some(host) = host {
            builder = builder.header("Host", host);
        }
        for (name, value) in self.headers {
            builder = builder.header(name, value);
        }
        (builder, auto_gzip)
    }
}

/// How one request hop is sent: its client, and an exact header order for Claude's
/// native transport (`None` is Go net/http's order). Go's `http.Client` picks the
/// round tripper per hop, so a redirect can change both.
pub struct Route {
    pub client: wreq::Client,
    pub order: Option<Vec<String>>,
}

/// A response with Go net/http semantics: the body is decoded only for gzip the
/// transport requested itself, and is never framed or buffered here.
pub struct Upstream {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, ExecError>>,
}

/// Largest error body kept from an upstream rejection.
// ponytail: Go reads error bodies without a bound; 16 MiB keeps a hostile upstream from
// exhausting a small VPS while covering any real provider error.
pub const MAX_ERROR_BODY: usize = 16 << 20;

fn body_error(_: std::io::Error) -> ExecError {
    ExecError::local(502, FailureScope::Transport, "upstream request failed")
}

/// POSTs `body` the way Go's `http.Client.Do` does, following redirects itself: at
/// most ten requests, 301/302/303 turn a POST into a body-less GET, 307/308 resend the
/// body, the initial headers are copied to every hop (credentials only within the
/// original domain), a custom Host survives only relative redirects, and the previous
/// URL becomes `Referer`. `timeout` bounds the exchange.
pub async fn send(
    client: &wreq::Client,
    url: &str,
    headers: GoHeaders,
    body: impl Into<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let route = |_: &url::Url| {
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    send_routed(&route, url, headers, body.into(), timeout).await
}

/// [`send`] with the client and header order chosen per hop.
pub async fn send_routed(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    url: &str,
    headers: GoHeaders,
    body: Bytes,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let initial =
        url::Url::parse(url).map_err(|_| ExecError::local(500, FailureScope::Request, "invalid upstream URL"))?;
    let explicit_referer = headers.get("Referer").map(str::to_owned);
    let mut current = initial.clone();
    // req.Host: the custom Host of the current hop, if any.
    let mut host = headers.get("Host").filter(|h| !h.is_empty()).map(str::to_owned);
    let mut method = wreq::Method::POST;
    let mut include_body = true;
    let mut strip_sensitive = false;
    let mut hop_headers = headers.clone();
    let mut sent = 0;
    let (response, auto_gzip) = loop {
        let hop = route(&current)?;
        let mut builder = hop
            .client
            .request(method.clone(), current.as_str())
            .redirect(wreq::redirect::Policy::none());
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let (builder, auto_gzip) = hop_headers.clone().apply(builder, hop.order.as_deref());
        let builder = if include_body {
            builder.body(body.clone())
        } else {
            builder
        };
        let response = builder.send().await.map_err(crate::upstream::transport_error)?;
        sent += 1;
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
        let next = current.join(&location).map_err(|_| {
            ExecError::local(
                500,
                FailureScope::Transport,
                format!("failed to parse Location header {location:?}"),
            )
        })?;
        // defaultCheckRedirect: len(via) >= 10.
        if sent >= 10 {
            return Err(ExecError::local(
                500,
                FailureScope::Transport,
                "stopped after 10 redirects",
            ));
        }
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
        // Only a relative Location keeps the current hop's custom Host (go.dev/issue/22233).
        hop_headers.take("Host");
        host = host.filter(|_| url::Url::parse(&location).is_err());
        if let Some(host) = &host {
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

/// `io.ReadAll` up to `limit` bytes; with `lossy`, a read error keeps what arrived
/// (Go's error paths ignore it).
pub async fn read_all(
    mut body: BoxStream<'static, Result<Bytes, ExecError>>,
    limit: usize,
    lossy: bool,
) -> Result<Bytes, ExecError> {
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

/// `bufio.Scanner` with `ScanLines`: one item per line without its `\n`, a trailing
/// `\r` dropped, the final unterminated line included, and Go's error once a line
/// reaches `max` bytes without a newline.
pub fn lines(
    body: BoxStream<'static, Result<Bytes, ExecError>>,
    max: usize,
) -> BoxStream<'static, Result<Bytes, ExecError>> {
    struct State {
        body: BoxStream<'static, Result<Bytes, ExecError>>,
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

/// Shared handle for executors: Go clients plus the hooks used to build them.
pub type SharedGoClients = Arc<GoClients>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_parsing_scheme_mapping_and_redaction() {
        assert_eq!(Proxy::parse(" "), Proxy::Inherit);
        assert_eq!(Proxy::parse("DIRECT"), Proxy::Direct);
        assert_eq!(Proxy::parse("none"), Proxy::Direct);
        assert_eq!(Proxy::parse("http://p:1"), Proxy::Url("http://p:1".into()));
        assert_eq!(Proxy::parse("socks5://u:p@p:1"), Proxy::Url("socks5://u:p@p:1".into()));
        assert_eq!(Proxy::parse("ftp://p"), Proxy::Invalid);
        assert_eq!(Proxy::parse("p:1"), Proxy::Invalid);
        assert_eq!(wreq_proxy_url("socks5://h:1"), "socks5h://h:1");
        assert_eq!(wreq_proxy_url("SOCKS5://h:1"), "socks5h://h:1");
        assert_eq!(wreq_proxy_url("socks5h://h:1"), "socks5h://h:1");
        assert_eq!(wreq_proxy_url("http://h:1"), "http://h:1");
        assert_eq!(redact("http://user:secret@p:1/"), "http://xxxxx@p:1/");
        assert!(!redact("socks5://user:secret@p:1").contains("secret"));
    }

    #[test]
    fn effective_proxy_precedence() {
        let cfg = Config::parse("requests:\n  proxy-url: http://global:1\n").unwrap();
        let mut c = Credential::from_file(
            std::path::Path::new("/a"),
            std::path::Path::new("/a/c.json"),
            serde_json::json!({"type":"claude","proxy_url":"socks5://meta:2"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(Proxy::effective(&c, &cfg), Proxy::Url("socks5://meta:2".into()));
        c.attributes.insert("proxy_url".into(), "direct".into());
        assert_eq!(Proxy::effective(&c, &cfg), Proxy::Direct);
        c.attributes.clear();
        c.metadata.remove("proxy_url");
        assert_eq!(Proxy::effective(&c, &cfg), Proxy::Url("http://global:1".into()));
    }

    #[test]
    fn canonical_header_matches_go() {
        assert_eq!(canonical_header("x-msh-device-id"), "X-Msh-Device-Id");
        assert_eq!(canonical_header("content-TYPE"), "Content-Type");
        assert_eq!(canonical_header("bad header"), "bad header");
    }

    #[tokio::test]
    async fn lines_follow_bufio_scanner() {
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

    #[test]
    fn client_cache_is_bounded_lru() {
        let clients = GoClients::new(Hooks::default());
        for i in 0..=CACHE_CAPACITY {
            clients.get(&Proxy::Url(format!("http://p{i}:1")));
        }
        assert_eq!(clients.cache.lock().unwrap().len(), CACHE_CAPACITY);
        assert!(
            clients
                .cache
                .lock()
                .unwrap()
                .iter()
                .all(|(p, _)| *p != Proxy::Url("http://p0:1".into()))
        );
        clients.get(&Proxy::Url("http://p1:1".into()));
        assert_eq!(clients.cache.lock().unwrap()[0].0, Proxy::Url("http://p1:1".into()));
    }
}
