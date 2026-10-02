//! Credential uploads as gin's `c.MultipartForm()` reads them: Go's
//! `mime.ParseMediaType`, `mime/multipart.Reader` and the `textproto` header rules,
//! applied to a body that is already fully in memory.
//!
//! ponytail: no quoted-printable part decoding, no header count or size limits, and
//! `%q` quoting is exact for ASCII only.

use std::collections::{BTreeMap, HashMap};

/// A file field of the form.
pub(super) struct FilePart {
    /// `filepath.Base` of the part's `filename` parameter; never empty.
    pub filename: String,
    pub data: Vec<u8>,
}

const NOT_MULTIPART: &str = "request Content-Type isn't multipart/form-data";
const MISSING_BOUNDARY: &str = "no multipart boundary param in Content-Type";
const TOO_LARGE: &str = "multipart: message too large";
const MAX_PARTS: usize = 1000;

/// gin `Context.ContentType`: the header up to the first space or `;`, unchanged.
pub(super) fn is_form(content_type: &str) -> bool {
    content_type.split([' ', ';']).next() == Some("multipart/form-data")
}

/// File parts grouped by sorted field name (gin's handler sorts `form.File` keys),
/// in body order within a field. Errors carry Go's text.
pub(super) fn form_files(content_type: &str, body: &[u8]) -> Result<Vec<FilePart>, String> {
    let (media, params) = parse_media_type(content_type).map_err(|_| NOT_MULTIPART.to_owned())?;
    if media != "multipart/form-data" {
        return Err(NOT_MULTIPART.into());
    }
    let boundary = params.get("boundary").ok_or(MISSING_BOUNDARY)?;
    let mut reader = Reader {
        buf: body,
        pos: 0,
        nl: b"\r\n",
        dash_boundary: format!("--{boundary}").into_bytes(),
        parts_read: 0,
    };
    let mut files: BTreeMap<String, Vec<FilePart>> = BTreeMap::new();
    let mut parts = 0;
    while let Some(headers) = reader.next_part()? {
        parts += 1;
        if parts > MAX_PARTS {
            return Err(TOO_LARGE.into());
        }
        let (disposition, params) = headers
            .get("content-disposition")
            .and_then(|v| parse_media_type(v).ok())
            .unwrap_or_default();
        let name = if disposition == "form-data" {
            params.get("name").cloned().unwrap_or_default()
        } else {
            String::new()
        };
        if name.is_empty() {
            // Go skips the part; a truncated body surfaces at the next boundary scan.
            if reader.body().is_err() {
                reader.pos = reader.buf.len();
            }
            continue;
        }
        let data = reader.body()?;
        let filename = params.get("filename").map(String::as_str).unwrap_or_default();
        if !filename.is_empty() {
            files.entry(name).or_default().push(FilePart {
                filename: go_base(filename).to_owned(),
                data,
            });
        }
    }
    Ok(files.into_values().flatten().collect())
}

/// Go `filepath.Base` on Unix.
pub(super) fn go_base(path: &str) -> &str {
    if path.is_empty() {
        return ".";
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/";
    }
    trimmed.rsplit('/').next().unwrap_or(trimmed)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    nl: &'static [u8],
    dash_boundary: Vec<u8>,
    parts_read: usize,
}

enum Scan {
    Data,
    End,
    UnexpectedEof,
}

impl<'a> Reader<'a> {
    /// `bufio.Reader.ReadSlice('\n')`: the line with its newline, and whether the
    /// body ended before one.
    fn read_slice(&mut self) -> (&'a [u8], bool) {
        let rest = &self.buf[self.pos..];
        match rest.iter().position(|&b| b == b'\n') {
            Some(i) => {
                self.pos += i + 1;
                (&rest[..=i], false)
            }
            None => {
                self.pos = self.buf.len();
                (rest, true)
            }
        }
    }

    fn is_final_boundary(&self, line: &[u8]) -> bool {
        let mut dash_dash = self.dash_boundary.clone();
        dash_dash.extend_from_slice(b"--");
        line.strip_prefix(dash_dash.as_slice())
            .map(skip_lwsp)
            .is_some_and(|rest| rest.is_empty() || rest == self.nl)
    }

    fn is_boundary_delimiter_line(&mut self, line: &[u8]) -> bool {
        let Some(rest) = line.strip_prefix(self.dash_boundary.as_slice()).map(skip_lwsp) else {
            return false;
        };
        // Go switches to bare-LF mode when the first delimiter line ends in "\n".
        if self.parts_read == 0 && rest == b"\n" {
            self.nl = b"\n";
        }
        rest == self.nl
    }

