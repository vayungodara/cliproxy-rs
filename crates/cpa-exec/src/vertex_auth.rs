//! Vertex service-account credentials (internal/auth/vertex, internal/cmd/vertex_import.go)
//! and the Google JWT token source the Vertex executor uses (golang.org/x/oauth2/google
//! `CredentialsFromJSON` with the cloud-platform scope, v0.30.0).
//!
//! Go creates a fresh token source for every request, so every Vertex request with a
//! service account first exchanges a signed JWT at the key's `token_uri`. RS256 with
//! PKCS#1 v1.5 is deterministic, so the exchange is byte-identical to Go's for the same
//! clock second.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine;
use bytes::Bytes;
use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, GoValue};
use serde_json::{Map, Value};

use crate::meta_wire::{self, Slot};
use crate::proxy::{self, GoHeaders};

/// `https://www.googleapis.com/auth/cloud-platform`, the only scope Vertex asks for.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// `google.JWTTokenURL`, used when a key has no `token_uri`.
pub const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Go's `base64.StdEncoding.DecodeString`: padded, line breaks ignored, non-zero
/// trailing bits accepted. The error is the `CorruptInputError` offset Go reports
/// ("illegal base64 data at input byte N"), from the same quantum-by-quantum scan
/// (encoding/base64 decodeQuantum).
fn std_decode(src: &[u8]) -> Result<Vec<u8>, usize> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let newline = |c: u8| c == b'\n' || c == b'\r';
    let mut out = Vec::with_capacity(src.len() / 4 * 3);
    let mut si = 0;
    while si < src.len() {
        let mut quantum = [0u8; 4];
        let mut len = 4;
        let mut trailing = None;
        let mut j = 0;
        while j < 4 {
            if si == src.len() {
                if j == 0 {
                    return Ok(out);
                }
                return Err(si - j);
            }
            let c = src[si];
            si += 1;
            if let Some(v) = value(c) {
                quantum[j] = v;
                j += 1;
                continue;
            }
            if newline(c) {
                continue;
            }
            if c != b'=' || j < 2 {
                return Err(si - 1);
            }
            if j == 2 {
                // "==" is expected; the first "=" is consumed.
                while si < src.len() && newline(src[si]) {
                    si += 1;
                }
                if si == src.len() {
                    return Err(src.len());
                }
                if src[si] != b'=' {
                    return Err(si - 1);
                }
                si += 1;
            }
            while si < src.len() && newline(src[si]) {
                si += 1;
            }
            if si < src.len() {
                trailing = Some(si);
            }
            len = j;
            break;
        }
        let v = u32::from(quantum[0]) << 18
            | u32::from(quantum[1]) << 12
            | u32::from(quantum[2]) << 6
            | u32::from(quantum[3]);
        out.extend_from_slice(&[(v >> 16) as u8, (v >> 8) as u8, v as u8][..len - 1]);
        if let Some(at) = trailing {
            return Err(at);
        }
    }
    Ok(out)
}

/// One PEM block (`encoding/pem.Block`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemBlock {
    pub kind: String,
    pub headers: BTreeMap<String, String>,
    pub bytes: Vec<u8>,
}

