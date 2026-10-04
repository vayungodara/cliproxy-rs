//! Devin's Connect-RPC wire (helps/devin_wire.go): Connect envelopes, the
//! `GetChatMessageRequest` protobuf, response frame decoding, EOS trailers and the
//! UTF-8 split buffer, plus the system-prompt and tool-description sanitizers the
//! request builder applies (helps/cloak_obfuscate.go, translator/common/devin_tools.go).
//!
//! Protobuf is hand-encoded with Go `protowire` semantics: field order, wire types,
//! varint limits and the parse errors that decide whether a frame is skipped.
//! Text fields are bytes, as Go strings are: upstream bytes pass through unchanged.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::ExecError;
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use sha2::{Digest, Sha256};

pub(crate) const FLAG_COMPRESSED: u8 = 0x01;
pub(crate) const FLAG_END_STREAM: u8 = 0x02;
/// `DevinDefaultBaseURL`.
pub const DEFAULT_BASE_URL: &str = "https://server.codeium.com";
/// `DevinChatPath`.
pub const CHAT_PATH: &str = "/exa.api_server_pb.ApiServerService/GetChatMessage";
const CLIENT_NAME: &str = "chisel";
const CLIENT_VERSION: &str = "3000.10.21";
const FINGERPRINT_HEX_LEN: usize = 732;
/// `DevinDefaultMaxTokens`.
pub(crate) const DEFAULT_MAX_TOKENS: i64 = 128_000;
const MAX_FRAME: u32 = 16 * 1024 * 1024;
const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;
const MAX_SESSION_TURN_COUNTERS: usize = 5000;
const ZWSP: &str = "\u{200B}";

/// Go `protowire` encoding and decoding.
pub(crate) mod pb {
    /// `protowire.ParseError` messages.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Error {
        Truncated,
        FieldNumber,
        Overflow,
        Reserved,
        EndGroup,
        RecursionDepth,
    }

    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(match self {
                Error::Truncated => "unexpected EOF",
                Error::FieldNumber => "invalid field number",
                Error::Overflow => "variable length integer overflow",
                Error::Reserved => "cannot parse reserved wire type",
                Error::EndGroup => "mismatching end group marker",
                Error::RecursionDepth => "exceeded maximum recursion depth",
            })
        }
    }

    pub(crate) const VARINT: u8 = 0;
    pub(crate) const FIXED64: u8 = 1;
    pub(crate) const BYTES: u8 = 2;
    pub(crate) const START_GROUP: u8 = 3;
    pub(crate) const END_GROUP: u8 = 4;
    pub(crate) const FIXED32: u8 = 5;

    pub(crate) fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    pub(crate) fn tag(out: &mut Vec<u8>, num: u32, wire: u8) {
        varint(out, (u64::from(num) << 3) | u64::from(wire));
    }

    pub(crate) fn bytes(out: &mut Vec<u8>, num: u32, value: &[u8]) {
        tag(out, num, BYTES);
        varint(out, value.len() as u64);
        out.extend_from_slice(value);
    }

    pub(crate) fn uint(out: &mut Vec<u8>, num: u32, value: u64) {
        tag(out, num, VARINT);
        varint(out, value);
    }

    pub(crate) fn fixed64(out: &mut Vec<u8>, num: u32, value: u64) {
        tag(out, num, FIXED64);
        out.extend_from_slice(&value.to_le_bytes());
    }

    /// `ConsumeVarint`.
    pub(crate) fn consume_varint(b: &[u8]) -> Result<(u64, usize), Error> {
        let mut v = 0u64;
        for i in 0..10 {
            let Some(&byte) = b.get(i) else {
                return Err(Error::Truncated);
            };
            if i == 9 {
                if byte >= 2 {
                    return Err(Error::Overflow);
                }
                return Ok((v | (u64::from(byte) << 63), 10));
            }
            v |= u64::from(byte & 0x7f) << (7 * i);
            if byte < 0x80 {
                return Ok((v, i + 1));
            }
        }
        unreachable!("the tenth byte always returns")
    }

    /// `ConsumeTag`: field number, wire type, length.
    pub(crate) fn consume_tag(b: &[u8]) -> Result<(i64, u8, usize), Error> {
        let (v, n) = consume_varint(b)?;
        if v >> 3 > i32::MAX as u64 {
            return Err(Error::FieldNumber);
        }
        let num = (v >> 3) as i64;
        if num < 1 {
            return Err(Error::FieldNumber);
        }
        Ok((num, (v & 7) as u8, n))
    }

    /// `ConsumeBytes`.
    pub(crate) fn consume_bytes(b: &[u8]) -> Result<(&[u8], usize), Error> {
        let (m, n) = consume_varint(b)?;
        let rest = &b[n..];
        if m > rest.len() as u64 {
            return Err(Error::Truncated);
        }
        Ok((&rest[..m as usize], n + m as usize))
    }

    pub(crate) fn consume_fixed32(b: &[u8]) -> Result<(u32, usize), Error> {
        let raw: [u8; 4] = b.get(..4).ok_or(Error::Truncated)?.try_into().expect("4 bytes");
        Ok((u32::from_le_bytes(raw), 4))
    }

    pub(crate) fn consume_fixed64(b: &[u8]) -> Result<(u64, usize), Error> {
        let raw: [u8; 8] = b.get(..8).ok_or(Error::Truncated)?.try_into().expect("8 bytes");
        Ok((u64::from_le_bytes(raw), 8))
    }

    fn consume_scalar(wire: u8, b: &[u8]) -> Result<usize, Error> {
        match wire {
            VARINT => consume_varint(b).map(|(_, n)| n),
            FIXED32 => consume_fixed32(b).map(|(_, n)| n),
            FIXED64 => consume_fixed64(b).map(|(_, n)| n),
            BYTES => consume_bytes(b).map(|(_, n)| n),
            END_GROUP => Err(Error::EndGroup),
            _ => Err(Error::Reserved),
        }
    }

    /// `ConsumeFieldValue`, groups included (iteratively, Go's recursion limit).
    pub(crate) fn consume_field_value(num: i64, wire: u8, b: &[u8]) -> Result<usize, Error> {
        if wire != START_GROUP {
            return consume_scalar(wire, b);
        }
        let mut open = vec![num];
        let mut pos = 0;
        loop {
            let (inner, inner_wire, n) = consume_tag(&b[pos..])?;
            pos += n;
            match inner_wire {
                END_GROUP => {
                    if open.last() != Some(&inner) {
                        return Err(Error::EndGroup);
                    }
                    open.pop();
                    if open.is_empty() {
                        return Ok(pos);
                    }
                }
                START_GROUP => {
                    if open.len() > 10_000 {
                        return Err(Error::RecursionDepth);
                    }
                    open.push(inner);
                }
                _ => pos += consume_scalar(inner_wire, &b[pos..])?,
            }
        }
    }
}

/// `WrapConnectEnvelope`.
pub(crate) fn wrap_envelope(proto: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + proto.len());
    out.push(0);
    out.extend_from_slice(&(proto.len() as u32).to_be_bytes());
    out.extend_from_slice(proto);
    out
}

/// A failed `ReadConnectFrame`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FrameError {
    /// `io.EOF`: the body ended exactly where a frame header or payload should start.
    Eof,
    /// Anything else, with Go's message where it is deterministic.
    Failed(String),
}