    /// Go `Reader.nextPart`; `None` is a clean end of the form.
    fn next_part(&mut self) -> Result<Option<HashMap<String, String>>, String> {
        if self.dash_boundary == b"--" {
            return Err("multipart: boundary is empty".into());
        }
        let mut expect_new_part = false;
        loop {
            let (line, eof) = self.read_slice();
            if eof && self.is_final_boundary(line) {
                return Ok(None);
            }
            if eof {
                return Err("multipart: NextPart: EOF".into());
            }
            if self.is_boundary_delimiter_line(line) {
                self.parts_read += 1;
                return self.headers();
            }
            if self.is_final_boundary(line) {
                return Ok(None);
            }
            if expect_new_part {
                return Err(format!("multipart: expecting a new Part; got line {}", go_quote(line)));
            }
            if self.parts_read == 0 {
                continue;
            }
            if line == self.nl {
                expect_new_part = true;
                continue;
            }
            return Err(format!("multipart: unexpected line in Next(): {}", go_quote(line)));
        }
    }

    /// `textproto.Reader.readLineSlice`: one line without its "\n" or "\r\n";
    /// `None` at the end of the body.
    fn read_line(&mut self) -> Option<&'a [u8]> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let (line, _) = self.read_slice();
        Some(match line.strip_suffix(b"\n") {
            Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
            None => line,
        })
    }

    /// Go `readMIMEHeader`, keyed by lowercased name; the first value wins for
    /// `Header.Get`. EOF inside the headers ends the form cleanly, as in Go.
    fn headers(&mut self) -> Result<Option<HashMap<String, String>>, String> {
        let mut headers = HashMap::new();
        if matches!(self.buf.get(self.pos), Some(b' ' | b'\t')) {
            let Some(line) = self.read_line() else {
                return Ok(None);
            };
            if line.len() > 80 {
                return Err(TOO_LARGE.into());
            }
            return Err(format!(
                "malformed MIME header initial line: {}",
                String::from_utf8_lossy(line)
            ));
        }
        loop {
            let Some(first) = self.read_line() else {
                return Ok(None);
            };
            if first.is_empty() {
                return Ok(Some(headers));
            }
            if !first.contains(&b':') {
                return Err(format!("malformed MIME header: missing colon: {}", go_quote(first)));
            }
            let mut kv = trim_ascii_space(first).to_vec();
            loop {
                let spaces = self.buf[self.pos..]
                    .iter()
                    .take_while(|&&b| b == b' ' || b == b'\t')
                    .count();
                if spaces == 0 {
                    break;
                }
                self.pos += spaces;
                kv.push(b' ');
                match self.read_line() {
                    Some(line) => kv.extend_from_slice(trim_ascii_space(line)),
                    None => break,
                }
            }
            let malformed = || format!("malformed MIME header line: {}", String::from_utf8_lossy(&kv));
            let colon = kv.iter().position(|&b| b == b':').unwrap_or(kv.len());
            let (key, value) = (&kv[..colon], &kv[(colon + 1).min(kv.len())..]);
            if !key.iter().all(|&b| is_header_field_byte(b) || b == b' ') {
                return Err(malformed());
            }
            if value.iter().any(|&b| (b < 0x20 && b != b'\t') || b == 0x7f) {
                return Err(malformed());
            }
            // Keys with a space are kept but never canonicalized, so `Get` misses them.
            if !key.contains(&b' ') {
                let value = value.iter().skip_while(|&&b| b == b' ' || b == b'\t');
                headers
                    .entry(String::from_utf8_lossy(key).to_ascii_lowercase())
                    .or_insert_with(|| String::from_utf8_lossy(&value.copied().collect::<Vec<_>>()).into_owned());
            }
        }
    }

    /// The current part's body, up to its closing boundary (Go `partReader`).
    fn body(&mut self) -> Result<Vec<u8>, String> {
        let mut nl_dash = self.nl.to_vec();
        nl_dash.extend_from_slice(&self.dash_boundary);
        let mut data = Vec::new();
        let mut total = 0;
        loop {
            let rest = &self.buf[self.pos..];
            let (n, scan) = scan_until_boundary(rest, &self.dash_boundary, &nl_dash, total);
            data.extend_from_slice(&rest[..n]);
            self.pos += n;
            total += n;
            match scan {
                Scan::Data => {}
                Scan::End => return Ok(data),
                Scan::UnexpectedEof => return Err("unexpected EOF".into()),
            }
        }
    }
}

