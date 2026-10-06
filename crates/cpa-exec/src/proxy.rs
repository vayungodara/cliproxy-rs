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
        Self::parse(&Self::effective_url(credential, cfg))
    }

    /// The setting `effective` parses, trimmed (Go `proxyURLForAuth`).
    pub fn effective_url(credential: &Credential, cfg: &Config) -> String {
        let own = credential
            .attributes
            .get("proxy_url")
            .map(String::as_str)
            .filter(|s| !s.trim().is_empty())
            .or_else(|| credential.str("proxy_url"))
            .map(str::trim)
            .unwrap_or_default();
        if !own.is_empty() {
            return own.to_owned();
        }
        cfg.document
            .get("requests")
            .and_then(|r| r.get("proxy-url"))
            .and_then(serde_yaml_ng::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    /// Applies this proxy to a client. `inherit_env` mirrors Go's standard transport,
    /// which honours environment proxies when nothing is configured; the Claude uTLS
    /// transports dial directly instead.
    pub fn apply(&self, builder: wreq::ClientBuilder, inherit_env: bool) -> wreq::Result<wreq::ClientBuilder> {
        match self {
            Proxy::Url(url) => Ok(builder.proxy(wreq::Proxy::all(wreq_proxy_url(url).as_str())?)),
            Proxy::Inherit | Proxy::Invalid if inherit_env => EnvProxy::current().apply(builder),
            _ => Ok(builder.no_proxy()),
        }
    }
}

/// Go's `http.ProxyFromEnvironment` (golang.org/x/net/http/httpproxy), read once like
/// Go's `envProxyFunc`: `HTTP_PROXY` for http and `HTTPS_PROXY` for https targets (upper
/// case first), never `ALL_PROXY`; `localhost` and loopback addresses always bypass;
/// `NO_PROXY` adds exclusions, and `*` disables both proxies.
///
/// ponytail: wreq's exclusion matcher stands in for Go's. Port-specific `NO_PROXY`
/// entries are dropped (Go bypasses only that port), a leading-dot entry also matches
/// the bare domain, and the CGI rule ignores `HTTP_PROXY` instead of failing requests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvProxy {
    http: Option<String>,
    https: Option<String>,
    /// wreq `NoProxy` list including Go's implicit loopback bypass; `None` for `*`.
    no_proxy: Option<String>,
}

impl EnvProxy {
    fn current() -> &'static Self {
        static ENV: std::sync::OnceLock<EnvProxy> = std::sync::OnceLock::new();
        ENV.get_or_init(|| Self::from_lookup(|name| std::env::var(name).ok()))
    }

    /// `FromEnvironment` + `config.init` over a variable lookup.
    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let any = |names: [&str; 2]| names.iter().find_map(|n| get(n).filter(|v| !v.is_empty()));
        let cgi = get("REQUEST_METHOD").is_some_and(|v| !v.is_empty());
        let http = any(["HTTP_PROXY", "http_proxy"])
            .filter(|_| !cgi)
            .and_then(|v| env_proxy_url(&v));
        let https = any(["HTTPS_PROXY", "https_proxy"]).and_then(|v| env_proxy_url(&v));
        let mut entries: Vec<String> = ["localhost", "127.0.0.0/8", "::1", "::ffff:127.0.0.0/104"]
            .map(String::from)
            .into();
        let mut all = false;
        for entry in any(["NO_PROXY", "no_proxy"]).unwrap_or_default().split(',') {
            let entry = entry.trim().to_lowercase();
            if entry.is_empty() {
                continue;
            }
            if entry == "*" {
                all = true;
                break;
            }
            let bare_ip = entry.parse::<std::net::IpAddr>().is_ok() || entry.contains('/');
            let has_port = !bare_ip
                && entry
                    .rsplit_once(':')
                    .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
            if has_port {
                continue;
            }
            entries.push(
                entry
                    .strip_prefix('*')
                    .filter(|e| e.starts_with('.'))
                    .unwrap_or(&entry)
                    .to_owned(),
            );
        }
        Self {
            http,
            https,
            no_proxy: (!all).then(|| entries.join(",")),
        }
    }

    fn apply(&self, builder: wreq::ClientBuilder) -> wreq::Result<wreq::ClientBuilder> {
        let mut builder = builder.no_proxy();
        let Some(no_proxy) = &self.no_proxy else {
            return Ok(builder);
        };
        let exclusions = || wreq::NoProxy::from_string(no_proxy);
        if let Some(url) = &self.http {
            builder = builder.proxy(wreq::Proxy::http(url.as_str())?.no_proxy(exclusions()));
        }
        if let Some(url) = &self.https {
            builder = builder.proxy(wreq::Proxy::https(url.as_str())?.no_proxy(exclusions()));
        }
        Ok(builder)
    }
}

/// httpproxy `parseProxy`: a value without scheme or host is retried as `http://`.
fn env_proxy_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw)
        .ok()
        .filter(|u| u.host_str().is_some_and(|h| !h.is_empty()))
        .or_else(|| url::Url::parse(&format!("http://{raw}")).ok())?;
    Some(wreq_proxy_url(parsed.as_str().trim_end_matches('/')))
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
        self.try_get(proxy).unwrap_or_else(|| {
            tracing::warn!(proxy = %match proxy { Proxy::Url(u) => redact(u), _ => String::new() }, "proxy client failed; using the default transport");
            self.default.clone().unwrap_or_else(default_client)
        })
    }

    /// The client for exactly `proxy`, or `None` when it cannot be built; never falls
    /// back to another transport (management `api-call` applies Go's own fallbacks).
    pub fn try_get(&self, proxy: &Proxy) -> Option<wreq::Client> {
        let mut cache = self.cache.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(i) = cache.iter().position(|(p, _)| p == proxy) {
            let entry = cache.remove(i);
            let client = entry.1.clone();
            cache.insert(0, entry);
            return Some(client);
        }
        let built = proxy
            .apply(
                self.hooks
                    .apply(wreq::Client::builder().redirect(wreq::redirect::Policy::none())),
                true,
            )
            .and_then(wreq::ClientBuilder::build);
        let client = built.ok()?;
        cache.insert(0, (proxy.clone(), client.clone()));
        cache.truncate(CACHE_CAPACITY);
        Some(client)
    }
}

