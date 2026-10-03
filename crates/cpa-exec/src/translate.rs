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
///
/// With `is_compat` (an is-compat API-key model), OpenAI Chat and Responses clients use
/// Go's `...WithCompat` translators, which keep unsigned reasoning history, between the
/// same summary extraction and application
/// (`TranslateRequestWithAPIKeyModelCompatibilityForExecutor`).
// ponytail: the Codex orphan-delegation and multi-agent v2 input rewrites Go applies to
// compat Responses payloads first are cpa_common::codex_client's (Codex thread).
pub(crate) fn request(req: &ExecRequest, model: &str, is_compat: bool) -> Result<Bytes, ExecError> {
    translate_body(req, &req.body, model, is_compat)
}

/// Go `originalTranslated` from `TranslateRequestPairWithAPIKeyModelCompatibility`: the
/// client's original payload translated like the working one (`translated`), which
/// payload rules read for `original` conditions.
pub(crate) fn original(
    req: &ExecRequest,
    translated: &Bytes,
    model: &str,
    is_compat: bool,
) -> Result<Bytes, ExecError> {
    // Go reuses the translation when both payloads are the same slice; equal bytes
    // translate identically.
    if req.original_body.is_empty() || req.original_body == req.body {
        return Ok(translated.clone());
    }
    translate_body(req, &req.original_body, model, is_compat)
}

fn translate_body(req: &ExecRequest, body: &[u8], model: &str, is_compat: bool) -> Result<Bytes, ExecError> {
    // TranslateRequestWithAPIKeyModelCompatibilityForExecutor: Codex clients' integer
    // tool schemas are normalized before any translation to a non-Codex executor.
    let normalized = cpa_common::payload::normalize_codex_tool_integer_types(body, &req.headers);
    let body = normalized.as_slice();
    let ctx = RequestCtx {
        model,
        stream: req.stream || req.source_format != Format::Claude,
    };
    let compat: Option<cpa_translate::RequestFn> = match req.source_format {
        Format::OpenAI if is_compat => Some(cpa_translate::openai_to_claude_with_compat),
        Format::OpenAIResponse if is_compat => Some(cpa_translate::responses_to_claude_with_compat),
        _ => None,
    };
    let Some(translate) = compat else {
        return cpa_translate::translate_request(req.source_format, Format::Claude, &ctx, body)
            .map(Bytes::from)
            .map_err(error);
    };
    use cpa_common::thinking::{apply_summary_config_for_model, extract_translated_summary_config};
    let (from, to) = (req.source_format.as_str(), Format::Claude.as_str());
    let summary = extract_translated_summary_config(body, from, to);
    let translated = translate(&ctx, body).map_err(error)?;
    Ok(Bytes::from(apply_summary_config_for_model(
        &translated,
        to,
        model,
        summary,
    )))
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
        error: Option<ExecError>,
        done: bool,
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            ready: VecDeque::new(),
            error: None,
            done: false,
        },
        |mut state| async move {
            loop {
                if let Some(event) = state.ready.pop_front() {
                    return Some((Ok(event), state));
                }
                if let Some(error) = state.error.take() {
                    return Some((Err(error), state));
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
                        // Go's responsesSSEFramer.Flush: a Responses client gets the frame
                        // still being joined before the terminal error.
                        state.ready.extend(state.translator.flush_frames());
                        state.error = Some(error);
                        state.done = true;
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
            derived_session: None,
            resolved_model: None,
            usage: Default::default(),
            request_path: String::new(),
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

    /// Holds each event as a pending frame, like Go's responsesSSEFramer.
    #[derive(Default)]
    struct Framer(Vec<Bytes>);
    impl StreamTranslator for Framer {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            if event == b"bad" {
                return Err(cpa_translate::Error("malformed event".into()));
            }
            self.0.push(Bytes::copy_from_slice(event));
            Ok(Vec::new())
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            Ok(std::mem::take(&mut self.0))
        }
        fn flush_frames(&mut self) -> Vec<Bytes> {
            std::mem::take(&mut self.0)
        }
    }

    #[tokio::test]
    async fn pending_frames_flush_before_a_terminal_error() {
        let failing = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"frame")),
            Err(ExecError::local(502, FailureScope::Transport, "upstream reset")),
        ])
        .chain(futures_util::stream::pending())
        .boxed();
        let result: Vec<_> = streaming(failing, Box::<Framer>::default()).collect().await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap(), "frame");
        assert_eq!(result[1].as_ref().unwrap_err().status, 502);
        // A translator error flushes the same way.
        let result: Vec<_> = streaming(stream(&[b"frame", b"bad"]), Box::<Framer>::default())
            .collect()
            .await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap(), "frame");
        assert!(result[1].is_err());
    }

    #[test]
    fn native_identity_keeps_request_bytes_and_rewrites_only_a_different_model() {
        let mut request = req(false, Operation::Generate);
        request.source_format = Format::Claude;
        request.body = Bytes::from_static(br#"{  "model" : "claude", "messages": [] }"#);
        assert_eq!(super::request(&request, "claude", false).unwrap(), request.body);
        assert_eq!(
            super::request(&request, "claude-base", false).unwrap(),
            br#"{  "model" : "claude-base", "messages": [] }"#.as_slice()
        );
    }
}
