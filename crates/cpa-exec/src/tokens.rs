//! Custom-origin count_tokens: Go's O200kBase estimate, without generation cloaking.

use bytes::Bytes;
use cpa_core::exec::{ExecError, FailureScope};
use serde_json::Value;

pub(crate) fn first_party(base: &str) -> bool {
    let authority = base
        .trim()
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or_default());
    if authority.is_none_or(|s| s.contains('@')) {
        return false;
    }
    url::Url::parse(base.trim()).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.anthropic.com")
            && url.port().is_none_or(|port| port == 443)
            && url.username().is_empty()
            && url.password().is_none()
    })
}

pub(crate) fn count(body: &[u8]) -> Result<Bytes, ExecError> {
    let root: Value = serde_json::from_slice(body).map_err(|_| invalid("invalid Claude token count request JSON"))?;
    if !root.is_object() {
        return Err(invalid("Claude token count request must be a JSON object"));
    }
    let messages = root["messages"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| invalid("Claude token count request messages must be a non-empty array"))?;
    for message in messages {
        if !message.is_object() {
            return Err(invalid("Claude token count request messages must contain objects"));
        }
        if !matches!(message["role"].as_str(), Some("user" | "assistant")) {
            return Err(invalid(
                "Claude token count request message role must be user or assistant",
            ));
        }
        if !message["content"].is_string() {
            let content = message["content"]
                .as_array()
                .ok_or_else(|| invalid("Claude token count request message content must be a string or array"))?;
            if content
                .iter()
                .any(|v| !v.is_object() || v["type"].as_str().is_none_or(str::is_empty))
            {
                return Err(invalid(
                    "Claude token count request content blocks must be typed objects",
                ));
            }
        }
    }
    let mut segments = Vec::new();
    let system = &root["system"];
    if system.is_string() {
        text(&mut segments, system);
    } else if let Some(parts) = system.as_array() {
        for part in parts {
            if part.is_string() {
                text(&mut segments, part);
            } else if part["type"] == "text" {
                text(&mut segments, &part["text"]);
            }
        }
    }
    for message in messages {
        text(&mut segments, &message["role"]);
        content(&mut segments, &message["content"]);
    }
    if let Some(tools) = root["tools"].as_array() {
        for tool in tools {
            fields(&mut segments, tool, &["type", "name", "description"]);
            compact(&mut segments, tool.get("input_schema"));
        }
    }
    let choice = &root["tool_choice"];
    if choice.is_string() {
        text(&mut segments, choice);
    } else {
        fields(&mut segments, choice, &["type", "name"]);
    }
    let encoder = crate::tokenizer::encoder(crate::tokenizer::Encoding::O200kBase)
        .map_err(|_| ExecError::local(500, FailureScope::Request, "cannot initialize O200kBase tokenizer"))?;
    let count = encoder.encode_ordinary(&segments.join("\n")).len();
    Ok(Bytes::from(format!(r#"{{"input_tokens":{count}}}"#)))
}

fn content(segments: &mut Vec<String>, value: &Value) {
    if value.is_string() {
        text(segments, value);
        return;
    }
    if let Some(parts) = value.as_array() {
        for part in parts {
            content(segments, part);
        }
        return;
    }
    if !value.is_object() {
        return;
    }
    match value["type"].as_str().unwrap_or_default() {
        "text" => text(segments, &value["text"]),
        "thinking" => text(segments, &value["thinking"]),
        "document" if value["source"]["type"] == "text" => {
            fields(segments, value, &["title", "context"]);
            fields(segments, &value["source"], &["data", "content"]);
        }
        "document" | "image" | "input_audio" | "audio" | "video" | "redacted_thinking" => {}
        "tool_use" | "server_tool_use" | "mcp_tool_use" => {
            fields(segments, value, &["id", "name"]);
            compact(segments, value.get("input"));
        }
        "tool_result"
        | "mcp_tool_result"
        | "web_search_tool_result"
        | "web_fetch_tool_result"
        | "code_execution_tool_result"
        | "bash_code_execution_tool_result"
        | "text_editor_code_execution_tool_result" => {
            fields(segments, value, &["tool_use_id", "tool_call_id"]);
            content(segments, &value["content"]);
        }
        "web_search_result" | "search_result" => {
            fields(segments, value, &["source", "title", "url", "page_age"]);
            content(segments, &value["content"]);
        }
        "web_fetch_result" => {
            fields(segments, value, &["url", "retrieved_at"]);
            content(segments, &value["content"]);
        }
        "code_execution_result" | "bash_code_execution_result" | "text_editor_code_execution_result" => {
            fields(segments, value, &["stdout", "stderr"]);
            if let Some(number) = value.get("return_code").filter(|v| !v.is_null()) {
                let number = if number.is_string() {
                    number.as_str().unwrap().to_owned()
                } else {
                    number.to_string()
                };
                if !number.trim().is_empty() {
                    segments.push(number.trim().into());
                }
            }
            content(segments, &value["content"]);
            content(segments, &value["output"]);
        }
        "tool_reference" => text(segments, &value["tool_name"]),
        "" => compact(segments, Some(value)),
        _ => text(segments, &value["text"]),
    }
}

fn fields(segments: &mut Vec<String>, value: &Value, names: &[&str]) {
    for name in names {
        text(segments, &value[*name]);
    }
}
fn text(segments: &mut Vec<String>, value: &Value) {
    if let Some(text) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
        segments.push(text.into());
    }
}
fn compact(segments: &mut Vec<String>, value: Option<&Value>) {
    if let Some(value) = value {
        if value.is_string() {
            text(segments, value);
        } else {
            segments.push(value.to_string());
        }
    }
}
fn invalid(message: &str) -> ExecError {
    ExecError::local(400, FailureScope::Request, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_first_party_origin_counts_upstream() {
        for url in [
            "https://api.anthropic.com",
            "HTTPS://API.ANTHROPIC.COM:443/",
            "https://api.anthropic.com/v1",
        ] {
            assert!(first_party(url), "{url}");
        }
        for url in [
            "http://api.anthropic.com",
            "https://api.anthropic.com:444",
            "https://@api.anthropic.com",
            "https://fake@api.anthropic.com",
            "https://api.anthropic.com.example",
            "http://127.0.0.1:9",
            "bad URL",
        ] {
            assert!(!first_party(url), "{url}");
        }
    }

    #[test]
    fn estimate_matches_independent_o200k_counts_and_excludes_images_and_controls() {
        // Expectations from Python tiktoken's O200kBase over Go's documented segment order.
        let body = br#"{"system":"System text.","messages":[{"role":"user","content":[{"type":"text","text":"User text."},{"type":"image","source":{"data":"ignored-large-binary"}},{"type":"redacted_thinking","data":"ignored"}]}],"tools":[{"name":"lookup","description":"Looks up data.","input_schema":{ "type": "object" }}],"metadata":{"ignored":123},"max_tokens":9999}"#;
        assert_eq!(count(body).unwrap(), br#"{"input_tokens":19}"#.as_slice());
        assert_eq!(
            count(br#"{"messages":[{"role":"user","content":"Hello."}]}"#).unwrap(),
            br#"{"input_tokens":4}"#.as_slice()
        );
        let tools = br#"{"messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"Thinking text."},{"type":"tool_use","id":"call_1","name":"lookup","input":{"query":"Rust"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"Found it."}]}]}"#;
        assert_eq!(count(tools).unwrap(), br#"{"input_tokens":25}"#.as_slice());
    }

    #[test]
    fn count_validation_is_request_scoped() {
        for body in [
            "invalid",
            "[]",
            r#"{"messages":[]}"#,
            r#"{"messages":[{"role":"system","content":"x"}]}"#,
            r#"{"messages":[{"role":"user","content":42}]}"#,
            r#"{"messages":[{"role":"user","content":[{}]}]}"#,
        ] {
            let error = count(body.as_bytes()).unwrap_err();
            assert_eq!((error.status, error.scope), (400, FailureScope::Request));
        }
    }
}