/// Go `scanUntilBoundary` with the whole body buffered (the read error is set).
fn scan_until_boundary(buf: &[u8], dash: &[u8], nl_dash: &[u8], total: usize) -> (usize, Scan) {
    if total == 0 {
        if buf.starts_with(dash) {
            return match match_after_prefix(buf, dash) {
                true => (0, Scan::End),
                false => (dash.len(), Scan::Data),
            };
        }
        if dash.starts_with(buf) {
            return (0, Scan::UnexpectedEof);
        }
    }
    if let Some(i) = buf.windows(nl_dash.len()).position(|w| w == nl_dash) {
        return match match_after_prefix(&buf[i..], nl_dash) {
            true => (i, Scan::End),
            false => (i + nl_dash.len(), Scan::Data),
        };
    }
    if nl_dash.starts_with(buf) {
        return (0, Scan::UnexpectedEof);
    }
    if let Some(i) = buf.iter().rposition(|&b| b == nl_dash[0])
        && nl_dash.starts_with(&buf[i..])
    {
        return (i, Scan::Data);
    }
    (buf.len(), Scan::UnexpectedEof)
}

/// Go `matchAfterPrefix` at end of input: true when the boundary ends here.
fn match_after_prefix(buf: &[u8], prefix: &[u8]) -> bool {
    match buf.get(prefix.len()) {
        None => true,
        Some(b' ' | b'\t' | b'\r' | b'\n') => true,
        Some(b'-') => buf.get(prefix.len() + 1) == Some(&b'-'),
        Some(_) => false,
    }
}

fn skip_lwsp(b: &[u8]) -> &[u8] {
    let n = b.iter().take_while(|&&c| c == b' ' || c == b'\t').count();
    &b[n..]
}

fn trim_ascii_space(b: &[u8]) -> &[u8] {
    let is_space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    let start = b.iter().position(|c| !is_space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !is_space(c)).map_or(start, |i| i + 1);
    &b[start..end]
}

fn is_header_field_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Go `strconv.Quote` for header and boundary lines.
fn go_quote(b: &[u8]) -> String {
    let mut out = String::from("\"");
    for chunk in b.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                '\x07' => out.push_str("\\a"),
                '\x08' => out.push_str("\\b"),
                '\x0c' => out.push_str("\\f"),
                '\x0b' => out.push_str("\\v"),
                c if (c as u32) < 0x20 || c == '\x7f' => out.push_str(&format!("\\x{:02x}", c as u32)),
                c => out.push(c),
            }
        }
        for byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out.push('"');
    out
}

fn is_tspecial(c: u8) -> bool {
    b"()<>@,;:\\\"/[]?=".contains(&c)
}

fn is_token_char(c: u8) -> bool {
    c > 0x20 && c < 0x7f && !is_tspecial(c)
}

fn consume_token(v: &str) -> (&str, &str) {
    let i = v.bytes().position(|c| !is_token_char(c)).unwrap_or(v.len());
    v.split_at(i)
}

fn consume_value(v: &str) -> Option<(String, &str)> {
    if !v.starts_with('"') {
        let (token, rest) = consume_token(v);
        return Some((token.to_owned(), rest));
    }
    let bytes = v.as_bytes();
    let mut out = Vec::new();
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((String::from_utf8_lossy(&out).into_owned(), &v[i + 1..])),
            // Unnecessary escapes stay literal backslashes (MSIE paths).
            b'\\' if i + 1 < bytes.len() && is_tspecial(bytes[i + 1]) => {
                out.push(bytes[i + 1]);
                i += 1;
            }
            b'\r' | b'\n' => return None,
            b => out.push(b),
        }
        i += 1;
    }
    None
}

/// Go `consumeMediaParam`: `None` when `v` does not start with a valid parameter.
fn consume_media_param(v: &str) -> Option<(String, String, &str)> {
    let rest = v.trim_start().strip_prefix(';')?.trim_start();
    let (param, rest) = consume_token(rest);
    if param.is_empty() {
        return None;
    }
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let (value, after) = consume_value(rest)?;
    if value.is_empty() && after.len() == rest.len() {
        return None;
    }
    Some((param.to_lowercase(), value, after))
}