/// `ReadConnectFrame` over the upstream body.
pub(crate) struct FrameReader {
    body: BoxStream<'static, Result<Bytes, ExecError>>,
    buf: BytesMut,
    done: bool,
}

impl FrameReader {
    pub(crate) fn new(body: BoxStream<'static, Result<Bytes, ExecError>>) -> Self {
        Self {
            body,
            buf: BytesMut::new(),
            done: false,
        }
    }

    /// Buffers until `n` bytes are available; false when the body ended first.
    async fn fill(&mut self, n: usize) -> Result<bool, FrameError> {
        while self.buf.len() < n {
            if self.done {
                return Ok(false);
            }
            match self.body.next().await {
                Some(Ok(chunk)) => self.buf.extend_from_slice(&chunk),
                Some(Err(e)) => {
                    self.done = true;
                    return Err(FrameError::Failed(String::from_utf8_lossy(&e.body).into_owned()));
                }
                None => self.done = true,
            }
        }
        Ok(true)
    }

    /// One frame: its flag and (decompressed) payload.
    pub(crate) async fn next(&mut self) -> Result<(u8, Vec<u8>), FrameError> {
        if !self.fill(5).await? {
            // io.ReadFull: nothing read is EOF, a partial header is unexpected EOF.
            return Err(if self.buf.is_empty() {
                FrameError::Eof
            } else {
                FrameError::Failed("unexpected EOF".into())
            });
        }
        let header = self.buf.split_to(5);
        let flag = header[0];
        if !matches!(flag, 0x00 | FLAG_COMPRESSED | FLAG_END_STREAM | 0x03) {
            return Err(FrameError::Failed(format!("invalid connect frame flag: 0x{flag:02x}")));
        }
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
        if length > MAX_FRAME {
            return Err(FrameError::Failed(format!(
                "connect frame length {length} exceeds maximum limit ({MAX_FRAME})"
            )));
        }
        let length = length as usize;
        if !self.fill(length).await? {
            return Err(if self.buf.is_empty() && length > 0 {
                FrameError::Eof
            } else {
                FrameError::Failed("unexpected EOF".into())
            });
        }
        let payload = self.buf.split_to(length).to_vec();
        if flag & FLAG_COMPRESSED == 0 {
            return Ok((flag, payload));
        }
        Ok((flag, gunzip(&payload)?))
    }
}

/// The gzip branch of `ReadConnectFrame` (multistream, 64 MiB cap).
// ponytail: decoder error texts are flate2's, not Go's compress/gzip wording.
fn gunzip(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    use std::io::Read;
    let mut decoder = flate2::read::MultiGzDecoder::new(payload);
    let mut out = Vec::new();
    let mut limited = (&mut decoder).take(MAX_DECOMPRESSED as u64 + 1);
    if let Err(e) = limited.read_to_end(&mut out) {
        return Err(FrameError::Failed(
            if out.is_empty() && e.kind() == std::io::ErrorKind::InvalidInput {
                format!("decompress gzip connect frame: {e}")
            } else {
                format!("read decompressed connect frame: {e}")
            },
        ));
    }
    if out.len() > MAX_DECOMPRESSED {
        return Err(FrameError::Failed(format!(
            "decompressed frame size exceeds maximum limit ({MAX_DECOMPRESSED})"
        )));
    }
    Ok(out)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `GenerateDevinDeviceFingerprint` (also `devin.GenerateDeviceFingerprint`): 732 hex
/// characters, random without a seed, else SHA-256 blocks of `<seed>-<counter>`.
pub(crate) fn device_fingerprint(seed: &str) -> String {
    let mut seed = seed.to_owned();
    if seed.is_empty() {
        let mut b = [0u8; FINGERPRINT_HEX_LEN / 2];
        if getrandom::fill(&mut b).is_ok() {
            return hex(&b);
        }
        seed = uuid::Uuid::new_v4().to_string();
    }
    let mut out = String::with_capacity(FINGERPRINT_HEX_LEN + 64);
    let mut counter = 0;
    while out.len() < FINGERPRINT_HEX_LEN {
        out.push_str(&hex(&Sha256::digest(format!("{seed}-{counter}").as_bytes())));
        counter += 1;
    }
    out.truncate(FINGERPRINT_HEX_LEN);
    out
}

/// `GenerateDevinSentryTrace`: `<32 hex trace>-<16 hex span>-1`.
pub(crate) fn sentry_trace() -> String {
    let mut b = [0u8; 24];
    if getrandom::fill(&mut b).is_err() {
        let a = uuid::Uuid::new_v4().simple().to_string();
        let c = uuid::Uuid::new_v4().simple().to_string();
        return format!("{a}-{}-1", &c[..16]);
    }
    format!("{}-{}-1", hex(&b[..16]), hex(&b[16..]))
}

/// The process-wide per-session request counter (`sessionTurnLRU`, 5000 sessions).
struct TurnCounters {
    counters: HashMap<String, (u64, u64)>,
    tick: u64,
}

static TURNS: Mutex<Option<TurnCounters>> = Mutex::new(None);

/// `NextDevinSessionTurnIndex`: 0 for a session's first request, then 1, 2, ...
pub(crate) fn next_turn_index(session_id: &str) -> u64 {
    let id = session_id.trim();
    if id.is_empty() {
        return 0;
    }
    let mut guard = TURNS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let turns = guard.get_or_insert_with(|| TurnCounters {
        counters: HashMap::new(),
        tick: 0,
    });
    turns.tick += 1;
    let tick = turns.tick;
    if let Some((count, used)) = turns.counters.get_mut(id) {
        *used = tick;
        let index = *count;
        *count += 1;
        return index;
    }
    if turns.counters.len() >= MAX_SESSION_TURN_COUNTERS
        && let Some(oldest) = turns
            .counters
            .iter()
            .min_by_key(|(_, (_, used))| *used)
            .map(|(k, _)| k.clone())
    {
        turns.counters.remove(&oldest);
    }
    turns.counters.insert(id.to_owned(), (1, tick));
    0
}

/// An image attached to a prompt (`DevinImage`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Image {
    pub base64: Vec<u8>,
    pub mime: Vec<u8>,
}

/// `DevinToolCall`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolCall {
    pub id: Vec<u8>,
    pub name: Vec<u8>,
    pub arguments: Vec<u8>,
}

/// `DevinPrompt`: one history turn. `source` is 1 user, 2 assistant, 4 tool.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Prompt {
    pub message_id: String,
    pub source: i64,
    pub content: Vec<u8>,
    pub images: Vec<Image>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Vec<u8>,
    pub original_tool_call_id: Vec<u8>,
    pub is_orphaned_tool: bool,
    pub thinking: Vec<u8>,
    pub signature: Vec<u8>,
    pub signature_type: Vec<u8>,
}

/// `DevinTool`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Tool {
    pub name: Vec<u8>,
    pub description: Vec<u8>,
    pub parameters: Vec<u8>,
}

/// Everything `BuildDevinGetChatMessageRequest` encodes.
pub(crate) struct ChatRequest<'a> {
    pub session_token: &'a str,
    pub device_seed: &'a str,
    pub model_uid: &'a str,
    pub system_prompt: &'a [u8],
    pub prompts: &'a [Prompt],
    pub tools: &'a [Tool],
    pub temperature: Option<f64>,
    pub max_tokens: i64,
    pub session_id: &'a str,
    pub cascade_id: &'a str,
    pub matcher: Option<&'a SensitiveWords>,
}

