//! Turning a wreq response into the execution envelope. Shared by provider executors.

use std::time::Duration;

use bytes::Bytes;
use cpa_core::exec::{ExecError, ExecResponse, FailureScope, ResponseBody};
use cpa_translate::sse::Framer;
use futures_util::StreamExt;
use http::HeaderMap;

/// Largest upstream error body kept; the rest is dropped.
const MAX_ERROR_BODY: usize = 64 * 1024;

pub(crate) fn transport_error(e: wreq::Error) -> ExecError {
    ExecError::local(
        502,
        FailureScope::Credential,
        format!("upstream request failed: {e}"),
    )
}

/// Non-2xx responses become [`ExecError`]; event streams are framed; anything else is
/// buffered.
pub(crate) async fn into_response(res: wreq::Response) -> Result<ExecResponse, ExecError> {
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    if !(200..300).contains(&status) {
        let mut body = res.bytes().await.unwrap_or_default();
        body.truncate(MAX_ERROR_BODY);
        return Err(ExecError {
            status,
            scope: scope_for(status),
            body,
            retry_after: retry_after(&headers),
        });
    }
    let is_sse = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"));
    let body = if is_sse {
        ResponseBody::Stream(framed(res).boxed())
    } else {
        ResponseBody::Buffered(res.bytes().await.map_err(transport_error)?)
    };
    Ok(ExecResponse {
        status,
        headers,
        body,
    })
}

fn framed(res: wreq::Response) -> impl futures_util::Stream<Item = Result<Bytes, ExecError>> {
    futures_util::stream::unfold(
        (
            res.bytes_stream(),
            Framer::default(),
            std::collections::VecDeque::new(),
            false,
        ),
        |(mut body, mut framer, mut ready, mut done)| async move {
            loop {
                if let Some(event) = ready.pop_front() {
                    return Some((Ok(event), (body, framer, ready, done)));
                }
                if done {
                    return None;
                }
                match body.next().await {
                    Some(Ok(chunk)) => ready.extend(framer.push(&chunk)),
                    Some(Err(e)) => {
                        done = true;
                        return Some((Err(transport_error(e)), (body, framer, ready, done)));
                    }
                    None => {
                        done = true;
                        ready.extend(framer.finish());
                    }
                }
            }
        },
    )
}

// ponytail: status-only classification. The scheduler port (sdk/cliproxy/auth, custom
// request-error rules) replaces this with per-provider and per-credential rules.
fn scope_for(status: u16) -> FailureScope {
    match status {
        401 | 402 | 403 | 408 | 429 | 500.. => FailureScope::Credential,
        _ => FailureScope::Request,
    }
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let secs = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs))
}
