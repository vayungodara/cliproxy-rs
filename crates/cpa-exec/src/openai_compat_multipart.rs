//! Go `mime/multipart` form reading and writing, as the images paths use them
//! (`Reader.ReadForm` then `Writer.WriteField` / `CreatePart`).
//!
//! ponytail: no quoted-printable part decoding and no ReadForm part/header limits; the
//! images clients send plain binary parts. Field and file order follow first appearance,
//! one of the orders Go's map iteration can produce.

/// One parsed form part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Canonical header names in arrival order, with values.
    pub headers: Vec<(String, String)>,
    pub name: String,
    /// `Part.FileName()`: the base name, empty for plain fields.
    pub filename: String,
    pub body: Vec<u8>,
}

impl Part {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A parsed form: values and files keyed by field name in first-appearance order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    pub values: Vec<(String, Vec<String>)>,
    pub files: Vec<(String, Vec<Part>)>,
}

impl Form {
    /// `c.PostForm(key)`: the first value, or empty.
    pub fn value(&self, key: &str) -> &str {
        self.values
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.first())
            .map_or("", String::as_str)
    }

    pub fn files(&self, key: &str) -> &[Part] {
        self.files
            .iter()
            .find(|(k, _)| k == key)
            .map_or(&[], |(_, v)| v.as_slice())
    }
}

/// `mime.ParseMediaType` reduced to what callers branch on: the lowercased media type and
/// parameters, or `None` when the value does not parse.
pub fn parse_media_type(value: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut parts = value.split(';');
    let media = parts.next()?.trim().to_ascii_lowercase();
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_graphic() && !b"()<>@,;:\\\"/[]?=".contains(&b))
    };
    let (main, sub) = media.split_once('/').unwrap_or((&media, ""));
    if !token(main) || (media.contains('/') && !token(sub)) {
        return None;
    }
    let mut params = Vec::new();
    let mut rest = value[value.find(';').unwrap_or(value.len())..].to_owned();
    loop {
        rest = rest.trim_start_matches([' ', '\t']).to_owned();
        let Some(stripped) = rest.strip_prefix(';') else {
            break;
        };
        let stripped = stripped.trim_start_matches([' ', '\t']);
        if stripped.is_empty() {
            break;
        }
        let (key, after) = stripped.split_once('=')?;
        let key = key.trim().to_ascii_lowercase();
        if !token(&key) {
            return None;
        }
        let after = after.trim_start();
        let (val, remainder) = if let Some(quoted) = after.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = quoted.char_indices();
            let mut end = None;
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => {
                        end = Some(i + 1);
                        break;
                    }
                    c => out.push(c),
                }
            }
            (out, &quoted[end?..])
        } else {
            let end = after.find([';', ' ', '\t']).unwrap_or(after.len());
            let v = &after[..end];
            if !token(v) {
                return None;
            }
            (v.to_owned(), &after[end..])
        };
        params.push((key, val));
        rest = remainder.to_owned();
    }
    Some((media, params))
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// The boundary of a `multipart/*` content type, `Err` with Go's message when absent,
/// `None` when the content type is not multipart (callers pass the body through).
pub fn boundary(content_type: &str) -> Option<Result<String, String>> {
    let (media, params) = parse_media_type(content_type.trim())?;
    if !media.starts_with("multipart/") {
        return None;
    }
    match param(&params, "boundary").map(str::trim).filter(|b| !b.is_empty()) {
        Some(b) => Some(Ok(b.to_owned())),
        None => Some(Err("multipart boundary is missing".into())),
    }
}

/// `Reader.ReadForm`.
pub fn read_form(body: &[u8], boundary: &str) -> Result<Form, String> {
    let delimiter = format!("--{boundary}").into_bytes();
    let find = |hay: &[u8], from: usize| {
        hay.get(from..)
            .and_then(|h| h.windows(delimiter.len()).position(|w| w == delimiter.as_slice()))
            .map(|p| p + from)
    };
    // The first boundary may follow a preamble; it must start a line.
    let mut pos = None;
    let mut from = 0;
    while let Some(p) = find(body, from) {
        if p == 0 || body[p - 1] == b'\n' {
            pos = Some(p);
            break;
        }
        from = p + 1;
    }
    let mut pos = pos.ok_or("multipart: NextPart: EOF")?;
    let mut form = Form::default();
    loop {
        let after = pos + delimiter.len();
        let rest = &body[after..];
        if rest.starts_with(b"--") {
            return Ok(form);
        }
        // Skip transport padding and the line break after the boundary.
        let line_end = rest
            .iter()
            .position(|b| *b == b'\n')
            .ok_or("multipart: NextPart: EOF")?;
        if rest[..line_end].iter().any(|b| !matches!(b, b' ' | b'\t' | b'\r')) {
            return Err("multipart: expecting a new Part; got line".into());
        }
        let start = after + line_end + 1;
        let crlf = line_end > 0 && rest[line_end - 1] == b'\r';
        let separator: Vec<u8> = [if crlf { &b"\r\n"[..] } else { &b"\n"[..] }, &delimiter].concat();
        let end = body[start..]
            .windows(separator.len())
            .position(|w| w == separator.as_slice())
            .map(|p| p + start)
            .ok_or("multipart: NextPart: EOF")?;
        let part = parse_part(&body[start..end])?;
        if !part.name.is_empty() {
            if part.filename.is_empty() {
                let value = String::from_utf8_lossy(&part.body).into_owned();
                push(&mut form.values, part.name, value);
            } else {
                let name = part.name.clone();
                push(&mut form.files, name, part);
            }
        }
        pos = end + separator.len() - delimiter.len();
    }
}