/// `BuildDevinClientMetadataBytes`.
pub(crate) fn client_metadata(session_token: &str, device_seed: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(1024);
    pb::bytes(&mut out, 1, CLIENT_NAME.as_bytes());
    pb::bytes(&mut out, 2, CLIENT_VERSION.as_bytes());
    pb::bytes(&mut out, 3, session_token.as_bytes());
    pb::bytes(&mut out, 4, b"en");
    pb::bytes(&mut out, 5, crate::kimi_http::go_os().as_bytes());
    pb::bytes(&mut out, 7, CLIENT_VERSION.as_bytes());
    pb::bytes(&mut out, 12, CLIENT_NAME.as_bytes());
    pb::bytes(&mut out, 31, device_fingerprint(device_seed).as_bytes());
    out
}

/// `BuildDevinGetChatMessageRequest`. Takes the next turn index of `session_id`.
pub(crate) fn build_chat_request(r: &ChatRequest<'_>) -> Vec<u8> {
    let max_tokens = if r.max_tokens <= 0 {
        DEFAULT_MAX_TOKENS
    } else {
        r.max_tokens
    };
    let session_id = if r.session_id.is_empty() {
        uuid::Uuid::new_v4().to_string()
    } else {
        r.session_id.to_owned()
    };
    let cascade_id = if r.cascade_id.is_empty() {
        session_id.as_str()
    } else {
        r.cascade_id
    };
    let mut out = Vec::with_capacity(4096 + r.system_prompt.len());
    pb::bytes(&mut out, 1, &client_metadata(r.session_token, r.device_seed));
    if !r.system_prompt.is_empty() {
        let sanitized = sanitize_system_prompt(r.system_prompt, r.matcher);
        if !sanitized.is_empty() {
            pb::bytes(&mut out, 2, &sanitized);
        }
    }
    for p in r.prompts {
        let mut m = Vec::with_capacity(256 + p.content.len());
        let message_id = if p.message_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            p.message_id.clone()
        };
        pb::bytes(&mut m, 1, message_id.as_bytes());
        pb::uint(&mut m, 2, if p.source <= 0 { 1 } else { p.source as u64 });
        pb::bytes(&mut m, 3, &p.content);
        for tc in &p.tool_calls {
            let mut t = Vec::new();
            for (num, value) in [(1, &tc.id), (2, &tc.name), (3, &tc.arguments)] {
                if !value.is_empty() {
                    pb::bytes(&mut t, num, value);
                }
            }
            pb::bytes(&mut m, 6, &t);
        }
        if !p.tool_call_id.is_empty() {
            pb::bytes(&mut m, 7, &p.tool_call_id);
        }
        for img in &p.images {
            let data = cpa_common::gostr::trim_space(&img.base64);
            if data.is_empty() {
                continue;
            }
            let mut i = Vec::with_capacity(data.len() + 32);
            pb::bytes(&mut i, 1, data);
            let mime = cpa_common::gostr::trim_space(&img.mime);
            pb::bytes(&mut i, 2, if mime.is_empty() { b"image/png" } else { mime });
            pb::bytes(&mut m, 10, &i);
        }
        if !p.thinking.is_empty() {
            pb::bytes(&mut m, 11, &p.thinking);
        }
        if !p.signature.is_empty() {
            pb::bytes(&mut m, 12, &p.signature);
        }
        if !p.signature_type.is_empty() {
            pb::bytes(&mut m, 18, &p.signature_type);
        }
        pb::bytes(&mut out, 3, &m);
    }
    pb::uint(&mut out, 7, 5);
    let mut config = Vec::with_capacity(64);
    pb::uint(&mut config, 1, 1);
    pb::uint(&mut config, 2, max_tokens as u64);
    pb::uint(&mut config, 3, 400);
    pb::fixed64(&mut config, 5, r.temperature.unwrap_or(1.0).to_bits());
    pb::uint(&mut config, 7, 40);
    pb::fixed64(&mut config, 8, f64::from(0.95f32).to_bits());
    pb::bytes(&mut out, 8, &config);
    for tool in r.tools {
        if tool.name.is_empty() || is_codex_app_automation_update(b"", &tool.name) {
            continue;
        }
        let mut t = Vec::new();
        pb::bytes(&mut t, 1, &tool.name);
        let description = tool_description(&tool.name, &tool.description);
        if !description.is_empty() {
            pb::bytes(&mut t, 2, &description);
        }
        if !tool.parameters.is_empty() {
            pb::bytes(&mut t, 3, &tool.parameters);
        }
        pb::bytes(&mut out, 10, &t);
    }
    let turn = next_turn_index(&session_id);
    let mut session = Vec::with_capacity(64);
    pb::bytes(&mut session, 1, session_id.as_bytes());
    if turn > 0 {
        pb::uint(&mut session, 2, turn);
    }
    pb::uint(&mut session, 3, 4);
    // 15.4 = 14 marks a user-turn boundary.
    let n = r.prompts.len();
    if n > 0 && r.prompts[n - 1].source == 1 && (turn == 0 || n < 2 || r.prompts[n - 2].source != 1) {
        pb::uint(&mut session, 4, 14);
    }
    pb::bytes(&mut out, 15, &session);
    pb::bytes(&mut out, 16, cascade_id.as_bytes());
    pb::uint(&mut out, 20, 1);
    pb::bytes(&mut out, 21, r.model_uid.as_bytes());
    out
}