/// A plain client whose redirects are followed by [`send`], not by wreq, with Go's
/// environment proxy rules.
pub fn default_client() -> wreq::Client {
    let builder = wreq::Client::builder().redirect(wreq::redirect::Policy::none());
    EnvProxy::current()
        .apply(builder)
        .and_then(wreq::ClientBuilder::build)
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
    /// Go `http.Transport.DisableCompression`: the transport never asks for gzip.
    compression_disabled: bool,
    /// Send the first request's target exactly as the caller wrote the URL.
    exact_target: bool,
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

    /// Go `http.Transport.DisableCompression` (Devin's transport): no automatic
    /// `Accept-Encoding: gzip`, so responses are never decoded transparently.
    pub fn disable_compression(&mut self) {
        self.compression_disabled = true;
    }

    /// Go's `URL.RequestURI()`: the first request's target is the caller's URL as
    /// written, so `.` and `..` segments and the caller's escaping reach the upstream
    /// (Go's `PathEscape` output). Without it the URL is normalized as WHATWG parses it.
    /// A relative redirect from that request resolves against the path as written, as
    /// Go's `ResolveReference` does.
    pub fn exact_target(&mut self) {
        self.exact_target = true;
    }

    /// The headers as Go's `http.Header` holds them before sending (`Header.Clone()`, what
    /// request logging records): set names canonical, raw names as written, values in
    /// order. The transport's own lines (Host from the URL, the default User-Agent,
    /// Content-Length, Accept-Encoding) are not included.
    pub fn pairs(&self) -> &[(String, String)] {
        &self.headers
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
    pub fn apply(self, builder: wreq::RequestBuilder, order: Option<&[String]>) -> (wreq::RequestBuilder, bool) {
        self.apply_gzip(builder, order, true, false)
    }

    /// [`GoHeaders::apply`]; `gzip_allowed` is false for HEAD, which Go's transport
    /// never asks to compress, and `http2` picks Go's HTTP/2 default User-Agent.
    fn apply_gzip(
        mut self,
        builder: wreq::RequestBuilder,
        order: Option<&[String]>,
        gzip_allowed: bool,
        http2: bool,
    ) -> (wreq::RequestBuilder, bool) {
        let auto_gzip = gzip_allowed
            && !self.compression_disabled
            && self.get("Accept-Encoding").is_none()
            && self.get("Range").is_none();
        // A custom Host header becomes the request Host (util.applyCustomHeaders).
        let host = self.take("Host").filter(|h| !h.is_empty());
        self.take("Content-Length");
        match self.get("User-Agent") {
            // net/http's defaultUserAgent, or x/net/http2's on an HTTP/2 connection.
            None => {
                let ua = if http2 {
                    "Go-http-client/2.0"
                } else {
                    "Go-http-client/1.1"
                };
                self.headers.push(("User-Agent".into(), ua.into()));
            }
            // Go writes no User-Agent line for an empty value (`Header["User-Agent"] = []string{""}`).
            Some("") => {
                self.take("User-Agent");
            }
            Some(_) => {}
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
    request(client, wreq::Method::POST, url, headers, Some(body.into()), timeout).await
}

/// [`send`] for any method: `http.NewRequest(method, url, body)` then `Client.Do`, with
/// Go's redirect rules for that method (a GET follows 301-303 and 307/308 as a GET).
/// `None` is Go's nil body.
pub async fn request(
    client: &wreq::Client,
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let route = |_: &url::Url| {
        Ok(Route {
            client: client.clone(),
            order: None,
        })
    };
    send_request(&route, method, url, headers, body, timeout).await
}

/// [`send`] with the client and header order chosen per hop.
pub async fn send_routed(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    url: &str,
    headers: GoHeaders,
    body: Bytes,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    send_request(route, wreq::Method::POST, url, headers, Some(body), timeout).await
}

/// [`send_routed`] for any method. `None` is Go's nil body: no body and no
/// Content-Length (management `api-call`).
pub async fn send_request(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<Upstream, ExecError> {
    let raw = send_request_raw(route, method, url, headers, body, timeout)
        .await
        .map_err(|e| match e {
            SendError::Transport { error, .. } => crate::upstream::transport_error(error),
            SendError::Local { error, .. } => error,
            // What `transport_error` makes of the transport's own timeout.
            SendError::Timeout { .. } => ExecError::local(502, FailureScope::Transport, "upstream request failed"),
        })?;
    Ok(Upstream {
        status: raw.status,
        headers: raw.headers,
        body: raw.body.map(|r| r.map_err(body_error)).boxed(),
    })
}

/// Why [`send_request_raw`] failed: the transport's own error (which may name the URL,
/// so callers decide what to show), a locally generated one, or `Timeout`: the
/// deadline passed before a hop was sent, so nothing was sent, and callers report it
/// as they report the transport's own timeout. `url` is the URL Go's client names in
/// its `*url.Error`: the failing hop (the caller's URL as written for the first
/// request), or the rejected `Location` when the redirect limit is hit.
// ponytail: wreq has no public constructor for its timeout error, so `Timeout` cannot be
// a `Transport`; each renderer repeats what it makes of one. Fold it into `Transport`
// if wreq ever exposes that constructor.
#[derive(Debug)]
pub enum SendError {
    Transport { error: wreq::Error, url: String },
    Local { error: ExecError, url: String },
    Timeout { url: String },
}

/// [`Upstream`] with the transport's raw body errors and the response's protocol
/// version (Go's response reader keeps framing headers by version).
pub struct RawUpstream {
    pub status: u16,
    pub version: http::Version,
    pub headers: HeaderMap,
    pub body: BoxStream<'static, Result<Bytes, std::io::Error>>,
}

/// [`send_request`] without error sanitising, for callers that report Go's own error
/// text (the plugin host's `host.http.*`).
pub async fn send_request_raw(
    route: &(dyn Fn(&url::Url) -> Result<Route, ExecError> + Sync),
    method: wreq::Method,
    url: &str,
    headers: GoHeaders,
    body: Option<Bytes>,
    timeout: Option<std::time::Duration>,
) -> Result<RawUpstream, SendError> {
    let local = |error: ExecError, url: &str| SendError::Local {
        error,
        url: url.to_owned(),
    };
    let initial = url::Url::parse(url).map_err(|_| {
        local(
            ExecError::local(500, FailureScope::Request, "invalid upstream URL"),
            url,
        )
    })?;
    let explicit_referer = headers.get("Referer").map(str::to_owned);
    // The caller's URL as written, for the first request only (GoHeaders::exact_target);
    // a URL with user info falls back to the parsed form (Go strips it from Referer).
    let exact =
        (headers.exact_target && initial.username().is_empty() && initial.password().is_none()).then(|| url.to_owned());
    let mut current = initial.clone();
    // req.Host: the custom Host of the current hop, if any.
    let mut host = headers.get("Host").filter(|h| !h.is_empty()).map(str::to_owned);
    let mut method = method;
    // Go's redirect `includeBody` says whether a 301/302/303 has dropped the body; it
    // starts true even for a nil body.
    let has_body = body.is_some();
    let mut include_body = true;
    let body = body.unwrap_or_default();
    let mut strip_sensitive = false;
    let mut hop_headers = headers.clone();
    let mut sent = 0;
    // Go's `Client.Timeout`: one deadline for every hop, taken before the first. Cost:
    // one `Instant` per request and one clock read per hop.
    // ponytail: wreq starts the body's share of a hop timeout afresh when the headers
    // arrive, so the final body can take up to that hop's budget again (the whole
    // exchange stays under twice `timeout`). Bounding it by the deadline needs a second
    // timer per response; add one if a caller ever reads long bodies under a timeout.
    let deadline = timeout.and_then(|t| std::time::Instant::now().checked_add(t));
    let (response, auto_gzip) = loop {
        // The URL Go's client reports for this hop.
        let hop_url = if sent == 0 { url.to_owned() } else { current.to_string() };
        let hop = route(&current).map_err(|e| local(e, &hop_url))?;
        let builder = hop
            .client
            .request(
                method.clone(),
                exact.as_deref().filter(|_| sent == 0).unwrap_or(current.as_str()),
            )
            .redirect(wreq::redirect::Policy::none());
        // Go's standard transport (no exact order) speaks HTTP/2 wherever ALPN picks it.
        let go_transport = hop.order.is_none();
        let (builder, auto_gzip) = hop_headers.clone().apply_gzip(
            builder,
            hop.order.as_deref(),
            method != wreq::Method::HEAD,
            go_transport && expects_http2(&current),
        );
        let sends_body = has_body && include_body && !body.is_empty();
        let builder = if sends_body {
            builder.body(body.clone())
        } else if matches!(method.as_str(), "POST" | "PUT" | "PATCH") {
            // transferWriter.shouldSendContentLength: these methods always carry a length,
            // so a nil or empty body still says Content-Length: 0.
            builder.header(http::header::CONTENT_LENGTH, "0")
        } else {
            builder
        };
        // Last, so the hop's setup counts too. Go sends nothing once the deadline has
        // passed; wreq would send even with a zero timeout: it polls the request before
        // its timer, which fires on a 1 ms tick.
        let builder = match deadline.map(|d| d.saturating_duration_since(std::time::Instant::now())) {
            Some(remaining) if remaining.is_zero() => return Err(SendError::Timeout { url: hop_url }),
            Some(remaining) => builder.timeout(remaining),
            None => builder,
        };
        let response = builder.send().await.map_err(|error| SendError::Transport {
            error,
            url: hop_url.clone(),
        })?;
        if go_transport {
            remember_protocol(&current, response.version() == http::Version::HTTP_2);
        }
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
        let from_exact = exact.as_deref().filter(|_| sent == 1);
        let next = match from_exact {
            Some(raw) => resolve_exact(raw, &location, &current),
            None => current.join(&location),
        }
        .map_err(|_| {
            local(
                ExecError::local(
                    500,
                    FailureScope::Transport,
                    format!("failed to parse Location header {location:?}"),
                ),
                &hop_url,
            )
        })?;
        // defaultCheckRedirect: len(via) >= 10.
        if sent >= 10 {
            return Err(local(
                ExecError::local(500, FailureScope::Transport, "stopped after 10 redirects"),
                &location,
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
            let referer = explicit_referer
                .clone()
                .or_else(|| from_exact.map(str::to_owned))
                .unwrap_or_else(|| {
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
    let version = response.version();
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
        tokio_util::io::ReaderStream::new(decoder).boxed()
    } else {
        stream.boxed()
    };
    Ok(RawUpstream {
        status,
        version,
        headers,
        body,
    })
}

/// Origins (`scheme://host:port`) and whether their last answer came over HTTP/2.
/// Go's default User-Agent names the negotiated protocol, which wreq only reports
/// after the request is written.
fn origin_protocols() -> &'static Mutex<std::collections::HashMap<String, bool>> {
    static PROTOCOLS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, bool>>> = std::sync::OnceLock::new();
    PROTOCOLS.get_or_init(Mutex::default)
}

fn origin(url: &url::Url) -> String {
    format!(
        "{}://{}:{}",
        url.scheme(),
        url.host_str().unwrap_or_default(),
        url.port_or_known_default().unwrap_or_default()
    )
}

/// Whether Go's transport would speak HTTP/2 to `url`: what its origin last answered
/// with; never over plain HTTP (no h2c).
// ponytail: an origin not seen yet counts as HTTP/1.1, so the first request to an h2
// upstream says Go-http-client/1.1 where Go says /2.0 (only callers that set no
// User-Agent). The protocol itself is always the negotiated one.
fn expects_http2(url: &url::Url) -> bool {
    url.scheme() == "https"
        && origin_protocols()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&origin(url))
            .copied()
            .unwrap_or(false)
}

fn remember_protocol(url: &url::Url, http2: bool) {
    let mut protocols = origin_protocols()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Bounded: forgetting only brings back the first-request assumption.
    if protocols.len() >= 1024 {
        protocols.clear();
    }
    protocols.insert(origin(url), http2);
}

/// Go `URL.ResolveReference` against the base URL as written (`Response.Location`):
/// the scheme and authority stay; the path is Go's `resolvePath` over the written base
/// path, so dot segments merge as written and a merged `//` path stays a path; the base
/// query survives only a reference with neither path nor query. Absolute and
/// network-path references ignore the base.
// ponytail: the next hop is a `url::Url`, which reads `%2e` as a dot segment where Go
// keeps it; only the first request goes out exactly as written.
fn resolve_exact(raw: &str, location: &str, current: &url::Url) -> Result<url::Url, url::ParseError> {
    if location.starts_with("//") || url::Url::parse(location).is_ok() {
        return current.join(location);
    }
    let (reference, fragment) = match location.split_once('#') {
        Some((reference, fragment)) => (reference, Some(fragment)),
        None => (location, None),
    };
    let (ref_path, ref_query) = match reference.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (reference, None),
    };
    // Go's parse order: the fragment, then the query at the first `?`, then the
    // authority (up to the first `/`) after the scheme.
    let base = raw.split_once('#').map_or(raw, |(base, _)| base);
    let (base, base_query) = match base.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (base, None),
    };
    let rest = base.find("://").map_or(base, |i| &base[i + 3..]);
    let base_path = rest.find('/').map_or("", |i| &rest[i..]);
    let mut next = current.clone();
    next.set_path(&go_resolve_path(base_path, ref_path));
    if ref_path.is_empty() && ref_query.is_none() {
        next.set_query(base_query);
    } else {
        next.set_query(ref_query);
    }
    next.set_fragment(fragment);
    Ok(next)
}

/// Go `net/url.resolvePath`: merge `reference` with `base`, then remove dot segments.
fn go_resolve_path(base: &str, reference: &str) -> String {
    let full = if reference.is_empty() {
        base.to_owned()
    } else if !reference.starts_with('/') {
        let i = base.rfind('/').map_or(0, |i| i + 1);
        format!("{}{reference}", &base[..i])
    } else {
        reference.to_owned()
    };
    if full.is_empty() {
        return String::new();
    }
    let mut dst = String::with_capacity(full.len() + 1);
    dst.push('/');
    let mut first = true;
    let mut last = "";
    for elem in full.split('/') {
        last = elem;
        match elem {
            "." => first = false,
            ".." => {
                match dst[1..].rfind('/') {
                    Some(i) => dst.truncate(i + 1),
                    None => dst.truncate(1),
                }
                first = dst.len() == 1;
            }
            _ => {
                if !first {
                    dst.push('/');
                }
                dst.push_str(elem);
                first = false;
            }
        }
    }
    if last == "." || last == ".." {
        dst.push('/');
    }
    // An initial `/` was written; never two.
    if dst.len() > 1 && dst.as_bytes()[1] == b'/' {
        dst.remove(0);
    }
    dst
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

/// `bufio.Scanner` with `ScanLines` and `Buffer(nil, max)`: one item per line without
/// its `\n`, a trailing `\r` dropped, the final unterminated line included, and
/// `bufio.ErrTooLong` when no newline falls within `max` bytes of a line's start (the
/// scanner's buffer never holds more). An I/O error ends the stream after the line
/// buffered before it, as `Scan` returns that last token before reporting `Err`.
// ponytail: the body's end (or error) is taken to arrive with its last bytes, as net/http
// returns EOF for a Content-Length body and gzip.Reader at its end; the stream cannot
// tell. When Go's reader reports the end on a read of its own, Go rejects an
// unterminated final line of exactly `max` bytes that this accepts.
pub fn lines(
    body: BoxStream<'static, Result<Bytes, ExecError>>,
    max: usize,
) -> BoxStream<'static, Result<Bytes, ExecError>> {
    struct State {
        body: BoxStream<'static, Result<Bytes, ExecError>>,
        /// Unconsumed input from the current line's start.
        buf: bytes::BytesMut,
        /// Bytes of `buf` already searched for a newline.
        scanned: usize,
        /// The reader ended (EOF or the error below).
        ended: bool,
        error: Option<ExecError>,
        finished: bool,
    }
    let drop_cr = |line: &[u8]| Bytes::copy_from_slice(line.strip_suffix(b"\r").unwrap_or(line));
    futures_util::stream::unfold(
        State {
            body,
            buf: bytes::BytesMut::new(),
            scanned: 0,
            ended: false,
            error: None,
            finished: false,
        },
        move |mut st| async move {
            if st.finished {
                return None;
            }
            loop {
                let window = st.buf.len().min(max);
                if let Some(pos) = st.buf[st.scanned.min(window)..window].iter().position(|b| *b == b'\n') {
                    let at = st.scanned.min(window) + pos;
                    let line = st.buf.split_to(at + 1);
                    st.scanned = 0;
                    return Some((Ok(drop_cr(&line[..at])), st));
                }
                st.scanned = window;
                // A full buffer without a newline: more data is too long, while the end
                // of the body makes it the final line.
                if st.buf.len() > max {
                    st.finished = true;
                    let error = ExecError::local(500, FailureScope::Request, "bufio.Scanner: token too long");
                    return Some((Err(error), st));
                }
                if st.ended {
                    if !st.buf.is_empty() {
                        let line = st.buf.split();
                        st.scanned = 0;
                        return Some((Ok(drop_cr(&line)), st));
                    }
                    st.finished = true;
                    return st.error.take().map(|error| (Err(error), st));
                }
                match st.body.next().await {
                    Some(Ok(chunk)) => st.buf.extend_from_slice(&chunk),
                    Some(Err(error)) => {
                        st.ended = true;
                        st.error = Some(error);
                    }
                    None => st.ended = true,
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
    fn environment_proxies_follow_go_httpproxy() {
        let env = |vars: &[(&str, &str)]| {
            let vars: std::collections::HashMap<String, String> =
                vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            EnvProxy::from_lookup(|name| vars.get(name).cloned())
        };
        let e = env(&[
            ("ALL_PROXY", "http://all:1"),
            ("http_proxy", "lower:2"),
            ("HTTP_PROXY", "upper:3"),
            ("https_proxy", "socks5://s:4"),
            ("no_proxy", " Example.com, *.corp.test, 10.0.0.0/8, host:8080, ::2 "),
        ]);
        assert_eq!(
            e.http.as_deref(),
            Some("http://upper:3"),
            "upper case first; schemeless is http"
        );
        assert_eq!(e.https.as_deref(), Some("socks5h://s:4"));
        assert_eq!(
            e.no_proxy.as_deref(),
            Some("localhost,127.0.0.0/8,::1,::ffff:127.0.0.0/104,example.com,.corp.test,10.0.0.0/8,::2")
        );
        assert_eq!(
            env(&[("ALL_PROXY", "http://all:1")]),
            env(&[]),
            "ALL_PROXY is never read"
        );
        assert_eq!(env(&[("HTTP_PROXY", "http://p:1"), ("NO_PROXY", "a,*")]).no_proxy, None);
        assert_eq!(
            env(&[("HTTP_PROXY", "http://p:1"), ("REQUEST_METHOD", "GET")]).http,
            None
        );
    }

    #[tokio::test]
    async fn environment_proxy_bypasses_loopback_and_proxies_the_rest() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        async fn first_line(listener: tokio::net::TcpListener) -> String {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let _ = socket
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
            String::from_utf8_lossy(&buf[..n]).lines().next().unwrap().to_owned()
        }
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (proxy_addr, target_addr) = (proxy.local_addr().unwrap(), target.local_addr().unwrap());
        let env = EnvProxy::from_lookup(|name| (name == "HTTP_PROXY").then(|| format!("http://{proxy_addr}")));
        let client = env
            .apply(wreq::Client::builder().redirect(wreq::redirect::Policy::none()))
            .unwrap()
            .build()
            .unwrap();
        let direct = tokio::spawn(first_line(target));
        client.post(format!("http://{target_addr}/v1")).send().await.unwrap();
        assert_eq!(
            direct.await.unwrap(),
            "POST /v1 HTTP/1.1",
            "loopback never uses the proxy"
        );
        let proxied = tokio::spawn(first_line(proxy));
        client.post("http://upstream.invalid/v1").send().await.unwrap();
        assert_eq!(proxied.await.unwrap(), "POST http://upstream.invalid/v1 HTTP/1.1");
    }

    #[test]
    fn canonical_header_matches_go() {
        assert_eq!(canonical_header("x-msh-device-id"), "X-Msh-Device-Id");
        assert_eq!(canonical_header("content-TYPE"), "Content-Type");
        assert_eq!(canonical_header("bad header"), "bad header");
    }

    fn go_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/proxy_go.json")).unwrap()
    }

    /// What a local upstream saw of one request.
    #[derive(Debug, Default, Clone, PartialEq)]
    struct Saw {
        proto: String,
        client_alpn: Option<Vec<String>>,
        user_agent: String,
    }

    /// Answers one HTTP/1.1 request on `io` with `ok`, recording its User-Agent.
    async fn serve_h1<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut io: S, saw: Arc<Mutex<Saw>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if io.read(&mut byte).await.unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        {
            let mut s = saw.lock().unwrap();
            s.proto = head
                .lines()
                .next()
                .unwrap_or_default()
                .rsplit(' ')
                .next()
                .unwrap_or_default()
                .to_owned();
            s.user_agent = head
                .lines()
                .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("user-agent")))
                .map(|(_, v)| v.trim().to_owned())
                .unwrap_or_default();
        }
        let _ = io
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await;
        let _ = io.shutdown().await;
    }

    /// A local upstream: TLS offering the `alpn` protocols (HTTP/2 when the client picks
    /// it), or plain HTTP/1.1 for `None`. Returns its address, CA PEM and observations.
    async fn upstream(alpn: Option<&'static [u8]>) -> (SocketAddr, Vec<u8>, Arc<Mutex<Saw>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let saw = Arc::new(Mutex::new(Saw::default()));
        let Some(alpn) = alpn else {
            let seen = saw.clone();
            tokio::spawn(async move {
                while let Ok((tcp, _)) = listener.accept().await {
                    tokio::spawn(serve_h1(tcp, seen.clone()));
                }
            });
            return (addr, Vec::new(), saw);
        };
        let (ca, mut builder) = crate::test_tls::builder(&["upstream.test.invalid".to_owned()]);
        let offered = saw.clone();
        builder.set_alpn_select_callback(move |_, client| {
            // The client's offer, decoded from the wire's length-prefixed names.
            let (mut names, mut rest) = (Vec::new(), client);
            while let Some((&len, tail)) = rest.split_first() {
                let (name, tail) = tail.split_at(usize::from(len).min(tail.len()));
                names.push(String::from_utf8_lossy(name).into_owned());
                rest = tail;
            }
            offered.lock().unwrap().client_alpn = Some(names);
            btls::ssl::select_next_proto(alpn, client).ok_or(btls::ssl::AlpnError::NOACK)
        });
        let acceptor = builder.build();
        let seen = saw.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
                if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                    continue;
                }
                let seen = seen.clone();
                tokio::spawn(async move {
                    if tls.ssl().selected_alpn_protocol() != Some(b"h2") {
                        return serve_h1(tls, seen).await;
                    }
                    let mut conn = http2::server::handshake(tls).await.unwrap();
                    while let Some(Ok((request, mut respond))) = conn.accept().await {
                        {
                            let mut s = seen.lock().unwrap();
                            s.proto = "HTTP/2.0".into();
                            s.user_agent = request
                                .headers()
                                .get("user-agent")
                                .map(|v| v.to_str().unwrap().to_owned())
                                .unwrap_or_default();
                        }
                        let response = http::Response::builder().status(200).body(()).unwrap();
                        let mut body = respond.send_response(response, false).unwrap();
                        body.send_data(Bytes::from_static(b"ok"), true).unwrap();
                    }
                });
            }
        });
        (addr, ca, saw)
    }

    /// An HTTP CONNECT proxy tunnelling every request to `target`.
    async fn connect_proxy(target: SocketAddr) -> SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if client.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(byte[0]);
                    }
                    if !head.starts_with(b"CONNECT ") {
                        return;
                    }
                    let mut upstream = tokio::net::TcpStream::connect(target).await.unwrap();
                    client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        addr
    }

    /// Go's `http.DefaultTransport` keeps idle connections per host; the standard clients
    /// (API-key base URLs, OpenAI-compatible hosts) reuse theirs over HTTP/2 and HTTP/1.1.
    #[tokio::test]
    async fn go_clients_reuse_connections() {
        use std::sync::atomic::Ordering::SeqCst;
        let host = "upstream-reuse.test";
        for alpn in [&b"\x02h2\x08http/1.1"[..], b"\x08http/1.1"] {
            let (ca, addr, accepted) = crate::test_tls::counting_upstream(host, alpn).await;
            let clients = GoClients::new(Hooks {
                trust: Some(CertStore::from_pem_stack(ca).unwrap()),
                resolve: vec![(host.to_owned(), addr)],
            });
            let url = format!("https://{host}:{}/v1/messages", addr.port());
            for _ in 0..3 {
                let upstream = send(&clients.get(&Proxy::Direct), &url, GoHeaders::new(), "{}", None)
                    .await
                    .unwrap();
                let body: Vec<Bytes> = upstream.body.map(Result::unwrap).collect().await;
                assert_eq!(body.concat(), b"ok");
            }
            assert_eq!(accepted.load(SeqCst), 1, "ALPN {alpn:?}");
        }
    }

    /// Go's cloned default transport (proxyutil, ForceAttemptHTTP2) offers h2 and
    /// http/1.1 to every TLS upstream and speaks HTTP/2 wherever the upstream picks it,
    /// directly or through a CONNECT proxy; plain HTTP stays HTTP/1.1. Go's default
    /// User-Agent names the protocol (tests/reference/proxy/main.go `protocols`).
    #[tokio::test]
    async fn go_clients_negotiate_http2_like_go() {
        let fixture = go_fixture();
        let cases = fixture["protocols"].as_array().unwrap();
        assert_eq!(cases.len(), 5);
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let alpn: Option<&'static [u8]> = match case["server"].as_str().unwrap() {
                "h2" => Some(b"\x02h2\x08http/1.1"),
                "h1" => Some(b"\x08http/1.1"),
                _ => None,
            };
            let (addr, ca, saw) = upstream(alpn).await;
            let host = "upstream.test.invalid";
            let proxy = match case["proxy"].as_str().unwrap() {
                "http" => Proxy::Url(format!("http://{}", connect_proxy(addr).await)),
                _ => Proxy::Direct,
            };
            let clients = GoClients::new(Hooks {
                trust: (!ca.is_empty()).then(|| CertStore::from_pem_stack(ca).unwrap()),
                resolve: vec![(host.to_owned(), addr)],
            });
            let scheme = if alpn.is_some() { "https" } else { "http" };
            let route = |_: &url::Url| {
                Ok(Route {
                    client: clients.get(&proxy),
                    order: None,
                })
            };
            let want = Saw {
                proto: case["proto"].as_str().unwrap().to_owned(),
                client_alpn: case["client_alpn"]
                    .as_array()
                    .map(|a| a.iter().map(|p| p.as_str().unwrap().to_owned()).collect()),
                user_agent: case["user_agent"].as_str().unwrap().to_owned(),
            };
            // The second request knows the origin's protocol; the first assumes HTTP/1.1
            // for the default User-Agent only (see `expects_http2`).
            for attempt in ["first", "second"] {
                let upstream = send_request(
                    &route,
                    wreq::Method::GET,
                    &format!("{scheme}://{host}:{}/", addr.port()),
                    GoHeaders::new(),
                    None,
                    Some(std::time::Duration::from_secs(10)),
                )
                .await
                .unwrap_or_else(|e| panic!("{name}: {e:?}"));
                assert_eq!(upstream.status, 200, "{name}");
                assert_eq!(read_all(upstream.body, 16, false).await.unwrap(), "ok", "{name}");
                let mut want = want.clone();
                if attempt == "first" && want.proto == "HTTP/2.0" {
                    want.user_agent = "Go-http-client/1.1".into();
                }
                assert_eq!(*saw.lock().unwrap(), want, "{name} ({attempt} request)");
            }
        }
    }

    /// Every case runs the real `bufio.Scanner` in tests/reference/proxy/main.go.
    #[tokio::test]
    async fn lines_follow_go_bufio_scanner() {
        for case in go_fixture()["lines"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut items: Vec<Result<Bytes, ExecError>> = case["chunks"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| Ok(Bytes::from(c.as_str().unwrap().to_owned())))
                .collect();
            let failed = case["io_error"].as_bool().unwrap();
            if failed {
                items.push(Err(ExecError::local(502, FailureScope::Transport, "read failed")));
            }
            let body = futures_util::stream::iter(items);
            // A failed reader is never read again; otherwise the body ends (EOF).
            let body = if failed {
                body.chain(futures_util::stream::pending()).boxed()
            } else {
                body.boxed()
            };
            let max = case["max"].as_u64().unwrap() as usize;
            let got: Vec<_> = lines(body, max).collect().await;
            // Go with a separate terminal read differs only where the ponytail says.
            let separate = (case["separate_tokens"].clone(), case["separate_error"].clone());
            let attached = (case["tokens"].clone(), case["error"].clone());
            let differs = ["exact-max-unterminated", "exact-max-then-io-error"].contains(&name);
            assert_eq!(separate != attached, differs, "{name}: Go's two terminal modes");
            let (tokens, errors): (Vec<_>, Vec<_>) = got.into_iter().partition(Result::is_ok);
            let tokens: Vec<String> = tokens
                .into_iter()
                .map(|t| String::from_utf8(t.unwrap().to_vec()).unwrap())
                .collect();
            let want: Vec<&str> = case["tokens"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t.as_str().unwrap())
                .collect();
            assert_eq!(tokens, want, "{name}");
            let error = errors.into_iter().map(|e| e.unwrap_err()).collect::<Vec<_>>();
            match case["error"].as_str().unwrap() {
                "" => assert!(error.is_empty(), "{name}"),
                "too_long" => {
                    assert_eq!(error.len(), 1, "{name}");
                    assert_eq!(error[0].body, "bufio.Scanner: token too long", "{name}");
                }
                _ => {
                    assert_eq!(error.len(), 1, "{name}");
                    assert_eq!(error[0].body, "read failed", "{name}");
                }
            }
        }
    }

    /// Go's own table (net/url url_test.go `resolvePathTests`), plus merged `//` paths.
    #[test]
    fn resolve_path_matches_go() {
        for (base, reference, want) in [
            ("a/b", ".", "/a/"),
            ("a/b", "c", "/a/c"),
            ("a/b", "..", "/"),
            ("a/", "..", "/"),
            ("a/", "../..", "/"),
            ("a/b/c", "..", "/a/"),
            ("a/b/c", "../d", "/a/d"),
            ("a/b/c", ".././d", "/a/d"),
            ("a/b", "./..", "/"),
            ("a/./b", ".", "/a/"),
            ("a/../", ".", "/"),
            ("a/.././b", "c", "/c"),
            ("//api/item", "next", "//api/next"),
            ("", "next", "/next"),
            ("", "", ""),
        ] {
            assert_eq!(go_resolve_path(base, reference), want, "{base:?} + {reference:?}");
        }
    }

    /// Go's `http.Client` redirect hops for GET and POST (tests/reference/proxy/main.go),
    /// replayed through [`request`] against the same scripted servers.
    #[tokio::test]
    async fn redirects_follow_go_http_client() {
        type Hops = Arc<Mutex<Vec<serde_json::Value>>>;
        async fn record(
            hops: Hops,
            script: Arc<serde_json::Value>,
            other: Arc<String>,
            request: axum::extract::Request,
        ) -> axum::response::Response {
            use axum::response::IntoResponse;
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
            let header = |name: &str| {
                parts
                    .headers
                    .get_all(name)
                    .iter()
                    .map(|v| v.to_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let path = parts.uri.path().to_owned();
            let target = parts.uri.path_and_query().map_or("/", |p| p.as_str()).to_owned();
            hops.lock().unwrap().push(serde_json::json!({
                "user_agent": header("user-agent"),
                "has_user_agent": parts.headers.contains_key("user-agent"),
                "accept_encoding": header("accept-encoding"),
                "method": parts.method.as_str(),
                "path": target,
                "host": header("host"),
                "referer": header("referer"),
                "authorization": header("authorization"),
                "content_type": header("content-type"),
                "content_length": header("content-length"),
                "body": String::from_utf8_lossy(&body),
            }));
            let Some(step) = script.get(&path) else {
                return (http::StatusCode::OK, "done").into_response();
            };
            let status = http::StatusCode::from_u16(step["status"].as_u64().unwrap() as u16).unwrap();
            let location = step["location"].as_str().unwrap().replacen("OTHER", &other, 1);
            if location.is_empty() {
                return status.into_response();
            }
            (status, [(http::header::LOCATION, location)]).into_response()
        }
        let fixture = go_fixture();
        let cases = fixture["redirects"].as_array().unwrap();
        // Go always sends the URL as written; Rust does with exact_target, and normalizes
        // otherwise, which only dot segments and escaping can tell apart.
        let runs = cases.iter().flat_map(|case| {
            let start = case["start"].as_str().unwrap();
            let both = start == "/start";
            [(case, true)].into_iter().chain(both.then_some((case, false)))
        });
        for (case, exact) in runs {
            let name = format!("{} (exact: {exact})", case["name"].as_str().unwrap());
            let main = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (main_addr, other_port) = (main.local_addr().unwrap(), second.local_addr().unwrap().port());
            let other = Arc::new(format!("http://localhost:{other_port}"));
            let hops: Hops = Arc::default();
            let script = Arc::new(case["script"].clone());
            for listener in [main, second] {
                let (hops, script, other) = (hops.clone(), script.clone(), other.clone());
                let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
                    record(hops.clone(), script.clone(), other.clone(), request)
                });
                tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            }
            let mut headers = GoHeaders::new();
            for (k, v) in case["headers"].as_object().unwrap() {
                headers.set(k, v.as_str().unwrap());
            }
            if exact {
                headers.exact_target();
            }
            if case["disable_compression"].as_bool().unwrap() {
                headers.disable_compression();
            }
            let base = format!("http://{main_addr}");
            let method = wreq::Method::from_bytes(case["method"].as_str().unwrap().as_bytes()).unwrap();
            let body = case["body"].as_str().map(|b| Bytes::from(b.to_owned()));
            let result = request(
                &default_client(),
                method,
                &format!("{base}{}", case["start"].as_str().unwrap()),
                headers,
                body,
                Some(std::time::Duration::from_secs(10)),
            )
            .await;
            match &result {
                Ok(upstream) => assert_eq!(u64::from(upstream.status), case["status"].as_u64().unwrap(), "{name}"),
                Err(_) => assert!(case["error"].as_bool().unwrap(), "{name}: unexpected error"),
            }
            let got: Vec<serde_json::Value> = hops
                .lock()
                .unwrap()
                .iter()
                .map(|h| {
                    let mut h = h.clone();
                    let host = h["host"]
                        .as_str()
                        .unwrap()
                        .replace(&main_addr.to_string(), "BASE")
                        .replace(&format!("localhost:{other_port}"), "OTHER");
                    h["host"] = host.into();
                    h["referer"] = h["referer"].as_str().unwrap().replacen(&base, "BASE", 1).into();
                    h
                })
                .collect();
            assert_eq!(&got, case["hops"].as_array().unwrap(), "{name}");
        }
    }

    /// Go's `Client.Timeout` bounds the whole exchange: redirect hops share one deadline
    /// instead of each getting the full timeout.
    #[tokio::test]
    async fn timeout_bounds_all_redirect_hops_together() {
        use axum::response::IntoResponse;
        const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);
        const HOP: std::time::Duration = std::time::Duration::from_millis(600);
        async fn get(target: String) -> Result<RawUpstream, SendError> {
            let client = default_client();
            let route = |_: &url::Url| {
                Ok(Route {
                    client: client.clone(),
                    order: None,
                })
            };
            send_request_raw(
                &route,
                wreq::Method::GET,
                &target,
                GoHeaders::new(),
                None,
                Some(TIMEOUT),
            )
            .await
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().fallback(|uri: http::Uri| async move {
            tokio::time::sleep(HOP).await;
            match uri.path() {
                "/1" => (http::StatusCode::FOUND, [(http::header::LOCATION, "/2")]).into_response(),
                "/2" => (http::StatusCode::FOUND, [(http::header::LOCATION, "/3")]).into_response(),
                _ => "done".into_response(),
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // One slow hop within the timeout still succeeds, body included.
        let upstream = get(format!("http://{addr}/3")).await.unwrap();
        assert_eq!(upstream.status, 200);
        let body: Vec<Bytes> = upstream.body.map(Result::unwrap).collect().await;
        assert_eq!(body.concat(), b"done");
        // Each hop is under the timeout, but the second ends past the shared deadline.
        match get(format!("http://{addr}/1")).await {
            Err(SendError::Transport { error, url }) => {
                assert!(error.is_timeout(), "{error:?}");
                assert_eq!(url, format!("http://{addr}/2"));
            }
            Err(other) => panic!("{other:?}"),
            Ok(upstream) => panic!("followed both redirects: {}", upstream.status),
        }
    }

    /// Once the deadline has passed, Go sends nothing: a 307 that would resend a POST
    /// body to its target ends without a request, with the error a transport timeout
    /// gives callers.
    #[tokio::test]
    async fn no_hop_is_sent_after_the_deadline() {
        use axum::response::IntoResponse;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        const TIMEOUT: std::time::Duration = std::time::Duration::from_millis(200);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = hits.clone();
        let app = axum::Router::new().fallback(move |uri: http::Uri| {
            let seen = seen.clone();
            async move {
                if uri.path() == "/start" {
                    return (
                        http::StatusCode::TEMPORARY_REDIRECT,
                        [(http::header::LOCATION, "/target")],
                    )
                        .into_response();
                }
                seen.fetch_add(1, SeqCst);
                "done".into_response()
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = default_client();
        // The redirect handling uses up the budget: choosing the second hop's client
        // takes longer than the whole timeout.
        let route = |next: &url::Url| {
            if next.path() == "/target" {
                std::thread::sleep(TIMEOUT);
            }
            Ok(Route {
                client: client.clone(),
                order: None,
            })
        };
        let start = format!("http://{addr}/start");
        let post = || Some(Bytes::from_static(b"{}"));
        let sent = send_request_raw(
            &route,
            wreq::Method::POST,
            &start,
            GoHeaders::new(),
            post(),
            Some(TIMEOUT),
        )
        .await;
        match sent {
            Err(SendError::Timeout { url }) => assert_eq!(url, format!("http://{addr}/target")),
            Err(other) => panic!("{other:?}"),
            Ok(upstream) => panic!("followed the redirect: {}", upstream.status),
        }
        // The same error as a transport timeout through `send_request`.
        let error = send_request(
            &route,
            wreq::Method::POST,
            &start,
            GoHeaders::new(),
            post(),
            Some(TIMEOUT),
        )
        .await
        .err()
        .unwrap();
        // `transport_error` reads nothing from the wreq error; any one will do.
        let any = default_client().get("not a url").send().await.unwrap_err();
        let transport = crate::upstream::transport_error(any);
        assert_eq!(
            (error.status, error.scope, error.body),
            (transport.status, transport.scope, transport.body)
        );
        assert_eq!(hits.load(SeqCst), 0, "the redirect target got a request");
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
