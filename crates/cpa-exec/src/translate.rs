//! Executor-side use of the fixed translation contract, after provider restoration.

use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use cpa_core::exec::{ExecError, ExecRequest, ExecResponse, ExecStream, FailureScope, Operation, ResponseBody};
use cpa_core::format::Format;
use cpa_translate::{Pair, RequestCtx, ResponseCtx, StreamTranslator};
use futures_util::StreamExt;

use crate::openai_compat_payload::ensure_responses_usage_details;

/// `TranslateRequestWithAPIKeyModelCompatibilityForExecutor` to Claude for `model` (the
/// base model, without a thinking suffix), through the shared Codex client rewrites
/// ([`crate::codex_client::translate_request`]): Codex integer tool types, the Responses
/// orphan-delegation and multi-agent v2 rewrites, Go's `...WithCompat` translators for an
/// is-compat model, else `sdktranslator.TranslateRequest` (a registered pair with the
/// summary pipeline, or Go's top-level model rewrite). Streaming translation whenever
/// the client is not Claude.
pub(crate) fn request(
    req: &ExecRequest,
    codex: cpa_common::codex_client::Settings,
    model: &str,
    is_compat: bool,
) -> Result<Bytes, ExecError> {
    translate_body(req, &req.body, codex, model, is_compat)
}

/// Go `originalTranslated` from `TranslateRequestPairWithAPIKeyModelCompatibility`: the
/// client's original payload translated like the working one (`translated`), which
/// payload rules read for `original` conditions.
pub(crate) fn original(
    req: &ExecRequest,
    translated: &Bytes,
    codex: cpa_common::codex_client::Settings,
    model: &str,
    is_compat: bool,
) -> Result<Bytes, ExecError> {
    // Go reuses the translation when both payloads are the same slice; equal bytes
    // translate identically.
    if req.original_body.is_empty() || req.original_body == req.body {
        return Ok(translated.clone());
    }
    translate_body(req, &req.original_body, codex, model, is_compat)
}

