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
