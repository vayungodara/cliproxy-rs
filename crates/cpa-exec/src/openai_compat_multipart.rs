//! Go `mime/multipart` and `mime.ParseMediaType`, as the images paths use them
//! (`Reader.ReadForm`, then `Writer.WriteField` / `CreatePart`). Ported from Go 1.26
//! `mime/multipart/multipart.go`, `formdata.go` and `mime/mediatype.go`.
//!
//! ponytail: quoted-printable parts are not decoded (Go's `NextPart` does) and ReadForm's
//! part, header and memory limits are not enforced beyond the route's body cap; image
//! clients send binary parts. Field and file order follow first appearance, one of the
//! orders Go's map iteration can produce.

use std::collections::BTreeMap;

/// One parsed form part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Canonical header names in arrival order, with values.
    pub headers: Vec<(String, String)>,
    /// `Part.FormName()`.
    pub name: String,
    /// `Part.FileName()`: `filepath.Base` of the filename parameter, empty for fields.
    pub filename: String,
    pub body: Vec<u8>,
}

impl Part {
    /// `textproto.MIMEHeader.Get`.
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = crate::proxy::canonical_header(name);
        self.headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
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
    /// No usable media type.
    Invalid,
    /// The media type parsed but its parameters did not (Go returns the type and an error).
    BadParams(String),
    Ok(String, BTreeMap<String, String>),
}

