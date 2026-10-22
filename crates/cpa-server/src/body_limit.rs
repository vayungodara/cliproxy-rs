//! `server.max-request-body-bytes`, Go v8.0.35 `maxRequestBodyBytesMiddleware`
//! (dd7b4334). Off by default. When set, a declared `Content-Length` over the limit is
//! refused before anything is read, and every other body ends in [`TooLarge`] as soon
//! as it passes the limit, so whoever reads it (request capture, client auth plugins,
//! handlers) answers Go's 413 for its route.
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, ready};

use axum::body::{Body, Bytes, HttpBody};
use axum::extract::rejection::BytesRejection;
use axum::extract::{FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use http_body::{Frame, SizeHint};

use crate::Runtime;

/// Go `http.MaxBytesError.Error()`.
pub(crate) const MESSAGE: &str = "http: request body too large";

/// A body read stopped at the configured limit.
#[derive(Debug)]
pub(crate) struct TooLarge;

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MESSAGE)
    }
}

impl std::error::Error for TooLarge {}

/// The limit the middleware applied to this request, `None` when off (Go stores it on
/// the request as `MaxRequestBodyBytesContextKey`). Readers use this value, never the
/// live config, so a reload during the request does not change it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Limit(pub Option<u64>);

impl<S: Send + Sync> FromRequestParts<S> for Limit {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<Limit>().copied().unwrap_or_default())
    }
}

/// Whether `error`, or anything it wraps, is [`TooLarge`] (Go `errors.As(err, &maxBytesErr)`).
pub(crate) fn is_too_large(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut next = Some(error);
    while let Some(error) = next {
        if error.is::<TooLarge>() {
            return true;
        }
        next = error.source();
    }
    false
}

