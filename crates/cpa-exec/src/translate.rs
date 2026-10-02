//! Executor-side use of the fixed translation contract, after provider restoration.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{Pair, RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

/// sdktranslator.TranslateRequest to Claude for `model` (the base model, without a
/// thinking suffix): a registered pair with Go's summary pipeline, else Go's top-level
/// model rewrite. Streaming translation whenever the client is not Claude.
pub(crate) fn request(req: &ExecRequest, model: &str) -> Result<Bytes, ExecError> {
    cpa_translate::translate_request(
        req.source_format,
        Format::Claude,
        &RequestCtx {
            model,
            stream: req.stream || req.source_format != Format::Claude,
        },
        &req.body,
    )
    .map(Bytes::from)
    .map_err(error)
}

pub(crate) async fn response(
    req: ExecRequest,
    translated: Bytes,
    mut response: ExecResponse,
) -> Result<ExecResponse, ExecError> {
    let pair = cpa_translate::pair(req.response_format, Format::Claude);
    if pair.is_none() && req.response_format != Format::Claude {
        return Err(ExecError::local(
            501,
            FailureScope::Request,
            "Claude response translation pair is not registered",
        ));
    }
    if let Some(pair) = pair {
        response.body = transform(pair, req, translated, response.body).await?;
    }
    Ok(response)
}

async fn transform(
    pair: &'static Pair,
    req: ExecRequest,
    translated: Bytes,
    body: ResponseBody,
) -> Result<ResponseBody, ExecError> {
    let context = ResponseCtx {
        model: &req.model,
        original_request: &req.original_body,
        translated_request: &translated,
    };
    if req.stream && req.operation == Operation::Generate {
        match body {
            ResponseBody::Stream(stream) => Ok(ResponseBody::Stream(streaming(stream, (pair.stream)(&context)))),
            ResponseBody::Buffered(_) => Err(ExecError::local(
                502,
                FailureScope::Request,
                "Claude upstream did not send an event stream",
            )),
        }
    } else {
        let body = match body {
            ResponseBody::Buffered(body) => body,
            ResponseBody::Stream(mut stream) => {
                let mut body = BytesMut::new();
                while let Some(event) = stream.next().await {
                    body.extend_from_slice(&event?);
                }
                body.freeze()
            }
        };
        let transform = if req.operation == Operation::CountTokens {
            pair.count_tokens.ok_or_else(|| {
                ExecError::local(501, FailureScope::Request, "Claude count translation is not registered")
            })?
        } else {
            pair.non_stream
        };
        Ok(ResponseBody::Buffered(Bytes::from(
            transform(&context, &body).map_err(error)?,
        )))
    }
}

fn streaming(upstream: ExecStream, translator: Box<dyn StreamTranslator>) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        ready: VecDeque<Bytes>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: VecDeque::new(),
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.ready.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.done {
                    return None;
                }
                let result = match state.upstream.next().await {
                    Some(Ok(event)) => state.translator.event(&event).map_err(error),
                    Some(Err(error)) => Err(error),
                    None => {
                        state.done = true;
                        state.translator.finish().map_err(error)
                    }
                };
                match result {
                    Ok(events) => state.ready.extend(events),
                    Err(error) => {
                        state.done = true;
                        return Some((Err(error), state));
                    }
                }
            }
        },
    )
    .boxed()
}

