//! Codex transports (helps/utls_client.go `NewUtlsHTTPClient`, as the Codex executor and
//! Alpha Search use it; codex_websockets_connection.go `newProxyAwareWebsocketDialer`).
//!
//! - `https://chatgpt.com` (OAuth inference and Alpha Search): uTLS `HelloChrome_Auto`,
//!   which is Chrome 133 in uTLS v1.8.2. GREASE, permuted extensions, X25519MLKEM768,
//!   ALPS, brotli certificate compression, ECH GREASE and no session resumption. Dialled
//!   directly or through the configured proxy, never an environment proxy. By default
//!   one connection per request, closed with the body, like Go's dedicated uTLS
//!   connections. With `oauth.providers.codex.chatgpt-keep-alive` (a cliproxy-rs
//!   addition, off by default) the client keeps up to [`CHROME_IDLE_PER_HOST`] idle
//!   connections per host and proxy for 90 seconds (docs/DIFFERENCES-FROM-GO.md).
//! - Every other origin (API-key base URLs, local mocks) and the Responses WebSocket:
//!   Go's standard transport from [`crate::proxy`].
//!
//! Both are cached per effective proxy (and, for Chrome, per keep-alive setting) in
//! bounded 64-entry LRUs.

use std::sync::Mutex;

use wreq::tls::compress::{CertificateCompressionAlgorithm, CertificateCompressor, Codec};
use wreq::tls::{AlpnProtocol, AlpsProtocol, KeyShare, TlsOptions, TlsVersion};

use crate::proxy::{CACHE_CAPACITY, GoClients, Hooks, Proxy};

/// Idle chatgpt.com connections kept per host, in each per-proxy client, when keep-alive
/// is on: Go's default `MaxIdleConnsPerHost`, as its Anthropic transport keeps. HTTP/2
/// multiplexes on one.
pub(crate) const CHROME_IDLE_PER_HOST: usize = 2;

/// How long an idle chatgpt.com connection is kept.
const CHROME_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

pub(crate) struct Transport {
    standard: GoClients,
    hooks: Hooks,
    /// Chrome clients by effective proxy and keep-alive setting, most recent first.
    chrome: Mutex<Vec<((Proxy, bool), wreq::Client)>>,
}

impl Transport {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            standard: GoClients::new(hooks.clone()),
            hooks,
            chrome: Mutex::default(),
        }
    }

    /// Unproxied standard requests use `client` (tests point it at local mocks).
    pub fn with_default(client: wreq::Client) -> Self {
        Self {
            standard: GoClients::with_default(client),
            hooks: Hooks::default(),
            chrome: Mutex::default(),
        }
    }

    /// The client for one request to `url` (`fallbackRoundTripper.RoundTrip`).
    /// `keep_alive` is `chatgpt-keep-alive` and only affects chatgpt.com.
    pub fn for_url(&self, url: &str, proxy: &Proxy, keep_alive: bool) -> wreq::Client {
        if is_chatgpt(url) {
            self.chrome(proxy, keep_alive)
        } else {
            self.standard.get(proxy)
        }
    }

    /// Go's standard transport (the WebSocket dialer honours environment proxies too).
    pub fn standard(&self, proxy: &Proxy) -> wreq::Client {
        self.standard.get(proxy)
    }

    fn chrome(&self, proxy: &Proxy, keep_alive: bool) -> wreq::Client {
        let mut cache = self.chrome.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !keep_alive {
            // Keep-alive is off for this request, so it was turned off (or never on): retire
            // every pooled client. Requests in flight hold their own clone and finish; when
            // the last clone goes, its pool closes the idle sockets and its timer stops.
            cache.retain(|((_, pooled), _)| !pooled);
        }
        if let Some(i) = cache.iter().position(|((p, k), _)| p == proxy && *k == keep_alive) {
            let entry = cache.remove(i);
            let client = entry.1.clone();
            cache.insert(0, entry);
            return client;
        }
        // Go logs an unusable proxy dialer and dials directly.
        let client = self
            .build_chrome(proxy, keep_alive)
            .or_else(|_| self.build_chrome(&Proxy::Direct, keep_alive))
            .expect("Chrome TLS client");
        cache.insert(0, ((proxy.clone(), keep_alive), client.clone()));
        cache.truncate(CACHE_CAPACITY);
        client
    }

    fn build_chrome(&self, proxy: &Proxy, keep_alive: bool) -> wreq::Result<wreq::Client> {
        let builder = wreq::Client::builder()
            .redirect(wreq::redirect::Policy::none())
            .tls_options(chrome_options());
        let builder = if keep_alive {
            // Opt-in: the same ClientHello and headers, without a handshake per request.
            builder
                .pool_max_idle_per_host(CHROME_IDLE_PER_HOST)
                .pool_idle_timeout(CHROME_IDLE_TIMEOUT)
        } else {
            // Go opens a dedicated uTLS connection per request and closes it with the body.
            builder.pool_max_idle_per_host(0)
        };
        // ponytail: HTTP/2 framing (SETTINGS, window sizes) is wreq's, not Go's x/net/http2.
        proxy.apply(self.hooks.apply(builder), false)?.build()
    }
}