/// The tool description the request carries: Claude Code's `task_id` wording becomes
/// `taskId`, then [`sanitize_tool_description`].
fn tool_description(name: &[u8], description: &[u8]) -> Vec<u8> {
    const TASK_ID: &[u8] = b"Takes a task_id parameter identifying the task";
    const TASK_ID_CAMEL: &[u8] = b"Takes a taskId parameter identifying the task";
    let desc = replace_all(description, TASK_ID, TASK_ID_CAMEL);
    sanitize_tool_description(name, &desc)
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

pub(crate) fn contains(hay: &[u8], needle: &[u8]) -> bool {
    find(hay, needle).is_some()
}

pub(crate) fn replace_all(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut rest = hay;
    while let Some(i) = find(rest, from) {
        out.extend_from_slice(&rest[..i]);
        out.extend_from_slice(to);
        rest = &rest[i + from.len()..];
    }
    out.extend_from_slice(rest);
    out
}

fn eq_fold(a: &[u8], b: &str) -> bool {
    use cpa_common::gostr::GoStr;
    String::from_utf8_lossy(a).go_eq_fold(b)
}

/// `IsDevinCodexAppAutomationUpdate`.
pub(crate) fn is_codex_app_automation_update(namespace: &[u8], tool: &[u8]) -> bool {
    use cpa_common::gostr::trim_space;
    let (namespace, tool) = (trim_space(namespace), trim_space(tool));
    (eq_fold(namespace, "mcp__codex_app") && eq_fold(tool, "automation_update"))
        || eq_fold(tool, "mcp__codex_app__automation_update")
}

/// `SanitizeDevinToolDescription`: Codex's `exec_command` and `write_stdin` wording
/// gets a one-letter change.
pub(crate) fn sanitize_tool_description(name: &[u8], description: &[u8]) -> Vec<u8> {
    use std::sync::LazyLock;
    const EXEC_TARGET: &[u8] = b"returning output or a session ID for ongoing interaction";
    const EXEC_OBFUSCATED: &[u8] = b"returning output or an session ID for ongoing interaction";
    const STDIN_TARGET: &[u8] = b"Writes characters to an existing unified exec session and returns recent output.";
    const STDIN_OBFUSCATED: &[u8] = b"Writes characters to a existing unified exec session and returns recent output.";
    static EXEC: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        regex::bytes::Regex::new(r"(?i)returning output or a session ID for ongoing interaction").expect("valid")
    });
    static STDIN: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        regex::bytes::Regex::new(
            r"(?i)Writes characters to an existing unified exec session and returns recent output(\.?)",
        )
        .expect("valid")
    });
    if description.is_empty() {
        return Vec::new();
    }
    let lower = String::from_utf8_lossy(cpa_common::gostr::trim_space(name)).to_lowercase();
    let mut desc = description.to_vec();
    if lower == "exec_command" || lower.ends_with("__exec_command") {
        desc = if contains(&desc, EXEC_OBFUSCATED) {
            desc
        } else if contains(&desc, EXEC_TARGET) {
            replace_all(&desc, EXEC_TARGET, EXEC_OBFUSCATED)
        } else {
            EXEC.replace_all(&desc, EXEC_OBFUSCATED).into_owned()
        };
    }
    if lower == "write_stdin" || lower.ends_with("__write_stdin") {
        desc = if contains(&desc, STDIN_OBFUSCATED) {
            desc
        } else if contains(&desc, STDIN_TARGET) {
            replace_all(&desc, STDIN_TARGET, STDIN_OBFUSCATED)
        } else {
            STDIN
                .replace_all(
                    &desc,
                    &b"Writes characters to a existing unified exec session and returns recent output${1}"[..],
                )
                .into_owned()
        };
    }
    desc
}

/// `BuildSensitiveWordMatcher` / `SensitiveWordMatcher` (`devin.sensitive-words`).
// ponytail: same matcher as claude::cloak::SensitiveWords (Claude thread); this one
// works on bytes and adds `Matches`. Share one if a third provider needs it.
pub(crate) struct SensitiveWords(regex::bytes::Regex);

impl SensitiveWords {
    pub(crate) fn new(words: &[String]) -> Option<Self> {
        let mut valid: Vec<&str> = words
            .iter()
            .map(|w| w.trim())
            .filter(|w| w.chars().count() >= 2 && !w.contains(ZWSP))
            .collect();
        if valid.is_empty() {
            return None;
        }
        // Longest first; equal-length alternatives cannot both match at one position
        // unless they are equal ignoring case, so Go's unstable sort cannot matter.
        valid.sort_by_key(|w| std::cmp::Reverse(w.len()));
        let pattern = format!(
            "(?i){}",
            valid.iter().map(|w| regex::escape(w)).collect::<Vec<_>>().join("|")
        );
        regex::bytes::Regex::new(&pattern).ok().map(Self)
    }

    pub(crate) fn matches(&self, text: &[u8]) -> bool {
        self.0.is_match(text)
    }

    /// `ObfuscateText`: a zero-width space after the first rune of every match.
    pub(crate) fn obfuscate(&self, text: &[u8]) -> Vec<u8> {
        self.0
            .replace_all(text, |c: &regex::bytes::Captures<'_>| {
                let word = &c[0];
                if contains(word, ZWSP.as_bytes()) {
                    return word.to_vec();
                }
                match cpa_common::json::decode_rune(word) {
                    (Some(r), size) if r != char::REPLACEMENT_CHARACTER && size < word.len() => {
                        let mut out = word[..size].to_vec();
                        out.extend_from_slice(ZWSP.as_bytes());
                        out.extend_from_slice(&word[size..]);
                        out
                    }
                    _ => word.to_vec(),
                }
            })
            .into_owned()
    }
}

/// `SanitizeDevinSystemPrompt`: drops Claude Code and Codex identity lines and lines with
/// sensitive words, then obfuscates what remains.
pub(crate) fn sanitize_system_prompt(prompt: &[u8], matcher: Option<&SensitiveWords>) -> Vec<u8> {
    use cpa_common::gostr::trim_space;
    if prompt.is_empty() {
        return Vec::new();
    }
    let normalized = replace_all(prompt, b"\r\n", b"\n");
    let mut kept: Vec<&[u8]> = Vec::new();
    for line in normalized.split(|b| *b == b'\n') {
        let trimmed = trim_space(line);
        let drop = trimmed.starts_with(b"x-anthropic-billing-header:")
            || trimmed.starts_with(b"You are Claude Code")
            || contains(trimmed, b"authorized security testing")
            || contains(trimmed, b"destructive techniques, DoS attacks")
            || contains(trimmed, b"Claude Code is available as a CLI")
            || contains(trimmed, b"Fast mode for Claude Code")
            || contains(trimmed, b"Codex refers to the open-source agentic coding interface")
            || contains(
                trimmed,
                "- Don\u{2019}t output ANSI escape codes directly \u{2014} the CLI renderer applies them.".as_bytes(),
            )
            || matcher.is_some_and(|m| m.matches(trimmed));
        if !drop {
            kept.push(line);
        }
    }
    let joined = kept.join(&b'\n');
    let res = trim_space(&joined).to_vec();
    match matcher {
        Some(m) if !res.is_empty() => m.obfuscate(&res),
        _ => res,
    }
}

/// `DevinToolCallDelta`: one streamed tool-call chunk (response field 6).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolCallDelta {
    pub id: Vec<u8>,
    pub name: Vec<u8>,
    pub arguments: Vec<u8>,
    pub invalid_json: Vec<u8>,
    pub invalid_json_error: Vec<u8>,
    pub is_custom: bool,
}

/// `DevinUsage` (response field 7).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_write_tokens: i64,
    pub status_code: u64,
    pub request_id: Vec<u8>,
    pub model_name: Vec<u8>,
    pub headers: BTreeMap<Vec<u8>, Vec<u8>>,
}

/// `DevinFrameResult`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Frame {
    pub output_id: Vec<u8>,
    pub timestamp: u64,
    pub content: Vec<u8>,
    pub delta_tokens: u64,
    pub stop_reason: u64,
    pub tool_calls: Vec<ToolCallDelta>,
    pub thinking: Vec<u8>,
    pub signature: Vec<u8>,
    pub signature_type: Vec<u8>,
    pub latency: f64,
    pub message_id: Vec<u8>,
    pub usage: Option<Usage>,
    pub dimension_groups: Vec<Vec<u8>>,
    pub unknown_fields: Vec<i64>,
}

/// `ParseDevinFrame`. On error the partly filled result is returned with it (Go's
/// callers skip the frame).
pub(crate) fn parse_frame(payload: &[u8]) -> (Frame, Option<String>) {
    let mut res = Frame::default();
    let error = parse_frame_into(payload, &mut res).err();
    (res, error)
}

