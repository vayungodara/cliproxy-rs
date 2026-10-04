//! A local fake Home for tests: a RESP2 server that records every command, answers
//! through a handler and can push pub/sub messages to subscribed connections, over plain
//! TCP or mTLS with a throwaway test CA ([`Pki`]).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufStream};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use btls::asn1::Asn1Time;
use btls::bn::BigNum;
use btls::hash::MessageDigest;
use btls::pkey::{PKey, Private};
use btls::rsa::Rsa;
use btls::ssl::{SslAcceptor, SslMethod, SslVerifyMode};
use btls::x509::extension::{BasicConstraints, SubjectAlternativeName};
use btls::x509::{X509, X509NameBuilder, X509Req};

use crate::client::Client;
use crate::config::HomeConfig;
use crate::resp::{self, Value};

/// What the fake does with one command.
pub enum Reply {
    /// Raw RESP bytes.
    Bytes(Vec<u8>),
    /// Read the command, never answer (the connection stays open).
    Hang,
    /// Close the connection without answering.
    Close,
}

pub fn bulk(s: impl AsRef<[u8]>) -> Reply {
    let s = s.as_ref();
    let mut out = format!("${}\r\n", s.len()).into_bytes();
    out.extend_from_slice(s);
    out.extend_from_slice(b"\r\n");
    Reply::Bytes(out)
}

/// Marks `client`'s heartbeat healthy or not, as the subscriber's heartbeat would.
pub fn set_heartbeat(client: &Client, ok: bool) {
    client.set_heartbeat(ok);
}

pub fn raw(s: &str) -> Reply {
    Reply::Bytes(s.as_bytes().to_vec())
}

/// `["message", channel, payload]`.
pub fn message(channel: &str, payload: &[u8]) -> Vec<u8> {
    resp::encode(&[b"message".as_slice(), channel.as_bytes(), payload])
}

pub fn pong() -> Vec<u8> {
    resp::encode(&["pong", ""])
}

/// `["subscribe", "config", 1]`, Home's single ACK.
pub fn subscribe_ack() -> Reply {
    raw("*3\r\n$9\r\nsubscribe\r\n$6\r\nconfig\r\n:1\r\n")
}

type Handler = Arc<dyn Fn(&[String]) -> Reply + Send + Sync>;

pub struct FakeHome {
    pub addr: SocketAddr,
    log: Arc<Mutex<Vec<Vec<String>>>>,
    push: broadcast::Sender<Vec<u8>>,
    shutdown: CancellationToken,
}

impl Drop for FakeHome {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl FakeHome {
    pub async fn start(handler: impl Fn(&[String]) -> Reply + Send + Sync + 'static) -> Self {
        Self::start_with(None, handler).await
    }

    /// An mTLS fake: clients must present a certificate issued by `acceptor`'s CA.
    pub async fn start_tls(
        acceptor: SslAcceptor,
        handler: impl Fn(&[String]) -> Reply + Send + Sync + 'static,
    ) -> Self {
        Self::start_with(Some(acceptor), handler).await
    }

    async fn start_with(
        tls: Option<SslAcceptor>,
        handler: impl Fn(&[String]) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind fake home");
        let addr = listener.local_addr().expect("fake home address");
        let log = Arc::new(Mutex::new(Vec::new()));
        let (push, _) = broadcast::channel(64);
        let shutdown = CancellationToken::new();
        let handler: Handler = Arc::new(handler);
        tokio::spawn({
            let (log, push, shutdown) = (log.clone(), push.clone(), shutdown.clone());
            async move {
                loop {
                    let accepted = tokio::select! {
                        _ = shutdown.cancelled() => return,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((socket, _)) = accepted else { return };
                    let (log, handler, push, shutdown) = (log.clone(), handler.clone(), push.clone(), shutdown.clone());
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        let io: Box<dyn resp::Io> = match tls {
                            None => Box::new(socket),
                            Some(acceptor) => {
                                let Ok(ssl) = btls::ssl::Ssl::new(acceptor.context()) else {
                                    return;
                                };
                                let Ok(mut stream) = tokio_btls::SslStream::new(ssl, socket) else {
                                    return;
                                };
                                if std::pin::Pin::new(&mut stream).accept().await.is_err() {
                                    return;
                                }
                                Box::new(stream)
                            }
                        };
                        tokio::select! {
                            _ = shutdown.cancelled() => {}
                            _ = serve(io, log, handler, push) => {}
                        }
                    });
                }
            }
        });
        Self {
            addr,
            log,
            push,
            shutdown,
        }
    }

    /// Every command received so far, lossily decoded.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.log.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn count(&self, name: &str) -> usize {
        self.commands()
            .iter()
            .filter(|c| c.first().is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .count()
    }

    /// Sends raw RESP bytes to every subscribed connection.
    pub fn push(&self, bytes: Vec<u8>) {
        let _ = self.push.send(bytes);
    }

    pub fn config(&self) -> HomeConfig {
        HomeConfig {
            enabled: true,
            node_id: "node-1".into(),
            host: "127.0.0.1".into(),
            port: self.addr.port(),
            disable_cluster_discovery: true,
            ..HomeConfig::default()
        }
    }

    /// A client for this fake with short operation timeouts.
    pub fn client(&self) -> Client {
        Client::with_options(
            self.config(),
            Duration::from_millis(300),
            "11111111-2222-3333-4444-555555555555".into(),
        )
    }
}

