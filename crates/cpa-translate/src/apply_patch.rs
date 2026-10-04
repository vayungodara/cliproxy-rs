//! Codex `apply_patch` support: the custom tool declaration
//! (internal/client/codex/apply-patch) and the streamed input decoding and events that
//! turn `{"input": "..."}` function arguments back into a freeform custom tool call
//! (internal/translator/common/apply_patch_{input,events}.go).

use cpa_common::json::{self as gj, Res};

pub(crate) const PARAMETERS: &str = r#"{"type":"object","properties":{"input":{"type":"string","description":"The complete apply_patch patch text."}},"required":["input"],"additionalProperties":false}"#;

const INSTRUCTIONS: &str = "Call this function with a JSON object whose input field contains the complete patch text.
Use the Codex apply_patch format, not a conventional git unified diff.
Start with *** Begin Patch and end with *** End Patch.
Use *** Add File: path, *** Delete File: path, or *** Update File: path.
Every added-file content line starts with +.
For updates, use @@; context lines start with one space, removed lines with -, and added lines with +.
Use *** Move to: path for a rename and *** End of File when required by the patch grammar.
Example input:
*** Begin Patch
*** Update File: src/main.go
@@
-old
+new
*** End Patch";

/// applypatch.IsCustomTool.
pub(crate) fn is_custom_tool(tool: &Res<'_>) -> bool {
    tool.get("type").str() == "custom" && crate::common::trim_space(&tool.get("name").bytes()) == b"apply_patch"
}

/// applypatch.Description: the original description (minus the freeform warning), the
/// JSON-wrapper instructions, and the original grammar.
pub(crate) fn description(tool: &Res<'_>) -> Vec<u8> {
    let original = tool.get("description").bytes();
    let warning: &[u8] = b"This is a FREEFORM tool, so do not wrap the patch in JSON.";
    let mut stripped = vec![];
    let mut rest = &original[..];
    while let Some(i) = rest.windows(warning.len()).position(|w| w == warning) {
        stripped.extend_from_slice(&rest[..i]);
        rest = &rest[i + warning.len()..];
    }
    stripped.extend_from_slice(rest);
    let mut out = vec![];
    if !crate::common::trim_space(&stripped).is_empty() {
        out.extend_from_slice(&stripped);
        out.extend_from_slice(b"\n\n");
    }
    out.extend_from_slice(INSTRUCTIONS.as_bytes());
    let grammar = tool.get("format.definition").bytes();
    if !grammar.is_empty() {
        if grammar.windows(19).any(|w| w == b"*** Environment ID:") {
            out.extend_from_slice(b"\n\nUse *** Environment ID: as specified by the patch grammar.");
        }
        out.extend_from_slice(b"\n\nOriginal patch grammar:\n");
        out.extend_from_slice(&grammar);
    }
    out
}

// ---------------------------------------------------------------------------------------
// Streamed input decoding (translator/common/apply_patch_input.go)

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    BeforeObject,
    BeforeKey,
    InKey,
    BeforeColon,
    BeforeValue,
    InValue,
    AfterValue,
    Complete,
}

/// ApplyPatchInputDecoder: decodes the `input` string of streamed `{"input": "..."}`
/// function arguments, emitting only complete, validated characters. Errors are sticky
/// and carry Go's messages (they are never shown to clients).
#[derive(Clone, Debug, Default)]
pub(crate) struct InputDecoder {
    phase: Phase,
    key_raw: Vec<u8>,
    escape_raw: Vec<u8>,
    utf8_pending: Vec<u8>,
    high_surrogate: u16,
    input: String,
    finished: bool,
    err: Option<String>,
}

fn json_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\r' | b'\n')
}

fn hex(c: u8) -> Option<u16> {
    (c as char).to_digit(16).map(|d| d as u16)
}

impl InputDecoder {
    fn fail(&mut self, err: &str) -> String {
        self.err = Some(err.to_owned());
        err.to_owned()
    }