fn parse_frame_into(payload: &[u8], res: &mut Frame) -> Result<(), String> {
    let mut pos = 0;
    let at = |what: &str, pos: usize, e: pb::Error| format!("consume {what} error at offset {pos}: {e}");
    let mut text: Vec<u8> = Vec::new();
    let mut thinking: Vec<u8> = Vec::new();
    let finish = |res: &mut Frame, text: Vec<u8>, thinking: Vec<u8>| {
        res.content = text;
        res.thinking = thinking;
    };
    while pos < payload.len() {
        let (num, wire, n) = match pb::consume_tag(&payload[pos..]) {
            Ok(t) => t,
            Err(e) => {
                finish(res, text, thinking);
                return Err(at("tag", pos, e));
            }
        };
        pos += n;
        match wire {
            pb::VARINT => {
                let (v, n) = match pb::consume_varint(&payload[pos..]) {
                    Ok(v) => v,
                    Err(e) => {
                        finish(res, text, thinking);
                        return Err(at("varint", pos, e));
                    }
                };
                pos += n;
                match num {
                    2 => res.timestamp = v,
                    4 => res.delta_tokens = v,
                    5 => res.stop_reason = v,
                    _ => {}
                }
            }
            pb::FIXED64 => {
                let (v, n) = match pb::consume_fixed64(&payload[pos..]) {
                    Ok(v) => v,
                    Err(e) => {
                        finish(res, text, thinking);
                        return Err(at("fixed64", pos, e));
                    }
                };
                pos += n;
                if num == 12 {
                    res.latency = f64::from_bits(v);
                }
            }
            pb::FIXED32 => match pb::consume_fixed32(&payload[pos..]) {
                Ok((_, n)) => pos += n,
                Err(e) => {
                    finish(res, text, thinking);
                    return Err(at("fixed32", pos, e));
                }
            },
            pb::BYTES => {
                let (val, n) = match pb::consume_bytes(&payload[pos..]) {
                    Ok(v) => v,
                    Err(e) => {
                        finish(res, text, thinking);
                        return Err(at("bytes", pos, e));
                    }
                };
                pos += n;
                match num {
                    1 => res.output_id = val.to_vec(),
                    2 => res.timestamp = parse_timestamp(val),
                    3 => text.extend_from_slice(val),
                    6 => {
                        if let Ok(tc) = parse_tool_call_delta(val) {
                            res.tool_calls.push(tc);
                        }
                    }
                    7 => res.usage = Some(parse_usage(val)),
                    9 => thinking.extend_from_slice(val),
                    10 => res.signature.extend_from_slice(val),
                    17 => res.message_id = val.to_vec(),
                    21 => res.signature_type = val.to_vec(),
                    28 => res.dimension_groups.push(val.to_vec()),
                    _ => res.unknown_fields.push(num),
                }
            }
            _ => {
                finish(res, text, thinking);
                return Err(format!("unsupported wire type {wire} at offset {pos}"));
            }
        }
    }
    finish(res, text, thinking);
    Ok(())
}

fn parse_tool_call_delta(data: &[u8]) -> Result<ToolCallDelta, pb::Error> {
    let mut tc = ToolCallDelta::default();
    let mut pos = 0;
    while pos < data.len() {
        let (num, wire, n) = pb::consume_tag(&data[pos..])?;
        pos += n;
        match wire {
            pb::VARINT => {
                let (v, n) = pb::consume_varint(&data[pos..])?;
                pos += n;
                if num == 6 {
                    tc.is_custom = v != 0;
                }
            }
            pb::BYTES => {
                let (val, n) = pb::consume_bytes(&data[pos..])?;
                pos += n;
                let val = val.to_vec();
                match num {
                    1 => tc.id = val,
                    2 => tc.name = val,
                    3 => tc.arguments = val,
                    4 => tc.invalid_json = val,
                    5 => tc.invalid_json_error = val,
                    _ => {}
                }
            }
            _ => pos += pb::consume_field_value(num, wire, &data[pos..])?,
        }
    }
    Ok(tc)
}

/// `parseDevinTimestamp`: subfield 1 (seconds) of a Timestamp message.
fn parse_timestamp(data: &[u8]) -> u64 {
    let mut pos = 0;
    let mut secs = 0;
    while pos < data.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if wire != pb::VARINT {
            break;
        }
        let Ok((v, n)) = pb::consume_varint(&data[pos..]) else {
            break;
        };
        pos += n;
        if num == 1 {
            secs = v;
        }
    }
    secs
}

/// `parseDevinHeaderField`: a (name, value) submessage; empty on a parse error.
fn parse_header_field(data: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let (mut key, mut val) = (Vec::new(), Vec::new());
    let mut pos = 0;
    while pos < data.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        if wire == pb::BYTES {
            let Ok((b, n)) = pb::consume_bytes(&data[pos..]) else {
                return (key, val);
            };
            pos += n;
            match num {
                1 => key = b.to_vec(),
                2 => val = b.to_vec(),
                _ => {}
            }
        } else {
            let Ok(n) = pb::consume_field_value(num, wire, &data[pos..]) else {
                return (key, val);
            };
            pos += n;
        }
    }
    (key, val)
}

fn printable_ascii(b: &[u8]) -> bool {
    b.iter().all(|c| (32..=126).contains(c))
}

/// `parseDevinUsageField`.
fn parse_usage(data: &[u8]) -> Usage {
    let mut u = Usage::default();
    let mut pos = 0;
    while pos < data.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&data[pos..]) else {
            break;
        };
        pos += n;
        match wire {
            pb::VARINT => {
                let Ok((v, n)) = pb::consume_varint(&data[pos..]) else {
                    return u;
                };
                pos += n;
                match num {
                    2 => u.prompt_tokens = u.prompt_tokens.wrapping_add(v as i64),
                    3 => u.completion_tokens = v as i64,
                    4 => u.cache_write_tokens = u.cache_write_tokens.wrapping_add(v as i64),
                    5 => u.cached_tokens = v as i64,
                    6 => u.status_code = v,
                    _ => {}
                }
            }
            pb::BYTES => {
                let Ok((val, n)) = pb::consume_bytes(&data[pos..]) else {
                    return u;
                };
                pos += n;
                match num {
                    8 => {
                        let (k, v) = parse_header_field(val);
                        if !k.is_empty() {
                            let is_request_id =
                                k.eq_ignore_ascii_case(b"x-request-id") || k.eq_ignore_ascii_case(b"request-id");
                            if is_request_id && !v.is_empty() {
                                u.request_id.clone_from(&v);
                            }
                            u.headers.insert(k, v);
                        } else if !val.is_empty() && printable_ascii(val) && u.request_id.is_empty() {
                            u.request_id = val.to_vec();
                        }
                    }
                    9 => u.model_name = val.to_vec(),
                    _ => {}
                }
            }
            pb::FIXED64 => match pb::consume_fixed64(&data[pos..]) {
                Ok((_, n)) => pos += n,
                Err(_) => return u,
            },
            pb::FIXED32 => match pb::consume_fixed32(&data[pos..]) {
                Ok((_, n)) => pos += n,
                Err(_) => return u,
            },
            _ => match pb::consume_field_value(num, wire, &data[pos..]) {
                Ok(n) => pos += n,
                Err(_) => return u,
            },
        }
    }
    u
}

/// Go's `int64(float32)` on amd64: truncation, and the integer indefinite value for
/// NaN and out-of-range values.
fn go_int64(v: f32) -> i64 {
    let v = f64::from(v);
    if v.is_nan() || v >= 9_223_372_036_854_775_808.0 || v < -9_223_372_036_854_775_808.0 {
        return i64::MIN;
    }
    v as i64
}