async fn serve(
    socket: Box<dyn resp::Io>,
    log: Arc<Mutex<Vec<Vec<String>>>>,
    handler: Handler,
    push: broadcast::Sender<Vec<u8>>,
) {
    let mut io = BufStream::new(socket);
    let mut pushed: Option<broadcast::Receiver<Vec<u8>>> = None;
    loop {
        let command = match &mut pushed {
            None => resp::read(&mut io).await,
            Some(rx) => tokio::select! {
                command = resp::read(&mut io) => command,
                bytes = rx.recv() => {
                    let Ok(bytes) = bytes else { return };
                    if io.write_all(&bytes).await.is_err() || io.flush().await.is_err() {
                        return;
                    }
                    continue;
                }
            },
        };
        let Ok(Value::Array(items)) = command else { return };
        let args: Vec<String> = items
            .iter()
            .map(|v| match v {
                Value::Bulk(b) => String::from_utf8_lossy(b).into_owned(),
                other => format!("{other:?}"),
            })
            .collect();
        log.lock().unwrap_or_else(PoisonError::into_inner).push(args.clone());
        let subscribe = args.first().is_some_and(|c| c.eq_ignore_ascii_case("subscribe"));
        match handler(&args) {
            Reply::Bytes(bytes) => {
                if io.write_all(&bytes).await.is_err() || io.flush().await.is_err() {
                    return;
                }
                if subscribe && !bytes.starts_with(b"-") {
                    pushed = Some(push.subscribe());
                }
            }
            Reply::Hang => std::future::pending::<()>().await,
            Reply::Close => return,
        }
    }
}

enum San<'a> {
    None,
    Ip(&'a str),
    Dns(&'a str),
}

/// A throwaway CA with a `127.0.0.1` server certificate.
pub struct Pki {
    pub ca: X509,
    ca_key: PKey<Private>,
    server: X509,
    server_key: PKey<Private>,
}

fn key() -> PKey<Private> {
    PKey::from_rsa(Rsa::generate(2048).expect("rsa")).expect("pkey")
}

fn name(cn: &str) -> btls::x509::X509Name {
    let mut name = X509NameBuilder::new().expect("name");
    name.append_entry_by_text("CN", cn).expect("cn");
    name.build()
}

impl Pki {
    pub fn new(ca_name: &str) -> Self {
        let ca_key = key();
        let mut ca = X509::builder().expect("x509");
        ca.set_version(2).unwrap();
        ca.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        ca.set_subject_name(&name(ca_name)).unwrap();
        ca.set_issuer_name(&name(ca_name)).unwrap();
        ca.set_pubkey(&ca_key).unwrap();
        ca.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        ca.set_not_after(&Asn1Time::days_from_now(30).unwrap()).unwrap();
        ca.append_extension(&BasicConstraints::new().critical().ca().build().unwrap())
            .unwrap();
        ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
        let ca = ca.build();
        let server_key = key();
        let server = Self::issue(&ca, &ca_key, &name("home"), &server_key, 2, San::Ip("127.0.0.1"));
        Self {
            ca,
            ca_key,
            server,
            server_key,
        }
    }

    fn issue<T: btls::pkey::HasPublic>(
        ca: &X509,
        ca_key: &PKey<Private>,
        subject: &btls::x509::X509NameRef,
        public: &PKey<T>,
        serial: u32,
        san: San,
    ) -> X509 {
        let mut cert = X509::builder().unwrap();
        cert.set_version(2).unwrap();
        cert.set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        cert.set_subject_name(subject).unwrap();
        cert.set_issuer_name(ca.subject_name()).unwrap();
        cert.set_pubkey(public).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(30).unwrap()).unwrap();
        let mut names = SubjectAlternativeName::new();
        let names = match san {
            San::None => None,
            San::Ip(ip) => Some(names.ip(ip)),
            San::Dns(dns) => Some(names.dns(dns)),
        };
        if let Some(names) = names {
            let extension = names.build(&cert.x509v3_context(Some(ca), None)).unwrap();
            cert.append_extension(&extension).unwrap();
        }
        cert.sign(ca_key, MessageDigest::sha256()).unwrap();
        cert.build()
    }

    pub fn ca_pem(&self) -> String {
        String::from_utf8(self.ca.to_pem().unwrap()).unwrap()
    }

    /// Signs a client CSR after checking its self-signature; returns the CN and PEM.
    pub fn sign_csr(&self, csr_pem: &[u8]) -> Result<(String, String), String> {
        let csr = X509Req::from_pem(csr_pem).map_err(|e| e.to_string())?;
        let public = csr.public_key().map_err(|e| e.to_string())?;
        if !csr.verify(&public).map_err(|e| e.to_string())? {
            return Err("csr signature".into());
        }
        let cn = csr
            .subject_name()
            .entries()
            .next()
            .and_then(|e| e.data().as_utf8().ok())
            .map(|s| s.to_string())
            .unwrap_or_default();
        let cert = Self::issue(&self.ca, &self.ca_key, csr.subject_name(), &public, 3, San::None);
        Ok((cn, String::from_utf8(cert.to_pem().unwrap()).unwrap()))
    }

    /// A server that requires client certificates issued by this CA.
    pub fn acceptor(&self) -> SslAcceptor {
        self.acceptor_with(&self.server)
    }

    /// The same server under a certificate named `cn`, with an optional DNS SAN.
    pub fn named_acceptor(&self, cn: &str, dns_san: Option<&str>) -> SslAcceptor {
        let san = dns_san.map_or(San::None, San::Dns);
        let cert = Self::issue(&self.ca, &self.ca_key, &name(cn), &self.server_key, 4, san);
        self.acceptor_with(&cert)
    }

    fn acceptor_with(&self, cert: &X509) -> SslAcceptor {
        let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        builder.set_certificate(cert).unwrap();
        builder.set_private_key(&self.server_key).unwrap();
        builder.cert_store_mut().add_cert(self.ca.clone()).unwrap();
        builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
        builder.build()
    }
}