/// `pem.getLine`: the line without its newline, a trailing `\r` and trailing spaces or
/// tabs; the rest; and the bytes consumed.
fn get_line(data: &[u8]) -> (&[u8], &[u8], usize) {
    let (mut i, j) = match data.iter().position(|b| *b == b'\n') {
        None => (data.len(), data.len()),
        Some(i) => (i, i + 1),
    };
    if j > i && i > 0 && data[i - 1] == b'\r' {
        i -= 1;
    }
    let mut line = &data[..i];
    while let [rest @ .., b' ' | b'\t'] = line {
        line = rest;
    }
    (line, &data[j..], j)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// Go 1.26 `pem.Decode`: the first END line, the last BEGIN line before it, headers,
/// and the base64 body; `None` when no block parses.
pub fn pem_decode(data: &[u8]) -> Option<PemBlock> {
    const START: &[u8] = b"\n-----BEGIN ";
    const END: &[u8] = b"\n-----END ";
    const EOL: &[u8] = b"-----";
    let mut rest = data;
    let mut end_trailer_index: isize = 0;
    loop {
        if end_trailer_index < 0 || end_trailer_index as usize > rest.len() {
            return None;
        }
        rest = &rest[end_trailer_index as usize..];
        let mut end_index = find(rest, END)? as isize;
        end_trailer_index = end_index + END.len() as isize;
        let Some(begin) = rfind(&rest[..end_index as usize], &START[1..]) else {
            continue;
        };
        if begin > 0 && rest[begin - 1] != b'\n' {
            continue;
        }
        let skip = (begin + START.len() - 1) as isize;
        rest = &rest[skip as usize..];
        end_index -= skip;
        end_trailer_index -= skip;
        let (type_line, next, consumed) = get_line(rest);
        rest = next;
        end_index -= consumed as isize;
        end_trailer_index -= consumed as isize;
        let Some(kind) = type_line.strip_suffix(EOL) else {
            continue;
        };
        let mut headers = BTreeMap::new();
        loop {
            if rest.is_empty() {
                return None;
            }
            let (line, next, consumed) = get_line(rest);
            let Some(colon) = line.iter().position(|b| *b == b':') else {
                break;
            };
            let key = String::from_utf8_lossy(trim_space(&line[..colon])).into_owned();
            let value = String::from_utf8_lossy(trim_space(&line[colon + 1..])).into_owned();
            headers.insert(key, value);
            rest = next;
            end_index -= consumed as isize;
            end_trailer_index -= consumed as isize;
        }
        if !headers.is_empty() && end_index < 0 {
            continue;
        }
        if end_trailer_index < 0 || end_trailer_index as usize > rest.len() {
            continue;
        }
        let trailer = &rest[end_trailer_index as usize..];
        let trailer_len = kind.len() + EOL.len();
        if trailer.len() < trailer_len {
            continue;
        }
        let (end_line, rest_of_end_line) = trailer.split_at(trailer_len);
        if !end_line.starts_with(kind) || !end_line.ends_with(EOL) {
            continue;
        }
        if !get_line(rest_of_end_line).0.is_empty() {
            continue;
        }
        let mut bytes = Vec::new();
        if end_index > 0 {
            let body: Vec<u8> = rest[..end_index as usize]
                .iter()
                .copied()
                .filter(|b| *b != b' ' && *b != b'\t')
                .collect();
            match std_decode(&body) {
                Ok(decoded) => bytes = decoded,
                Err(_) => continue,
            }
        }
        return Some(PemBlock {
            kind: String::from_utf8_lossy(kind).into_owned(),
            headers,
            bytes,
        });
    }
}

/// `pem.EncodeToMemory`: `Proc-Type` first, other headers sorted, then the body in
/// 64-column base64 lines.
pub fn pem_encode(block: &PemBlock) -> String {
    let mut out = format!("-----BEGIN {}-----\n", block.kind);
    if !block.headers.is_empty() {
        if let Some(value) = block.headers.get("Proc-Type") {
            out.push_str(&format!("Proc-Type: {value}\n"));
        }
        for (key, value) in &block.headers {
            if key != "Proc-Type" {
                out.push_str(&format!("{key}: {value}\n"));
            }
        }
        out.push('\n');
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(&block.bytes);
    for line in encoded.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {}-----\n", block.kind));
    out
}

/// `stripANSIEscape`: OSC sequences up to BEL or ST, CSI sequences up to their final
/// letter, and lone ESC characters.
fn strip_ansi(s: &str) -> String {
    let input: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        if c != '\u{1b}' {
            out.push(c);
            i += 1;
            continue;
        }
        if i + 1 >= input.len() {
            i += 1;
            continue;
        }
        match input[i + 1] {
            ']' => {
                i += 2;
                while i < input.len() {
                    if input[i] == '\u{7}' {
                        break;
                    }
                    if input[i] == '\u{1b}' && i + 1 < input.len() && input[i + 1] == '\\' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            '[' => {
                i += 2;
                while i < input.len() {
                    if input[i].is_ascii_alphabetic() {
                        break;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// `rebuildPEM`: the base64 alphabet between the markers of a block that lost its line
/// structure.
fn rebuild_pem(raw: &str) -> Result<String, String> {
    let kind = if raw.contains("RSA PRIVATE KEY") {
        "RSA PRIVATE KEY"
    } else {
        "PRIVATE KEY"
    };
    let header = format!("-----BEGIN {kind}-----");
    let footer = format!("-----END {kind}-----");
    let (Some(start), Some(end)) = (raw.find(&header), raw.find(&footer)) else {
        return Err("missing pem markers".into());
    };
    if end <= start {
        return Err("missing pem markers".into());
    }
    let payload: String = raw[start + header.len()..end]
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '='))
        .collect();
    if payload.is_empty() {
        return Err("private_key base64 payload empty".into());
    }
    let der = std_decode(payload.as_bytes())
        .map_err(|at| format!("private_key base64 decode failed: illegal base64 data at input byte {at}"))?;
    Ok(pem_encode(&PemBlock {
        kind: kind.into(),
        headers: BTreeMap::new(),
        bytes: der,
    }))
}

/// A parsed RSA private key and its PKCS#1 DER (`x509.MarshalPKCS1PrivateKey`).
fn pkcs1_der(rsa: &btls::rsa::Rsa<btls::pkey::Private>) -> Result<Vec<u8>, String> {
    rsa.private_key_to_der().map_err(|e| e.to_string())
}

/// `x509.ParsePKCS1PrivateKey`. The error is Go's for trailing data and a generic
/// x509 message otherwise.
// ponytail: Go also accepts keys that omit the CRT values and multi-prime keys, which
// BoringSSL rejects; Google issues neither. Other parse errors carry no asn1 detail.
fn parse_pkcs1(der: &[u8]) -> Result<btls::rsa::Rsa<btls::pkey::Private>, String> {
    const FAILED: &str = "x509: failed to parse private key";
    let key = btls::rsa::Rsa::private_key_from_der(der).map_err(|_| FAILED)?;
    // BoringSSL's d2i parser stops after the key; asn1.Unmarshal rejects what follows,
    // before any key validation.
    if !single_sequence(der) {
        return Err("asn1: syntax error: trailing data".into());
    }
    if !key.check_key().unwrap_or(false) {
        return Err(FAILED.into());
    }
    Ok(key)
}

/// Whether `der` is exactly one DER SEQUENCE, nothing after it.
fn single_sequence(der: &[u8]) -> bool {
    let [0x30, first, rest @ ..] = der else {
        return false;
    };
    let (len, header) = match *first {
        n if n < 0x80 => (usize::from(n), 2),
        n @ 0x81..=0x84 => {
            let k = usize::from(n & 0x7f);
            let Some(bytes) = rest.get(..k) else {
                return false;
            };
            (bytes.iter().fold(0usize, |a, b| a << 8 | usize::from(*b)), 2 + k)
        }
        _ => return false,
    };
    header.checked_add(len) == Some(der.len())
}

/// PKCS#8: `Err(true)` for a parsed non-RSA key, `Err(false)` for unparseable input.
fn parse_pkcs8_rsa(der: &[u8]) -> Result<btls::rsa::Rsa<btls::pkey::Private>, bool> {
    let key = btls::pkey::PKey::private_key_from_pkcs8(der).map_err(|_| false)?;
    key.rsa().map_err(|_| true)
}

/// `ensureRSAPrivateKey`.
// ponytail: x509 parse errors carry BoringSSL's reason rather than Go's encoding/asn1
// text; which inputs fail matches.
fn ensure_rsa(block: PemBlock) -> Result<PemBlock, String> {
    let pkcs1 = |bytes: Vec<u8>| PemBlock {
        kind: "RSA PRIVATE KEY".into(),
        headers: BTreeMap::new(),
        bytes,
    };
    match block.kind.as_str() {
        "RSA PRIVATE KEY" => match parse_pkcs1(&block.bytes) {
            Ok(_) => Ok(block),
            Err(e) => Err(format!("private_key invalid rsa: {e}")),
        },
        "PRIVATE KEY" => match parse_pkcs8_rsa(&block.bytes) {
            Ok(rsa) => Ok(pkcs1(pkcs1_der(&rsa)?)),
            Err(true) => Err("private_key is not an RSA key".into()),
            Err(false) => Err("private_key invalid pkcs8: x509: failed to parse private key".into()),
        },
        _ => {
            if let Ok(rsa) = parse_pkcs1(&block.bytes) {
                return Ok(pkcs1(pkcs1_der(&rsa)?));
            }
            if let Ok(rsa) = parse_pkcs8_rsa(&block.bytes) {
                return Ok(pkcs1(pkcs1_der(&rsa)?));
            }
            Err("private_key uses unsupported format".into())
        }
    }
}

/// `sanitizePrivateKey`: a valid `RSA PRIVATE KEY` PEM block for a pasted key.
pub fn sanitize_private_key(raw: &str) -> Result<String, String> {
    let pk = raw.replace("\r\n", "\n").replace('\r', "\n");
    let pk = strip_ansi(&pk);
    let pk = String::from_utf8_lossy(trim_space(pk.as_bytes())).into_owned();
    let normalized = if pem_decode(pk.as_bytes()).is_some() {
        pk
    } else {
        rebuild_pem(&pk).map_err(|e| format!("private_key is not valid pem: {e}"))?
    };
    let block = pem_decode(normalized.as_bytes()).ok_or("private_key pem decode failed")?;
    Ok(pem_encode(&ensure_rsa(block)?))
}

/// `NormalizeServiceAccountMap`: a copy whose `private_key` is a valid RSA PEM block.
pub fn normalize_service_account(sa: &Map<String, Value>) -> Result<Map<String, Value>, String> {
    let pk = sa.get("private_key").and_then(Value::as_str).unwrap_or_default();
    if pk.trim().is_empty() {
        return Err("service account missing private_key".into());
    }
    let mut out = sa.clone();
    out.insert("private_key".into(), Value::String(sanitize_private_key(pk)?));
    Ok(out)
}

/// The key-file fields the JWT token source reads.
#[derive(Default)]
struct KeyFile {
    kind: String,
    client_email: String,
    private_key: String,
    private_key_id: String,
    token_uri: String,
    audience: String,
}

impl KeyFile {
    /// `google.CredentialsFromJSON`: Go marshals the normalized map with its keys sorted
    /// and unmarshals it into `credentialsFile` (oauth2 v0.30.0), so a wrongly typed
    /// member of any field fails and, of keys that fold together, the last sorted wins.
    /// The workspace map keeps file order, so the keys are sorted here.
    fn parse(sa: &Map<String, Value>) -> Result<Self, String> {
        let sorted: BTreeMap<&String, &Value> = sa.iter().collect();
        let data = serde_json::to_vec(&sorted).map_err(|e| e.to_string())?;
        let mut f = Self::default();
        let mut unused: [String; 13] = Default::default();
        let mut unused = unused.iter_mut();
        let mut other = || Slot::Str(unused.next().expect("one string per unread field"));
        let mut fields = [
            ("type", Slot::Str(&mut f.kind)),
            ("client_email", Slot::Str(&mut f.client_email)),
            ("private_key_id", Slot::Str(&mut f.private_key_id)),
            ("private_key", Slot::Str(&mut f.private_key)),
            ("auth_uri", other()),
            ("token_uri", Slot::Str(&mut f.token_uri)),
            ("project_id", other()),
            ("universe_domain", other()),
            ("client_secret", other()),
            ("client_id", other()),
            ("refresh_token", other()),
            ("audience", Slot::Str(&mut f.audience)),
            ("subject_token_type", other()),
            ("token_url", other()),
            ("token_info_url", other()),
            ("service_account_impersonation_url", other()),
            ("service_account_impersonation", Slot::Object),
            ("delegates", Slot::Strs),
            ("credential_source", Slot::Object),
            ("quota_project_id", other()),
            ("workforce_pool_user_project", other()),
            ("revoke_url", other()),
            ("source_credentials", Slot::Object),
        ];
        meta_wire::unmarshal(&data, "google", "credentialsFile", &mut fields)?;
        Ok(f)
    }
}

/// `oauth2/internal.ParseKey`: a PEM block or bare DER, PKCS#8 then PKCS#1, RSA only.
fn signing_key(pem: &str) -> Result<btls::pkey::PKey<btls::pkey::Private>, String> {
    let der = pem_decode(pem.as_bytes()).map_or_else(|| pem.as_bytes().to_vec(), |b| b.bytes);
    let rsa = match parse_pkcs8_rsa(&der) {
        Ok(rsa) => rsa,
        Err(true) => return Err("private key is invalid".into()),
        Err(false) => parse_pkcs1(&der)
            .map_err(|e| format!("private key should be a PEM or plain PKCS1 or PKCS8; parse error: {e}"))?,
    };
    btls::pkey::PKey::from_rsa(rsa).map_err(|e| e.to_string())
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn quote(s: &str) -> String {
    let mut out = Vec::new();
    gj::marshal_str(&mut out, s.as_bytes(), true);
    String::from_utf8(out).unwrap_or_default()
}

/// `jws.Encode` of the service account's claim set at `now` (Unix seconds): issued ten
/// seconds early, valid for an hour.
pub fn signed_assertion(key: &KeyFileView<'_>, now: i64) -> Result<String, String> {
    let pkey = signing_key(key.private_key)?;
    let mut header = String::from(r#"{"alg":"RS256","typ":"JWT""#);
    if !key.private_key_id.is_empty() {
        header.push_str(&format!(r#","kid":{}"#, quote(key.private_key_id)));
    }
    header.push('}');
    let iat = now - 10;
    let mut claims = format!(r#"{{"iss":{}"#, quote(key.client_email));
    claims.push_str(&format!(r#","scope":{}"#, quote(SCOPE)));
    claims.push_str(&format!(
        r#","aud":{},"exp":{},"iat":{iat}}}"#,
        quote(key.audience),
        iat + 3600
    ));
    let signing_input = format!("{}.{}", b64url(header.as_bytes()), b64url(claims.as_bytes()));
    let mut signer = btls::sign::Signer::new(btls::hash::MessageDigest::sha256(), &pkey).map_err(|e| e.to_string())?;
    let signature = signer
        .sign_oneshot_to_vec(signing_input.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(format!("{signing_input}.{}", b64url(&signature)))
}

/// The service-account fields the JWT needs, with Go's defaults applied.
pub struct KeyFileView<'a> {
    pub client_email: &'a str,
    pub private_key: &'a str,
    pub private_key_id: &'a str,
    /// `aud`: the key's `audience`, else the token URL.
    pub audience: &'a str,
}

/// Go `url.QueryEscape`.
fn query_escape(s: &str) -> String {
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

/// The token request for a key file: Go's `jwtSource` URL and form body, signed at
/// `now` (Unix seconds), after `CredentialsFromJSON` decoded the key.
pub fn token_request(sa: &Map<String, Value>, now: i64) -> Result<(String, String), String> {
    // ponytail: Go first tries the key as a `web`/`installed` OAuth client file
    // (ConfigFromJSON), whose token source then needs an interactive handler; a Go
    // service-account file never has those members.
    let file = KeyFile::parse(sa)?;
    match file.kind.as_str() {
        "service_account" => {}
        // ponytail: with a refresh token Go refreshes an `authorized_user` key at its token
        // URL, and it runs the external-account, impersonation and GDCH flows for those
        // types. Vertex service-account files carry none of them; without a refresh token
        // Go fails like this.
        "authorized_user" => return Err("oauth2: token expired and refresh token is not set".into()),
        "" => return Err("missing 'type' field in credentials".into()),
        other => return Err(format!("unknown credential type: {}", gostr_quote(other))),
    }
    let token_url = if file.token_uri.is_empty() {
        DEFAULT_TOKEN_URL
    } else {
        file.token_uri.as_str()
    };
    let audience = if file.audience.is_empty() {
        token_url
    } else {
        file.audience.as_str()
    };
    let assertion = signed_assertion(
        &KeyFileView {
            client_email: &file.client_email,
            private_key: &file.private_key,
            private_key_id: &file.private_key_id,
            audience,
        },
        now,
    )?;
    let body = format!(
        "assertion={}&grant_type={}",
        query_escape(&assertion),
        query_escape(GRANT_TYPE)
    );
    Ok((token_url.to_owned(), body))
}

/// One token from the key file: `google.CredentialsFromJSON(...).TokenSource.Token()`.
/// Returns the access token, which may be empty (Go then sends no `Authorization`).
pub async fn access_token(client: &wreq::Client, sa: &Map<String, Value>, now: i64) -> Result<String, String> {
    let (token_url, body) = token_request(sa, now)?;
    let token_url = token_url.as_str();
    let mut headers = GoHeaders::new();
    headers.set("Content-Type", "application/x-www-form-urlencoded");
    let upstream = proxy::send(client, token_url, headers, Bytes::from(body), None)
        .await
        .map_err(|e| format!("oauth2: cannot fetch token: {}", String::from_utf8_lossy(&e.body)))?;
    let status = upstream.status;
    let data = proxy::read_all(upstream.body, 1 << 20, false)
        .await
        .map_err(|e| format!("oauth2: cannot fetch token: {}", String::from_utf8_lossy(&e.body)))?;
    if !(200..=299).contains(&status) {
        // oauth2.RetrieveError: the response status line, then the body.
        let reason = http::StatusCode::from_u16(status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or_default();
        return Err(format!(
            "oauth2: cannot fetch token: {status} {reason}\nResponse: {}",
            String::from_utf8_lossy(&data)
        ));
    }
    parse_token(&data)
}

fn gostr_quote(s: &str) -> String {
    cpa_common::gostr::quote(s)
}

/// The access token from a token response, decoded as Go's `jwtSource.Token` does into
/// `struct { oauth2.Token; IDToken string }`: a wrongly typed member fails, and an
/// `id_token` must decode as a JWT claim set.
fn parse_token(data: &[u8]) -> Result<String, String> {
    let (mut access, mut token_type, mut refresh, mut id_token, mut expires_in) = Default::default();
    let mut fields = [
        ("access_token", Slot::Str(&mut access)),
        ("token_type", Slot::Str(&mut token_type)),
        ("refresh_token", Slot::Str(&mut refresh)),
        ("expiry", Slot::Time),
        ("expires_in", Slot::Int(&mut expires_in, "int64")),
        ("id_token", Slot::Str(&mut id_token)),
    ];
    meta_wire::unmarshal(data, "oauth2", "Token", &mut fields)
        .map_err(|e| format!("oauth2: cannot fetch token: {e}"))?;
    if !id_token.is_empty() {
        decode_id_token(&id_token).map_err(|e| format!("oauth2: error decoding JWT token: {e}"))?;
    }
    Ok(access)
}

/// `jws.Decode`: exactly three dot-separated segments; the second is unpadded base64url
/// (line breaks ignored) whose first JSON value decodes into a `jws.ClaimSet`. What
/// follows that value is never read.
fn decode_id_token(token: &str) -> Result<(), String> {
    let segments: Vec<&str> = token.split('.').collect();
    if segments.len() != 3 {
        return Err("jws: invalid token received".into());
    }
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireNone),
    );
    let claims: Vec<u8> = segments[1].bytes().filter(|b| *b != b'\r' && *b != b'\n').collect();
    let claims = engine.decode(claims).map_err(|e| format!("illegal base64 data: {e}"))?;
    let Some(start) = claims.iter().position(|c| !matches!(c, b' ' | b'\t' | b'\r' | b'\n')) else {
        return Err("EOF".into());
    };
    let value = &claims[start..];
    if value.starts_with(b"null") {
        return Ok(());
    }
    if !value.starts_with(b"{") {
        // Any other first value fails: a non-object cannot fill the struct.
        return Err("json: cannot unmarshal into Go value of type jws.ClaimSet".into());
    }
    let end = meta_wire::object_end(value).ok_or("unexpected EOF")?;
    let (mut iss, mut scope, mut aud, mut typ, mut sub, mut prn) = Default::default();
    let (mut exp, mut iat) = (0, 0);
    let mut fields = [
        ("iss", Slot::Str(&mut iss)),
        ("scope", Slot::Str(&mut scope)),
        ("aud", Slot::Str(&mut aud)),
        ("exp", Slot::Int(&mut exp, "int64")),
        ("iat", Slot::Int(&mut iat, "int64")),
        ("typ", Slot::Str(&mut typ)),
        ("sub", Slot::Str(&mut sub)),
        ("prn", Slot::Str(&mut prn)),
    ];
    meta_wire::unmarshal(&value[..end], "jws", "ClaimSet", &mut fields)
}

/// `sanitizeFilePart`.
fn file_part(s: &str) -> String {
    s.trim().replace(['/', '\\', ':'], "_").replace(' ', "-")
}

/// `labelForVertex`.
fn label(project: &str, email: &str) -> String {
    match (project.trim(), email.trim()) {
        ("", "") => "vertex".into(),
        (p, "") => p.into(),
        ("", e) => e.into(),
        (p, e) => format!("{p} ({e})"),
    }
}

/// `-vertex-import`: reads a service-account key, normalizes its private key and writes
/// `vertex-[<prefix>-]<project>.json` in `auth_dir`. Errors are Go's log messages.
pub fn import(auth_dir: &Path, key_path: &str, prefix: &str) -> Result<PathBuf, String> {
    let path = key_path.trim();
    if path.is_empty() {
        return Err("vertex-import: missing service account key path".into());
    }
    let data = std::fs::read(path).map_err(|e| {
        format!(
            "vertex-import: read file failed: {}",
            path_error("open", Path::new(path), &e)
        )
    })?;
    // json.Unmarshal into map[string]any: numbers become float64.
    let mut sa = match GoValue::parse_f64(&data) {
        Some(GoValue::Object(sa)) => sa,
        // `null` leaves the map nil, which NormalizeServiceAccountMap rejects.
        Some(GoValue::Null) => return Err("vertex-import: service account payload is empty".into()),
        Some(other) => {
            let kind = match other {
                GoValue::Array(_) => "array",
                GoValue::String(_) => "string",
                GoValue::Bool(_) => "bool",
                _ => "number",
            };
            return Err(format!(
                "vertex-import: invalid service account json: json: cannot unmarshal {kind} into Go value of type map[string]interface {{}}"
            ));
        }
        None => {
            // Go's syntax error, else the first number beyond float64.
            let error = meta_wire::check_valid(&data).err().unwrap_or_else(|| {
                let number = first_overflow(&gj::parse(&data)).unwrap_or_default();
                format!("json: cannot unmarshal number {number} into Go value of type float64")
            });
            return Err(format!("vertex-import: invalid service account json: {error}"));
        }
    };
    let text = |sa: &BTreeMap<String, GoValue>, key: &str| match sa.get(key) {
        Some(GoValue::String(s)) => s.clone(),
        _ => String::new(),
    };
    let private_key = text(&sa, "private_key");
    if private_key.trim().is_empty() {
        return Err("vertex-import: service account missing private_key".into());
    }
    let private_key = sanitize_private_key(&private_key).map_err(|e| format!("vertex-import: {e}"))?;
    sa.insert("private_key".into(), GoValue::String(private_key));
    let (email, project) = (text(&sa, "client_email"), text(&sa, "project_id"));
    if project.trim().is_empty() {
        return Err("vertex-import: project_id missing in service account json".into());
    }
    if email.trim().is_empty() {
        tracing::warn!("vertex-import: client_email missing in service account json");
    }
    let prefix = prefix.trim().trim_matches('/');
    if prefix.contains('/') {
        return Err(format!(
            "vertex-import: prefix must be a single segment (no '/' allowed): {}",
            gostr_quote(prefix)
        ));
    }
    let mut base = file_part(&project);
    if !prefix.is_empty() {
        base = format!("{}-{base}", file_part(prefix));
    }
    let mut doc = BTreeMap::new();
    doc.insert("disabled".to_owned(), GoValue::Bool(false));
    doc.insert("email".to_owned(), GoValue::String(email.clone()));
    doc.insert("label".to_owned(), GoValue::String(label(&project, &email)));
    doc.insert("location".to_owned(), GoValue::String("us-central1".into()));
    doc.insert("prefix".to_owned(), GoValue::String(prefix.to_owned()));
    doc.insert("project_id".to_owned(), GoValue::String(project));
    doc.insert("service_account".to_owned(), GoValue::Object(sa));
    doc.insert("type".to_owned(), GoValue::String("vertex".into()));
    // FileTokenStore.Save: filepath.Join (cleaned), MkdirAll of its directory, then
    // VertexCredentialStorage.SaveTokenToFile.
    let out = go_clean(&auth_dir.join(format!("vertex-{base}.json")));
    let save_failed = |e: String| format!("vertex-import: save credential failed: {e}");
    let dir = out.parent().unwrap_or(Path::new("."));
    go_mkdir_all(dir).map_err(|e| save_failed(format!("auth filestore: create dir failed: {e}")))?;
    // misc.LogSavingCredentials.
    println!("Saving credentials to {}", out.display());
    std::fs::write(&out, GoValue::Object(doc).encode_indented()).map_err(|e| {
        save_failed(format!(
            "vertex credential: create file failed: {}",
            path_error("open", &out, &e)
        ))
    })?;
    Ok(out)
}

/// The first number, in document order, that does not fit a float64.
fn first_overflow(value: &gj::Res<'_>) -> Option<String> {
    if value.kind == gj::Kind::Number {
        let raw = String::from_utf8_lossy(value.raw()).into_owned();
        return gj::go_parse_float(value.raw()).is_err().then_some(raw);
    }
    let mut found = None;
    if value.kind == gj::Kind::Json {
        value.each(|_, item| {
            found = first_overflow(&item);
            found.is_none()
        });
    }
    found
}

/// Go's `*fs.PathError` text: `<op> <path>: <errno text>`.
fn path_error(op: &str, path: &Path, e: &std::io::Error) -> String {
    // syscall.Errno strings are strerror in lower case.
    let text = e.to_string();
    let text = text.split(" (os error").next().unwrap_or_default();
    let mut chars = text.chars();
    let errno: String = chars
        .next()
        .map_or_else(String::new, |c| c.to_lowercase().chain(chars).collect());
    format!("{op} {}: {errno}", path.display())
}

/// `filepath.Clean`, lexically: no `.` elements, `..` folded into its parent, no
/// trailing separator; `.` for an empty result.
fn go_clean(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// `os.MkdirAll(dir, 0o700)` with Go's error: an existing non-directory fails as
/// `mkdir <path>: not a directory`, parents are created first, and a failed `mkdir`
/// names the directory it could not create.
fn go_mkdir_all(dir: &Path) -> Result<(), String> {
    match std::fs::metadata(dir) {
        Ok(meta) if meta.is_dir() => return Ok(()),
        Ok(_) => return Err(format!("mkdir {}: not a directory", dir.display())),
        Err(_) => {}
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        go_mkdir_all(parent)?;
    }
    #[cfg(unix)]
    let builder = {
        let mut builder = std::fs::DirBuilder::new();
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    match builder.create(dir) {
        Ok(()) => Ok(()),
        Err(_) if std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) => Ok(()),
        Err(e) => Err(path_error("mkdir", dir, &e)),
    }
}
