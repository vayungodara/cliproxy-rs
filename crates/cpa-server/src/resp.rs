//! Go's Redis protocol on the main listener (internal/api/redis_queue_protocol.go):
//! management clients `AUTH` with the management key, then `LPOP`/`RPOP` queued usage
//! records or `SUBSCRIBE` to the `usage` and `errors` channels. The listener routes a
//! connection here when its first byte is a RESP type prefix.
//!
//! In Home mode (a remote dispatcher is installed) Go refuses these connections with
//! one error before reading a command.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter};

use crate::management::Management;

/// Go `isRedisRESPPrefix`.
pub(crate) fn is_resp_prefix(byte: u8) -> bool {
    matches!(byte, b'*' | b'$' | b'+' | b'-' | b':')
}

/// Why a command could not be read. A clean end (Go `io.EOF`) gets no reply.
#[derive(Debug)]
enum ReadError {
    Eof,
    UnexpectedEof,
    Protocol,
    Io(std::io::Error),
}

impl ReadError {
    /// The `ERR` text Go writes, or `None` at a clean end.
    fn message(&self) -> Option<String> {
        match self {
            ReadError::Eof => None,
            ReadError::UnexpectedEof => Some("unexpected EOF".into()),
            ReadError::Protocol => Some("protocol error".into()),
            ReadError::Io(e) => Some(e.to_string()),
        }
    }
}

/// Go `readRESPLine`: up to `\n`, without `\n` and one `\r`. A line cut off by the end
/// of the stream is `io.EOF`, as `bufio.Reader.ReadString` reports it.
async fn read_line<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<String, ReadError> {
    let mut buf = Vec::new();
    reader.read_until(b'\n', &mut buf).await.map_err(ReadError::Io)?;
    if buf.pop() != Some(b'\n') {
        return Err(ReadError::Eof);
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Go `strconv.Atoi`.
fn atoi(s: &str) -> Option<i64> {
    s.parse().ok()
}

/// Go `readRESPBulkString`; a negative length is the empty string.
// ponytail: lengths above MAX_REQUEST_BYTES are refused as protocol errors; Go
// allocates whatever the client declares.
async fn read_bulk<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<String, ReadError> {
    let length = atoi(&read_line(reader).await?).ok_or(ReadError::Protocol)?;
    if length < 0 {
        return Ok(String::new());
    }
    let length = usize::try_from(length).map_err(|_| ReadError::Protocol)?;
    if length > crate::MAX_REQUEST_BYTES {
        return Err(ReadError::Protocol);
    }
    // Go `io.ReadFull`: EOF when nothing arrived, "unexpected EOF" when part did.
    let mut buf = vec![0u8; length + 2];
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await.map_err(ReadError::Io)? {
            0 if filled == 0 => return Err(ReadError::Eof),
            0 => return Err(ReadError::UnexpectedEof),
            n => filled += n,
        }
    }
    if &buf[length..] != b"\r\n" {
        return Err(ReadError::Protocol);
    }
    buf.truncate(length);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Go `readRESPArray`: one command as an array of bulk or simple strings.
async fn read_command<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Vec<String>, ReadError> {
    let prefix = match reader.read_u8().await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ReadError::Eof),
        Err(e) => return Err(ReadError::Io(e)),
    };
    if prefix != b'*' {
        return Err(ReadError::Protocol);
    }
    let count = atoi(&read_line(reader).await?)
        .filter(|n| *n >= 0)
        .ok_or(ReadError::Protocol)?;
    let mut args = Vec::new();
    for _ in 0..count {
        let kind = match reader.read_u8().await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(ReadError::Eof),
            Err(e) => return Err(ReadError::Io(e)),
        };
        args.push(match kind {
            b'$' => read_bulk(reader).await?,
            b'+' | b':' => read_line(reader).await?,
            _ => return Err(ReadError::Protocol),
        });
    }
    Ok(args)
}

/// RESP replies, buffered until [`flush`].
struct Reply(Vec<u8>);

impl Reply {
    fn simple(&mut self, value: &str) {
        self.0.extend_from_slice(format!("+{value}\r\n").as_bytes());
    }

    fn error(&mut self, message: &str) {
        self.0.extend_from_slice(format!("-{message}\r\n").as_bytes());
    }

    fn bulk(&mut self, payload: Option<&[u8]>) {
        match payload {
            None => self.0.extend_from_slice(b"$-1\r\n"),
            Some(p) => {
                self.0.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
                self.0.extend_from_slice(p);
                self.0.extend_from_slice(b"\r\n");
            }
        }
    }