    /// `Push`: the newly decoded text of `fragment`.
    pub(crate) fn push(&mut self, fragment: &[u8]) -> Result<String, String> {
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        if self.finished {
            if fragment.is_empty() {
                return Ok(String::new());
            }
            return Err(self.fail("apply_patch arguments received after completion"));
        }
        let start = self.input.len();
        for &c in fragment {
            let failure = match self.phase {
                Phase::BeforeObject if json_space(c) => None,
                Phase::BeforeObject if c != b'{' => Some("apply_patch arguments must be a JSON object"),
                Phase::BeforeObject => {
                    self.phase = Phase::BeforeKey;
                    None
                }
                Phase::BeforeKey if json_space(c) => None,
                Phase::BeforeKey if c != b'"' => Some("apply_patch arguments must contain the input field"),
                Phase::BeforeKey => {
                    self.key_raw.push(c);
                    self.phase = Phase::InKey;
                    None
                }
                Phase::InKey => self.key_byte(c),
                Phase::BeforeColon if json_space(c) => None,
                Phase::BeforeColon if c != b':' => Some("apply_patch input key must be followed by a colon"),
                Phase::BeforeColon => {
                    self.phase = Phase::BeforeValue;
                    None
                }
                Phase::BeforeValue if json_space(c) => None,
                Phase::BeforeValue if c != b'"' => Some("apply_patch input must be a string"),
                Phase::BeforeValue => {
                    self.phase = Phase::InValue;
                    None
                }
                Phase::InValue => self.value_byte(c),
                Phase::AfterValue if json_space(c) => None,
                Phase::AfterValue if c != b'}' => Some("apply_patch arguments must contain only one input field"),
                Phase::AfterValue => {
                    self.phase = Phase::Complete;
                    None
                }
                Phase::Complete if json_space(c) => None,
                Phase::Complete => Some("apply_patch arguments must not contain trailing JSON"),
            };
            if let Some(err) = failure {
                return Err(self.fail(err));
            }
        }
        Ok(self.input[start..].to_owned())
    }

    fn key_byte(&mut self, c: u8) -> Option<&'static str> {
        self.key_raw.push(c);
        if !self.escape_raw.is_empty() {
            self.escape_raw.clear();
            return None;
        }
        match c {
            b'\\' => self.escape_raw.push(c),
            ..0x20 => return Some("invalid control character in apply_patch input key"),
            b'"' => {
                let key = gj::valid(&self.key_raw)
                    .then(|| gj::go_unquote(&self.key_raw))
                    .flatten();
                if key.is_none() {
                    return Some("decode apply_patch input key");
                }
                if key.as_deref() != Some("input") {
                    return Some("apply_patch arguments must contain the input field");
                }
                self.key_raw.clear();
                self.phase = Phase::BeforeColon;
            }
            _ => {}
        }
        None
    }

    fn value_byte(&mut self, c: u8) -> Option<&'static str> {
        if !self.utf8_pending.is_empty() || c >= 0x80 {
            if !self.escape_raw.is_empty() || self.high_surrogate != 0 {
                return Some("invalid Unicode escape in apply_patch input");
            }
            self.utf8_pending.push(c);
            // utf8.FullRune: wait while the bytes are a valid but incomplete prefix.
            match std::str::from_utf8(&self.utf8_pending) {
                Ok(s) => self.input.push_str(s),
                Err(e) if e.error_len().is_none() => return None,
                Err(_) => return Some("invalid UTF-8 in apply_patch input"),
            }
            self.utf8_pending.clear();
            return None;
        }
        if !self.escape_raw.is_empty() {
            self.escape_raw.push(c);
            if self.escape_raw.len() == 2 {
                if self.high_surrogate != 0 && c != b'u' {
                    return Some("apply_patch input high surrogate requires a low surrogate");
                }
                let decoded = match c {
                    b'u' => return None,
                    b'"' | b'\\' | b'/' => c,
                    b'b' => 8,
                    b'f' => 12,
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    _ => return Some("invalid escape in apply_patch input"),
                };
                self.input.push(decoded as char);
                self.escape_raw.clear();
                return None;
            }
            if hex(c).is_none() {
                return Some("invalid Unicode escape in apply_patch input");
            }
            if self.escape_raw.len() < 6 {
                return None;
            }
            let code = self.escape_raw[2..]
                .iter()
                .fold(0u16, |acc, &d| acc << 4 | hex(d).unwrap());
            self.escape_raw.clear();
            match code {
                _ if self.high_surrogate != 0 => {
                    if !(0xdc00..=0xdfff).contains(&code) {
                        return Some("apply_patch input high surrogate requires a low surrogate");
                    }
                    let c = 0x10000 + ((self.high_surrogate as u32 - 0xd800) << 10) + (code as u32 - 0xdc00);
                    self.input.push(char::from_u32(c).unwrap());
                    self.high_surrogate = 0;
                }
                0xd800..=0xdbff => self.high_surrogate = code,
                0xdc00..=0xdfff => return Some("unpaired low surrogate in apply_patch input"),
                _ => self.input.push(char::from_u32(code as u32).unwrap()),
            }
            return None;
        }
        if self.high_surrogate != 0 && c != b'\\' {
            return Some("apply_patch input high surrogate requires a low surrogate");
        }
        match c {
            b'\\' => self.escape_raw.push(c),
            b'"' => self.phase = Phase::AfterValue,
            ..0x20 => return Some("invalid control character in apply_patch input"),
            _ => self.input.push(c as char),
        }
        None
    }

    /// `Finish`: validates the complete arguments and returns the unsent suffix.
    pub(crate) fn finish(&mut self, arguments: &[u8]) -> Result<String, String> {
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        // applypatch.UnwrapInput and the strict re-scan of the final snapshot accept
        // exactly the arguments a fresh decoder scans to completion without error.
        let mut fresh = InputDecoder::default();
        if let Err(err) = fresh.push(arguments) {
            return Err(self.fail(&err));
        }
        if fresh.phase != Phase::Complete {
            // applypatch.UnwrapInput's json.Decoder.Token error for truncated arguments:
            // `EOF` at a token boundary, `unexpected EOF` inside a token.
            // ponytail: other malformed arguments fail with this decoder's own messages,
            // not encoding/json's syntax-error wording. The text is internal: executors
            // answer with the fixed APPLY_PATCH_UPSTREAM_ERROR.
            let message = match fresh.phase {
                Phase::BeforeObject => "decode apply_patch arguments object: EOF",
                Phase::BeforeKey => "decode apply_patch input key: EOF",
                Phase::InKey => "decode apply_patch input key: unexpected EOF",
                Phase::BeforeColon | Phase::BeforeValue => "decode apply_patch input value: EOF",
                Phase::InValue => "decode apply_patch input value: unexpected EOF",
                Phase::AfterValue | Phase::Complete => "decode apply_patch arguments closing brace: EOF",
            };
            return Err(self.fail(message));
        }
        let input = fresh.input;
        if self.finished {
            if input != self.input {
                return Err(self.fail("conflicting apply_patch arguments completion"));
            }
            return Ok(String::new());
        }
        if !input.starts_with(&self.input) {
            return Err(self.fail("final apply_patch input conflicts with streamed input"));
        }
        let tail = input[self.input.len()..].to_owned();
        self.input.push_str(&tail);
        self.finished = true;
        self.phase = Phase::Complete;
        self.key_raw.clear();
        self.escape_raw.clear();
        self.utf8_pending.clear();
        self.high_surrogate = 0;
        Ok(tail)
    }

    /// `Input`: the decoded text so far.
    pub(crate) fn input(&self) -> &str {
        &self.input
    }
}

