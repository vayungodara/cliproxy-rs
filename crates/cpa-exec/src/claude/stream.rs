//! Native Claude SSE relay (claude_executor_stream.go).
//!
//! Upstream bytes are read as lines like Go's `bufio.Scanner` (LF split, one
//! trailing CR dropped), MCP aliases are restored per line, and each blank line ends
//! an event. The relay stops after the event carrying `message_stop`, so anything an
//! upstream appends after the terminal event is never forwarded. A transport read
//! failure before `message_stop` becomes Go's `unexpected EOF`.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecStream, FailureScope};
use futures_util::StreamExt;

use super::alias::{self, Reverse};
use crate::rawjson;

/// Go `bufio.Scanner` buffer limit (50 MB).
const MAX_LINE: usize = 52_428_800;

/// `observeClaudeStreamLine`.
fn observe(line: &[u8], message_id: &mut String, completed: &mut bool) {
    let line = line.trim_ascii();
    let Some(payload) = line.strip_prefix(b"data:") else {
        return;
    };
    let Ok(payload) = std::str::from_utf8(payload.trim_ascii()) else {
        return;
    };
    if !gjson::valid(payload) {
        return;
    }
    match rawjson::get(payload, "type").str() {
        "message_start" => {
            let id = rawjson::string(payload, "message.id");
            if !id.trim().is_empty() {
                *message_id = id.trim().to_owned();
            }
        }
        "message_stop" => *completed = true,
        _ => {}
    }
}

/// `reverseRemapOAuthToolNamesFromStreamLine` on one raw line.
pub(crate) fn restore_line(line: &[u8], reverse: &Reverse) -> Result<Vec<u8>, String> {
    if reverse.is_empty() {
        return Ok(line.to_vec());
    }
    let trimmed = line.trim_ascii();
    if trimmed.is_empty() || trimmed == b"[DONE]" || trimmed.starts_with(b"event:") {
        return Ok(line.to_vec());
    }
    let is_data = trimmed.starts_with(b"data:");
    let payload = if is_data { trimmed[5..].trim_ascii() } else { trimmed };
    let Ok(payload) = std::str::from_utf8(payload) else {
        return Ok(line.to_vec());
    };
    Ok(match alias::restore_event_payload(payload, reverse)? {
        Some(updated) if is_data => [b"data: ".as_slice(), updated.as_bytes()].concat(),
        Some(updated) => updated.into_bytes(),
        None => line.to_vec(),
    })
}

/// Message ID of a completed stream; `None` when `message_stop` was never seen.
pub(crate) type OnComplete = Box<dyn FnOnce(String) + Send>;

struct Relay {
    body: ExecStream,
    reverse: Reverse,
    line: BytesMut,
    event: BytesMut,
    ready: VecDeque<Result<Bytes, ExecError>>,
    message_id: String,
    completed: bool,
    finished: bool,
    done: Option<OnComplete>,
}

impl Relay {
    /// Handles one line; returns false once the relay has finished.
    fn accept(&mut self, line: &[u8]) -> bool {
        if self.finished {
            return false;
        }
        observe(line, &mut self.message_id, &mut self.completed);
        let restored = match restore_line(line, &self.reverse) {
            Ok(restored) => restored,
            Err(message) => {
                self.fail(ExecError::local(
                    500,
                    FailureScope::Request,
                    format!("restore Claude OAuth tool name from streaming response: {message}"),
                ));
                return false;
            }
        };
        self.event.extend_from_slice(&restored);
        self.event.extend_from_slice(b"\n");
        if restored.trim_ascii().is_empty() {
            self.flush();
            if self.completed {
                self.finish();
                return false;
            }
        }
        true
    }

    fn take_partial(&mut self) {
        let mut partial = std::mem::take(&mut self.line);
        if partial.is_empty() {
            return;
        }
        if partial.last() == Some(&b'\r') {
            partial.truncate(partial.len() - 1);
        }
        self.accept(&partial);
    }

    fn flush(&mut self) {
        if !self.event.is_empty() {
            let event = std::mem::take(&mut self.event).freeze();
            self.ready.push_back(Ok(event));
        }
    }

    fn fail(&mut self, error: ExecError) {
        self.event.clear();
        self.ready.push_back(Err(error));
        self.finished = true;
        self.done = None;
    }

    fn finish(&mut self) {
        self.finished = true;
        if self.completed
            && let Some(done) = self.done.take()
        {
            done(std::mem::take(&mut self.message_id));
        }
    }

    fn chunk(&mut self, chunk: &[u8]) {
        self.line.extend_from_slice(chunk);
        while let Some(pos) = self.line.iter().position(|b| *b == b'\n') {
            let mut line = self.line.split_to(pos + 1);
            line.truncate(pos);
            if line.last() == Some(&b'\r') {
                line.truncate(line.len() - 1);
            }
            if !self.accept(&line) {
                return;
            }
        }
        if self.line.len() > MAX_LINE {
            self.fail(ExecError::local(
                500,
                FailureScope::Request,
                "bufio.Scanner: token too long",
            ));
        }
    }