/// The middleware's own answer, and request capture's.
pub(crate) fn too_large() -> Response {
    crate::respond::gin_json(413, format!(r#"{{"error":"{MESSAGE}"}}"#))
}

/// Applies the limit to `app`, outside request capture (Go registers it before the
/// request-logging middleware). The limit is read from the current config per request.
pub fn layer(rt: &Arc<Runtime>, app: axum::Router) -> axum::Router {
    app.layer(axum::middleware::from_fn_with_state(rt.clone(), limit))
}

async fn limit(State(rt): State<Arc<Runtime>>, mut request: Request, next: Next) -> Response {
    let Some(max) = rt.config().max_request_body_bytes() else {
        return next.run(request).await;
    };
    // hyper reports a declared Content-Length as the body's exact size.
    if request.body().size_hint().lower() > max {
        return too_large();
    }
    request.extensions_mut().insert(Limit(Some(max)));
    next.run(request.map(|body| {
        Body::new(Capped {
            body,
            left: max,
            over: false,
        })
    }))
    .await
}

/// Go `http.MaxBytesReader`: passes frames through until more than `left` bytes have
/// arrived. The frame that crosses the limit yields its permitted prefix (a slice, not a
/// copy), and every later poll fails. Nothing is buffered.
struct Capped {
    body: Body,
    left: u64,
    over: bool,
}

impl HttpBody for Capped {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        if self.over {
            return Poll::Ready(Some(Err(axum::Error::new(TooLarge))));
        }
        let frame = ready!(Pin::new(&mut self.body).poll_frame(cx));
        let Some(Ok(frame)) = frame else {
            return Poll::Ready(frame);
        };
        let Some(len) = frame.data_ref().map(|data| data.len() as u64) else {
            return Poll::Ready(Some(Ok(frame)));
        };
        if len <= self.left {
            self.left -= len;
            return Poll::Ready(Some(Ok(frame)));
        }
        self.over = true;
        let left = std::mem::take(&mut self.left) as usize;
        match frame.into_data() {
            Ok(data) if left > 0 => Poll::Ready(Some(Ok(Frame::data(data.slice(..left))))),
            _ => Poll::Ready(Some(Err(axum::Error::new(TooLarge)))),
        }
    }

    fn is_end_stream(&self) -> bool {
        !self.over && self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

/// A body read the way Go handlers read it under the limit: the bytes, or the read error
/// together with the bytes that arrived before it, which Go's JSON and multipart decoders
/// scan before they report the error.
pub(crate) struct Read {
    pub(crate) body: Result<Bytes, BytesRejection>,
    /// The permitted prefix when the limit stopped the read, else empty.
    pub(crate) prefix: Bytes,
}

impl<S: Send + Sync> FromRequest<S> for Read {
    type Rejection = std::convert::Infallible;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Without a limit there is no prefix to keep.
        if request.extensions().get::<Limit>().is_none_or(|l| l.0.is_none()) {
            let body = Bytes::from_request(request, state).await;
            return Ok(Read {
                body,
                prefix: Bytes::new(),
            });
        }
        let (parts, body) = request.into_parts();
        let (body, seen) = Tee::wrap(body);
        let body = Bytes::from_request(Request::from_parts(parts, body), state).await;
        let prefix = match &body {
            Err(rejection) if is_too_large(rejection) => seen.prefix(),
            _ => Bytes::new(),
        };
        Ok(Read { body, prefix })
    }
}

/// [`Read::first_json`] over `axum::body::to_bytes(body, usize::MAX)`.
pub(crate) async fn first_json_body(request: Request) -> Result<Bytes, axum::Error> {
    if request.extensions().get::<Limit>().is_none_or(|l| l.0.is_none()) {
        return axum::body::to_bytes(request.into_body(), usize::MAX).await;
    }
    let (body, seen) = Tee::wrap(request.into_body());
    match axum::body::to_bytes(body, usize::MAX).await {
        Err(error) if is_too_large(&error) => {
            let prefix = seen.prefix();
            match first_value(&prefix) {
                Some(value) => Ok(prefix.slice_ref(value)),
                None => Err(error),
            }
        }
        body => body,
    }
}

impl Read {
    /// gin `ShouldBindJSON` and `json.Decoder.Decode`: when the limit stopped the read,
    /// a first JSON value complete before it binds as if the body were exactly that
    /// value. A body read whole is returned unchanged.
    pub(crate) fn first_json(self) -> Result<Bytes, BytesRejection> {
        match self.body {
            Err(rejection) => match first_value(&self.prefix) {
                Some(value) => Ok(self.prefix.slice_ref(value)),
                None => Err(rejection),
            },
            body => body,
        }
    }
}

#[cfg(test)]
impl From<Bytes> for Read {
    fn from(body: Bytes) -> Self {
        Read {
            body: Ok(body),
            prefix: Bytes::new(),
        }
    }
}

/// [`Read::first_json`] as an extractor.
pub(crate) struct FirstJson(pub(crate) Bytes);

impl<S: Send + Sync> FromRequest<S> for FirstJson {
    type Rejection = BytesRejection;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Ok(read) = Read::from_request(request, state).await;
        read.first_json().map(FirstJson)
    }
}

/// The bytes Go's `json.Decoder.Decode` takes from `prefix` when the read error
/// follows it: the first value once it completes, or the bytes up to the one its
/// scanner rejects (the handler's decode then fails on them as Go's does); `None` when
/// the value is cut off, so the limit error is Go's answer.
fn first_value(prefix: &[u8]) -> Option<&[u8]> {
    match GoScan::default().first_value(prefix) {
        Scanned::Complete(end) => Some(&prefix[..end]),
        Scanned::Invalid(at) => Some(&prefix[..=at]),
        Scanned::Cut => None,
    }
}

/// The first JSON value of a body read whole, as Go's `Decoder.Decode` takes it: a
/// top-level scalar ends at the end of the body. `None` when it is cut off or invalid.
pub(crate) fn whole_first_value(body: &[u8]) -> Option<&[u8]> {
    let mut scan = GoScan::default();
    match scan.first_value(body) {
        Scanned::Complete(end) => Some(&body[..end]),
        Scanned::Cut if matches!(scan.step(b' '), Step::Ended) => Some(body),
        _ => None,
    }
}

enum Scanned {
    /// The first value is `prefix[..end]`.
    Complete(usize),
    /// The scanner rejects the byte at this offset.
    Invalid(usize),
    Cut,
}

/// Go `encoding/json`'s scanner (scanner.go) as `Decoder.readValue` drives it: an
/// object or array ends at its closing bracket, a top-level scalar only at the byte
/// after it (whatever that byte is), and nesting past 10,000 levels is an error.
#[derive(Default)]
struct GoScan {
    stack: Vec<Ctx>,
    phase: Phase,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    ObjectKey,
    ObjectValue,
    Array,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Phase {
    #[default]
    BeginValue,
    BeginValueOrEmpty,
    BeginStringOrEmpty,
    BeginString,
    EndValue,
    InString,
    Esc,
    EscU(u8),
    Neg,
    Int,
    Zero,
    Dot,
    Frac,
    Exp,
    ExpSign,
    ExpDigits,
    Literal(&'static [u8], u8),
}

enum Step {
    Continue,
    /// A top-level container closed with this byte.
    Closed,
    /// A top-level scalar ended before this byte.
    Ended,
    Error,
}

impl GoScan {
    const MAX_DEPTH: usize = 10_000;

    fn first_value(&mut self, prefix: &[u8]) -> Scanned {
        for (i, &c) in prefix.iter().enumerate() {
            match self.step(c) {
                Step::Continue => {}
                Step::Closed => return Scanned::Complete(i + 1),
                Step::Ended => return Scanned::Complete(i),
                Step::Error => return Scanned::Invalid(i),
            }
        }
        Scanned::Cut
    }

    fn push(&mut self, ctx: Ctx, next: Phase) -> Step {
        self.stack.push(ctx);
        if self.stack.len() > Self::MAX_DEPTH {
            return Step::Error;
        }
        self.phase = next;
        Step::Continue
    }

    fn step(&mut self, c: u8) -> Step {
        let space = matches!(c, b' ' | b'\t' | b'\r' | b'\n');
        match self.phase {
            Phase::BeginValueOrEmpty if space => Step::Continue,
            Phase::BeginValueOrEmpty if c == b']' => self.end_value(c),
            Phase::BeginValueOrEmpty => self.begin_value(c),
            Phase::BeginValue if space => Step::Continue,
            Phase::BeginValue => self.begin_value(c),
            Phase::BeginStringOrEmpty if space => Step::Continue,
            Phase::BeginStringOrEmpty if c == b'}' => {
                *self.stack.last_mut().expect("inside an object") = Ctx::ObjectValue;
                self.end_value(c)
            }
            Phase::BeginStringOrEmpty | Phase::BeginString if space => Step::Continue,
            Phase::BeginStringOrEmpty | Phase::BeginString if c == b'"' => self.next(Phase::InString),
            Phase::BeginStringOrEmpty | Phase::BeginString => Step::Error,
            Phase::EndValue => self.end_value(c),
            Phase::InString => match c {
                b'"' => self.next(Phase::EndValue),
                b'\\' => self.next(Phase::Esc),
                c if c < 0x20 => Step::Error,
                _ => Step::Continue,
            },
            Phase::Esc => match c {
                b'b' | b'f' | b'n' | b'r' | b't' | b'\\' | b'/' | b'"' => self.next(Phase::InString),
                b'u' => self.next(Phase::EscU(0)),
                _ => Step::Error,
            },
            Phase::EscU(n) if c.is_ascii_hexdigit() => {
                self.next(if n == 3 { Phase::InString } else { Phase::EscU(n + 1) })
            }
            Phase::EscU(_) => Step::Error,
            Phase::Neg => match c {
                b'0' => self.next(Phase::Zero),
                b'1'..=b'9' => self.next(Phase::Int),
                _ => Step::Error,
            },
            Phase::Int if c.is_ascii_digit() => Step::Continue,
            Phase::Int | Phase::Zero => match c {
                b'.' => self.next(Phase::Dot),
                b'e' | b'E' => self.next(Phase::Exp),
                _ => self.end_value(c),
            },
            Phase::Dot if c.is_ascii_digit() => self.next(Phase::Frac),
            Phase::Dot => Step::Error,
            Phase::Frac if c.is_ascii_digit() => Step::Continue,
            Phase::Frac if matches!(c, b'e' | b'E') => self.next(Phase::Exp),
            Phase::Frac => self.end_value(c),
            Phase::Exp if matches!(c, b'+' | b'-') => self.next(Phase::ExpSign),
            Phase::Exp | Phase::ExpSign if c.is_ascii_digit() => self.next(Phase::ExpDigits),
            Phase::Exp | Phase::ExpSign => Step::Error,
            Phase::ExpDigits if c.is_ascii_digit() => Step::Continue,
            Phase::ExpDigits => self.end_value(c),
            Phase::Literal(word, n) if word.get(usize::from(n)) == Some(&c) => {
                let n = n + 1;
                self.next(if usize::from(n) == word.len() {
                    Phase::EndValue
                } else {
                    Phase::Literal(word, n)
                })
            }
            Phase::Literal(..) => Step::Error,
        }
    }

    fn next(&mut self, state: Phase) -> Step {
        self.phase = state;
        Step::Continue
    }

    fn begin_value(&mut self, c: u8) -> Step {
        match c {
            b'{' => self.push(Ctx::ObjectKey, Phase::BeginStringOrEmpty),
            b'[' => self.push(Ctx::Array, Phase::BeginValueOrEmpty),
            b'"' => self.next(Phase::InString),
            b'-' => self.next(Phase::Neg),
            b'0' => self.next(Phase::Zero),
            b'1'..=b'9' => self.next(Phase::Int),
            b't' => self.next(Phase::Literal(b"true", 1)),
            b'f' => self.next(Phase::Literal(b"false", 1)),
            b'n' => self.next(Phase::Literal(b"null", 1)),
            _ => Step::Error,
        }
    }

    /// Go `stateEndValue`, entered with the byte after a value.
    fn end_value(&mut self, c: u8) -> Step {
        let Some(&ctx) = self.stack.last() else {
            // stateEndTop: the decoder stops here whatever `c` is.
            return Step::Ended;
        };
        self.phase = Phase::EndValue;
        if matches!(c, b' ' | b'\t' | b'\r' | b'\n') {
            return Step::Continue;
        }
        let top = self.stack.len() - 1;
        match (ctx, c) {
            (Ctx::ObjectKey, b':') => {
                self.stack[top] = Ctx::ObjectValue;
                self.next(Phase::BeginValue)
            }
            (Ctx::ObjectValue, b',') => {
                self.stack[top] = Ctx::ObjectKey;
                self.next(Phase::BeginString)
            }
            (Ctx::Array, b',') => self.next(Phase::BeginValue),
            (Ctx::ObjectValue, b'}') | (Ctx::Array, b']') => {
                self.stack.pop();
                match self.stack.is_empty() {
                    true => Step::Closed,
                    false => Step::Continue,
                }
            }
            _ => Step::Error,
        }
    }
}

/// Keeps a handle on each data frame that passes (no copy).
struct Tee {
    body: Body,
    seen: Seen,
}

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Bytes>>>);

impl Seen {
    /// The frames joined; one frame is returned as it is.
    fn prefix(&self) -> Bytes {
        let mut frames = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
        match frames.len() {
            1 => frames.pop().unwrap_or_default(),
            _ => frames.concat().into(),
        }
    }
}

impl Tee {
    /// `body`, wrapped, and the handle on what passes.
    fn wrap(body: Body) -> (Body, Seen) {
        let seen = Seen::default();
        (
            Body::new(Tee {
                body,
                seen: seen.clone(),
            }),
            seen,
        )
    }
}

impl HttpBody for Tee {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let frame = ready!(Pin::new(&mut self.body).poll_frame(cx));
        if let Some(Ok(frame)) = &frame
            && let Some(data) = frame.data_ref()
        {
            self.seen
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(data.clone());
        }
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn frame(body: &mut Capped) -> Option<Result<Frame<Bytes>, axum::Error>> {
        std::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)).await
    }

    /// The frame that crosses the limit yields its permitted prefix, then the error.
    #[tokio::test]
    async fn the_crossing_frame_keeps_its_permitted_prefix() {
        let frames =
            futures_util::stream::iter(["abc", "defgh", "ij"].map(|s| Ok::<_, std::io::Error>(Bytes::from(s))));
        let mut body = Capped {
            body: Body::from_stream(frames),
            left: 5,
            over: false,
        };
        let mut seen = Vec::new();
        let error = loop {
            match frame(&mut body).await {
                Some(Ok(frame)) => seen.push(frame.into_data().unwrap()),
                Some(Err(error)) => break error,
                None => panic!("the body ended without the limit error"),
            }
        };
        assert_eq!(seen, ["abc", "de"]);
        assert!(is_too_large(&error));
        assert!(matches!(frame(&mut body).await, Some(Err(_))), "the error is sticky");
    }
}