pub fn parse_media_type(v: &str) -> MediaType {
    let base = v.split(';').next().unwrap_or_default();
    let media = base.to_ascii_lowercase().trim().to_owned();
    let (main, rest) = consume_token(&media);
    let valid_type = !main.is_empty()
        && (rest.is_empty()
            || rest.strip_prefix('/').is_some_and(|r| {
                let (sub, tail) = consume_token(r);
                !sub.is_empty() && tail.is_empty()
            }));
    if !valid_type {
        return MediaType::Invalid;
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
            return MediaType::Invalid;
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

/// `matchAfterPrefix` with the whole body available (a read error always follows).
fn match_after_prefix(buf: &[u8], prefix_len: usize) -> bool {
    match buf.get(prefix_len) {
        None | Some(b' ' | b'\t' | b'\r' | b'\n') => true,
        Some(b'-') => buf.get(prefix_len + 1) == Some(&b'-'),
        Some(_) => false,
    }
}

impl Reader<'_> {
    fn read_line(&mut self) -> (Vec<u8>, bool) {
        let rest = &self.body[self.pos..];
        match rest.iter().position(|b| *b == b'\n') {
            Some(i) => {
                self.pos += i + 1;
                (rest[..=i].to_vec(), false)
            }
            None => {
                self.pos = self.body.len();
                (rest.to_vec(), true)
            }
        }
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

    /// `nextPart`; `Ok(None)` is EOF.
    fn next_part(&mut self) -> Result<Option<Part>, String> {
        let mut expect_new = false;
        loop {
            let (line, eof) = self.read_line();
            if eof && self.is_final(&line) {
                return Ok(None);
            }
            if eof {
                return Err("multipart: NextPart: EOF".into());
            }
            if self.is_delimiter(&line) {
                self.parts_read += 1;
                return self.part().map(Some);
            }
            if self.is_final(&line) {
                return Ok(None);
            }
            if expect_new {
                return Err(format!(
                    "multipart: expecting a new Part; got line {:?}",
                    String::from_utf8_lossy(&line)
                ));
            }
            if self.parts_read == 0 {
                continue;
            }
            if line == self.nl {
                expect_new = true;
                continue;
            }
            return Err(format!(
                "multipart: unexpected line in Next(): {:?}",
                String::from_utf8_lossy(&line)
            ));
        }
    }

    /// `textproto.ReadMIMEHeader`, then the body up to the next boundary.
    fn part(&mut self) -> Result<Part, String> {
        let malformed = |line: &[u8]| format!("malformed MIME header line: {}", String::from_utf8_lossy(line));
        let mut headers: Vec<(String, String)> = Vec::new();
        let mut first = true;
        loop {
            let (raw, eof) = self.read_line();
            let line = raw.strip_suffix(b"\n").unwrap_or(&raw);
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if first && matches!(line.first(), Some(b' ' | b'\t')) {
                return Err(format!(
                    "malformed MIME header initial line: {}",
                    String::from_utf8_lossy(line)
                ));
            }
            first = false;
            if line.is_empty() {
                if eof {
                    return Err("unexpected EOF".into());
                }
                break;
            }
            let trimmed = line.trim_ascii();
            if matches!(line.first(), Some(b' ' | b'\t')) {
                // A continuation line folds into the previous value.
                match headers.last_mut() {
                    Some((_, v)) => {
                        v.push(' ');
                        v.push_str(&String::from_utf8_lossy(trimmed));
                        continue;
                    }
                    None => return Err(malformed(line)),
                }
            }
            let Some(colon) = trimmed.iter().position(|b| *b == b':') else {
                return Err(malformed(trimmed));
            };
            let (key, value) = (&trimmed[..colon], &trimmed[colon + 1..]);
            // canonicalMIMEHeaderKey: token bytes are canonicalized; keys with spaces are
            // accepted as they are; anything else is malformed.
            let key = if key.is_empty() || !key.iter().all(|c| token_char(*c) || *c == b' ') {
                return Err(malformed(trimmed));
            } else if key.contains(&b' ') {
                String::from_utf8_lossy(key).into_owned()
            } else {
                crate::proxy::canonical_header(&String::from_utf8_lossy(key))
            };
            if value.iter().any(|c| *c < 0x20 && *c != b'\t' || *c == 0x7f) {
                return Err(malformed(trimmed));
            }
            let value = value
                .iter()
                .skip_while(|c| matches!(c, b' ' | b'\t'))
                .copied()
                .collect::<Vec<u8>>();
            headers.push((key, String::from_utf8_lossy(&value).into_owned()));
            if eof {
                return Err("unexpected EOF".into());
            }
        }
        let body = self.part_body()?;
        let disposition = headers
            .iter()
            .find(|(n, _)| n == "Content-Disposition")
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        let (kind, params) = match parse_media_type(disposition) {
            MediaType::Ok(kind, params) => (kind, params),
            MediaType::BadParams(kind) => (kind, BTreeMap::new()),
            MediaType::Invalid => (String::new(), BTreeMap::new()),
        };
        let name = if kind == "form-data" {
            params.get("name").cloned().unwrap_or_default()
        } else {
            String::new()
        };
        let filename = params
            .get("filename")
            .filter(|f| !f.is_empty())
            .map(|f| base_name(f))
            .unwrap_or_default();
        Ok(Part {
            headers,
            name,
            filename,
            body,
        })
    }

    /// `partReader` with `scanUntilBoundary`: the body ends before `nl--boundary`
    /// followed by `--`, whitespace or the end of input.
    fn part_body(&mut self) -> Result<Vec<u8>, String> {
        let buf = &self.body[self.pos..];
        if buf.starts_with(&self.dash_boundary) && match_after_prefix(buf, self.dash_boundary.len()) {
            return Ok(Vec::new());
        }
        let mut nl_dash = self.nl.to_vec();
        nl_dash.extend_from_slice(&self.dash_boundary);
        let mut from = 0;
        while let Some(i) = buf
            .get(from..)
            .and_then(|b| b.windows(nl_dash.len()).position(|w| w == nl_dash.as_slice()))
            .map(|i| i + from)
        {
            if match_after_prefix(&buf[i..], nl_dash.len()) {
                let body = buf[..i].to_vec();
                self.pos += i;
                return Ok(body);
            }
            from = i + nl_dash.len();
        }
        Err("unexpected EOF".into())
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
    pub fn part(&mut self, headers: &[(String, String)], body: &[u8]) {
        if self.parts > 0 {
            self.out.extend_from_slice(b"\r\n");
        }
        self.parts += 1;
        self.out
            .extend_from_slice(format!("--{}\r\n", self.boundary).as_bytes());
        let mut sorted: Vec<&(String, String)> = headers.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, value) in sorted {
            self.out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        self.out.extend_from_slice(b"\r\n");
        self.out.extend_from_slice(body);
    }

    /// `WriteField`.
    pub fn field(&mut self, name: &str, value: &[u8]) {
        let disposition = format!(r#"form-data; name="{}""#, escape_quotes(name));
        self.part(&[("Content-Disposition".into(), disposition)], value);
    }

    /// A file part re-emitted under `field` the way the images paths do: the original
    /// part headers, a fresh Content-Disposition, and a default Content-Type.
    pub fn file(&mut self, field: &str, part: &Part) {
        let mut headers: Vec<(String, String)> = part
            .headers
            .iter()
            .filter(|(n, _)| n != "Content-Disposition")
            .cloned()
            .collect();
        headers.push((
            "Content-Disposition".into(),
            file_content_disposition(field, &part.filename),
        ));
        if part.header("Content-Type").is_none_or(str::is_empty) {
            headers.retain(|(n, _)| n != "Content-Type");
            headers.push(("Content-Type".into(), "application/octet-stream".into()));
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
        assert!(part.headers.contains(&("X Note".into(), "ok".into())));
        let bad = b"--b\r\nContent-Disposition: form-data; name=\"p\"\r\nX-A: a\x01\r\n\r\nx\r\n--b--\r\n";
        assert!(form(bad).is_err());
        let split = b"--b\r\nContent-Disposition: form-data; name=\"image\"; filename*0*=utf-8''%C3; filename*1*=%A9.png\r\n\r\nx\r\n--b--\r\n";
        assert_eq!(form(split).unwrap().files("image")[0].filename, "\u{e9}.png");
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
