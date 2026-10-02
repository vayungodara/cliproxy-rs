//! Gemini generateContent responses -> OpenAI Chat Completions
//! (internal/translator/gemini/openai/chat-completions/gemini_openai_response.go).

use crate::{
    Error, ResponseCtx,
    common::{self, go_lower, go_upper, now_nanos, restore_sanitized_tool_name, trim_space},
    stream::GoStream,
};
use cpa_common::json::{self as gj, Res};
use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

type NameMap = Option<HashMap<Vec<u8>, Vec<u8>>>;

/// functionCallIDCounter.
static FUNCTION_CALL_IDS: AtomicU64 = AtomicU64::new(0);

/// `<name>-<unix nanos>-<process counter>`.
fn function_call_id(name: &[u8]) -> Vec<u8> {
    let n = FUNCTION_CALL_IDS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    [name, format!("-{}-{n}", now_nanos()).as_bytes()].concat()
}

/// Model, creation time, response ID and usage shared by every choice of a chunk.
/// `created` keeps the last parsed `createTime` when the current one does not parse.
fn base(raw: &[u8], template: &[u8], created: &mut i64) -> Vec<u8> {
    let mut out = template.to_vec();
    let version = gj::get(raw, "modelVersion");
    if version.exists() {
        gj::set_str(&mut out, "model", version.bytes());
    }
    let create_time = gj::get(raw, "createTime");
    if create_time.exists()
        && let Some(t) = common::parse_rfc3339_unix(&create_time.bytes())
    {
        *created = t;
    }
    gj::set_int(&mut out, "created", *created);
    let id = gj::get(raw, "responseId");
    if id.exists() {
        gj::set_str(&mut out, "id", id.bytes());
    }
    let usage = gj::get(raw, "usageMetadata");
    if usage.exists() {
        let thoughts = usage.get("thoughtsTokenCount").int();
        gj::set_int(
            &mut out,
            "usage.completion_tokens",
            usage.get("candidatesTokenCount").int().wrapping_add(thoughts),
        );
        let total = usage.get("totalTokenCount");
        if total.exists() {
            gj::set_int(&mut out, "usage.total_tokens", total.int());
        }
        gj::set_int(&mut out, "usage.prompt_tokens", usage.get("promptTokenCount").int());
        if thoughts > 0 {
            gj::set_int(&mut out, "usage.completion_tokens_details.reasoning_tokens", thoughts);
        }
        let cached = usage.get("cachedContentTokenCount").int();
        if cached > 0 {
            gj::set_int(&mut out, "usage.prompt_tokens_details.cached_tokens", cached);
        }
    }
    out
}

/// The part's text: `text`, or a transcription's text when there is none.
fn part_text<'a>(part: &Res<'a>) -> Res<'a> {
    let text = part.get("text");
    let transcription = part.get("audioTranscription");
    if transcription.exists() && !text.exists() {
        return transcription.get("text");
    }
    text
}

fn inline_data<'a>(part: &Res<'a>) -> Res<'a> {
    let data = part.get("inlineData");
    if data.exists() { data } else { part.get("inline_data") }
}

/// The data URL for an inline-data part, or `None` without data.
fn image_url(data: &Res<'_>) -> Option<Vec<u8>> {
    let payload = data.get("data").bytes();
    if payload.is_empty() {
        return None;
    }
    let mut mime = data.get("mimeType").bytes().into_owned();
    if mime.is_empty() {
        mime = data.get("mime_type").bytes().into_owned();
    }
    if mime.is_empty() {
        mime = b"image/png".to_vec();
    }
    Some([&b"data:"[..], &mime, b";base64,", &payload].concat())
}

fn image_payload(index: usize, url: &[u8]) -> Vec<u8> {
    let mut payload = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
    gj::set_int(&mut payload, "index", index as i64);
    gj::set_str(&mut payload, "image_url.url", url);
    payload
}

pub fn go_stream(ctx: &ResponseCtx<'_>) -> Box<dyn GoStream> {
    Box::new(State {
        names: common::sanitized_tool_name_map(ctx.original_request),
        ..State::default()
    })
}