/// Iterates the length-delimited fields of `data` (non-bytes fields skipped); stops at
/// the first parse error.
fn bytes_fields<'a>(data: &'a [u8], mut f: impl FnMut(i64, &'a [u8])) {
    let mut pos = 0;
    while pos < data.len() {
        let Ok((num, wire, n)) = pb::consume_tag(&data[pos..]) else {
            return;
        };
        pos += n;
        if wire != pb::BYTES {
            let Ok(n) = pb::consume_field_value(num, wire, &data[pos..]) else {
                return;
            };
            pos += n;
            continue;
        }
        let Ok((b, n)) = pb::consume_bytes(&data[pos..]) else {
            return;
        };
        pos += n;
        f(num, b);
    }
}

/// `ParseDevinResponseDimensionGroups`: the "Token Usage" group's input, output and
/// cached-input counts.
pub(crate) fn parse_dimension_groups(groups: &[Vec<u8>]) -> Option<(i64, i64, i64)> {
    let (mut prompt, mut completion, mut cached, mut found) = (0, 0, 0, false);
    for group in groups {
        if group.is_empty() {
            continue;
        }
        let mut g: &[u8] = group;
        if let Ok((28, pb::BYTES, n)) = pb::consume_tag(g)
            && let Ok((inner, _)) = pb::consume_bytes(&g[n..])
        {
            g = inner;
        }
        let mut title: &[u8] = b"";
        let mut metrics: Vec<(Vec<u8>, f32)> = Vec::new();
        bytes_fields(g, |num, b| match num {
            1 => title = b,
            2 => {
                let (mut key, mut value) = (Vec::new(), 0f32);
                bytes_fields(b, |num, mb| match num {
                    5 => key = mb.to_vec(),
                    4 => {
                        let mut pos = 0;
                        while pos < mb.len() {
                            let Ok((dnum, dwire, n)) = pb::consume_tag(&mb[pos..]) else {
                                break;
                            };
                            pos += n;
                            if dwire == pb::FIXED32 {
                                let Ok((v, n)) = pb::consume_fixed32(&mb[pos..]) else {
                                    break;
                                };
                                pos += n;
                                if dnum == 2 {
                                    value = f32::from_bits(v);
                                }
                            } else {
                                let Ok(n) = pb::consume_field_value(dnum, dwire, &mb[pos..]) else {
                                    break;
                                };
                                pos += n;
                            }
                        }
                    }
                    _ => {}
                });
                if !key.is_empty() {
                    metrics.push((key, value));
                }
            }
            _ => {}
        });
        if String::from_utf8_lossy(title).to_lowercase() == "token usage" {
            for (key, value) in &metrics {
                match key.as_slice() {
                    b"input_tokens" => {
                        prompt = go_int64(*value);
                        found = true;
                    }
                    b"output_tokens" => {
                        completion = go_int64(*value);
                        found = true;
                    }
                    b"cached_input_tokens" => {
                        cached = go_int64(*value);
                        found = true;
                    }
                    _ => {}
                }
            }
            if found {
                return Some((prompt, completion, cached));
            }
        }
    }
    found.then_some((prompt, completion, cached))
}

/// A JSON string token as Go's `encoding/json` decodes it (a lone surrogate or an
/// invalid UTF-8 byte becomes U+FFFD; a `\u` pair is joined only when valid).
fn go_json_string(value: &cpa_common::json::Res<'_>) -> String {
    cpa_common::json::go_unquote(value.raw()).unwrap_or_default()
}

/// `json.Unmarshal` of an EOS trailer into Go's `{Error *struct{Code, Message string}}`:
/// field names match case-insensitively (Go's fold), duplicates decode in order into the
/// same struct, and any type mismatch makes the whole decode an error (`None`).
fn decode_trailer(payload: &[u8]) -> Option<(String, String)> {
    use cpa_common::gostr::GoStr;
    use cpa_common::json::{self as gj, Kind};
    if !gj::std_valid(payload) {
        return None;
    }
    let root = gj::parse(payload);
    if root.kind == Kind::Null {
        return None;
    }
    if !root.is_object() {
        return None;
    }
    let mut type_error = false;
    let mut error: Option<(String, String)> = None;
    root.each(|key, value| {
        if !key.str().go_eq_fold("error") {
            return true;
        }
        match value.kind {
            Kind::Null => error = None,
            Kind::Json if value.is_object() => {
                let fields = error.get_or_insert_with(Default::default);
                value.each(|k, v| {
                    let target = if k.str().go_eq_fold("code") {
                        &mut fields.0
                    } else if k.str().go_eq_fold("message") {
                        &mut fields.1
                    } else {
                        return true;
                    };
                    match v.kind {
                        Kind::String => *target = go_json_string(&v),
                        Kind::Null => {}
                        _ => type_error = true,
                    }
                    true
                });
            }
            _ => type_error = true,
        }
        true
    });
    if type_error {
        return None;
    }
    error
}

/// `ParseDevinTrailerError`: the HTTP status and message of an EOS trailer error;
/// `None` for a clean end.
pub(crate) fn parse_trailer_error(payload: &[u8]) -> Option<(u16, String)> {
    use cpa_common::gostr::GoStr;
    let trimmed = cpa_common::gostr::trim_space(payload);
    if trimmed.is_empty() || trimmed == b"{}" {
        return None;
    }
    let (code, message) = decode_trailer(trimmed)?;
    let code_lower = code.go_lower();
    let msg = message.go_lower();
    let status = match code_lower.as_str() {
        "invalid_argument" if msg.contains("internal error") => 502,
        "invalid_argument" => 400,
        "internal" => 502,
        "unauthenticated" => 401,
        "permission_denied" if msg.contains("high demand") => 429,
        "permission_denied" => 403,
        "resource_exhausted" => 429,
        "unavailable" => 503,
        "canceled" => 499,
        "deadline_exceeded" => 504,
        "failed_precondition"
            if ["quota", "credit", "acu", "exhausted", "limit"]
                .iter()
                .any(|w| msg.contains(w)) =>
        {
            429
        }
        "failed_precondition" => 400,
        _ => 502,
    };
    Some((status, format!("devin upstream error ({code}): {message}")))
}

/// Go `utf8.FullRune`.
fn full_rune(p: &[u8]) -> bool {
    let Some(&first) = p.first() else {
        return false;
    };
    let (size, lo, hi) = match first {
        0x00..=0x7F | 0x80..=0xC1 | 0xF5..=0xFF => return true,
        0xC2..=0xDF => (2, 0x80, 0xBF),
        0xE0 => (3, 0xA0, 0xBF),
        0xE1..=0xEC | 0xEE..=0xEF => (3, 0x80, 0xBF),
        0xED => (3, 0x80, 0x9F),
        0xF0 => (4, 0x90, 0xBF),
        0xF1..=0xF3 => (4, 0x80, 0xBF),
        0xF4 => (4, 0x80, 0x8F),
    };
    if p.len() >= size {
        return true;
    }
    if p.len() > 1 && (p[1] < lo || hi < p[1]) {
        return true;
    }
    p.len() > 2 && !(0x80..=0xBF).contains(&p[2])
}

/// `UTF8SplitBuffer`: holds back an incomplete trailing UTF-8 sequence; complete invalid
/// bytes pass through.
#[derive(Default)]
pub(crate) struct Utf8Split {
    remainder: Vec<u8>,
}