// ---------------------------------------------------------------------------------------
// Call state and events (translator/common/apply_patch_events.go)

/// ApplyPatchCallState: one apply_patch call's identity and input decoder.
#[derive(Clone, Debug, Default)]
pub(crate) struct CallState {
    pub item_id: Vec<u8>,
    pub call_id: Vec<u8>,
    pub name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub output_index: i64,
    pub decoder: InputDecoder,
}

impl CallState {
    pub(crate) fn push_arguments(&mut self, fragment: &[u8]) -> Result<String, String> {
        self.decoder.push(fragment)
    }

    /// `FinishArguments`: the unsent suffix and the complete input.
    pub(crate) fn finish_arguments(&mut self, arguments: &[u8]) -> Result<(String, String), String> {
        let tail = self.decoder.finish(arguments)?;
        Ok((tail, self.decoder.input().to_owned()))
    }

    fn event(&self, template: &[u8], sequence: i64, field: &str, value: &str) -> Vec<u8> {
        let mut payload = template.to_vec();
        gj::set_str(&mut payload, "item_id", &self.item_id);
        gj::set_str(&mut payload, "call_id", &self.call_id);
        gj::set_int(&mut payload, "output_index", self.output_index);
        gj::set_int(&mut payload, "sequence_number", sequence);
        gj::set_str(&mut payload, field, value);
        payload
    }

    /// ApplyPatchInputDelta (unframed).
    pub(crate) fn input_delta(&self, delta: &str, sequence: i64) -> Vec<u8> {
        self.event(
            br#"{"type":"response.custom_tool_call_input.delta","item_id":"","call_id":"","output_index":0,"sequence_number":0,"delta":""}"#,
            sequence,
            "delta",
            delta,
        )
    }