#[derive(Default)]
struct State {
    created: i64,
    function_index: HashMap<i64, i64>,
    saw_tool_call: HashMap<i64, bool>,
    finish: HashMap<i64, Vec<u8>>,
    names: NameMap,
}

const CHUNK: &[u8] = br#"{"id":"","object":"chat.completion.chunk","created":12345,"model":"model","choices":[{"index":0,"delta":{"role":null,"content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}]}"#;

impl GoStream for State {
    fn line(&mut self, line: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
        let raw = match line.strip_prefix(b"data:") {
            Some(rest) => trim_space(rest),
            None => line,
        };
        if raw == b"[DONE]" {
            return Ok(vec![]);
        }
        let base = base(raw, CHUNK, &mut self.created);
        let candidates = gj::get(raw, "candidates");
        let has_usage = gj::get(raw, "usageMetadata").exists();
        if !candidates.is_array() {
            return Ok(if has_usage { vec![base] } else { vec![] });
        }
        let mut out = vec![];
        candidates.each(|_, candidate| {
            out.push(self.candidate(&base, &candidate, has_usage));
            true
        });
        Ok(out)
    }
}

impl State {
    fn candidate(&mut self, base: &[u8], candidate: &Res<'_>, has_usage: bool) -> Vec<u8> {
        let mut t = base.to_vec();
        let index = candidate.get("index").int();
        gj::set_int(&mut t, "choices.0.index", index);
        let finish = candidate.get("finishReason");
        if finish.exists() {
            self.finish.insert(index, go_upper(&finish.bytes()));
        }
        let mut role_set = false;
        let mut set_role = |t: &mut Vec<u8>| {
            if !role_set {
                gj::set_str(t, "choices.0.delta.role", "assistant");
                role_set = true;
            }
        };
        let parts = candidate.get("content.parts");
        if parts.is_array() {
            for part in parts.array() {
                let text = part_text(&part);
                let call = part.get("functionCall");
                let data = inline_data(&part);
                let mut signature = part.get("thoughtSignature");
                if !signature.exists() {
                    signature = part.get("thought_signature");
                }
                let has_signature = signature.exists() && !signature.bytes().is_empty();
                if has_signature && !(text.exists() || call.exists() || data.exists()) {
                    continue;
                }
                if text.exists() {
                    set_role(&mut t);
                    let path = if part.get("thought").bool() {
                        "choices.0.delta.reasoning_content"
                    } else {
                        "choices.0.delta.content"
                    };
                    gj::set_str(&mut t, path, text.bytes());
                } else if call.exists() {
                    self.saw_tool_call.insert(index, true);
                    let counter = self.function_index.entry(index).or_default();
                    let mut call_index = *counter;
                    *counter += 1;
                    let calls = gj::get(&t, "choices.0.delta.tool_calls");
                    if calls.exists() && calls.is_array() {
                        call_index = calls.array().len() as i64;
                    } else {
                        gj::set_raw(&mut t, "choices.0.delta.tool_calls", b"[]");
                    }
                    let name = restore_sanitized_tool_name(self.names.as_ref(), &call.get("name").bytes());
                    let mut item =
                        br#"{"id":"","index":0,"type":"function","function":{"name":"","arguments":""}}"#.to_vec();
                    gj::set_str(&mut item, "id", function_call_id(&name));
                    gj::set_int(&mut item, "index", call_index);
                    gj::set_str(&mut item, "function.name", &name);
                    let args = call.get("args");
                    if args.exists() {
                        gj::set_str(&mut item, "function.arguments", &args.raw);
                    }
                    set_role(&mut t);
                    gj::set_raw(&mut t, "choices.0.delta.tool_calls.-1", item);
                } else if data.exists() {
                    let Some(url) = image_url(&data) else {
                        continue;
                    };
                    let images = gj::get(&t, "choices.0.delta.images");
                    if !images.exists() || !images.is_array() {
                        gj::set_raw(&mut t, "choices.0.delta.images", b"[]");
                    }
                    let count = gj::get(&t, "choices.0.delta.images").array().len();
                    set_role(&mut t);
                    gj::set_raw(&mut t, "choices.0.delta.images.-1", image_payload(count, &url));
                }
            }
        }
        let upstream = self.finish.get(&index).cloned().unwrap_or_default();
        if !upstream.is_empty() && has_usage {
            let reason = if self.saw_tool_call.get(&index).copied().unwrap_or(false) {
                "tool_calls"
            } else if upstream == b"MAX_TOKENS" {
                "max_tokens"
            } else {
                "stop"
            };
            gj::set_str(&mut t, "choices.0.finish_reason", reason);
            gj::set_str(&mut t, "choices.0.native_finish_reason", go_lower(&upstream));
        }
        t
    }
}