impl Utf8Split {
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut combined = std::mem::take(&mut self.remainder);
        combined.extend_from_slice(chunk);
        let mut valid = 0;
        while valid < combined.len() {
            match cpa_common::json::decode_rune(&combined[valid..]) {
                (None, 1) => {
                    let tail = &combined[valid..];
                    if tail.len() < 4 && !full_rune(tail) {
                        break;
                    }
                    valid += 1;
                }
                (_, size) => valid += size.max(1),
            }
        }
        self.remainder = combined.split_off(valid);
        combined
    }
}

// ---- request logging (helps/devin_wire.go log bodies) ----------------------------------

/// Go `json.Indent(dst, src, "", "  ")`: whitespace outside strings dropped, each
/// element on its own line, empty objects and arrays kept as `{}` and `[]`. `None` when
/// Go's scanner rejects `src` (callers then log it as is).
pub(crate) fn go_indent(src: &[u8]) -> Option<Vec<u8>> {
    if !cpa_common::json::std_valid(src) {
        return None;
    }
    fn newline(out: &mut Vec<u8>, depth: usize) {
        out.push(b'\n');
        for _ in 0..depth {
            out.extend_from_slice(b"  ");
        }
    }
    let mut out = Vec::with_capacity(src.len() * 2);
    let (mut depth, mut need_indent, mut in_string, mut escaped) = (0usize, false, false, false);
    for &c in src {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
            continue;
        }
        if need_indent && c != b'}' && c != b']' {
            need_indent = false;
            depth += 1;
            newline(&mut out, depth);
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(c);
            }
            b'{' | b'[' => {
                need_indent = true;
                out.push(c);
            }
            b',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            b':' => out.extend_from_slice(b": "),
            b'}' | b']' => {
                if need_indent {
                    need_indent = false;
                } else {
                    depth -= 1;
                    newline(&mut out, depth);
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    Some(out)
}

/// A JSON object written in Go struct-field order with `json.Marshal`'s escaping.
struct GoObject(Vec<u8>);

impl GoObject {
    fn new() -> Self {
        Self(vec![b'{'])
    }

    fn key(&mut self, key: &str) -> &mut Vec<u8> {
        if self.0.len() > 1 {
            self.0.push(b',');
        }
        cpa_common::json::marshal_str(&mut self.0, key.as_bytes(), true);
        self.0.push(b':');
        &mut self.0
    }

    fn string(&mut self, key: &str, value: &[u8]) {
        cpa_common::json::marshal_str(self.key(key), value, true);
    }

    /// A string field with `omitempty`.
    fn string_omit(&mut self, key: &str, value: &[u8]) {
        if !value.is_empty() {
            self.string(key, value);
        }
    }

    fn int(&mut self, key: &str, value: impl std::fmt::Display) {
        let value = value.to_string();
        self.key(key).extend_from_slice(value.as_bytes());
    }

    fn raw(&mut self, key: &str, raw: &[u8]) {
        self.key(key).extend_from_slice(raw);
    }

    fn end(mut self) -> Vec<u8> {
        self.0.push(b'}');
        self.0
    }
}

fn go_array(items: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut out = vec![b'['];
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(&item);
    }
    out.push(b']');
    out
}

/// `formatSignatureForLog`: sealed or printable-ASCII signatures as text, anything else
/// as standard base64.
fn signature_for_log(signature: &[u8]) -> Vec<u8> {
    use base64::Engine as _;
    if signature.starts_with(b"sealed.v1.") || signature.iter().all(|c| (32..=126).contains(c)) {
        return signature.to_vec();
    }
    base64::engine::general_purpose::STANDARD.encode(signature).into_bytes()
}

/// `[]DevinToolCall` as encoding/json writes it: the struct has no JSON tags.
fn tool_calls_json(calls: &[ToolCall]) -> Vec<u8> {
    go_array(calls.iter().map(|tc| {
        let mut o = GoObject::new();
        o.string("ID", &tc.id);
        o.string("Name", &tc.name);
        o.string("Arguments", &tc.arguments);
        o.end()
    }))
}

/// `*DevinUsage` as encoding/json writes it.
fn usage_json(u: &Usage) -> Vec<u8> {
    let mut o = GoObject::new();
    o.int("prompt_tokens", u.prompt_tokens);
    o.int("completion_tokens", u.completion_tokens);
    o.int("cached_tokens", u.cached_tokens);
    if u.cache_write_tokens != 0 {
        o.int("cache_write_tokens", u.cache_write_tokens);
    }
    if u.status_code != 0 {
        o.int("status_code", u.status_code);
    }
    o.string_omit("request_id", &u.request_id);
    o.string_omit("model_name", &u.model_name);
    if !u.headers.is_empty() {
        // A map: Go writes its keys sorted, as the BTreeMap holds them.
        let mut h = vec![b'{'];
        for (i, (k, v)) in u.headers.iter().enumerate() {
            if i > 0 {
                h.push(b',');
            }
            cpa_common::json::marshal_str(&mut h, k, true);
            h.push(b':');
            cpa_common::json::marshal_str(&mut h, v, true);
        }
        h.push(b'}');
        o.raw("headers", &h);
    }
    o.end()
}

/// What `BuildDevinUpstreamLogBody` receives.
pub(crate) struct RequestLog<'a> {
    pub interactions: &'a [u8],
    pub interactions_source: bool,
    pub model_uid: &'a str,
    /// The system prompt after `SanitizeDevinSystemPrompt`.
    pub system_prompt: &'a [u8],
    pub prompts: &'a [Prompt],
    pub tools: &'a [Tool],
    pub temperature: Option<f64>,
    pub max_tokens: i64,
    pub session_id: &'a str,
    pub cascade_id: &'a str,
}

