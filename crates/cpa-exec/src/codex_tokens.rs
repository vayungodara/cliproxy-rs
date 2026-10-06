//! Local input-token counting for Codex request bodies (codex_executor_tokens.go): the
//! text a Responses request carries, joined and counted with tiktoken.

use cpa_common::gostr::trim_space;
use cpa_common::json::{self as gj, Kind, Res};

use crate::tokenizer::{Encoding, encoder};

/// `tokenizerForCodexModel`: o200k for GPT-5, GPT-4.1 and GPT-4o, cl100k otherwise.
pub(crate) fn encoding_for_model(model: &str) -> Encoding {
    let model = model.trim().to_lowercase();
    if ["gpt-5", "gpt-4.1", "gpt-4o"].iter().any(|p| model.starts_with(p)) {
        Encoding::O200kBase
    } else {
        Encoding::Cl100kBase
    }
}

/// `countCodexInputTokens`: instructions, message text, function calls and outputs, other
/// items' `text`, tool names, descriptions and parameters, and the text format's name
/// and schema, each trimmed, joined with newlines.
pub(crate) fn count_input_tokens(encoding: Encoding, body: &[u8]) -> Result<i64, String> {
    if body.is_empty() {
        return Ok(0);
    }
    let mut segments: Vec<&[u8]> = Vec::new();
    let mut owned: Vec<Vec<u8>> = Vec::new();
    let mut push = |s: Vec<u8>| owned.push(s);
    // `params.Raw`, or the string value for a string.
    let raw_or_string = |v: &Res<'_>| {
        if v.kind == Kind::String {
            v.bytes().into_owned()
        } else {
            v.raw.to_vec()
        }
    };
    let root = gj::parse(body);
    push(root.get("instructions").bytes().into_owned());
    let input = root.get("input");
    if input.is_array() {
        for item in input.array() {
            match &*item.get("type").bytes() {
                b"message" => {
                    let content = item.get("content");
                    if content.is_array() {
                        for part in content.array() {
                            push(part.get("text").bytes().into_owned());
                        }
                    }
                }
                b"function_call" => {
                    push(item.get("name").bytes().into_owned());
                    push(item.get("arguments").bytes().into_owned());
                }
                b"function_call_output" => push(item.get("output").bytes().into_owned()),
                _ => push(item.get("text").bytes().into_owned()),
            }
        }
    }
    let tools = root.get("tools");
    if tools.is_array() {
        for tool in tools.array() {
            push(tool.get("name").bytes().into_owned());
            push(tool.get("description").bytes().into_owned());
            let params = tool.get("parameters");
            if params.exists() {
                push(raw_or_string(&params));
            }
        }
    }
    let format = root.get("text.format");
    if format.exists() {
        push(format.get("name").bytes().into_owned());
        let schema = format.get("schema");
        if schema.exists() {
            push(raw_or_string(&schema));
        }
    }
    segments.extend(owned.iter().map(|s| trim_space(s)).filter(|s| !s.is_empty()));
    let text = String::from_utf8_lossy(&segments.join(&b'\n')).into_owned();
    if text.is_empty() {
        return Ok(0);
    }
    Ok(encoder(encoding)?.encode_ordinary(&text).len() as i64)
}

#[cfg(test)]
#[path = "codex_tokens_tests.rs"]
mod tests;