    /// ApplyPatchInputDone (unframed).
    pub(crate) fn input_done(&self, input: &str, sequence: i64) -> Vec<u8> {
        self.event(
            br#"{"type":"response.custom_tool_call_input.done","item_id":"","call_id":"","output_index":0,"sequence_number":0,"input":""}"#,
            sequence,
            "input",
            input,
        )
    }
}

/// The message Go's executors return (HTTP 502) when apply_patch arguments from the
/// upstream are invalid (helps.ApplyPatchUpstreamErrorMessage).
pub const UPSTREAM_ERROR_MESSAGE: &str = "Invalid apply_patch tool arguments received from upstream.";

/// ApplyPatchFailure: a terminal `response.failed` payload (unframed).
pub(crate) fn failure(response_id: &[u8], sequence: i64) -> Vec<u8> {
    let mut payload = br#"{"type":"response.failed","sequence_number":0,"response":{"id":"","object":"response","status":"failed","error":{"type":"server_error","code":"invalid_tool_arguments","message":"Invalid apply_patch tool arguments received from upstream.","param":null}}}"#.to_vec();
    gj::set_str(&mut payload, "response.id", response_id);
    gj::set_int(&mut payload, "sequence_number", sequence);
    payload
}

/// applypatch.UnwrapInput: the `input` string of arguments that are exactly
/// `{"input": "<string>"}` (white space allowed). Decoding follows encoding/json, so
/// invalid UTF-8 and lone surrogates become U+FFFD.
pub(crate) fn unwrap_input(arguments: &[u8]) -> Option<String> {
    if !gj::valid(arguments) {
        return None;
    }
    let root = gj::parse(arguments);
    if !root.is_object() {
        return None;
    }
    let mut pairs = vec![];
    root.each(|key, value| {
        pairs.push((key, value));
        true
    });
    match pairs.as_slice() {
        [(key, value)] if value.kind == cpa_common::json::Kind::String => {
            (gj::go_unquote(&key.raw)? == "input").then(|| gj::go_unquote(&value.raw))?
        }
        _ => None,
    }
}

/// applypatch.WrapInput: `{"input":"..."}` as json.Marshal writes it.
pub(crate) fn wrap_input(input: &[u8]) -> Vec<u8> {
    let mut out = br#"{"input":"#.to_vec();
    gj::marshal_str(&mut out, input, true);
    out.push(b'}');
    out
}

/// applypatch.EscapeInputFragment: the JSON string encoding without its quotes.
pub(crate) fn escape_input_fragment(fragment: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    gj::marshal_str(&mut out, fragment, true);
    out[1..out.len() - 1].to_vec()
}

#[cfg(test)]
mod tests {
    //! Cases from Go's translator/common/apply_patch_input_test.go.
    use super::*;

    fn decode(fragments: &[&[u8]], arguments: &[u8]) -> Result<String, String> {
        let mut decoder = InputDecoder::default();
        let mut output = String::new();
        for fragment in fragments {
            output.push_str(&decoder.push(fragment)?);
            assert_eq!(decoder.input(), output, "emitted text tracks the decoded input");
        }
        output.push_str(&decoder.finish(arguments)?);
        assert_eq!(decoder.input(), output);
        Ok(output)
    }