/// Go `mime.ParseMediaType`, including RFC 2231 continuations and charsets.
pub(super) fn parse_media_type(v: &str) -> Result<(String, HashMap<String, String>), ()> {
    let base = v.split(';').next().unwrap_or_default();
    let media = base.to_lowercase().trim().to_owned();
    let (kind, rest) = consume_token(&media);
    if kind.is_empty() {
        return Err(());
    }
    if !rest.is_empty() {
        let (sub, rest) = consume_token(rest.strip_prefix('/').ok_or(())?);
        if sub.is_empty() || !rest.is_empty() {
            return Err(());
        }
    }
    let mut params = HashMap::new();
    let mut continuation: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut v = &v[base.len()..];
    loop {
        v = v.trim_start();
        if v.is_empty() {
            break;
        }
        let Some((key, value, rest)) = consume_media_param(v) else {
            if v.trim() == ";" {
                break;
            }
            return Err(());
        };
        let map = match key.split_once('*') {
            Some((base_name, _)) => continuation.entry(base_name.to_owned()).or_default(),
            None => &mut params,
        };
        if map.get(&key).is_some_and(|existing| *existing != value) {
            return Err(());
        }
        map.insert(key, value);
        v = rest;
    }
    for (key, pieces) in continuation {
        if let Some(v) = pieces.get(&format!("{key}*")) {
            if let Some(decoded) = decode_2231(v) {
                params.insert(key, decoded);
            }
            continue;
        }
        let mut buf = String::new();
        let mut valid = false;
        for n in 0.. {
            if let Some(v) = pieces.get(&format!("{key}*{n}")) {
                valid = true;
                buf.push_str(v);
                continue;
            }
            let Some(v) = pieces.get(&format!("{key}*{n}*")) else {
                break;
            };
            valid = true;
            if n == 0 {
                buf.push_str(&decode_2231(v).unwrap_or_default());
            } else {
                buf.push_str(&percent_unescape(v).unwrap_or_default());
            }
        }
        if valid {
            params.insert(key, buf);
        }
    }
    Ok((media, params))
}

fn decode_2231(v: &str) -> Option<String> {
    let (charset, rest) = v.split_once('\'')?;
    let (_, value) = rest.split_once('\'')?;
    match charset.to_lowercase().as_str() {
        "us-ascii" | "utf-8" => percent_unescape(value),
        _ => None,
    }
}

/// Go `percentHexUnescape`.
fn percent_unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(
                u8::from_str_radix(hex, 16)
                    .ok()
                    .filter(|_| hex.bytes().all(|c| c.is_ascii_hexdigit()))?,
            );
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CT: &str = "multipart/form-data; boundary=b";

    #[test]
    fn reads_file_fields_sorted_by_field_and_skips_values() {
        let body = b"--b\r\nContent-Disposition: form-data; name=\"z\"; filename=\"dir/a.json\"\r\n\r\n{}\r\n--b\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n--b\r\nContent-Disposition: form-data; name=\"c\"; filename=\"b.json\"\r\n\r\n[1]\r\n--b--\r\n";
        let files = form_files(CT, body).unwrap();
        let got: Vec<_> = files.iter().map(|f| (f.filename.as_str(), f.data.as_slice())).collect();
        // Field "c" sorts before field "z".
        assert_eq!(got, [("b.json", &b"[1]"[..]), ("a.json", &b"{}"[..])]);
    }

    #[test]
    fn media_type_params_follow_go() {
        let (t, p) = parse_media_type("Form-Data; Name=\"a\\\"b\"; filename*=UTF-8''x%2Ey; k*0=p; k*1*=%41").unwrap();
        assert_eq!(t, "form-data");
        assert_eq!(p["name"], "a\"b");
        assert_eq!(p["filename"], "x.y");
        assert_eq!(p["k"], "pA");
        assert!(parse_media_type("a/b; x=1; x=2").is_err());
        assert!(parse_media_type("a/b; x=1; x=1").is_ok());
        assert!(parse_media_type("a/b/c").is_err());
        assert!(parse_media_type("a/b; x=\"open").is_err());
    }

    /// Deterministic fuzz over boundary-shaped fragments: never panics, and any
    /// file returned has a non-empty base name.
    #[test]
    fn hostile_bodies_never_panic() {
        const PIECES: &[&[u8]] = &[
            b"--b",
            b"--",
            b"\r\n",
            b"\n",
            b"\r",
            b" ",
            b"\t",
            b"-",
            b"b",
            b"{}",
            b"Content-Disposition: form-data; name=\"f\"; filename=\"x.json\"",
            b"Content-Disposition: form-data; name=f",
            b"X:",
            b":",
            b"\x7f",
            b"\xff",
        ];
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..20_000 {
            let mut body = Vec::new();
            for _ in 0..(next() % 24) {
                body.extend_from_slice(PIECES[(next() % PIECES.len() as u64) as usize]);
            }
            if let Ok(files) = form_files(CT, &body) {
                assert!(
                    files
                        .iter()
                        .all(|f| !f.filename.is_empty() && !f.filename.contains('/'))
                );
            }
        }
    }
}
