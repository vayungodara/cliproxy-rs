//! OpenAI Chat Completions -> OpenAI Chat Completions normalization
//! (internal/translator/openai/openai/chat-completions). Byte-oriented: bodies pass
//! through untouched apart from the model rewrite, whatever bytes they contain.

use crate::{Error, Registered, RequestCtx, ResponseCtx, common::trim_space, stream};
use cpa_common::json::{self as gj, Kind};

pub static PAIR: Registered = registered!(
    OpenAI -> OpenAI,
    request: request,
    non_stream: |_, body| Ok(body.to_vec()),
    go_stream: go_stream,
    token_count: None,
);

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let model = gj::get(body, "model");
    if model.kind == Kind::String && *model.bytes() == *ctx.model.as_bytes() {
        return Ok(body.to_vec());
    }
    let mut out = body.to_vec();
    gj::set_str(&mut out, "model", ctx.model);
    Ok(out)
}

fn go_stream(_: &ResponseCtx<'_>) -> Box<dyn stream::GoStream> {
    Box::new(Passthrough { done: false })
}

struct Passthrough {
    done: bool,
}

impl stream::GoStream for Passthrough {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        if self.done {
            return Ok(vec![]);
        }
        let payload = match line.strip_prefix(b"data:") {
            Some(rest) => trim_space(rest),
            None => line,
        };
        if payload == b"[DONE]" {
            self.done = true;
            return Ok(vec![]);
        }
        Ok(vec![payload.to_vec()])
    }
}