    fn end(&mut self, error: Option<ExecError>) {
        self.take_partial();
        if self.finished {
            return;
        }
        self.flush();
        if !self.completed
            && let Some(mut error) = error
        {
            if error.scope == FailureScope::Transport {
                error.body = Bytes::from_static(b"unexpected EOF");
            }
            self.ready.push_back(Err(error));
        }
        self.finish();
    }
}

/// Relays decoded upstream bytes as whole native SSE events. `done` runs once with
/// the message ID when the upstream completed with `message_stop`.
pub(crate) fn relay(body: ExecStream, reverse: Reverse, done: OnComplete) -> ExecStream {
    let relay = Relay {
        body,
        reverse,
        line: BytesMut::new(),
        event: BytesMut::new(),
        ready: VecDeque::new(),
        message_id: String::new(),
        completed: false,
        finished: false,
        done: Some(done),
    };
    futures_util::stream::unfold(relay, |mut st| async move {
        loop {
            if let Some(item) = st.ready.pop_front() {
                return Some((item, st));
            }
            if st.finished {
                return None;
            }
            match st.body.next().await {
                Some(Ok(chunk)) => st.chunk(&chunk),
                Some(Err(error)) => st.end(Some(error)),
                None => st.end(None),
            }
        }
    })
    .boxed()
}

/// `validateClaudeStreamingResponse` for buffered SSE behind a non-stream client.
pub(crate) fn validate_buffered(data: &[u8]) -> Result<(), ExecError> {
    // statusErr{502}: classified by status like any upstream 502.
    let bad = |m: &str| ExecError::local(502, crate::upstream::scope_for(502), m.to_owned());
    let (mut has_data, mut start, mut delta) = (false, false, false);
    for line in data.split(|b| *b == b'\n') {
        let line = line.trim_ascii();
        let Some(payload) = line.strip_prefix(b"data:") else {
            continue;
        };
        let payload = payload.trim_ascii();
        if payload.is_empty() || payload == b"[DONE]" {
            continue;
        }
        has_data = true;
        let payload = std::str::from_utf8(payload).unwrap_or("");
        if !gjson::valid(payload) {
            return Err(bad("claude executor: upstream returned malformed stream data"));
        }
        match rawjson::get(payload, "type").str() {
            "error" => {
                let mut message = rawjson::string(payload, "error.message").trim().to_owned();
                if message.is_empty() {
                    message = rawjson::string(payload, "error.type").trim().to_owned();
                }
                if message.is_empty() {
                    message = "unknown upstream error".into();
                }
                return Err(bad(&format!(
                    "claude executor: upstream returned error event: {message}"
                )));
            }
            "message_start" => {
                if rawjson::string(payload, "message.id").trim().is_empty()
                    || rawjson::string(payload, "message.model").trim().is_empty()
                {
                    return Err(bad(
                        "claude executor: upstream stream message_start is missing id or model",
                    ));
                }
                start = true;
            }
            "message_delta" => delta = true,
            _ => {}
        }
    }
    if !has_data {
        return Err(bad("claude executor: upstream returned empty stream response"));
    }
    if !start {
        return Err(bad(
            "claude executor: upstream stream response is missing message_start",
        ));
    }
    if !delta {
        return Err(bad(
            "claude executor: upstream stream response ended before message completion",
        ));
    }
    Ok(())
}

/// `claudeMessageIDFromSSE`: the message ID only when the buffered stream completed.
pub(crate) fn buffered_message_id(data: &[u8]) -> String {
    let (mut id, mut completed) = (String::new(), false);
    for line in data.split(|b| *b == b'\n') {
        observe(line, &mut id, &mut completed);
    }
    if completed { id } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(items: Vec<Result<&'static [u8], ExecError>>) -> ExecStream {
        futures_util::stream::iter(items.into_iter().map(|r| r.map(Bytes::from_static))).boxed()
    }

    #[tokio::test]
    async fn crlf_terminal_and_truncation_follow_go() {
        let done = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = done.clone();
        let body = chunks(vec![
            Ok(b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\r\n\r\nevent: message_st"),
            Ok(b"op\r\ndata: {\"type\":\"message_stop\"}\r\n\r\nevent: ping\r\ndata: {}\r\n\r\n"),
        ]);
        let out: Vec<_> = relay(
            body,
            Reverse::new(),
            Box::new(move |id| *seen.lock().unwrap() = Some(id)),
        )
        .map(Result::unwrap)
        .collect()
        .await;
        assert_eq!(
            out,
            [
                Bytes::from_static(
                    b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\"}}\n\n"
                ),
                Bytes::from_static(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            ]
        );
        assert_eq!(done.lock().unwrap().as_deref(), Some("msg_1"));

        let body = chunks(vec![
            Ok(b"event: content_block_delta\ndata: {}\n\nevent: x\ndata: {"),
            Err(ExecError::local(
                502,
                FailureScope::Transport,
                "upstream request failed",
            )),
        ]);
        let out: Vec<_> = relay(body, Reverse::new(), Box::new(|_| panic!("not completed")))
            .collect()
            .await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].as_ref().unwrap().as_ref(), b"event: x\ndata: {\n");
        assert_eq!(out[2].as_ref().unwrap_err().body.as_ref(), b"unexpected EOF");
    }
}