    fn array(&mut self, count: usize) {
        self.0.extend_from_slice(format!("*{count}\r\n").as_bytes());
    }

    fn integer(&mut self, value: i64) {
        self.0.extend_from_slice(format!(":{value}\r\n").as_bytes());
    }

    /// Go `writeRedisPubSubSubscribe` / `Unsubscribe` / `Message`.
    fn pubsub(&mut self, kind: &str, channel: &str, last: PubSub<'_>) {
        self.array(3);
        self.bulk(Some(kind.as_bytes()));
        self.bulk(Some(channel.as_bytes()));
        match last {
            PubSub::Count(n) => self.integer(n),
            PubSub::Payload(p) => self.bulk(Some(p)),
        }
    }
}

enum PubSub<'a> {
    Count(i64),
    Payload(&'a [u8]),
}

/// Writes and flushes what `reply` holds; false when the client is gone.
async fn flush<W: AsyncWrite + Unpin>(writer: &mut BufWriter<W>, reply: &mut Reply) -> bool {
    let ok = writer.write_all(&reply.0).await.is_ok() && writer.flush().await.is_ok();
    reply.0.clear();
    if !ok {
        tracing::error!("redis protocol flush error");
    }
    ok
}

/// Go `resolveRemoteIP`: the peer address, IPv4-mapped addresses unmapped.
fn remote_ip(peer: SocketAddr) -> (String, bool) {
    let ip = match peer.ip() {
        std::net::IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or(std::net::IpAddr::V6(v6), std::net::IpAddr::V4),
        ip => ip,
    };
    let host = ip.to_string();
    let local = host == "127.0.0.1" || host == "::1";
    (host, local)
}

/// Go `handleRedisConnection`.
pub(crate) async fn serve<IO>(io: IO, peer: SocketAddr, mgmt: Arc<Management>)
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read, write) = tokio::io::split(io);
    let mut reader = BufReader::new(read);
    let mut writer = BufWriter::new(write);
    let mut reply = Reply(Vec::new());
    let (ip, local) = remote_ip(peer);
    let mut authed = false;
    // Go reports a ban instead of NOAUTH when an empty key check says so.
    let banned_or = |result: Result<(), (axum::http::StatusCode, String)>, otherwise: &str| match result {
        Err((status, message))
            if status == axum::http::StatusCode::FORBIDDEN
                && message.starts_with("IP banned due to too many failed attempts") =>
        {
            format!("ERR {message}")
        }
        _ => otherwise.to_owned(),
    };
    if mgmt.rt.remote_dispatch().is_some() {
        // Go's listener already buffered what arrived with the first byte (bufio
        // `Peek`); reading it too keeps the close a FIN instead of a reset.
        let _ = reader.fill_buf().await;
        reply.error("ERR redis usage output disabled in home mode");
        let _ = writer.write_all(&reply.0).await;
        let _ = writer.flush().await;
        return;
    }
    loop {
        if !mgmt.routes_enabled() {
            return;
        }
        let args = match read_command(&mut reader).await {
            Ok(args) => args,
            Err(e) => {
                if let Some(message) = e.message() {
                    reply.error(&format!("ERR {message}"));
                    flush(&mut writer, &mut reply).await;
                }
                return;
            }
        };
        let Some(first) = args.first() else {
            reply.error("ERR empty command");
            if !flush(&mut writer, &mut reply).await {
                return;
            }
            continue;
        };
        let cmd = first.trim().to_uppercase();
        if cmd != "AUTH" && !authed {
            let result = mgmt.authenticate_key(&ip, local, b"").await;
            reply.error(&banned_or(result, "NOAUTH Authentication required."));
            if !flush(&mut writer, &mut reply).await {
                return;
            }
            continue;
        }
        match cmd.as_str() {
            "AUTH" => {
                // Go `parseAuthPassword`: `AUTH password` or `AUTH user password`.
                let password = match args.len() {
                    2 => Some(&args[1]),
                    3 => Some(&args[2]),
                    _ => None,
                };
                match password {
                    None => {
                        let result = mgmt.authenticate_key(&ip, local, b"").await;
                        reply.error(&banned_or(result, "ERR wrong number of arguments for 'auth' command"));
                    }
                    Some(password) => match mgmt.authenticate_key(&ip, local, password.as_bytes()).await {
                        Ok(()) => {
                            authed = true;
                            reply.simple("OK");
                        }
                        Err((_, message)) => reply.error(&format!("ERR {message}")),
                    },
                }
            }
            "SUBSCRIBE" => {
                if args.len() != 2 {
                    reply.error("ERR wrong number of arguments for 'subscribe' command");
                } else {
                    let channel = args[1].trim().to_owned();
                    let queue = mgmt.rt.usage_queue();
                    let subscription = match channel.to_lowercase().as_str() {
                        "usage" => queue.subscribe_usage(),
                        "errors" => queue.subscribe_errors(),
                        _ => {
                            reply.error(&format!("ERR unsupported channel '{channel}'"));
                            if !flush(&mut writer, &mut reply).await {
                                return;
                            }
                            continue;
                        }
                    };
                    reply.pubsub("subscribe", &channel, PubSub::Count(1));
                    if flush(&mut writer, &mut reply).await {
                        stream(reader, writer, &channel, subscription).await;
                    }
                    return;
                }
            }
            "LPOP" | "RPOP" => {
                // Go `parsePopCount`: a count that does not parse is 0.
                let count = match args.len() {
                    2 => Some((1, false)),
                    3 => Some((atoi(args[2].trim()).unwrap_or(0), true)),
                    _ => None,
                };
                match count {
                    None => reply.error(&format!(
                        "ERR wrong number of arguments for '{}' command",
                        cmd.to_lowercase()
                    )),
                    Some((count, _)) if count <= 0 => reply.error("ERR value is not an integer or out of range"),
                    Some((count, has_count)) => {
                        let channel = args[1].trim();
                        if !channel.eq_ignore_ascii_case("usage") {
                            reply.error(&format!("ERR unsupported channel '{channel}'"));
                        } else {
                            let items = mgmt.rt.usage_queue().pop_oldest(count as usize);
                            if has_count {
                                reply.array(items.len());
                                for item in &items {
                                    reply.bulk(Some(item));
                                }
                            } else {
                                reply.bulk(items.first().map(Vec::as_slice));
                            }
                        }
                    }
                }
            }
            _ => reply.error(&format!("ERR unknown command '{}'", cmd.to_lowercase())),
        }
        if !flush(&mut writer, &mut reply).await {
            return;
        }
    }
}

