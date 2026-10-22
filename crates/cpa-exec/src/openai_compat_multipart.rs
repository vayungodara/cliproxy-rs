//! Go `mime/multipart` and `mime.ParseMediaType`, as the images paths use them
//! (`Reader.ReadForm`, then `Writer.WriteField` / `CreatePart`). Ported from Go 1.26
//! `mime/multipart/multipart.go`, `formdata.go` and `mime/mediatype.go`.
//!
//! ponytail: quoted-printable parts are not decoded (Go's `NextPart` does) and ReadForm's
//! part, header and memory limits are not enforced beyond the route's body cap; image
//! clients send binary parts. Lines are read as Go reads them from an in-memory body
//! (docs/DIFFERENCES-FROM-GO.md, images multipart forms). Field and file order follow first appearance, one of the
//! orders Go's map iteration can produce.

use cpa_common::gostr::quote;
use std::collections::BTreeMap;

/// MIME header names in arrival order, with their values' bytes.
pub type Headers = Vec<(String, Vec<u8>)>;

/// One parsed form part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Canonical header names (names with a space as sent).
    pub headers: Headers,
    /// `Part.FormName()`.
    pub name: String,
    /// `Part.FileName()`: `filepath.Base` of the filename parameter, empty for fields.
    pub filename: String,
    pub body: Vec<u8>,
}

impl Part {
    /// `textproto.MIMEHeader.Get`.
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        let name = crate::proxy::canonical_header(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_slice())
    }
}

/// A parsed form: raw values and files keyed by field name in first-appearance order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    pub values: Vec<(String, Vec<Vec<u8>>)>,
    pub files: Vec<(String, Vec<Part>)>,
}

impl Form {
    /// `c.PostForm(key)`: the first value as a Go string (lossy only for display).
    pub fn value(&self, key: &str) -> String {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.first())
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .unwrap_or_default()
    }

    pub fn files(&self, key: &str) -> &[Part] {
        self.files
            .iter()
            .find(|(k, _)| k == key)
            .map_or(&[], |(_, v)| v.as_slice())
    }
}

