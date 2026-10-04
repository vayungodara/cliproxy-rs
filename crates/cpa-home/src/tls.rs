//! Dialing Home: TCP, then optional mTLS (Go `newHomeTLSConfig`). TLS 1.2 minimum, the
//! system roots plus the pinned Home CA, the client certificate from bootstrap.

use std::time::Duration;

use btls::ssl::{SslConnector, SslFiletype, SslMethod, SslVerifyMode, SslVersion};
use btls::x509::X509;
use btls::x509::verify::X509CheckFlags;
use tokio::net::TcpStream;

use crate::config::HomeTlsConfig;
use crate::error::{Error, Result};
use crate::resp::Conn;

struct Tls {
    connector: SslConnector,
    server_name: String,
    verify: bool,
}

/// Everything needed to open a connection to one Home address.
pub(crate) struct Dialer {
    pub host: String,
    pub port: u16,
    pub timeout: Duration,
    tls: Option<Tls>,
}

impl Dialer {
    pub(crate) fn new(
        cfg: &HomeTlsConfig,
        host: &str,
        port: u16,
        server_name: &str,
        timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            host: host.to_owned(),
            port,
            timeout,
            tls: connector(cfg)?.map(|connector| Tls {
                connector,
                server_name: match cfg.server_name.trim() {
                    "" => server_name.trim().to_owned(),
                    explicit => explicit.to_owned(),
                },
                verify: !cfg.insecure_skip_verify,
            }),
        })
    }

    /// The name a TLS dial verifies, `None` without TLS.
    #[cfg(test)]
    pub(crate) fn server_name(&self) -> Option<&str> {
        self.tls.as_ref().map(|t| t.server_name.as_str())
    }

    /// Plain TCP, for certificate enrollment before the client has a certificate.
    pub(crate) fn plain(host: &str, port: u16, timeout: Duration) -> Self {
        Self {
            host: host.to_owned(),
            port,
            timeout,
            tls: None,
        }
    }

    /// Connects, including the TLS handshake, within the dial timeout.
    pub(crate) async fn dial(&self) -> Result<Conn> {
        tokio::time::timeout(self.timeout, self.connect())
            .await
            .unwrap_or(Err(Error::Timeout))
    }

    async fn connect(&self) -> Result<Conn> {
        let tcp = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| Error::Transport(format!("dial tcp {}: {e}", join_host_port(&self.host, self.port))))?;
        let _ = tcp.set_nodelay(true);
        let Some(tls) = &self.tls else {
            return Ok(Conn::new(Box::new(tcp)));
        };
        let mut ssl = tls
            .connector
            .configure()
            .and_then(|c| c.verify_hostname(tls.verify).into_ssl(&tls.server_name))
            .map_err(|e| Error::Transport(format!("home tls: {e}")))?;
        if tls.verify {
            // Go matches names against SANs only; BoringSSL would fall back to the
            // subject CN. Set after `into_ssl`, which overwrites the flags.
            ssl.param_mut()
                .set_hostflags(X509CheckFlags::NO_PARTIAL_WILDCARDS | X509CheckFlags::NEVER_CHECK_SUBJECT);
        }
        let mut stream =
            tokio_btls::SslStream::new(ssl, tcp).map_err(|e| Error::Transport(format!("home tls: {e}")))?;
        std::pin::Pin::new(&mut stream)
            .connect()
            .await
            .map_err(|e| Error::Transport(format!("home tls handshake: {e}")))?;
        Ok(Conn::new(Box::new(stream)))
    }
}

/// Go `net.JoinHostPort`.
pub(crate) fn join_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Go `newHomeTLSConfig`; `None` when TLS is off.
fn connector(cfg: &HomeTlsConfig) -> Result<Option<SslConnector>> {
    if !cfg.enable {
        return Ok(None);
    }
    let tls_error = |e: btls::error::ErrorStack| Error::Other(format!("home tls: {e}"));
    // `builder` loads the system roots, like Go's SystemCertPool.
    let mut builder = SslConnector::builder(SslMethod::tls()).map_err(tls_error)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1_2))
        .map_err(tls_error)?;
    if cfg.insecure_skip_verify {
        builder.set_verify(SslVerifyMode::NONE);
    }
    match (&cfg.client_cert, &cfg.client_key) {
        (None, None) => {}
        (Some(cert), Some(key)) => {
            let load = |e: btls::error::ErrorStack| Error::Other(format!("home tls: load client certificate: {e}"));
            builder.set_certificate_chain_file(cert).map_err(load)?;
            builder.set_private_key_file(key, SslFiletype::PEM).map_err(load)?;
            builder.check_private_key().map_err(load)?;
        }
        _ => {
            return Err(Error::Other(
                "home tls: client certificate and key must be set together".into(),
            ));
        }
    }
    if let Some(ca) = &cfg.ca_cert {
        let pem = std::fs::read(ca).map_err(|e| Error::Other(format!("home tls: read ca-cert: {e}")))?;
        let certs = X509::stack_from_pem(&pem).unwrap_or_default();
        if certs.is_empty() {
            return Err(Error::Other("home tls: ca-cert contains no PEM certificates".into()));
        }
        let store = builder.cert_store_mut();
        for cert in certs {
            store.add_cert(cert).map_err(tls_error)?;
        }
    }
    Ok(Some(builder.build()))
}
