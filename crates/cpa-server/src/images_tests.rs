//! Images handler tests. The routes are checked against the Go server in
//! tests/media_go.rs; these cover the content sniffer the goldens touch only twice.

use super::*;

/// Expectations printed by Go 1.x `http.DetectContentType` for the same bytes.
#[test]
fn sniffs_like_go() {
    let cases: [(&[u8], &str); 28] = [
        (b"", "text/plain; charset=utf-8"),
        (b"   ", "text/plain; charset=utf-8"),
        (b"\x89PNG\r\n\x1a\nxx", "image/png"),
        (b"GIF87a..", "image/gif"),
        (b"GIF89a..", "image/gif"),
        (b"\xff\xd8\xff\xe0", "image/jpeg"),
        (b"BMxx", "image/bmp"),
        (b"RIFF\x00\x00\x00\x00WEBPVP8 ", "image/webp"),
        (b"RIFF\x00\x00\x00\x00WAVEfmt ", "audio/wave"),
        (b"\x00\x00\x00\x18ftypmp42\x00\x00\x00\x00mp42isom", "video/mp4"),
        (b"\x1a\x45\xdf\xa3", "video/webm"),
        (b" \n<html>x", "text/html; charset=utf-8"),
        (b"<!DOCTYPE HTML>", "text/html; charset=utf-8"),
        (b"<p", "text/plain; charset=utf-8"),
        (b"<?xml version", "text/xml; charset=utf-8"),
        (b"%PDF-1.4", "application/pdf"),
        (b"\xef\xbb\xbfhi", "text/plain; charset=utf-8"),
        (b"\xfe\xffhi", "text/plain; charset=utf-16be"),
        (b"PK\x03\x04", "application/zip"),
        (b"\x1f\x8b\x08", "application/x-gzip"),
        (b"hello world", "text/plain; charset=utf-8"),
        (b"hi\x00there", "application/octet-stream"),
        (b"OggS\x00", "application/ogg"),
        (b"ID3\x03", "audio/mpeg"),
        (b"wOFF", "font/woff"),
        (b"\x00asm", "application/wasm"),
        (b"<b>bold</b>", "text/html; charset=utf-8"),
        (b"<br/>", "text/plain; charset=utf-8"),
    ];
    for (input, want) in cases {
        assert_eq!(detect_content_type(input), want, "{input:?}");
    }
}