fn token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`{|}~".contains(&c)
}

fn tspecial(c: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&c)
}

fn consume_token(v: &str) -> (&str, &str) {
    let end = v.bytes().position(|c| !token_char(c)).unwrap_or(v.len());
    v.split_at(end)
}

/// `consumeValue`: a token or a quoted-string (with MSIE's unescaped backslashes).
fn consume_value(v: &str) -> Option<(String, &str)> {
    let bytes = v.as_bytes();
    if bytes.first() != Some(&b'"') {
        let (token, rest) = consume_token(v);
        return Some((token.to_owned(), rest));
    }
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &v[i + 1..])),
            b'\\' if i + 1 < bytes.len() && tspecial(bytes[i + 1]) => {
                out.push(bytes[i + 1]);
                i += 2;
                continue;
            }
            b'\r' | b'\n' => return None,
            c => out.push(c),
        }
        i += 1;
    }
    None
}

fn trim_left_space(v: &str) -> &str {
    v.trim_start_matches(char::is_whitespace)
}

/// `consumeMediaParam`.
fn consume_param(v: &str) -> Option<(String, String, &str)> {
    let rest = trim_left_space(v).strip_prefix(';')?;
    let (param, rest) = consume_token(trim_left_space(rest));
    if param.is_empty() {
        return None;
    }
    let rest = trim_left_space(trim_left_space(rest).strip_prefix('=')?);
    let (value, rest2) = consume_value(rest)?;
    if value.is_empty() && rest2.len() == rest.len() {
        return None;
    }
    Some((param.to_ascii_lowercase(), value, rest2))
}

fn unhex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// `percentHexUnescape`, as bytes.
fn percent_unescape(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let (hi, lo) = (unhex(*b.get(i + 1)?)?, unhex(*b.get(i + 2)?)?);
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// `decode2231Enc`, as bytes.
fn decode_2231(v: &str) -> Option<Vec<u8>> {
    let (charset, rest) = v.split_once('\'')?;
    let (_, value) = rest.split_once('\'')?;
    matches!(charset.to_ascii_lowercase().as_str(), "us-ascii" | "utf-8")
        .then(|| percent_unescape(value))
        .flatten()
}

/// Outcome of `mime.ParseMediaType`.
#[derive(Debug, PartialEq, Eq)]
pub enum MediaType {
    /// No usable media type, with Go's error text.
    Invalid(&'static str),
    /// The media type parsed but its parameters did not (Go returns the type and an error).
    BadParams(String),
    Ok(String, BTreeMap<String, String>),
}

pub fn parse_media_type(v: &str) -> MediaType {
    let base = v.split(';').next().unwrap_or_default();
    let media = base.to_ascii_lowercase().trim().to_owned();
    // Go `checkMediaTypeDisposition`.
    let (main, rest) = consume_token(&media);
    if main.is_empty() {
        return MediaType::Invalid("mime: no media type");
    }
    if !rest.is_empty() {
        let Some(rest) = rest.strip_prefix('/') else {
            return MediaType::Invalid("mime: expected slash after first token");
        };
        let (sub, tail) = consume_token(rest);
        if sub.is_empty() {
            return MediaType::Invalid("mime: expected token after slash");
        }
        if !tail.is_empty() {
            return MediaType::Invalid("mime: unexpected content after media subtype");
        }
    }
    let mut params = BTreeMap::new();
    let mut continuation: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut v = &v[base.len()..];
    loop {
        v = trim_left_space(v);
        if v.is_empty() {
            break;
        }
        let Some((key, value, rest)) = consume_param(v) else {
            if v.trim() == ";" {
                break;
            }
            return MediaType::BadParams(media);
        };
        let map = match key.split_once('*') {
            Some((base_name, _)) => continuation.entry(base_name.to_owned()).or_default(),
            None => &mut params,
        };
        if map.get(&key).is_some_and(|existing| *existing != value) {
            return MediaType::Invalid("mime: duplicate parameter name");
        }
        map.insert(key, value);
        v = rest;
    }
    // ponytail: Go keeps parameter bytes as they are; values that are not UTF-8 after
    // RFC 2231 decoding are stored lossily here.
    for (key, pieces) in continuation {
        if let Some(v) = pieces.get(&format!("{key}*")) {
            if let Some(decoded) = decode_2231(v) {
                params.insert(key, String::from_utf8_lossy(&decoded).into_owned());
            }
            continue;
        }
        let mut buf = Vec::new();
        let mut valid = false;
        for n in 0.. {
            let simple = format!("{key}*{n}");
            if let Some(v) = pieces.get(&simple) {
                valid = true;
                buf.extend_from_slice(v.as_bytes());
                continue;
            }
            let Some(v) = pieces.get(&format!("{simple}*")) else {
                break;
            };
            valid = true;
            if n == 0 {
                if let Some(decoded) = decode_2231(v) {
                    buf.extend_from_slice(&decoded);
                }
            } else {
                buf.extend_from_slice(&percent_unescape(v).unwrap_or_default());
            }
        }
        if valid {
            params.insert(key, String::from_utf8_lossy(&buf).into_owned());
        }
    }
    MediaType::Ok(media, params)
}

/// The boundary of a `multipart/*` content type, `Err` with Go's message when absent,
/// `None` when the content type does not parse or is not multipart (callers pass the
/// body through).
pub fn boundary(content_type: &str) -> Option<Result<String, String>> {
    let MediaType::Ok(media, params) = parse_media_type(content_type.trim()) else {
        return None;
    };
    if !media.trim().starts_with("multipart/") {
        return None;
    }
    match params.get("boundary").map(|b| b.trim()).filter(|b| !b.is_empty()) {
        Some(b) => Some(Ok(b.to_owned())),
        None => Some(Err("multipart boundary is missing".into())),
    }
}

/// Go `filepath.Base` on Unix.
fn base_name(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".into();
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed).to_owned()
}

/// `Reader` over a complete body.
struct Reader<'a> {
    body: &'a [u8],
    pos: usize,
    nl: &'static [u8],
    dash_boundary: Vec<u8>,
    parts_read: usize,
}

fn skip_lwsp(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|c| matches!(c, b' ' | b'\t')).count();
    &b[n..]
}

/// What `partReader` makes of a delimiter prefix at the start of `rest`.
enum Delimiter {
    Match,
    Data,
    /// The part fails with `ErrBufferFull` once the bytes before this one are read.
    BufferFull,
}

/// `matchAfterPrefix` as `partReader` reaches it: from the prefix on it sees at most
/// the 4096-byte bufio buffer. When the buffer is full and still undecided, `Peek`
/// fails with `ErrBufferFull`, which counts as the end of input and then fails the part.
fn delimiter(rest: &[u8], prefix_len: usize) -> Delimiter {
    let window = &rest[..rest.len().min(PEEK_BUFFER_SIZE)];
    match window.get(prefix_len) {
        None | Some(b' ' | b'\t' | b'\r' | b'\n') => Delimiter::Match,
        Some(b'-') => match window.get(prefix_len + 1) {
            Some(b'-') => Delimiter::Match,
            None if window.len() == PEEK_BUFFER_SIZE => Delimiter::BufferFull,
            _ => Delimiter::Data,
        },
        Some(_) => Delimiter::Data,
    }
}

/// Why a part's body has no end.
enum BodyError {
    UnexpectedEof,
    /// `ErrBufferFull`, with where draining the part stops.
    BufferFull(usize),
}

/// `multipart.peekBufferSize`: `NextPart` reads its lines with a bufio buffer this size.
const PEEK_BUFFER_SIZE: usize = 4096;

/// textproto's `trim`: SP and HTAB only.
fn trim_sp_tab(b: &[u8]) -> &[u8] {
    let mut b = skip_lwsp(b);
    while let [rest @ .., b' ' | b'\t'] = b {
        b = rest;
    }
    b
}

/// textproto's `validHeaderValueByte`: HTAB, SP, VCHAR and obs-text.
fn header_value_byte(c: u8) -> bool {
    c == b'\t' || (0x20..0x7f).contains(&c) || c >= 0x80
}

impl<'a> Reader<'a> {
    /// `bufio.Reader.ReadSlice('\n')` over the rest of the body: the line with its
    /// ending, `bufio: buffer full` when no newline falls within the buffer, and
    /// `EOF` (with what is left) when the body ends first.
    fn read_slice(&mut self) -> Result<&'a [u8], (&'static str, &'a [u8])> {
        let body: &'a [u8] = self.body;
        let rest = &body[self.pos..];
        let window = &rest[..rest.len().min(PEEK_BUFFER_SIZE)];
        if let Some(i) = window.iter().position(|b| *b == b'\n') {
            self.pos += i + 1;
            return Ok(&rest[..=i]);
        }
        if rest.len() >= PEEK_BUFFER_SIZE {
            self.pos += PEEK_BUFFER_SIZE;
            return Err(("bufio: buffer full", window));
        }
        self.pos = body.len();
        Err(("EOF", rest))
    }

    /// textproto's `readLineSlice` (`bufio.Reader.ReadLine` joined): the line without
    /// `\n` or `\r\n`, a bare CR at the end of the body kept; `None` at EOF.
    fn text_line(&mut self) -> Option<&'a [u8]> {
        let body: &'a [u8] = self.body;
        let rest = &body[self.pos..];
        if rest.is_empty() {
            return None;
        }
        let Some(i) = rest.iter().position(|b| *b == b'\n') else {
            self.pos = body.len();
            // ReadLine hands an unterminated line out in buffer-sized fragments, one
            // byte shorter when the fragment would end in CR. When the last fragment
            // ends the body, the next ReadLine's io.EOF drops the whole line.
            let mut at = 0;
            while rest.len() - at >= PEEK_BUFFER_SIZE {
                at += PEEK_BUFFER_SIZE - usize::from(rest[at + PEEK_BUFFER_SIZE - 1] == b'\r');
            }
            return (at < rest.len()).then_some(rest);
        };
        self.pos += i + 1;
        let line = &rest[..i];
        Some(line.strip_suffix(b"\r").unwrap_or(line))
    }

    fn is_final(&self, line: &[u8]) -> bool {
        let mut prefix = self.dash_boundary.clone();
        prefix.extend_from_slice(b"--");
        let Some(rest) = line.strip_prefix(prefix.as_slice()) else {
            return false;
        };
        let rest = skip_lwsp(rest);
        rest.is_empty() || rest == self.nl
    }

    fn is_delimiter(&mut self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(self.dash_boundary.as_slice()) else {
            return false;
        };
        let rest = skip_lwsp(rest);
        if self.parts_read == 0 && rest == b"\n" {
            self.nl = b"\n";
        }
        rest == self.nl
    }

    /// `nextPart`; `Ok(None)` is `io.EOF`, which also ends the form when the body
    /// ends inside a part's headers.
    fn next_part(&mut self) -> Result<Option<Part>, String> {
        let mut expect_new = false;
        loop {
            let line = match self.read_slice() {
                Ok(line) => line,
                Err(("EOF", line)) if self.is_final(line) => return Ok(None),
                Err((e, _)) => return Err(format!("multipart: NextPart: {e}")),
            };
            if self.is_delimiter(line) {
                self.parts_read += 1;
                return self.part();
            }
            if self.is_final(line) {
                return Ok(None);
            }
            if expect_new {
                return Err(format!("multipart: expecting a new Part; got line {}", quote(line)));
            }
            if self.parts_read == 0 {
                continue;
            }
            if line == self.nl {
                expect_new = true;
                continue;
            }
            return Err(format!("multipart: unexpected line in Next(): {}", quote(line)));
        }
    }

    /// textproto's `readMIMEHeader`; `Ok(None)` is its `io.EOF`.
    fn headers(&mut self) -> Result<Option<Headers>, String> {
        // Go 1.26.4, v8.0.35's release toolchain, quotes these lines with %q.
        if matches!(self.body.get(self.pos), Some(b' ' | b'\t')) {
            let body: &'a [u8] = self.body;
            let start = self.pos;
            // A dropped line is at least 4095 bytes, so the limit refuses it first.
            let line = self.text_line().unwrap_or(&body[start..]);
            // readLineSlice(80): the diagnostic reads at most 80 bytes of the line.
            if line.len() > 80 {
                return Err("multipart: message too large".into());
            }
            return Err(format!("malformed MIME header initial line: {}", quote(line)));
        }
        let mut headers = Vec::new();
        loop {
            // readContinuedLineSlice: the first line must hold the colon; lines that
            // start with SP or HTAB join it after one space.
            let Some(first) = self.text_line() else {
                return Ok(None);
            };
            if first.is_empty() {
                return Ok(Some(headers));
            }
            if !first.contains(&b':') {
                return Err(format!("malformed MIME header: missing colon: {}", quote(first)));
            }
            let mut kv = trim_sp_tab(first).to_vec();
            loop {
                let spaces = self.body[self.pos..]
                    .iter()
                    .take_while(|c| matches!(c, b' ' | b'\t'))
                    .count();
                if spaces == 0 {
                    break;
                }
                self.pos += spaces;
                kv.push(b' ');
                let Some(line) = self.text_line() else {
                    break;
                };
                kv.extend_from_slice(trim_sp_tab(line));
            }
            let malformed = || format!("malformed MIME header line: {}", quote(&kv));
            let colon = kv.iter().position(|b| *b == b':').unwrap_or_default();
            let (key, value) = (&kv[..colon], &kv[colon + 1..]);
            // canonicalMIMEHeaderKey: token bytes are canonicalized; keys with spaces are
            // accepted as they are; anything else is malformed.
            let key = if key.is_empty() || !key.iter().all(|c| token_char(*c) || *c == b' ') {
                return Err(malformed());
            } else if key.contains(&b' ') {
                String::from_utf8_lossy(key).into_owned()
            } else {
                crate::proxy::canonical_header(&String::from_utf8_lossy(key))
            };
            if !value.iter().all(|c| header_value_byte(*c)) {
                // canonicalMIMEHeaderKey rewrote the key inside kv before this check.
                let line = [key.as_bytes(), b":", value].concat();
                return Err(format!("malformed MIME header line: {}", quote(line)));
            }
            headers.push((key, skip_lwsp(value).to_vec()));
        }
    }

    /// `newPart`, then what `ReadForm` does with its body.
    fn part(&mut self) -> Result<Option<Part>, String> {
        let Some(headers) = self.headers()? else {
            return Ok(None);
        };
        let body = self.part_body();
        let disposition = headers
            .iter()
            .find(|(n, _)| n == "Content-Disposition")
            .map(|(_, v)| String::from_utf8_lossy(v))
            .unwrap_or_default();
        let (kind, params) = match parse_media_type(&disposition) {
            MediaType::Ok(kind, params) => (kind, params),
            MediaType::BadParams(kind) => (kind, BTreeMap::new()),
            MediaType::Invalid(_) => (String::new(), BTreeMap::new()),
        };
        let name = if kind == "form-data" {
            params.get("name").cloned().unwrap_or_default()
        } else {
            String::new()
        };
        // ReadForm skips a nameless part; the next NextPart drains it, ignoring the
        // missing boundary, and then reads only what is left of a partial "\r\n--b".
        let body = match body {
            Ok(body) => body,
            Err(BodyError::UnexpectedEof) if name.is_empty() => return Err("multipart: NextPart: EOF".into()),
            Err(BodyError::UnexpectedEof) => return Err("unexpected EOF".into()),
            // Draining a nameless part stops at the error; NextPart reads on from there.
            Err(BodyError::BufferFull(end)) if name.is_empty() => {
                self.pos = end;
                Vec::new()
            }
            Err(BodyError::BufferFull(_)) => return Err("bufio: buffer full".into()),
        };
        let filename = params
            .get("filename")
            .filter(|f| !f.is_empty())
            .map(|f| base_name(f))
            .unwrap_or_default();
        Ok(Some(Part {
            headers,
            name,
            filename,
            body,
        }))
    }

    /// `partReader` with `scanUntilBoundary`: the body ends before `nl--boundary`
    /// followed by `--`, whitespace or the end of input.
    fn part_body(&mut self) -> Result<Vec<u8>, BodyError> {
        let buf = &self.body[self.pos..];
        if buf.starts_with(&self.dash_boundary) {
            match delimiter(buf, self.dash_boundary.len()) {
                Delimiter::Match => return Ok(Vec::new()),
                Delimiter::Data => {}
                Delimiter::BufferFull => return Err(BodyError::BufferFull(self.pos + PEEK_BUFFER_SIZE)),
            }
        }
        let mut nl_dash = self.nl.to_vec();
        nl_dash.extend_from_slice(&self.dash_boundary);
        let mut from = 0;
        while let Some(i) = buf
            .get(from..)
            .and_then(|b| b.windows(nl_dash.len()).position(|w| w == nl_dash.as_slice()))
            .map(|i| i + from)
        {
            match delimiter(&buf[i..], nl_dash.len()) {
                Delimiter::Match => {
                    let body = buf[..i].to_vec();
                    self.pos += i;
                    return Ok(body);
                }
                Delimiter::Data => from = i + nl_dash.len(),
                Delimiter::BufferFull => return Err(BodyError::BufferFull(self.pos + i + PEEK_BUFFER_SIZE)),
            }
        }
        Err(BodyError::UnexpectedEof)
    }
}

/// `Reader.ReadForm`.
pub fn read_form(body: &[u8], boundary: &str) -> Result<Form, String> {
    if boundary.is_empty() {
        return Err("multipart: boundary is empty".into());
    }
    let mut reader = Reader {
        body,
        pos: 0,
        nl: b"\r\n",
        dash_boundary: format!("--{boundary}").into_bytes(),
        parts_read: 0,
    };
    let mut form = Form::default();
    while let Some(part) = reader.next_part()? {
        if part.name.is_empty() {
            continue;
        }
        if part.filename.is_empty() {
            push(&mut form.values, part.name, part.body);
        } else {
            let name = part.name.clone();
            push(&mut form.files, name, part);
        }
    }
    Ok(form)
}

fn push<T>(list: &mut Vec<(String, Vec<T>)>, key: String, value: T) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some((_, values)) => values.push(value),
        None => list.push((key, vec![value])),
    }
}

/// Go `multipart.Writer` output.
pub struct Writer {
    boundary: String,
    out: Vec<u8>,
    parts: usize,
}

fn escape_quotes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `multipart.FileContentDisposition`.
pub fn file_content_disposition(field: &str, filename: &str) -> String {
    format!(
        r#"form-data; name="{}"; filename="{}""#,
        escape_quotes(field),
        escape_quotes(filename)
    )
}

impl Writer {
    /// A writer with Go's random 60-hex-digit boundary.
    pub fn new() -> Self {
        let mut bytes = [0u8; 30];
        getrandom::fill(&mut bytes).expect("random boundary");
        Self::with_boundary(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn with_boundary(boundary: String) -> Self {
        Self {
            boundary,
            out: Vec::new(),
            parts: 0,
        }
    }

    /// `CreatePart`: headers sorted by name, each value on its own line.
    pub fn part(&mut self, headers: &[(String, Vec<u8>)], body: &[u8]) {
        if self.parts > 0 {
            self.out.extend_from_slice(b"\r\n");
        }
        self.parts += 1;
        self.out
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        let mut sorted: Vec<&(String, Vec<u8>)> = headers.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in sorted {
            self.out.extend_from_slice(name.as_bytes());
            self.out.extend_from_slice(b": ");
            self.out.extend_from_slice(value);
            self.out.extend_from_slice(b"\r\n");
        }
        self.out.extend_from_slice(b"\r\n");
        self.out.extend_from_slice(body);
    }

    /// `WriteField`.
    pub fn field(&mut self, name: &str, value: &[u8]) {
        let disposition = format!(r#"form-data; name="{}""#, escape_quotes(name));
        self.part(&[("Content-Disposition".into(), disposition.into_bytes())], value);
    }

    /// A file part re-emitted under `field` the way the images paths do: the original
    /// part headers, a fresh Content-Disposition, and a default Content-Type.
    pub fn file(&mut self, field: &str, part: &Part) {
        let mut headers: Headers = part
            .headers
            .iter()
            .filter(|(n, _)| n != "Content-Disposition")
            .cloned()
            .collect();
        headers.push((
            "Content-Disposition".into(),
            file_content_disposition(field, &part.filename).into_bytes(),
        ));
        if part.header("Content-Type").is_none_or(<[u8]>::is_empty) {
            headers.retain(|(n, _)| n != "Content-Type");
            headers.push(("Content-Type".into(), b"application/octet-stream".to_vec()));
        }
        self.part(&headers, &part.body);
    }

    /// `Close` plus `FormDataContentType`.
    pub fn finish(mut self) -> (Vec<u8>, String) {
        self.out
            .extend_from_slice(format!("\r\n--{}--\r\n", self.boundary).as_bytes());
        (self.out, format!("multipart/form-data; boundary={}", self.boundary))
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// `rewriteOpenAICompatImagesMultipartPayload` (`always_model` false) and the handler's
/// `buildOpenAICompatImagesMultipartRequest` (`always_model` true): `model` and `stream`
/// first, then the other fields and every file.
pub fn rewrite_images_form(form: &Form, model: &str, stream: bool, always_model: bool) -> (Vec<u8>, String) {
    let mut writer = Writer::new();
    if always_model || !model.is_empty() {
        writer.field("model", model.as_bytes());
    }
    if stream {
        writer.field("stream", b"true");
    }
    for (key, values) in &form.values {
        if key == "model" || key == "stream" {
            continue;
        }
        for value in values {
            writer.field(key, value);
        }
    }
    for (key, parts) in &form.files {
        for part in parts {
            writer.file(key, part);
        }
    }
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(body: &[u8]) -> Result<Form, String> {
        read_form(body, "b")
    }

    #[test]
    fn reads_fields_and_files_and_writes_go_layout() {
        let body = b"preamble\r\n--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhi\r\n--b\r\ncontent-disposition: form-data; name=\"image\"; filename=\"dir/a.png\"\r\ncontent-type: image/png\r\n\r\n\x00\x01\r\n--b--\r\n";
        let form = form(body).unwrap();
        assert_eq!(form.value("prompt"), "hi");
        let file = &form.files("image")[0];
        assert_eq!(file.filename, "a.png");
        assert_eq!(file.body, b"\x00\x01");
        let mut writer = Writer::with_boundary("X".into());
        writer.field("model", b"m");
        writer.file("image", file);
        let (out, content_type) = writer.finish();
        assert_eq!(content_type, "multipart/form-data; boundary=X");
        assert_eq!(
            out,
            b"--X\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--X\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x00\x01\r\n--X--\r\n"
        );
    }

    #[test]
    fn boundary_prefix_inside_body_is_data_and_final_delimiter_is_strict() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x\"\r\n\r\nA\r\n--bXYZ\r\nB\r\n--b--\r\n";
        assert_eq!(form(body).unwrap().files("image")[0].body, b"A\r\n--bXYZ\r\nB");
        let bad_final = b"--b\r\nContent-Disposition: form-data; name=\"p\"\r\n\r\nv\r\n--b--garbage\r\n";
        assert!(form(bad_final).is_err());
        let lf_only = b"--b\nContent-Disposition: form-data; name=\"p\"\n\nv\n--b--\n";
        assert_eq!(form(lf_only).unwrap().value("p"), "v");
    }

    #[test]
    fn header_names_with_spaces_are_kept_and_controls_rejected() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename=\"x\"\r\nX Note: ok\r\n\r\nx\r\n--b--\r\n";
        let parsed = form(body).unwrap();
        let part = &parsed.files("image")[0];
        assert!(part.headers.contains(&("X Note".into(), b"ok".to_vec())));
        let bad = b"--b\r\nContent-Disposition: form-data; name=\"p\"\r\nX-A: a\x01\r\n\r\nx\r\n--b--\r\n";
        assert!(form(bad).is_err());
        let split = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename*0*=utf-8''%C3; filename*1*=%A9.png\r\n\r\nx\r\n--b--\r\n";
        assert_eq!(form(split).unwrap().files("image")[0].filename, "\u{e9}.png");
    }

    /// Go 1.26.4 `ReadForm` errors (v8.0.35's release toolchain quotes header lines).
    #[test]
    fn header_errors_read_as_go_prints_them() {
        let cases: [(&[u8], &str); 5] = [
            (
                b"--b\r\n X: y\r\n\r\nv\r\n--b--\r\n",
                r#"malformed MIME header initial line: " X: y""#,
            ),
            (
                b"--b\r\nNoColon\r\n\r\nv\r\n--b--\r\n",
                r#"malformed MIME header: missing colon: "NoColon""#,
            ),
            (
                b"--b\r\nX: a\x01\xff\xc3\xa9\r\n\r\nv\r\n--b--\r\n",
                "malformed MIME header line: \"X: a\\x01\\xff\u{e9}\"",
            ),
            (
                b"--b\r\nBad(Key): x\r\n\r\nv\r\n--b--\r\n",
                r#"malformed MIME header line: "Bad(Key): x""#,
            ),
            (
                b"--b\r\n\r\nv\r\n--b\r\njunk\r\n",
                r#"malformed MIME header: missing colon: "junk""#,
            ),
        ];
        for (body, want) in cases {
            assert_eq!(
                form(body).err().as_deref(),
                Some(want),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    /// `tests/fixtures/multipart_form_go.json`: Go 1.26.4 `ReadForm` on header, line and
    /// boundary edge cases (`tests/reference/multipart_form`).
    #[test]
    fn reads_forms_as_go_does() {
        use base64::Engine;
        use serde_json::{Map, Value, json};
        let doc: Value = serde_json::from_str(include_str!("../tests/fixtures/multipart_form_go.json")).unwrap();
        let b64 = |b: &[u8]| json!(base64::engine::general_purpose::STANDARD.encode(b));
        let mut checked = 0;
        for case in doc["cases"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let body = base64::engine::general_purpose::STANDARD
                .decode(case["body"].as_str().unwrap())
                .unwrap();
            let boundary = case.get("boundary").and_then(Value::as_str).unwrap_or("b");
            let got = match read_form(&body, boundary) {
                Err(e) => json!({ "error": e }),
                Ok(form) => {
                    let values: Map<String, Value> = form
                        .values
                        .iter()
                        .map(|(k, vs)| (k.clone(), vs.iter().map(|v| b64(v)).collect()))
                        .collect();
                    let files: Map<String, Value> = form
                        .files
                        .iter()
                        .map(|(k, parts)| {
                            let parts = parts.iter().map(|p| {
                                let mut header = Map::new();
                                for (n, v) in &p.headers {
                                    let list = header.entry(n.clone()).or_insert_with(|| json!([]));
                                    list.as_array_mut().unwrap().push(b64(v));
                                }
                                json!({ "filename": p.filename, "header": header, "body": b64(&p.body) })
                            });
                            (k.clone(), parts.collect())
                        })
                        .collect();
                    json!({ "values": values, "files": files })
                }
            };
            let want = match case.get("error") {
                Some(e) => json!({ "error": e }),
                // Go omits an empty map.
                None => json!({
                    "values": case.get("values").cloned().unwrap_or(json!({})),
                    "files": case.get("files").cloned().unwrap_or(json!({})),
                }),
            };
            assert_eq!(got, want, "{name}");
            checked += 1;
        }
        assert_eq!(checked, 65);
    }

    #[test]
    fn rfc2231_filename_makes_a_file_part() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename*=utf-8''dir%2F%C3%A9.png\r\n\r\nx\r\n--b--\r\n";
        let form = form(body).unwrap();
        assert_eq!(form.files("image")[0].filename, "\u{e9}.png");
        assert!(form.values.is_empty());
    }

    #[test]
    fn media_type_rules() {
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a b\""),
            Some(Ok("a b".into()))
        );
        assert_eq!(
            boundary("multipart/form-data"),
            Some(Err("multipart boundary is missing".into()))
        );
        assert_eq!(boundary("multipart/form-data; boundary=b;"), Some(Ok("b".into())));
        assert_eq!(boundary("multipart/form-data; boundary="), None, "bad parameter");
        assert_eq!(boundary("application/json"), None);
        assert!(matches!(
            parse_media_type(r#"form-data; name="a\"b"; filename*0="x"; filename*1*=%41"#),
            MediaType::Ok(_, p) if p["name"] == "a\"b" && p["filename"] == "xA"
        ));
    }
}