/// `req.URL.Scheme == "https" && strings.EqualFold(req.URL.Hostname(), "chatgpt.com")`.
pub(crate) fn is_chatgpt(url: &str) -> bool {
    url::Url::parse(url)
        .is_ok_and(|u| u.scheme() == "https" && u.host_str().is_some_and(|h| h.eq_ignore_ascii_case("chatgpt.com")))
}

/// uTLS `HelloChrome_133` (u_parrots.go) on BoringSSL.
pub(crate) fn chrome_options() -> TlsOptions {
    static BROTLI: Brotli = Brotli;
    TlsOptions::builder()
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .cipher_list(concat!(
            "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:",
            "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:",
            "ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:",
            "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:",
            "ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:",
            "AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
        ))
        .preserve_tls13_cipher_list(true)
        .curves_list("X25519MLKEM768:X25519:P-256:P-384")
        .sigalgs_list(concat!(
            "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:",
            "ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:",
            "rsa_pss_rsae_sha512:rsa_pkcs1_sha512",
        ))
        .key_shares(vec![KeyShare::X25519_MLKEM768, KeyShare::X25519])
        .grease_enabled(true)
        .permute_extensions(true)
        .alpn_protocols([AlpnProtocol::HTTP2, AlpnProtocol::HTTP1])
        .alps_protocols([AlpsProtocol::HTTP2])
        .alps_use_new_codepoint(true)
        .enable_ech_grease(true)
        .enable_ocsp_stapling(true)
        .enable_signed_cert_timestamps(true)
        .certificate_compressors(vec![&BROTLI as &'static dyn CertificateCompressor])
        .session_ticket(true)
        // Go's uTLS config has no ClientSessionCache: no resumption, no pre_shared_key.
        .pre_shared_key(false)
        .psk_dhe_ke(true)
        .renegotiation(true)
        .build()
}

/// Brotli certificate decompression (RFC 8879). Clients never compress certificates.
#[derive(Debug)]
struct Brotli;

impl CertificateCompressor for Brotli {
    fn compress(&self) -> Codec {
        Codec::Pointer(|_, _| Err(std::io::Error::other("client certificates are never compressed")))
    }

    fn decompress(&self) -> Codec {
        Codec::Pointer(|input, mut output| {
            let mut input = input;
            brotli_decompressor::BrotliDecompress(&mut input, &mut output)
        })
    }

    fn algorithm(&self) -> CertificateCompressionAlgorithm {
        CertificateCompressionAlgorithm::BROTLI
    }
}

#[cfg(test)]
#[path = "codex_tls_tests.rs"]
mod tests;
