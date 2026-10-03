//! RESP2, the only protocol Home speaks. Commands are arrays of bulk strings; replies are
//! read with size bounds so a misbehaving peer cannot exhaust memory.

use std::io;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufStream};

/// One RESP2 reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Simple(String),
    /// A `-` reply: Home rejected the command (go-redis `redis.Error`).
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    /// `$-1` or `*-1` (go-redis `redis.Nil`).
    Nil,
    Array(Vec<Value>),
}

// ponytail: fixed caps. Home's largest replies are config YAML and plugin sync manifests,
// far below these; raise them if a deployment ever sends more.
const MAX_BULK: usize = 64 << 20;
const MAX_ARRAY: usize = 1 << 20;
const MAX_DEPTH: usize = 4;
const MAX_LINE: usize = 64 << 10;

/// `*N\r\n$len\r\narg\r\n...`, as Go's `encodeRESPArray` and go-redis write commands.
pub fn encode<A: AsRef<[u8]>>(args: &[A]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + args.iter().map(|a| a.as_ref().len() + 16).sum::<usize>());
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        let arg = arg.as_ref();
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    let read = (&mut *reader)
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut line)
        .await?;
    if read == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if !line.ends_with(b"\r\n") {
        return Err(if line.len() >= MAX_LINE {
            invalid("resp line too long")
        } else {
            io::ErrorKind::UnexpectedEof.into()
        });
    }
    line.truncate(line.len() - 2);
    Ok(line)
}

fn number(text: &[u8]) -> io::Result<i64> {
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse::<i64>().ok())
        .ok_or_else(|| invalid(format!("invalid resp number {:?}", String::from_utf8_lossy(text))))
}

/// A bulk or array length: `-1` is Nil; any other negative length is a protocol error,
/// as in go-redis, so a corrupt reply to an issued RPOP stays ambiguous.
fn length(text: &[u8], kind: &str) -> io::Result<Option<usize>> {
    match number(text)? {
        -1 => Ok(None),
        n if n < 0 => Err(invalid(format!("invalid resp {kind} length {n}"))),
        n => usize::try_from(n)
            .map(Some)
            .map_err(|_| invalid(format!("resp {kind} too large"))),
    }
}

/// Reads one reply.
pub async fn read<R: AsyncBufRead + Unpin + Send>(reader: &mut R) -> io::Result<Value> {
    read_depth(reader, 0).await
}

fn read_depth<'a, R: AsyncBufRead + Unpin + Send>(
    reader: &'a mut R,
    depth: usize,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<Value>> + Send + 'a>> {
    Box::pin(async move {
        let line = read_line(reader).await?;
        let Some((&prefix, rest)) = line.split_first() else {
            return Err(invalid("empty resp line"));
        };
        match prefix {
            b'+' => Ok(Value::Simple(String::from_utf8_lossy(rest).into_owned())),
            b'-' => Ok(Value::Error(String::from_utf8_lossy(rest).into_owned())),
            b':' => number(rest).map(Value::Int),
            b'$' => {
                let Some(size) = length(rest, "bulk")? else {
                    return Ok(Value::Nil);
                };
                if size > MAX_BULK {
                    return Err(invalid("resp bulk too large"));
                }
                let mut payload = vec![0; size + 2];
                reader.read_exact(&mut payload).await?;
                if !payload.ends_with(b"\r\n") {
                    return Err(invalid("resp bulk missing terminator"));
                }
                payload.truncate(size);
                Ok(Value::Bulk(payload))
            }
            b'*' => {
                let Some(count) = length(rest, "array")? else {
                    return Ok(Value::Nil);
                };
                if count > MAX_ARRAY || depth >= MAX_DEPTH {
                    return Err(invalid("resp array too large"));
                }
                let mut items = Vec::with_capacity(count.min(64));
                for _ in 0..count {
                    items.push(read_depth(reader, depth + 1).await?);
                }
                Ok(Value::Array(items))
            }
            _ => Err(invalid(format!("unsupported resp prefix {:?}", char::from(prefix)))),
        }
    })
}

pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

/// One RESP connection (plain TCP or TLS).
pub struct Conn {
    io: BufStream<Box<dyn Io>>,
}

impl Conn {
    pub fn new(io: Box<dyn Io>) -> Self {
        Self { io: BufStream::new(io) }
    }

    /// Writes and flushes one command.
    pub async fn send<A: AsRef<[u8]>>(&mut self, args: &[A]) -> io::Result<()> {
        self.io.write_all(&encode(args)).await?;
        self.io.flush().await
    }

    pub async fn recv(&mut self) -> io::Result<Value> {
        read(&mut self.io).await
    }

    pub async fn shutdown(&mut self) {
        let _ = self.io.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parse(bytes: &[u8]) -> io::Result<Value> {
        let mut reader = tokio::io::BufReader::new(bytes);
        read(&mut reader).await
    }

    #[test]
    fn encodes_like_go_encode_resp_array() {
        assert_eq!(
            encode(&["CERTIFICATE", "REQUEST", "id", ""]),
            b"*4\r\n$11\r\nCERTIFICATE\r\n$7\r\nREQUEST\r\n$2\r\nid\r\n$0\r\n\r\n"
        );
    }

    #[tokio::test]
    async fn reads_every_reply_kind() {
        assert_eq!(parse(b"+OK\r\n").await.unwrap(), Value::Simple("OK".into()));
        assert_eq!(parse(b"-ERR no\r\n").await.unwrap(), Value::Error("ERR no".into()));
        assert_eq!(parse(b":-2\r\n").await.unwrap(), Value::Int(-2));
        assert_eq!(parse(b"$-1\r\n").await.unwrap(), Value::Nil);
        assert_eq!(parse(b"*-1\r\n").await.unwrap(), Value::Nil);
        assert_eq!(parse(b"$3\r\na\r\n\r\n").await.unwrap(), Value::Bulk(b"a\r\n".to_vec()));
        assert_eq!(
            parse(b"*3\r\n$7\r\nmessage\r\n$6\r\nconfig\r\n$0\r\n\r\n")
                .await
                .unwrap(),
            Value::Array(vec![
                Value::Bulk(b"message".to_vec()),
                Value::Bulk(b"config".to_vec()),
                Value::Bulk(Vec::new()),
            ])
        );
    }

    #[tokio::test]
    async fn truncated_and_oversized_replies_are_errors() {
        assert_eq!(
            parse(b"$5\r\nab").await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(parse(b"+OK").await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(parse(b"").await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert!(parse(format!("${}\r\n", MAX_BULK + 1).as_bytes()).await.is_err());
        assert!(parse(b"*1\r\n*1\r\n*1\r\n*1\r\n*1\r\n:1\r\n").await.is_err());
        assert!(parse(b"%1\r\n").await.is_err());
        assert!(parse(b"$2\r\nabXY").await.is_err());
        // Only -1 is Nil; go-redis rejects other negative lengths.
        assert!(parse(b"$-2\r\n").await.is_err());
        assert!(parse(b"*-2\r\n").await.is_err());
        // A multibyte discriminator is an error, not a panic.
        assert!(parse("é\r\n".as_bytes()).await.is_err());
        assert_eq!(
            parse(b"-ERR \xff\r\n").await.unwrap(),
            Value::Error("ERR \u{fffd}".into())
        );
    }
}