/// `BuildDevinUpstreamLogBody`: the request Go logs instead of the protobuf it sends.
pub(crate) fn request_log_body(r: &RequestLog<'_>) -> Vec<u8> {
    let prompts = r.prompts.iter().map(|p| {
        let role: &[u8] = match p.source {
            2 => b"assistant",
            4 => b"tool",
            _ => b"user",
        };
        let mut o = GoObject::new();
        o.string_omit("id", p.message_id.as_bytes());
        o.int("source", p.source);
        o.string_omit("role", role);
        o.string_omit("content", &p.content);
        o.string_omit("thinking", &p.thinking);
        o.string_omit("signature", &signature_for_log(&p.signature));
        o.string_omit("signature_type", &p.signature_type);
        if !p.tool_calls.is_empty() {
            o.raw("tool_calls", &tool_calls_json(&p.tool_calls));
        }
        o.string_omit("tool_call_id", &p.tool_call_id);
        if !p.images.is_empty() {
            o.raw(
                "images",
                &go_array(p.images.iter().map(|img| {
                    let mut i = GoObject::new();
                    i.string("mime_type", &img.mime);
                    i.int("data_len", img.base64.len());
                    i.end()
                })),
            );
        }
        o.end()
    });
    let tools = r
        .tools
        .iter()
        .filter(|t| !t.name.is_empty() && !is_codex_app_automation_update(b"", &t.name))
        .map(|t| {
            let mut o = GoObject::new();
            o.string("name", &t.name);
            o.string_omit("description", &tool_description(&t.name, &t.description));
            if !t.parameters.is_empty() && cpa_common::json::std_valid(&t.parameters) {
                // json.RawMessage: compacted with HTML escapes.
                o.raw("parameters", &cpa_common::json::compact(&t.parameters, true));
            }
            o.end()
        });
    let prompts: Vec<Vec<u8>> = prompts.collect();
    let tools: Vec<Vec<u8>> = tools.collect();
    let mut o = GoObject::new();
    o.string("model", r.model_uid.as_bytes());
    o.string_omit("session_id", r.session_id.as_bytes());
    o.string_omit("cascade_id", r.cascade_id.as_bytes());
    o.string_omit("system_prompt", r.system_prompt);
    let request = match r.temperature.map(cpa_common::json::json_float) {
        // MarshalIndent fails on NaN or an infinity: Go logs only the model.
        Some(None) => format!(r#"{{"model": {}}}"#, cpa_common::gostr::quote(r.model_uid)).into_bytes(),
        temperature => {
            if let Some(Some(t)) = temperature {
                o.raw("temperature", t.as_bytes());
            }
            if r.max_tokens != 0 {
                o.int("max_tokens", r.max_tokens);
            }
            if !prompts.is_empty() {
                o.raw("prompts", &go_array(prompts));
            }
            if !tools.is_empty() {
                o.raw("tools", &go_array(tools));
            }
            go_indent(&o.end()).unwrap_or_default()
        }
    };
    if r.interactions_source || r.interactions.is_empty() {
        return request;
    }
    let mut out = b"=== INTERMEDIATE INTERACTIONS ===\n".to_vec();
    out.extend_from_slice(&go_indent(r.interactions).unwrap_or_else(|| r.interactions.to_vec()));
    out.extend_from_slice(b"\n\n=== DEVIN UPSTREAM REQUEST ===\n");
    out.extend_from_slice(&request);
    out
}

/// `DevinUpstreamResponseLog`: the decoded frames Go logs for a response.
#[derive(Debug, Clone, Default)]
pub(crate) struct ResponseLog {
    pub status: String,
    pub frames: usize,
    pub content: Vec<u8>,
    pub thinking: Vec<u8>,
    pub signature: Vec<u8>,
    pub signature_type: Vec<u8>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<Usage>,
    pub unknown_fields: Vec<i64>,
}

impl ResponseLog {
    /// `json.MarshalIndent(log, "", "  ")`.
    pub(crate) fn marshal_indent(&self) -> Vec<u8> {
        let mut o = GoObject::new();
        o.string_omit("status", self.status.as_bytes());
        o.int("frames_count", self.frames);
        o.string_omit("content", &self.content);
        o.string_omit("thinking", &self.thinking);
        o.string_omit("signature", &self.signature);
        o.string_omit("signature_type", &self.signature_type);
        if !self.tool_calls.is_empty() {
            o.raw("tool_calls", &tool_calls_json(&self.tool_calls));
        }
        if let Some(u) = &self.usage {
            o.raw("usage", &usage_json(u));
        }
        if !self.unknown_fields.is_empty() {
            o.raw(
                "unknown_fields",
                &go_array(self.unknown_fields.iter().map(|n| n.to_string().into_bytes())),
            );
        }
        go_indent(&o.end()).unwrap_or_default()
    }
}

/// `BuildDevinUpstreamResponseLogBody`: the decoded response, then the interactions
/// built from it.
pub(crate) fn response_log_body(log: Option<&ResponseLog>, interactions: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(log) = log {
        let log = ResponseLog {
            signature: signature_for_log(&log.signature),
            ..log.clone()
        };
        out.extend_from_slice(b"=== DEVIN UPSTREAM RESPONSE ===\n");
        out.extend_from_slice(&log.marshal_indent());
        out.extend_from_slice(b"\n\n");
    }
    if !interactions.is_empty() {
        out.extend_from_slice(b"=== INTERMEDIATE INTERACTIONS ===\n");
        out.extend_from_slice(&go_indent(interactions).unwrap_or_else(|| interactions.to_vec()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_match_protowire_limits() {
        let mut out = Vec::new();
        pb::varint(&mut out, u64::MAX);
        assert_eq!(out.len(), 10);
        assert_eq!(pb::consume_varint(&out), Ok((u64::MAX, 10)));
        let mut overflow = vec![0xff; 9];
        overflow.push(0x02);
        assert_eq!(pb::consume_varint(&overflow), Err(pb::Error::Overflow));
        assert_eq!(pb::consume_varint(&[0x80]), Err(pb::Error::Truncated));
        assert_eq!(pb::consume_tag(&[0x00]), Err(pb::Error::FieldNumber));
        // A nested group is skipped whole; a mismatched end marker is an error.
        let group = [0x1b, 0x08, 0x01, 0x23, 0x2b, 0x2c, 0x24, 0x1c];
        assert_eq!(pb::consume_field_value(3, pb::START_GROUP, &group[1..]), Ok(7));
        assert_eq!(
            pb::consume_field_value(3, pb::START_GROUP, &[0x08, 0x01, 0x24]),
            Err(pb::Error::EndGroup)
        );
    }

    #[test]
    fn trailers_decode_like_go_json() {
        // Go replaces a lone surrogate and invalid UTF-8 with U+FFFD instead of failing.
        let lone = br#"{"error":{"code":"unauthenticated","message":"\ud800x"}}"#;
        assert_eq!(
            parse_trailer_error(lone),
            Some((401, "devin upstream error (unauthenticated): \u{FFFD}x".into()))
        );
        let raw = b"{\"error\":{\"code\":\"internal\",\"message\":\"a\xffb\"}}";
        assert_eq!(
            parse_trailer_error(raw),
            Some((502, "devin upstream error (internal): a\u{FFFD}b".into()))
        );
        // A lone high surrogate does not consume the next escape (Go 1.26 decode.go;
        // checked with json.Unmarshal: "\uFFFDinternal error", so 502 not 400).
        let pair = br#"{"error":{"code":"invalid_argument","message":"\ud800\u0069nternal error"}}"#;
        assert_eq!(
            parse_trailer_error(pair),
            Some((
                502,
                "devin upstream error (invalid_argument): \u{FFFD}internal error".into()
            ))
        );
        // Case-insensitive field names; duplicates decode into the same struct.
        let folded = br#"{"ERROR":{"Code":"canceled"},"error":{"MESSAGE":"m"}}"#;
        assert_eq!(
            parse_trailer_error(folded),
            Some((499, "devin upstream error (canceled): m".into()))
        );
        // A type mismatch anywhere is an Unmarshal error: a clean end.
        assert_eq!(parse_trailer_error(br#"{"error":{"code":5,"message":"m"}}"#), None);
        assert_eq!(parse_trailer_error(br#"{"error":"x"}"#), None);
        assert_eq!(parse_trailer_error(br#"[1]"#), None);
        assert_eq!(
            parse_trailer_error(br#"{"error":{"code":"internal"},"error":null}"#),
            None
        );
    }

    #[test]
    fn turn_index_counts_per_session() {
        let id = "turn-index-unit-test-session";
        assert_eq!(next_turn_index(id), 0);
        assert_eq!(next_turn_index(id), 1);
        assert_eq!(next_turn_index(&format!(" {id} ")), 2, "trimmed like Go");
        assert_eq!(next_turn_index(""), 0);
    }
}
