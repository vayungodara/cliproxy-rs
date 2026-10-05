//! Local TLS servers for tests: a throwaway CA per call; nothing leaves loopback.

/// A throwaway CA and a leaf for `hosts`: (CA PEM, acceptor). The acceptor picks the
/// first of the client's ALPN protocols that `alpn` (wire format: length-prefixed
/// names, in server preference order) lists, and refuses ALPN otherwise.
pub(crate) fn acceptor(hosts: &[String], alpn: &'static [u8]) -> (Vec<u8>, btls::ssl::SslAcceptor) {
    let (ca, mut acceptor) = builder(hosts);
    acceptor.set_alpn_select_callback(move |_, client| {
        btls::ssl::select_next_proto(alpn, client).ok_or(btls::ssl::AlpnError::NOACK)
    });
    (ca, acceptor.build())
}

/// [`acceptor`] before its ALPN policy: (CA PEM, acceptor builder).
pub(crate) fn builder(hosts: &[String]) -> (Vec<u8>, btls::ssl::SslAcceptorBuilder) {
    use btls::asn1::Asn1Time;
    use btls::bn::BigNum;
    use btls::ec::{EcGroup, EcKey};
    use btls::hash::MessageDigest;
    use btls::nid::Nid;
    use btls::pkey::PKey;
    use btls::ssl::{SslAcceptor, SslMethod};
    use btls::x509::extension::{BasicConstraints, SubjectAlternativeName};
    use btls::x509::{X509, X509NameBuilder};
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = |_: ()| PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let name = |cn: &str| {
        let mut n = X509NameBuilder::new().unwrap();
        n.append_entry_by_text("CN", cn).unwrap();
        n.build()
    };
    let (ca_key, leaf_key) = (key(()), key(()));
    let ca_name = name("cliproxy-rs Go replay CA");
    let mut ca = X509::builder().unwrap();
    ca.set_version(2).unwrap();
    ca.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    ca.set_subject_name(&ca_name).unwrap();
    ca.set_issuer_name(&ca_name).unwrap();
    ca.set_pubkey(&ca_key).unwrap();
    ca.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    ca.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    ca.append_extension(&BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    ca.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let ca = ca.build();
    let mut leaf = X509::builder().unwrap();
    leaf.set_version(2).unwrap();
    leaf.set_serial_number(&BigNum::from_u32(2).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    leaf.set_subject_name(&name(&hosts[0])).unwrap();
    leaf.set_issuer_name(&ca_name).unwrap();
    leaf.set_pubkey(&leaf_key).unwrap();
    leaf.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    leaf.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    let mut san = SubjectAlternativeName::new();
    for host in hosts {
        san.dns(host);
    }
    let san = san.build(&leaf.x509v3_context(Some(&ca), None)).unwrap();
    leaf.append_extension(&san).unwrap();
    leaf.sign(&ca_key, MessageDigest::sha256()).unwrap();
    let leaf = leaf.build();
    let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
    acceptor.set_private_key(&leaf_key).unwrap();
    acceptor.set_certificate(&leaf).unwrap();
    (ca.to_pem().unwrap(), acceptor)
}

/// A local TLS upstream for `host` that counts the TCP connections it accepts and answers
/// every request `200 ok`: over HTTP/2 when `alpn` selects `h2`, otherwise over HTTP/1.1
/// with keep-alive. Returns (CA PEM, address, accepted connections).
pub(crate) async fn counting_upstream(
    host: &str,
    alpn: &'static [u8],
) -> (
    Vec<u8>,
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (ca, acceptor) = acceptor(&[host.to_owned()], alpn);
    let (acceptor, accepted) = (Arc::new(acceptor), Arc::new(AtomicUsize::new(0)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            count.fetch_add(1, Ordering::SeqCst);
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let ssl = btls::ssl::Ssl::new(acceptor.context()).unwrap();
                let mut tls = tokio_btls::SslStream::new(ssl, tcp).unwrap();
                if std::pin::Pin::new(&mut tls).accept().await.is_err() {
                    return;
                }
                if tls.ssl().selected_alpn_protocol() == Some(b"h2") {
                    let Ok(mut conn) = http2::server::handshake(tls).await else {
                        return;
                    };
                    while let Some(Ok((_, mut respond))) = conn.accept().await {
                        let response = http::Response::builder().status(200).body(()).unwrap();
                        let mut body = respond.send_response(response, false).unwrap();
                        body.send_data(bytes::Bytes::from_static(b"ok"), true).unwrap();
                    }
                } else {
                    serve_h1(tls).await;
                }
            });
        }
    });
    (ca, addr, accepted)
}

/// HTTP/1.1 keep-alive: every request (any body announced by Content-Length is read)
/// gets `200 ok` on the same connection until the client closes it.
async fn serve_h1<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(mut io: S) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = Vec::new();
    loop {
        let head_end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            let mut chunk = [0u8; 4096];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            let mut chunk = [0u8; 4096];
            match io.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.drain(..head_end + length);
        let reply = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok";
        if io.write_all(reply).await.is_err() {
            return;
        }
    }
}