fn error(error: cpa_translate::Error) -> ExecError {
    ExecError::local(400, FailureScope::Request, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpa_core::exec::Caller;

    fn req(stream: bool, operation: Operation) -> ExecRequest {
        ExecRequest {
            operation,
            source_format: Format::OpenAI,
            response_format: Format::OpenAI,
            requested_model: "requested".into(),
            model: "resolved".into(),
            original_body: Bytes::from_static(b"original"),
            body: Bytes::from_static(b"working"),
            stream,
            alt: None,
            session: None,
            execution_session: None,
            headers: Default::default(),
            caller: Caller {
                principal: "fake-client-key".into(),
                source: "x-api-key",
            },
        }
    }
    fn check_context(context: &ResponseCtx<'_>) {
        assert_eq!(context.model, "resolved");
        assert_eq!(context.original_request, b"original");
        assert_eq!(context.translated_request, b"translated-before-aliases");
    }
    struct Translator;
    impl StreamTranslator for Translator {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            if event == b"skip" {
                return Ok(Vec::new());
            }
            if event == b"bad" {
                return Err(cpa_translate::Error("malformed event".into()));
            }
            Ok(vec![Bytes::from_static(b"first"), Bytes::copy_from_slice(event)])
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            Ok(vec![Bytes::from_static(b"finished")])
        }
    }
    static PAIR: Pair = Pair {
        request: |_, _| unreachable!(),
        non_stream: |context, body| {
            check_context(context);
            assert_eq!(body, b"SSE-oneSSE-two");
            Ok(b"buffered translated".to_vec())
        },
        stream: |context| {
            check_context(context);
            Box::new(Translator)
        },
        count_tokens: Some(|context, body| {
            check_context(context);
            assert_eq!(body, br#"{"input_tokens":7}"#);
            Ok(b"count translated".to_vec())
        }),
    };
    fn stream(items: &[&'static [u8]]) -> ExecStream {
        futures_util::stream::iter(items.iter().map(|v| Ok(Bytes::from_static(v))).collect::<Vec<_>>()).boxed()
    }
    async fn buffered(body: ResponseBody) -> Bytes {
        match body {
            ResponseBody::Buffered(bytes) => bytes,
            ResponseBody::Stream(_) => panic!("expected buffered"),
        }
    }

    #[tokio::test]
    async fn nonstream_buffers_sse_and_count_uses_count_transform() {
        let before_aliases = Bytes::from_static(b"translated-before-aliases");
        let result = transform(
            &PAIR,
            req(false, Operation::Generate),
            before_aliases.clone(),
            ResponseBody::Stream(stream(&[b"SSE-one", b"SSE-two"])),
        )
        .await
        .unwrap();
        assert_eq!(buffered(result).await, "buffered translated");
        let result = transform(
            &PAIR,
            req(false, Operation::CountTokens),
            before_aliases,
            ResponseBody::Buffered(Bytes::from_static(br#"{"input_tokens":7}"#)),
        )
        .await
        .unwrap();
        assert_eq!(buffered(result).await, "count translated");
    }

    #[tokio::test]
    async fn stream_zero_many_finish_and_errors_are_terminal() {
        let body = transform(
            &PAIR,
            req(true, Operation::Generate),
            Bytes::from_static(b"translated-before-aliases"),
            ResponseBody::Stream(stream(&[b"skip", b"one"])),
        )
        .await
        .unwrap();
        let ResponseBody::Stream(result) = body else {
            panic!("expected stream")
        };
        let result: Vec<_> = result.map(Result::unwrap).collect().await;
        assert_eq!(result, ["first", "one", "finished"]);
        let pending = stream(&[b"bad"]).chain(futures_util::stream::pending()).boxed();
        let result: Vec<_> = streaming(pending, Box::new(Translator)).collect().await;
        assert_eq!(
            result.len(),
            1,
            "must never poll pending upstream or finish after a transform error"
        );
        assert!(result[0].is_err());
    }

    #[test]
    fn native_identity_keeps_request_bytes_and_rewrites_only_a_different_model() {
        let mut request = req(false, Operation::Generate);
        request.source_format = Format::Claude;
        request.body = Bytes::from_static(br#"{  "model" : "claude", "messages": [] }"#);
        assert_eq!(super::request(&request, "claude").unwrap(), request.body);
        assert_eq!(
            super::request(&request, "claude-base").unwrap(),
            br#"{  "model" : "claude-base", "messages": [] }"#.as_slice()
        );
    }
}