    #[test]
    fn decodes_at_every_split() {
        let cases: [(&[u8], &str); 8] = [
            (br#"{"input":""}"#, ""),
            (
                b" \t\r\n{ \n\"input\" \t: \"  line  \\n\\t next\\r\\n\" \r}\n\t",
                "  line  \n\t next\r\n",
            ),
            (
                br#"{"input":"\"\\\/\b\f\n\r\t\u0000\u0041\u4e2d\u6587"}"#,
                "\"\\/\u{8}\u{c}\n\r\t\0A中文",
            ),
            (br#"{"in\u0070ut":"patch"}"#, "patch"),
            (br#"{"input":"before\uD83D\uDE00after"}"#, "before😀after"),
            (
                br#"{"input":"\ud800\udc00\uDBFF\uDFFF\uD7FF\uE000"}"#,
                "\u{10000}\u{10ffff}\u{d7ff}\u{e000}",
            ),
            ("{\"input\":\"¢中文😀\u{fffd}\"}".as_bytes(), "¢中文😀\u{fffd}"),
            (
                &wrap_input("*** Begin Patch\n+中文 \\\"\n*** End Patch\n".as_bytes()),
                "*** Begin Patch\n+中文 \\\"\n*** End Patch\n",
            ),
        ];
        for (arguments, want) in cases {
            for split in 0..=arguments.len() {
                let (a, b) = arguments.split_at(split);
                assert_eq!(decode(&[a, b], arguments).as_deref(), Ok(want), "split {split}");
                assert_eq!(
                    decode(&[a], arguments).as_deref(),
                    Ok(want),
                    "snapshot completes {split}"
                );
            }
            let bytes: Vec<&[u8]> = arguments.chunks(1).collect();
            assert_eq!(decode(&bytes, arguments).as_deref(), Ok(want));
        }
    }

    #[test]
    fn previews_complete_characters_before_the_closing_json() {
        let mut decoder = InputDecoder::default();
        let steps: [(&[u8], &str); 8] = [
            (b"{\"input\":\"*** Begin Patch\\n+  ", "*** Begin Patch\n+  "),
            (&[0xe4], ""),
            (&[0xb8], ""),
            (&[0xad, b'\\'], "中"),
            (b"uD8", ""),
            (b"3D", ""),
            (b"\\uDE", ""),
            (b"00\\n*** End Patch\\n", "😀\n*** End Patch\n"),
        ];
        for (i, (fragment, want)) in steps.iter().enumerate() {
            assert_eq!(decoder.push(fragment).as_deref(), Ok(*want), "fragment {i}");
        }
        let want = "*** Begin Patch\n+  中😀\n*** End Patch\n";
        assert_eq!(decoder.input(), want);
        assert_eq!(decoder.finish(&wrap_input(want.as_bytes())).as_deref(), Ok(""));
    }

    #[test]
    fn rejects_invalid_arguments_at_every_split() {
        let cases: [&[u8]; 36] = [
            b"",
            b"[]",
            b"{}",
            br#"{"patch":"x"}"#,
            br#"{input:"x"}"#,
            br#"{"in\qput":"x"}"#,
            br#"{"input" "x"}"#,
            br#"{"input":42}"#,
            br#"{"input":null}"#,
            br#"{"input":true}"#,
            br#"{"input":{}}"#,
            br#"{"input":[]}"#,
            br#"{"input":"x","input":"y"}"#,
            br#"{"input":"x","extra":"y"}"#,
            br#"{"input":"x",}"#,
            br#"{"input":"x"}{}"#,
            b"\x0b{\"input\":\"x\"}",
            br#"{"input":"\q"}"#,
            br#"{"input":"\u12G4"}"#,
            br#"{"input":"\u123"}"#,
            br#"{"input":"x\"#,
            b"{\"input\":\"x\ny\"}",
            b"{\"input\":\"x\x00y\"}",
            br#"{"input":"\uDE00"}"#,
            br#"{"input":"\uD83D"}"#,
            br#"{"input":"\uD83Dx"}"#,
            br#"{"input":"\uD83D\n"}"#,
            br#"{"input":"\uD83D\u0041"}"#,
            br#"{"input":"\uD83D\uD83D"}"#,
            b"{\"input\":\"\xe4A\"}",
            b"{\"input\":\"\xe4\xb8\"}",
            b"{\"input\":\"\xc0\xaf\"}",
            b"{\"input\":\"\xed\xa0\x80\"}",
            b"{\"input\":\"\xf4\x90\x80\x80\"}",
            br#"{"input":"x"#,
            br#"{"input":"x""#,
        ];
        for arguments in cases {
            for split in 0..=arguments.len() {
                let mut decoder = InputDecoder::default();
                for fragment in [&arguments[..split], &arguments[split..]] {
                    if decoder.push(fragment).is_err() {
                        break;
                    }
                }
                assert!(
                    decoder.finish(arguments).is_err(),
                    "split {split} accepted {arguments:?}"
                );
            }
            assert!(
                InputDecoder::default().finish(arguments).is_err(),
                "finish accepted {arguments:?}"
            );
        }
    }

    #[test]
    fn unwrap_input_follows_encoding_json() {
        assert_eq!(unwrap_input(br#" {"input" : "a\u00e9"} "#).as_deref(), Some("aé"));
        assert_eq!(unwrap_input(b"{\"input\":\"\xff\"}").as_deref(), Some("\u{fffd}"));
        assert_eq!(unwrap_input(br#"{"input":"\ud800"}"#).as_deref(), Some("\u{fffd}"));
        assert_eq!(unwrap_input(br#"{"input":"x","input":"y"}"#), None);
        assert_eq!(unwrap_input(br#"{"input":5}"#), None);
        assert_eq!(wrap_input(b"<a>&"), br#"{"input":"\u003ca\u003e\u0026"}"#.to_vec());
        assert_eq!(escape_input_fragment(b"q\"\n"), b"q\\\"\\n".to_vec());
    }
}
