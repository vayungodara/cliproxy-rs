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

use crate::proxy::{self, GoHeaders};

/// `https://www.googleapis.com/auth/cloud-platform`, the only scope Vertex asks for.
pub const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
/// `google.JWTTokenURL`, used when a key has no `token_uri`.
pub const DEFAULT_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";

/// Go's `base64.StdEncoding` decoder: padded, line breaks ignored, non-zero trailing
/// bits accepted.
fn std_decode(data: &[u8]) -> Option<Vec<u8>> {
    let stripped: Vec<u8> = data.iter().copied().filter(|b| *b != b'\r' && *b != b'\n').collect();
    let engine = base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::RequireCanonical),
    );
    engine.decode(stripped).ok()
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
                Some(decoded) => bytes = decoded,
                None => continue,
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
    // ponytail: Go's message carries base64's position detail ("illegal base64 data at
    // input byte N"); this one names only the failure.
    let der = std_decode(payload.as_bytes()).ok_or("private_key base64 decode failed: illegal base64 data")?;
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

fn parse_pkcs1(der: &[u8]) -> Option<btls::rsa::Rsa<btls::pkey::Private>> {
    btls::rsa::Rsa::private_key_from_der(der)
        .ok()
        .filter(|k| k.check_key().unwrap_or(false))
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
            Some(_) => Ok(block),
            None => Err("private_key invalid rsa: x509: failed to parse private key".into()),
        },
        "PRIVATE KEY" => match parse_pkcs8_rsa(&block.bytes) {
            Ok(rsa) => Ok(pkcs1(pkcs1_der(&rsa)?)),
            Err(true) => Err("private_key is not an RSA key".into()),
            Err(false) => Err("private_key invalid pkcs8: x509: failed to parse private key".into()),
        },
        _ => {
            if let Some(rsa) = parse_pkcs1(&block.bytes) {
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

/// The fields `google.CredentialsFromJSON` reads from a key file.
struct KeyFile {
    kind: String,
    client_email: String,
    private_key: String,
    private_key_id: String,
    token_uri: String,
    audience: String,
}

/// Go struct decoding: a string field accepts a string or null; other types fail.
fn string_field(sa: &Map<String, Value>, field: &str) -> Result<String, String> {
    match fold_get(sa, field) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(format!(
            "json: cannot unmarshal {} into Go struct field credentialsFile.{field} of type string",
            json_kind(other)
        )),
    }
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        Value::Null => "null",
    }
}

/// `encoding/json` field matching: the exact key, else a case-insensitive one.
fn fold_get<'a>(map: &'a Map<String, Value>, field: &str) -> Option<&'a Value> {
    map.get(field)
        .or_else(|| map.iter().find(|(k, _)| k.eq_ignore_ascii_case(field)).map(|(_, v)| v))
}

impl KeyFile {
    fn parse(sa: &Map<String, Value>) -> Result<Self, String> {
        Ok(Self {
            kind: string_field(sa, "type")?,
            client_email: string_field(sa, "client_email")?,
            private_key: string_field(sa, "private_key")?,
            private_key_id: string_field(sa, "private_key_id")?,
            token_uri: string_field(sa, "token_uri")?,
            audience: string_field(sa, "audience")?,
        })
    }
}

