//! Claude transports (helps/utls_client.go, auth/claude/utls_transport.go).
//!
//! First-party Anthropic inference uses the deterministic Node/OpenSSL ClientHello
//! with HTTP/1.1-only ALPN and a final pre_shared_key extension that stays silent
//! until a session is cached. Clients are cached per effective proxy in a bounded
//! 64-entry LRU, and each owns its own 32-entry TLS session cache, so resumption
//! never crosses proxy boundaries. OAuth acquisition has its own compact profile.
//! Every other origin uses Go's standard transport from [`crate::proxy`].

use std::sync::{Arc, Mutex};

use crate::proxy::{Hooks, Proxy};
use wreq::tls::session::LruTlsSessionCache;
use wreq::tls::{AlpnProtocol, ExtensionType, KeyShare, TlsOptions, TlsVersion};

pub(crate) const TRANSPORT_CACHE: usize = crate::proxy::CACHE_CAPACITY;
const SESSION_CACHE: usize = 32;

pub(crate) struct Clients {
    pub native: wreq::Client,
    pub oauth: wreq::Client,
}

/// Process-wide transport cache. Construct once per executor set.
pub struct Transport {
    hooks: Hooks,
    cache: Mutex<Vec<(Proxy, Arc<Clients>)>>,
}

impl Transport {
    pub fn new(hooks: Hooks) -> Self {
        Self {
            hooks,
            cache: Mutex::default(),
        }
    }

    /// Clients for `proxy`, most recently used first; evicts the least recently used.
    pub(crate) fn clients(&self, proxy: &Proxy) -> wreq::Result<Arc<Clients>> {
        let mut cache = self.cache.lock().expect("transport cache");
        if let Some(i) = cache.iter().position(|(p, _)| p == proxy) {
            let entry = cache.remove(i);
            let clients = entry.1.clone();
            cache.insert(0, entry);
            return Ok(clients);
        }
        let clients = Arc::new(Clients {
            native: self.build(Profile::Native, proxy)?,
            oauth: self.build(Profile::OAuth, proxy)?,
        });
        cache.insert(0, (proxy.clone(), clients.clone()));
        cache.truncate(TRANSPORT_CACHE);
        Ok(clients)
    }