fn translate_body(
    req: &ExecRequest,
    body: &[u8],
    codex: cpa_common::codex_client::Settings,
    model: &str,
    is_compat: bool,
) -> Result<Bytes, ExecError> {
    let ctx = RequestCtx {
        model,
        stream: req.stream || req.source_format != Format::Claude,
    };
    let client = crate::codex_client::Client {
        headers: &req.headers,
        settings: codex,
        target_executor: "claude",
        is_compat,
    };
    crate::codex_client::translate_request(req.source_format, Format::Claude, &ctx, body, &client)
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
    // EnsureResponsesUsageDetails on every translated Responses payload.
    let responses = req.response_format == Format::OpenAIResponse && req.operation == Operation::Generate;
    if req.stream && req.operation == Operation::Generate {
        match body {
            ResponseBody::Stream(stream) => Ok(ResponseBody::Stream(streaming(
                stream,
                (pair.stream)(&context),
                responses,
                req.usage.clone(),
            ))),
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
        if req.operation == Operation::CountTokens {
            let transform = pair.count_tokens.ok_or_else(|| {
                ExecError::local(501, FailureScope::Request, "Claude count translation is not registered")
            })?;
            return Ok(ResponseBody::Buffered(Bytes::from(
                transform(&context, &body).map_err(error)?,
            )));
        }
        // ApplyPatchTranslationError or an empty translation: Go's sanitized 502, whose
        // deferred TrackFailure publishes no tokens.
        let out = (pair.non_stream)(&context, &body)
            .ok()
            .filter(|out| !out.is_empty())
            .ok_or_else(|| {
                publish_apply_patch_failure(&req.usage);
                apply_patch_error()
            })?;
        Ok(ResponseBody::Buffered(Bytes::from(if responses {
            ensure_responses_usage_details(&out)
        } else {
            out
        })))
    }
}

/// Go `reporter.PublishFailure(statusErr{502, ApplyPatchUpstreamErrorMessage})`: the
/// rejection publishes no tokens, even after stream usage was seen.
fn publish_apply_patch_failure(usage: &cpa_core::exec::UsageSink) {
    usage.publish_failure(502, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
}

/// helps.ApplyPatchUpstreamErrorMessage with Go's 502 `statusErr`: a plain status error,
/// so the conductor classifies it by status (it is not request-scoped) and fails over.
fn apply_patch_error() -> ExecError {
    ExecError::local(
        502,
        crate::upstream::scope_for(502),
        cpa_translate::APPLY_PATCH_UPSTREAM_ERROR,
    )
}

/// Go's translated stream loop (claude_executor_stream.go): each upstream event goes
/// through the translator; Responses frames get usage details unless apply_patch input
/// failed, and a failure ends the stream with a 502 after that event's frames
/// (StopApplyPatchStream). When the transport ends, cleanly or not, tool input is
/// finalized first (EndApplyPatchStream); a failure there replaces any transport error.
/// An apply_patch failure publishes no tokens (RecordApplyPatchStreamFailure); other
/// failures keep the stream usage seen so far.
fn streaming(
    upstream: ExecStream,
    translator: Box<dyn StreamTranslator>,
    responses: bool,
    usage: cpa_core::exec::UsageSink,
) -> ExecStream {
    struct State {
        upstream: ExecStream,
        translator: Box<dyn StreamTranslator>,
        responses: bool,
        usage: cpa_core::exec::UsageSink,
        ready: VecDeque<Bytes>,
        error: Option<ExecError>,
        done: bool,
    }
    impl State {
        fn emit(&mut self, events: Vec<Bytes>) {
            let usage = self.responses && !self.translator.tool_input_failed();
            self.ready.extend(events.into_iter().map(|e| {
                if usage {
                    Bytes::from(ensure_responses_usage_details(&e))
                } else {
                    e
                }
            }));
        }

        /// Ends the stream with `error`, after the Responses frame still being joined
        /// (Go's responsesSSEFramer.Flush).
        fn fail(&mut self, error: ExecError) {
            let pending = self.translator.flush_frames();
            self.ready.extend(pending);
            self.error = Some(error);
            self.done = true;
        }

        /// The transport ended: `None` cleanly, else with its error.
        fn end(&mut self, transport: Option<ExecError>) {
            self.done = true;
            let finalized = self.translator.finalize_tool_input();
            self.emit(finalized);
            if self.translator.tool_input_failed() {
                publish_apply_patch_failure(&self.usage);
                return self.fail(apply_patch_error());
            }
            if let Some(error) = transport {
                return self.fail(error);
            }
            match self.translator.finish() {
                Ok(events) => self.emit(events),
                Err(e) => self.fail(stream_error(e)),
            }
        }
    }
    futures_util::stream::unfold(
        State {
            upstream,
            translator,
            responses,
            usage,
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
                match state.upstream.next().await {
                    Some(Ok(event)) => match state.translator.event(&event) {
                        Ok(events) => {
                            state.emit(events);
                            if state.translator.tool_input_failed() {
                                publish_apply_patch_failure(&state.usage);
                                state.error = Some(apply_patch_error());
                                state.done = true;
                            }
                        }
                        Err(e) => state.fail(stream_error(e)),
                    },
                    Some(Err(error)) => state.end(Some(error)),
                    None => state.end(None),
                }
            }
        },
    )
    .boxed()
}

/// A response translator rejecting upstream data: a bad gateway, never the client's fault,
/// classified by status like Go's plain `statusErr`.
fn stream_error(error: cpa_translate::Error) -> ExecError {
    ExecError::local(502, crate::upstream::scope_for(502), error.to_string())
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
        let result: Vec<_> = streaming(pending, Box::new(Translator), false, Default::default())
            .collect()
            .await;
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
        let result: Vec<_> = streaming(failing, Box::<Framer>::default(), false, Default::default())
            .collect()
            .await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap(), "frame");
        assert_eq!(result[1].as_ref().unwrap_err().status, 502);
        // A translator error flushes the same way.
        let result: Vec<_> = streaming(
            stream(&[b"frame", b"bad"]),
            Box::<Framer>::default(),
            false,
            Default::default(),
        )
        .collect()
        .await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].as_ref().unwrap(), "frame");
        assert!(result[1].is_err());
    }

    /// Fails apply_patch tool input on the event `patch`.
    #[derive(Default)]
    struct PatchFail(bool);
    impl StreamTranslator for PatchFail {
        fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
            if event == b"bad" {
                return Err(cpa_translate::Error("malformed event".into()));
            }
            self.0 |= event == b"patch";
            Ok(vec![Bytes::copy_from_slice(event)])
        }
        fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
            Ok(Vec::new())
        }
        fn tool_input_failed(&self) -> bool {
            self.0
        }
    }

    /// Counts `UsageObserver::publish_failure` reports, checking Go's status and body.
    #[derive(Default)]
    struct Failures(std::sync::atomic::AtomicUsize);
    impl cpa_core::exec::UsageObserver for Failures {
        fn response_body(&self, _: Format, _: &[u8]) {}
        fn response_line(&self, _: Format, _: &[u8]) {}
        fn request(&self, _: Format, _: &[u8]) {}
        fn publish_failure(&self, status: u16, body: &str) {
            assert_eq!((status, body), (502, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR));
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    impl Failures {
        fn sink() -> (std::sync::Arc<Self>, cpa_core::exec::UsageSink) {
            let failures = std::sync::Arc::new(Self::default());
            (failures.clone(), cpa_core::exec::UsageSink::new(failures))
        }
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// Go returns these as plain 502 `statusErr`s, not request-scoped errors: the
    /// conductor fails over to the next credential instead of answering the caller.
    /// Only the apply_patch rejection publishes its usage as empty
    /// (RecordApplyPatchStreamFailure); a transport error keeps the stream's usage.
    #[tokio::test]
    async fn apply_patch_and_translator_failures_are_not_request_scoped() {
        let (failures, usage) = Failures::sink();
        let pending = stream(&[b"patch"]).chain(futures_util::stream::pending()).boxed();
        let result: Vec<_> = streaming(pending, Box::<PatchFail>::default(), false, usage)
            .collect()
            .await;
        assert_eq!(result.len(), 2, "the failing event's frames, then the error");
        let error = result[1].as_ref().unwrap_err();
        assert_eq!(error.status, 502);
        assert_eq!(error.body, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
        assert_eq!(error.scope, FailureScope::Credential);
        assert_eq!(failures.count(), 1);
        let (failures, usage) = Failures::sink();
        let result: Vec<_> = streaming(stream(&[b"bad"]), Box::<PatchFail>::default(), false, usage)
            .collect()
            .await;
        assert_eq!(result[0].as_ref().unwrap_err().scope, FailureScope::Credential);
        assert_eq!(apply_patch_error().scope, FailureScope::Credential);
        assert_eq!(failures.count(), 0);
        let (failures, usage) = Failures::sink();
        let reset = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"frame")),
            Err(ExecError::local(502, FailureScope::Transport, "upstream reset")),
        ])
        .boxed();
        let result: Vec<_> = streaming(reset, Box::<PatchFail>::default(), false, usage)
            .collect()
            .await;
        assert_eq!(result[1].as_ref().unwrap_err().scope, FailureScope::Transport);
        assert_eq!(failures.count(), 0);
    }

    /// Finalizing tool input at EOF can fail too; that failure replaces the transport
    /// error and publishes no tokens.
    #[tokio::test]
    async fn apply_patch_failure_at_eof_replaces_transport_error() {
        #[derive(Default)]
        struct FailsAtEnd(bool);
        impl StreamTranslator for FailsAtEnd {
            fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, cpa_translate::Error> {
                Ok(vec![Bytes::copy_from_slice(event)])
            }
            fn finish(&mut self) -> Result<Vec<Bytes>, cpa_translate::Error> {
                Ok(Vec::new())
            }
            fn finalize_tool_input(&mut self) -> Vec<Bytes> {
                self.0 = true;
                vec![Bytes::from_static(b"finalized")]
            }
            fn tool_input_failed(&self) -> bool {
                self.0
            }
        }
        let (failures, usage) = Failures::sink();
        let reset = futures_util::stream::iter(vec![
            Ok(Bytes::from_static(b"frame")),
            Err(ExecError::local(502, FailureScope::Transport, "upstream reset")),
        ])
        .boxed();
        let result: Vec<_> = streaming(reset, Box::<FailsAtEnd>::default(), false, usage)
            .collect()
            .await;
        assert_eq!(result.len(), 3);
        assert_eq!(result[1].as_ref().unwrap(), "finalized");
        let error = result[2].as_ref().unwrap_err();
        assert_eq!(error.body, cpa_translate::APPLY_PATCH_UPSTREAM_ERROR);
        assert_eq!(error.scope, FailureScope::Credential);
        assert_eq!(failures.count(), 1);
    }

    #[tokio::test]
    async fn nonstream_apply_patch_rejection_publishes_no_tokens() {
        static REJECTS: Pair = Pair {
            request: |_, _| unreachable!(),
            non_stream: |_, _| Ok(Vec::new()),
            stream: |_| unreachable!(),
            count_tokens: None,
        };
        let (failures, usage) = Failures::sink();
        let mut request = req(false, Operation::Generate);
        request.usage = usage;
        let error = transform(
            &REJECTS,
            request,
            Bytes::new(),
            ResponseBody::Buffered(Bytes::from_static(b"{}")),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.status, 502);
        assert_eq!(error.scope, FailureScope::Credential);
        assert_eq!(failures.count(), 1);
    }

    #[test]
    fn native_identity_keeps_request_bytes_and_rewrites_only_a_different_model() {
        let mut request = req(false, Operation::Generate);
        request.source_format = Format::Claude;
        request.body = Bytes::from_static(br#"{  "model" : "claude", "messages": [] }"#);
        assert_eq!(
            super::request(&request, Default::default(), "claude", false).unwrap(),
            request.body
        );
        assert_eq!(
            super::request(&request, Default::default(), "claude-base", false).unwrap(),
            br#"{  "model" : "claude-base", "messages": [] }"#.as_slice()
        );
    }
}