/// `oauth2/internal.ParseKey`: a PEM block or bare DER, PKCS#8 then PKCS#1, RSA only.
fn signing_key(pem: &str) -> Result<btls::pkey::PKey<btls::pkey::Private>, String> {
    let der = pem_decode(pem.as_bytes()).map_or_else(|| pem.as_bytes().to_vec(), |b| b.bytes);
    let rsa = match parse_pkcs8_rsa(&der) {
        Ok(rsa) => rsa,
        Err(true) => return Err("private key is invalid".into()),
        Err(false) => parse_pkcs1(&der).ok_or(
            "private key should be a PEM or plain PKCS1 or PKCS8; parse error: x509: failed to parse private key",
        )?,
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

/// One token from the key file: `google.CredentialsFromJSON(...).TokenSource.Token()`.
/// Returns the access token, which may be empty (Go then sends no `Authorization`).
pub async fn access_token(client: &wreq::Client, sa: &Map<String, Value>, now: i64) -> Result<String, String> {
    let file = KeyFile::parse(sa)?;
    match file.kind.as_str() {
        "service_account" => {}
        // ponytail: Go refreshes `authorized_user` keys with their refresh token, but a
        // key without `private_key` never reaches this point (vertexCreds requires one);
        // with an empty refresh token Go fails the same way.
        "authorized_user" => return Err("oauth2: token expired and refresh token is not set".into()),
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
        return Err(format!(
            "oauth2: cannot fetch token: {status}\nResponse: {}",
            String::from_utf8_lossy(&data)
        ));
    }
    parse_token(&data)
}

fn gostr_quote(s: &str) -> String {
    cpa_common::gostr::quote(s)
}

/// The token response as Go's `jwtSource.Token` decodes it: struct-typed fields fail on
/// a wrong JSON type, and an `id_token` must decode as a JWT claim set.
fn parse_token(data: &[u8]) -> Result<String, String> {
    let fail = |m: &str| Err(format!("oauth2: cannot fetch token: {m}"));
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(data) else {
        return match serde_json::from_slice::<Value>(data) {
            Ok(_) => fail("json: cannot unmarshal into Go value"),
            Err(_) => fail("invalid character in token response"),
        };
    };
    let string = |field: &str| -> Result<String, String> {
        match fold_get(&root, field) {
            None | Some(Value::Null) => Ok(String::new()),
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(format!(
                "oauth2: cannot fetch token: json: cannot unmarshal into {field}"
            )),
        }
    };
    let access = string("access_token")?;
    string("token_type")?;
    string("refresh_token")?;
    match fold_get(&root, "expires_in") {
        None | Some(Value::Null) => {}
        Some(Value::Number(n)) if n.as_i64().is_some() => {}
        Some(_) => return fail("json: cannot unmarshal into expires_in"),
    }
    match fold_get(&root, "expiry") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if chrono::DateTime::parse_from_rfc3339(s).is_ok() => {}
        Some(_) => return fail("parsing time in expiry"),
    }
    let id_token = string("id_token")?;
    if !id_token.is_empty() {
        let parts: Vec<&str> = id_token.split('.').collect();
        let claims = parts
            .get(1)
            .filter(|_| parts.len() >= 2)
            .and_then(|p| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(p).ok());
        let valid = claims
            .and_then(|c| serde_json::from_slice::<Value>(&c).ok())
            .is_some_and(|c| c.is_object());
        if !valid {
            return Err("oauth2: error decoding JWT token".into());
        }
    }
    Ok(access)
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
    let data = std::fs::read(path).map_err(|e| format!("vertex-import: read file failed: {e}"))?;
    // ponytail: the decode message is serde_json's, not encoding/json's.
    let sa: Map<String, Value> =
        serde_json::from_slice(&data).map_err(|e| format!("vertex-import: invalid service account json: {e}"))?;
    let normalized = normalize_service_account(&sa).map_err(|e| format!("vertex-import: {e}"))?;
    let email = normalized
        .get("client_email")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let project = normalized
        .get("project_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
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
    let file_name = format!("vertex-{base}.json");
    // Go decodes the key into map[string]any (numbers become float64) and saves it with
    // the normalized private key.
    let Some(GoValue::Object(mut service_account)) = GoValue::parse_f64(&data) else {
        return Err("vertex-import: invalid service account json".into());
    };
    let private_key = normalized
        .get("private_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    service_account.insert("private_key".into(), GoValue::String(private_key.to_owned()));
    let service_account = GoValue::Object(service_account);
    let mut doc = BTreeMap::new();
    doc.insert("disabled".to_owned(), GoValue::Bool(false));
    doc.insert("email".to_owned(), GoValue::String(email.clone()));
    doc.insert("label".to_owned(), GoValue::String(label(&project, &email)));
    doc.insert("location".to_owned(), GoValue::String("us-central1".into()));
    doc.insert("prefix".to_owned(), GoValue::String(prefix.to_owned()));
    doc.insert("project_id".to_owned(), GoValue::String(project));
    doc.insert("service_account".to_owned(), service_account);
    doc.insert("type".to_owned(), GoValue::String("vertex".into()));
    create_private_dir(auth_dir).map_err(|e| format!("vertex credential: create directory failed: {e}"))?;
    let out = auth_dir.join(file_name);
    // misc.LogSavingCredentials.
    println!("Saving credentials to {}", out.display());
    std::fs::write(&out, GoValue::Object(doc).encode_indented())
        .map_err(|e| format!("vertex credential: create file failed: {e}"))?;
    Ok(out)
}

/// `os.MkdirAll(dir, 0o700)`.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}
