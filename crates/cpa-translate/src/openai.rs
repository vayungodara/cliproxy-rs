use crate::{Error, Pair, RequestCtx, StreamTranslator, json};
use bytes::Bytes;

pub static PAIR: Pair = Pair {
    request,
    non_stream: |_, body| Ok(body.to_vec()),
    stream: |_| Box::new(Stream { done: false }),
    count_tokens: None,
};

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    let input = json::text(body)?;
    let model = gjson::get(input, "model");
    if model.kind() == gjson::Kind::String && model.str() == ctx.model {
        return Ok(body.to_vec());
    }
    // sjson rejects a nonnumeric object key at the root of an array.
    if gjson::parse(input).kind() == gjson::Kind::Array {
        return Ok(body.to_vec());
    }
    let mut out = input.to_owned();
    json::set_string(&mut out, "model", ctx.model);
    Ok(out.into_bytes())
}

struct Stream {
    done: bool,
}

impl StreamTranslator for Stream {
    fn event(&mut self, event: &[u8]) -> Result<Vec<Bytes>, Error> {
        if self.done {
            return Ok(vec![]);
        }
        let input = json::text(event)?;
        let mut out = Vec::new();
        let lines: Vec<_> = json::data_lines(input).collect();
        if lines.is_empty() {
            if input == "[DONE]" {
                self.done = true;
                return Ok(vec![]);
            }
            // The public contract accepts frames; accept bare JSON too, like Go.
            out.push(json::frame(input));
        } else {
            for payload in lines {
                if payload == "[DONE]" {
                    self.done = true;
                    break;
                }
                out.push(json::frame(payload));
            }
        }
        Ok(out)
    }

    fn finish(&mut self) -> Result<Vec<Bytes>, Error> {
        Ok(vec![])
    }
}