/// Go `streamRedisSubscription`: channel messages out, `PING`, `UNSUBSCRIBE` and
/// `QUIT` in, until either side ends.
async fn stream<R, W>(
    mut reader: BufReader<R>,
    mut writer: BufWriter<W>,
    channel: &str,
    mut subscription: crate::usage::Subscription,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin,
{
    let (tx, mut commands) = tokio::sync::mpsc::channel::<Result<Vec<String>, String>>(1);
    let read_task = tokio::spawn(async move {
        loop {
            match read_command(&mut reader).await {
                Ok(args) => {
                    if tx.send(Ok(args)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    if let Some(message) = e.message() {
                        let _ = tx.send(Err(message)).await;
                    }
                    return;
                }
            }
        }
    });
    let mut reply = Reply(Vec::new());
    loop {
        tokio::select! {
            message = subscription.messages.recv() => {
                let Some(payload) = message else { break };
                reply.pubsub("message", channel, PubSub::Payload(&payload));
                if !flush(&mut writer, &mut reply).await {
                    break;
                }
            }
            command = commands.recv() => {
                let Some(command) = command else { break };
                let keep_open = match command {
                    Err(message) => {
                        reply.error(&format!("ERR {message}"));
                        false
                    }
                    Ok(args) => match args.first().map(|c| c.trim().to_uppercase()) {
                        None => {
                            reply.error("ERR empty command");
                            true
                        }
                        Some(cmd) if cmd == "PING" => {
                            reply.array(2);
                            reply.bulk(Some(b"pong"));
                            reply.bulk(args.get(1).map(String::as_bytes));
                            true
                        }
                        Some(cmd) if cmd == "UNSUBSCRIBE" => {
                            reply.pubsub("unsubscribe", channel, PubSub::Count(0));
                            false
                        }
                        Some(cmd) if cmd == "QUIT" => {
                            reply.simple("OK");
                            false
                        }
                        Some(cmd) => {
                            reply.error(&format!("ERR unknown command '{}'", cmd.to_lowercase()));
                            true
                        }
                    },
                };
                if !flush(&mut writer, &mut reply).await || !keep_open {
                    break;
                }
            }
        }
    }
    read_task.abort();
}