/// ConvertGeminiResponseToOpenAINonStream.
pub fn non_stream(ctx: &ResponseCtx<'_>, raw: &[u8]) -> Result<Vec<u8>, Error> {
    let names = common::sanitized_tool_name_map(ctx.original_request);
    let mut created = 0;
    let mut out = base(
        raw,
        br#"{"id":"","object":"chat.completion","created":123456,"model":"model","choices":[]}"#,
        &mut created,
    );
    let candidates = gj::get(raw, "candidates");
    if !candidates.is_array() {
        return Ok(out);
    }
    let mut choices = vec![];
    candidates.each(|_, candidate| {
        let mut choice = br#"{"index":0,"message":{"role":"assistant","content":null,"reasoning_content":null,"tool_calls":null},"finish_reason":null,"native_finish_reason":null}"#.to_vec();
        gj::set_int(&mut choice, "index", candidate.get("index").int());
        let finish = candidate.get("finishReason");
        if finish.exists() {
            let reason = go_lower(&finish.bytes());
            gj::set_str(&mut choice, "finish_reason", &reason);
            gj::set_str(&mut choice, "native_finish_reason", &reason);
        }
        let parts = candidate.get("content.parts");
        let mut has_call = false;
        if parts.is_array() {
            let (mut calls, mut images) = (vec![], vec![]);
            let (mut text, mut reasoning) = (None::<Vec<u8>>, None::<Vec<u8>>);
            for part in parts.array() {
                let part_text = part_text(&part);
                let call = part.get("functionCall");
                let data = inline_data(&part);
                if part_text.exists() {
                    let target = if part.get("thought").bool() { &mut reasoning } else { &mut text };
                    target.get_or_insert_default().extend_from_slice(&part_text.bytes());
                } else if call.exists() {
                    has_call = true;
                    let name = restore_sanitized_tool_name(names.as_ref(), &call.get("name").bytes());
                    let mut item = br#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
                    gj::set_str(&mut item, "id", function_call_id(&name));
                    gj::set_str(&mut item, "function.name", &name);
                    let args = call.get("args");
                    if args.exists() {
                        gj::set_str(&mut item, "function.arguments", &args.raw);
                    }
                    calls.push(item);
                } else if data.exists()
                    && let Some(url) = image_url(&data)
                {
                    images.push(image_payload(images.len(), &url));
                }
            }
            if let Some(text) = text {
                gj::set_str(&mut choice, "message.content", text);
            }
            if let Some(reasoning) = reasoning {
                gj::set_str(&mut choice, "message.reasoning_content", reasoning);
            }
            if !calls.is_empty() {
                gj::set_raw(&mut choice, "message.tool_calls", gj::join(&calls));
            }
            if !images.is_empty() {
                gj::set_raw(&mut choice, "message.images", gj::join(&images));
            }
        }
        if has_call {
            gj::set_str(&mut choice, "finish_reason", "tool_calls");
            gj::set_str(&mut choice, "native_finish_reason", "tool_calls");
        }
        choices.push(choice);
        true
    });
    gj::set_items(&mut out, "choices", &choices);
    Ok(out)
}