    fn build(&self, profile: Profile, proxy: &Proxy) -> wreq::Result<wreq::Client> {
        let mut builder = wreq::Client::builder().redirect(wreq::redirect::Policy::none());
        builder = match profile {
            Profile::Native => builder
                .http1_only()
                .tls_options(options(false))
                .tls_session_cache(LruTlsSessionCache::new(SESSION_CACHE)),
            // No ALPN extension on the compact OAuth hello; HTTP/1.1 follows.
            Profile::OAuth => builder.tls_options(options(true)),
        };
        // uTLS dialers never inherit environment proxies; an unusable proxy is logged
        // by Go and dialled as if none were configured.
        proxy.apply(self.hooks.apply(builder), false)?.build()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Profile {
    Native,
    OAuth,
}

pub(crate) fn options(oauth: bool) -> TlsOptions {
    // The cipher order is shared by both Go profiles; preserve the TLS 1.3 prefix.
    let ciphers = concat!(
        "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:",
        "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:",
        "ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:",
        "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:",
        "ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:",
        "ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:",
        "AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA",
    );
    let extensions: Vec<ExtensionType> = if oauth {
        [0, 23, 65281, 10, 11, 35, 13, 51, 45, 43, 21, 41]
            .into_iter()
            .map(Into::into)
            .collect()
    } else {
        [0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43, 21, 41]
            .into_iter()
            .map(Into::into)
            .collect()
    };
    TlsOptions::builder()
        .min_tls_version(TlsVersion::TLS_1_2).max_tls_version(TlsVersion::TLS_1_3)
        .cipher_list(ciphers).preserve_tls13_cipher_list(true)
        .curves_list("X25519:P-256:P-384")
        .sigalgs_list("ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:rsa_pkcs1_sha512:rsa_pkcs1_sha1")
        .key_shares(vec![KeyShare::X25519])
        .grease_enabled(false).permute_extensions(false).extension_permutation(extensions)
        .alpn_protocols(if oauth { Vec::new() } else { vec![AlpnProtocol::HTTP1] })
        .enable_ocsp_stapling(!oauth).enable_signed_cert_timestamps(!oauth)
        .session_ticket(true).pre_shared_key(true).psk_dhe_ke(true).renegotiation(true)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[derive(Debug)]
    struct Hello {
        ciphers: Vec<u16>,
        extensions: Vec<(u16, Vec<u8>)>,
        session_len: usize,
        record_len: usize,
    }

    async fn capture(oauth: bool) -> Hello {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let capture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = [0; 5];
            socket.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], 22, "TLS handshake record");
            let mut record = vec![0; u16::from_be_bytes([header[3], header[4]]) as usize];
            socket.read_exact(&mut record).await.unwrap();
            record
        });
        // The production builder, with only a dial override added.
        let transport = Transport::new(Hooks {
            trust: None,
            resolve: vec![("claude-capture.test".into(), address)],
        });
        let clients = transport.clients(&Proxy::Inherit).unwrap();
        let client = if oauth { &clients.oauth } else { &clients.native };
        // The local listener closes after the ClientHello, so TLS must fail before HTTP.
        assert!(
            client
                .get(format!("https://claude-capture.test:{}", address.port()))
                .send()
                .await
                .is_err()
        );
        let record = capture.await.unwrap();
        assert_eq!(record[0], 1);
        let mut cursor = 4 + 2 + 32;
        let session_len = record[cursor] as usize;
        cursor += 1 + session_len;
        let cipher_len = u16::from_be_bytes([record[cursor], record[cursor + 1]]) as usize;
        cursor += 2;
        let ciphers = record[cursor..cursor + cipher_len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .collect();
        cursor += cipher_len;
        let compression_len = record[cursor] as usize;
        cursor += 1 + compression_len;
        let extension_len = u16::from_be_bytes([record[cursor], record[cursor + 1]]) as usize;
        cursor += 2;
        let end = cursor + extension_len;
        let mut extensions = Vec::new();
        while cursor < end {
            let id = u16::from_be_bytes([record[cursor], record[cursor + 1]]);
            let len = u16::from_be_bytes([record[cursor + 2], record[cursor + 3]]) as usize;
            cursor += 4;
            extensions.push((id, record[cursor..cursor + len].to_vec()));
            cursor += len;
        }
        Hello {
            ciphers,
            extensions,
            session_len,
            record_len: record.len(),
        }
    }

    #[tokio::test]
    async fn capture_both_clienthellos_against_go_source_profile() {
        for oauth in [false, true] {
            let hello = capture(oauth).await;
            assert_eq!(hello.session_len, 32);
            // Captured from the actual four Go spec/config builders at 6fecc6e
            // using uTLS v1.8.2 and a local raw TCP listener with the same SNI.
            assert_eq!(hello.record_len, if oauth { 251 } else { 512 });
            // Source-derived cipher sequence: helps/utls_client.go and auth/claude/utls_transport.go.
            assert_eq!(
                hello.ciphers,
                [
                    0x1301, 0x1302, 0x1303, 0xc02b, 0xc02f, 0xc02c, 0xc030, 0xcca9, 0xcca8, 0xc009, 0xc013, 0xc00a,
                    0xc014, 0x009c, 0x009d, 0x002f, 0x0035
                ]
            );
            let extension = |id| {
                hello
                    .extensions
                    .iter()
                    .find(|(key, _)| *key == id)
                    .map(|(_, bytes)| bytes.as_slice())
            };
            assert_eq!(extension(10), Some([0, 6, 0, 29, 0, 23, 0, 24].as_slice()));
            assert_eq!(extension(43), Some([4, 3, 4, 3, 3].as_slice()));
            assert_eq!(extension(45), Some([1, 1].as_slice()));
            assert_eq!(
                extension(0),
                Some(b"\x00\x16\x00\x00\x13claude-capture.test".as_slice())
            );
            assert_eq!(extension(11), Some([1, 0].as_slice()));
            assert_eq!(extension(23), Some([].as_slice()));
            assert_eq!(extension(35), Some([].as_slice()));
            assert_eq!(extension(65281), Some([0].as_slice()));
            assert_eq!(
                extension(13),
                Some([0, 18, 4, 3, 8, 4, 4, 1, 5, 3, 8, 5, 5, 1, 8, 6, 6, 1, 2, 1].as_slice())
            );
            let key_share = extension(51).unwrap();
            assert_eq!(key_share.len(), 38);
            assert_eq!(&key_share[..6], [0, 36, 0, 29, 0, 32]);
            let ids: Vec<_> = hello.extensions.iter().map(|(id, _)| *id).collect();
            if !oauth {
                assert_eq!(ids, [0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43, 21]);
                assert_eq!(extension(16), Some(b"\x00\x09\x08http/1.1".as_slice()));
                assert_eq!(extension(5), Some([1, 0, 0, 0, 0].as_slice()));
                assert_eq!(extension(18), Some([].as_slice()));
                assert_eq!(extension(21), Some([0; 229].as_slice()));
            } else {
                assert_eq!(ids, [0, 23, 65281, 10, 11, 35, 13, 51, 45, 43]);
            }
        }
    }

    /// Go's Anthropic transport is an `http.Transport` cached per proxy, so a keep-alive
    /// connection carries the next request; the native client reuses its connection too.
    #[tokio::test]
    async fn native_client_reuses_its_connection() {
        let host = "claude-reuse.test";
        let (ca, addr, accepted) = crate::test_tls::counting_upstream(host, b"\x08http/1.1").await;
        let transport = Transport::new(Hooks {
            trust: Some(wreq::tls::trust::CertStore::from_pem_stack(ca).unwrap()),
            resolve: vec![(host.into(), addr)],
        });
        let url = format!("https://{host}:{}/v1/messages", addr.port());
        for _ in 0..3 {
            let clients = transport.clients(&Proxy::Inherit).unwrap();
            let response = clients.native.post(&url).body("{}").send().await.unwrap();
            assert_eq!(response.bytes().await.unwrap(), "ok");
        }
        assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1, "connections");
    }

    #[test]
    fn transport_lru_bound() {
        let t = Transport::new(Hooks::default());
        let first = t.clients(&Proxy::Url("http://p0:1".into())).unwrap();
        for i in 1..=TRANSPORT_CACHE {
            t.clients(&Proxy::Url(format!("http://p{i}:1"))).unwrap();
        }
        assert_eq!(t.cache.lock().unwrap().len(), TRANSPORT_CACHE);
        // p0 was least recently used and is rebuilt (a distinct client set).
        let again = t.clients(&Proxy::Url("http://p0:1".into())).unwrap();
        assert!(!Arc::ptr_eq(&first, &again));
        let cached = t.clients(&Proxy::Url("http://p0:1".into())).unwrap();
        assert!(Arc::ptr_eq(&again, &cached));
    }
}