fn push<T>(list: &mut Vec<(String, Vec<T>)>, key: String, value: T) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some((_, values)) => values.push(value),
        None => list.push((key, vec![value])),
    }
}

fn parse_part(raw: &[u8]) -> Result<Part, String> {
    let (head, body) = match raw.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(p) => (&raw[..p], &raw[p + 4..]),
        None => match raw.windows(2).position(|w| w == b"\n\n") {
            Some(p) => (&raw[..p], &raw[p + 2..]),
            None if raw.starts_with(b"\r\n") => (&raw[..0], &raw[2..]),
            None if raw.starts_with(b"\n") => (&raw[..0], &raw[1..]),
            None => return Err("multipart: malformed MIME header".into()),
        },
    };
    let mut headers = Vec::new();
    for line in String::from_utf8_lossy(head).split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed MIME header line: {line}"))?;
        headers.push((crate::kimi_http::canonical_header(name.trim()), value.trim().to_owned()));
    }
    let disposition = headers
        .iter()
        .find(|(n, _)| n == "Content-Disposition")
        .map(|(_, v)| v.as_str())
        .unwrap_or_default();
    let (kind, params) = parse_media_type(disposition).unwrap_or_default();
    let name = if kind == "form-data" {
        param(&params, "name").unwrap_or_default().to_owned()
    } else {
        String::new()
    };
    let filename = param(&params, "filename")
        .filter(|f| !f.is_empty())
        .map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f).to_owned())
        .unwrap_or_default();
    Ok(Part {
        headers,
        name,
        filename,
        body: body.to_vec(),
    })
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
    pub fn field(&mut self, name: &str, value: &str) {
        let disposition = format!(r#"form-data; name="{}""#, escape_quotes(name));
        self.part(&[("Content-Disposition".into(), disposition)], value.as_bytes());
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
        if !headers.iter().any(|(n, v)| n == "Content-Type" && !v.is_empty()) {
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

/// `rewriteOpenAICompatImagesMultipartPayload` / `buildOpenAICompatImagesMultipartRequest`:
/// `model` and `stream` first, then the other fields and every file.
pub fn rewrite_images_form(form: &Form, model: &str, stream: bool, always_model: bool) -> (Vec<u8>, String) {
    let mut writer = Writer::new();
    if always_model || !model.is_empty() {
        writer.field("model", model);
    }
    if stream {
        writer.field("stream", "true");
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

    #[test]
    fn reads_fields_and_files_and_writes_go_layout() {
        let body = b"preamble\r\n--b\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nhi\r\n--b\r\ncontent-disposition: form-data; name=\"image\"; filename=\"dir/a.png\"\r\ncontent-type: image/png\r\n\r\n\x00\x01\r\n--b--\r\n";
        let form = read_form(body, "b").unwrap();
        assert_eq!(form.value("prompt"), "hi");
        let file = &form.files("image")[0];
        assert_eq!(file.filename, "a.png");
        assert_eq!(file.body, b"\x00\x01");
        let mut writer = Writer::with_boundary("X".into());
        writer.field("model", "m");
        writer.file("image", file);
        let (out, content_type) = writer.finish();
        assert_eq!(content_type, "multipart/form-data; boundary=X");
        assert_eq!(
            out,
            b"--X\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm\r\n--X\r\nContent-Disposition: form-data; name=\"image\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n\x00\x01\r\n--X--\r\n"
        );
    }

    #[test]
    fn boundary_rules() {
        assert_eq!(
            boundary("multipart/form-data; boundary=\"a b\""),
            Some(Ok("a b".into()))
        );
        assert_eq!(
            boundary("multipart/form-data"),
            Some(Err("multipart boundary is missing".into()))
        );
        assert_eq!(boundary("application/json"), None);
        assert_eq!(boundary("text/plain"), None);
    }
}
