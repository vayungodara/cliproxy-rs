use crate::{
    Error, Pair, RequestCtx, claude_chat_response,
    json::{self, set, set_string},
};
use gjson::{Kind, Value};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub static PAIR: Pair = Pair {
    request,
    non_stream: claude_chat_response::non_stream,
    stream: claude_chat_response::stream,
    count_tokens: None,
};

fn sanitized(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn function_name(name: &str) -> String {
    sanitized(name).chars().take(64).collect()
}

fn tool_id(id: &str) -> String {
    if !id.is_empty() {
        return sanitized(id);
    }
    let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut rng = rand::rng();
    let suffix: String = (0..24).map(|_| alphabet[rng.random_range(0..62)] as char).collect();
    format!("toolu_{suffix}")
}

fn tool_result_id(id: &str) -> String {
    if !id.is_empty() {
        return sanitized(id);
    }
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "toolu_{nanos}_{}",
        COUNTER.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    )
}

fn user_id(root: &Value<'_>) -> String {
    for path in ["metadata.user_id", "user"] {
        let value = root.get(path);
        if value.kind() == Kind::String && !value.str().trim().is_empty() {
            return value.str().into();
        }
    }
    let mut seed = String::new();
    for (path, prefix) in [
        ("prompt_cache_key", "prompt_cache_key:"),
        ("session_id", "session_id:"),
        ("sessionId", "session_id:"),
        ("conversation.id", "conversation_id:"),
    ] {
        let value = root.get(path);
        if !value.str().trim().is_empty() {
            seed = format!("{prefix}{}", value.str().trim());
            break;
        }
    }
    if seed.is_empty() {
        let conversation = root.get("conversation");
        let value = if conversation.kind() == Kind::String {
            conversation
        } else {
            root.get("conversation_id")
        };
        if !value.str().trim().is_empty() {
            seed = format!("conversation_id:{}", value.str().trim());
        }
    }
    if seed.is_empty() {
        let messages = root.get("messages");
        for message in messages.array() {
            if message.get("role").str().trim().eq_ignore_ascii_case("user") {
                let content = message.get("content");
                let text = if content.kind() == Kind::String {
                    content.str().trim().into()
                } else {
                    content
                        .array()
                        .iter()
                        .filter(|part| part.get("type").str() == "text")
                        .map(|part| part.get("text").str().trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                if !text.is_empty() {
                    seed = format!("content:{text}");
                    break;
                }
            }
        }
    }
    if seed.is_empty() {
        let model = root.get("model");
        if !model.str().trim().is_empty() {
            seed = format!("model:{}", model.str().trim());
        }
        for path in ["instructions", "system", "systemInstruction", "system_instruction"] {
            let value = root.get(path);
            if value.exists() {
                seed.push_str(&format!(";{path}:{}", value.str()));
            }
        }
    }
    if seed.is_empty() {
        "unknown".into()
    } else {
        format!("{:x}", Sha256::digest(seed.as_bytes()))
    }
}

// ponytail: pinned built-in capabilities only. Replace this lookup with request-scoped
// model metadata when the shared model registry contract is available.
fn capabilities(model: &str) -> (bool, i64) {
    match model.trim() {
        "claude-sonnet-4-6" | "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" | "claude-opus-5"
        | "claude-sonnet-5" | "claude-fable-5" | "claude-fable-5-1" | "claude-opus-5-5" | "claude-sonnet-5-5" => {
            (true, 0)
        }
        "claude-haiku-4-5-20251001"
        | "claude-sonnet-4-5-20250929"
        | "claude-opus-4-5-20251101"
        | "claude-opus-4-1-20250805"
        | "claude-opus-4-20250514"
        | "claude-sonnet-4-20250514"
        | "claude-3-7-sonnet-20250219" => (false, 1024),
        _ => (false, 0),
    }
}

fn thinking(out: &mut String, root: &Value<'_>, model: &str) {
    let effort = root.get("reasoning_effort").str().trim().to_lowercase();
    if capabilities(model).0 && !effort.is_empty() {
        set_string(
            out,
            "thinking.type",
            if effort == "none" { "disabled" } else { "adaptive" },
        );
        if effort != "none" && effort != "auto" {
            let mapped = match effort.as_str() {
                "minimal" => "low",
                "xhigh" | "max" => "max",
                _ => &effort,
            };
            set_string(out, "output_config.effort", mapped);
        }
    } else {
        let budget = match effort.as_str() {
            "none" => Some(0),
            "auto" => Some(-1),
            "minimal" => Some(512),
            "low" => Some(1024),
            "medium" => Some(8192),
            "high" => Some(24576),
            "xhigh" => Some(32768),
            "max" => Some(128000),
            _ => None,
        };
        if let Some(budget) = budget {
            set_string(out, "thinking.type", if budget == 0 { "disabled" } else { "enabled" });
            if budget > 0 {
                set(out, "thinking.budget_tokens", &budget.to_string());
            }
        }
    }
}

fn summary(out: &mut String, root: &Value<'_>, model: &str) {
    let mut enabled = None;
    for path in [
        "extra_body.google.thinking_config.include_thoughts",
        "extra_body.google.thinking_config.includeThoughts",
        "extra_body.google.thinkingConfig.include_thoughts",
        "extra_body.google.thinkingConfig.includeThoughts",
        "extra_body.extra_body.google.thinking_config.include_thoughts",
        "extra_body.extra_body.google.thinking_config.includeThoughts",
        "google.thinking_config.include_thoughts",
        "google.thinking_config.includeThoughts",
        "thinking.includeThoughts",
        "thinking.include_thoughts",
        "reasoning.includeThoughts",
        "reasoning.include_thoughts",
        "generationConfig.thinkingConfig.includeThoughts",
        "generationConfig.thinkingConfig.include_thoughts",
        "generation_config.thinking_config.include_thoughts",
        "generation_config.thinking_config.includeThoughts",
    ] {
        let value = root.get(path);
        if matches!(value.kind(), Kind::True | Kind::False) {
            enabled = Some(value.bool());
            break;
        }
    }
    if enabled.is_none() {
        for path in ["reasoning.summary", "reasoning.generate_summary"] {
            let value = root.get(path);
            if value.exists() && value.kind() == Kind::Null {
                enabled = Some(false);
                break;
            }
            if value.kind() == Kind::String {
                enabled = match value.str().trim().to_lowercase().as_str() {
                    "auto" | "concise" | "detailed" => Some(true),
                    "none" => Some(false),
                    _ => None,
                };
                if enabled.is_some() {
                    break;
                }
            }
        }
    }
    if enabled.is_none() {
        for (path, invert) in [
            ("reasoning.exclude", true),
            ("include_reasoning", false),
            ("reasoning.enabled", false),
        ] {
            let value = root.get(path);
            if matches!(value.kind(), Kind::True | Kind::False) {
                enabled = Some(value.bool() != invert);
                break;
            }
        }
    }
    if let Some(enabled) = enabled {
        if enabled && !gjson::get(out, "thinking.type").exists() {
            // Summary activation strips a thinking suffix before model lookup in Go.
            let base = model.split('(').next().unwrap_or(model);
            let (adaptive, min) = capabilities(base);
            if adaptive {
                set_string(out, "thinking.type", "adaptive");
            } else if min > 0 && gjson::get(out, "max_tokens").i64() > min {
                set_string(out, "thinking.type", "enabled");
                set(out, "thinking.budget_tokens", &min.to_string());
            }
        }
        if matches!(gjson::get(out, "thinking.type").str(), "enabled" | "adaptive") {
            set_string(out, "thinking.display", if enabled { "summarized" } else { "omitted" });
        }
    }
}

fn text_part(text: &str) -> String {
    let mut out = r#"{"type":"text","text":""}"#.into();
    set_string(&mut out, "text", text);
    out
}

fn content_part(part: &Value<'_>, cache: bool) -> Option<String> {
    let mut out = match part.get("type").str() {
        "text" => text_part(part.get("text").str()),
        "image_url" => {
            let url = part.get("image_url.url");
            if url.str().is_empty() {
                return None;
            }
            if let Some(data) = url.str().strip_prefix("data:") {
                let (header, body) = data.split_once(',')?;
                let media = header.split(';').next().unwrap_or("");
                let mut out = r#"{"type":"image","source":{"type":"base64","media_type":"","data":""}}"#.into();
                set_string(
                    &mut out,
                    "source.media_type",
                    if media.is_empty() {
                        "application/octet-stream"
                    } else {
                        media
                    },
                );
                set_string(&mut out, "source.data", body);
                out
            } else {
                let mut out = r#"{"type":"image","source":{"type":"url","url":""}}"#.into();
                set_string(&mut out, "source.url", url.str());
                out
            }
        }
        "file" => {
            let data = part.get("file.file_data");
            let data = data.str().strip_prefix("data:")?;
            let (media, body) = data.split_once(';')?;
            let (_, body) = body.split_once(',')?;
            let mut out = r#"{"type":"document","source":{"type":"base64","media_type":"","data":""}}"#.into();
            set_string(&mut out, "source.media_type", media);
            set_string(&mut out, "source.data", body);
            out
        }
        _ => return None,
    };
    if cache {
        json::cache_control(&mut out, part);
    }
    Some(out)
}

fn parts(content: &Value<'_>) -> Vec<String> {
    if content.kind() == Kind::String {
        if content.str().is_empty() {
            vec![]
        } else {
            vec![text_part(content.str())]
        }
    } else {
        content
            .array()
            .iter()
            .filter_map(|part| content_part(part, true))
            .collect()
    }
}

fn tool_result(content: &Value<'_>) -> String {
    if !content.exists() {
        return json::string("");
    }
    if content.kind() == Kind::String {
        return json::string(content.str());
    }
    if content.kind() == Kind::Array {
        let mut parts = Vec::new();
        for part in content.array() {
            if part.kind() == Kind::String {
                parts.push(text_part(part.str()));
            } else if let Some(part) = content_part(&part, false) {
                parts.push(part);
            }
        }
        if !parts.is_empty() || content.array().is_empty() {
            return json::array(&parts);
        }
    } else if content.kind() == Kind::Object
        && let Some(part) = content_part(content, false)
    {
        return json::array(&[part]);
    }
    json::string(content.json())
}

fn attach_last_cache(parts: &mut [String], message: &Value<'_>) {
    if let Some(last) = parts.last_mut()
        && !gjson::get(last, "cache_control").exists()
    {
        json::cache_control(last, message);
    }
}

fn structured_output(format: &Value<'_>) -> String {
    const OBJECT: &str = "You must format your entire response as a valid JSON object. Do not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.";
    match format.get("type").str().trim().to_lowercase().as_str() {
        "json_object" => OBJECT.into(),
        "json_schema" => {
            let inner = format.get("json_schema");
            let schema = inner.get("schema");
            let schema = if schema.exists() { schema } else { format.get("schema") };
            if !schema.exists() {
                return OBJECT.into();
            }
            let mut out = "You must format your entire response as valid JSON that conforms strictly to the following JSON schema:\n".to_owned();
            for (path, label) in [("name", "Schema Name"), ("description", "Schema Description")] {
                let value = inner.get(path);
                let value = if value.str().trim().is_empty() {
                    format.get(path)
                } else {
                    value
                };
                if !value.str().trim().is_empty() {
                    out.push_str(&format!("{label}: {}\n", value.str().trim()));
                }
            }
            out.push_str("JSON Schema:\n");
            out.push_str(schema.json());
            out.push_str("\nDo not include any explanations, markdown code blocks (such as ```json), or any text outside of the JSON object.");
            out
        }
        _ => String::new(),
    }
}

fn schema(raw: &str) -> String {
    // Go uses encoding/json maps here, unlike the surrounding sjson transforms.
    let mut root: BTreeMap<String, Box<serde_json::value::RawValue>> = match serde_json::from_str(raw) {
        Ok(root) => root,
        Err(_) => return r#"{"type":"object","properties":{}}"#.into(),
    };
    let mut properties: BTreeMap<String, Box<serde_json::value::RawValue>> = root
        .get("properties")
        .and_then(|v| serde_json::from_str(v.get()).ok())
        .unwrap_or_default();
    for union in ["anyOf", "oneOf", "allOf"] {
        let Some(raw) = root.remove(union) else {
            continue;
        };
        let branches: Vec<Box<serde_json::value::RawValue>> = serde_json::from_str(raw.get()).unwrap_or_default();
        for branch in branches {
            let Ok(branch) = serde_json::from_str::<BTreeMap<String, Box<serde_json::value::RawValue>>>(branch.get())
            else {
                continue;
            };
            let accepts_object = branch.get("type").is_none_or(|v| {
                serde_json::from_str::<String>(v.get()).is_ok_and(|value| value == "object")
                    || serde_json::from_str::<Vec<String>>(v.get())
                        .is_ok_and(|types| types.iter().any(|t| t == "object"))
            });
            if !accepts_object {
                continue;
            }
            if let Some(raw) = branch.get("properties") {
                let branch_props: BTreeMap<String, Box<serde_json::value::RawValue>> =
                    serde_json::from_str(raw.get()).unwrap_or_default();
                for (key, value) in branch_props {
                    properties.entry(key).or_insert(value);
                }
            }
            if union == "allOf" {
                let mut required: Vec<String> = root
                    .get("required")
                    .and_then(|v| serde_json::from_str(v.get()).ok())
                    .unwrap_or_default();
                let Some(names) = branch.get("required") else {
                    continue;
                };
                let Ok(names) = serde_json::from_str::<Option<Vec<String>>>(names.get()) else {
                    continue;
                };
                let names = names.unwrap_or_default();
                for name in names {
                    if !required.contains(&name) {
                        required.push(name);
                    }
                }
                if !required.is_empty() {
                    root.insert(
                        "required".into(),
                        serde_json::value::RawValue::from_string(serde_json::to_string(&required).unwrap()).unwrap(),
                    );
                }
            }
        }
    }
    root.insert(
        "type".into(),
        serde_json::value::RawValue::from_string("\"object\"".into()).unwrap(),
    );
    root.insert(
        "properties".into(),
        serde_json::value::RawValue::from_string(serde_json::to_string(&properties).unwrap()).unwrap(),
    );
    // Compact raw subtrees without parsing numbers; encoding/json also HTML-escapes them.
    let raw = serde_json::to_string(&root).unwrap();
    let compact = gjson::get(&raw, "@ugly");
    compact
        .json()
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

fn request(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    convert(ctx, body, false)
}

/// Compatibility endpoints may accept unsigned assistant thinking history.
/// Native Claude must use the registered pair, which discards this history.
pub fn request_with_compat(ctx: &RequestCtx<'_>, body: &[u8]) -> Result<Vec<u8>, Error> {
    convert(ctx, body, true)
}

fn convert(ctx: &RequestCtx<'_>, body: &[u8], preserve_thinking: bool) -> Result<Vec<u8>, Error> {
    let input = json::text(body)?;
    let root = gjson::parse(input);
    let mut out = r#"{"model":"","max_tokens":32000,"messages":[],"metadata":{}}"#.into();
    set_string(&mut out, "metadata.user_id", &user_id(&root));
    thinking(&mut out, &root, ctx.model);
    set_string(&mut out, "model", ctx.model);
    for path in ["max_tokens", "max_completion_tokens"] {
        let value = root.get(path);
        if value.exists() {
            set(&mut out, "max_tokens", &value.i64().to_string());
            break;
        }
    }
    let top_p = root.get("top_p");
    if top_p.exists() {
        set(&mut out, "top_p", &top_p.f64().to_string());
    }
    let stop = root.get("stop");
    if stop.exists() {
        let values: Vec<_> = if stop.kind() == Kind::Array {
            stop.array().iter().map(|v| json::string(v.str())).collect()
        } else {
            vec![json::string(stop.str())]
        };
        if !values.is_empty() {
            set(&mut out, "stop_sequences", &json::array(&values));
        }
    }
    set(&mut out, "stream", if ctx.stream { "true" } else { "false" });
    let messages = root.get("messages");
    let messages = messages.array();
    let mut last_results = BTreeMap::new();
    for message in &messages {
        if message.get("role").str() == "tool" {
            let id = message.get("tool_call_id").str().to_owned();
            if !id.is_empty() {
                last_results.insert(id, message);
            }
        }
    }
    let mut emitted = HashSet::new();
    let mut system = Vec::new();
    let mut turns: Vec<(String, Vec<String>)> = Vec::new();
    for message in &messages {
        let role = message.get("role").str().to_owned();
        let content = message.get("content");
        if role == "system" || role == "developer" {
            let mut blocks = if content.kind() == Kind::String && !content.str().is_empty() {
                let mut part = text_part(content.str());
                json::cache_control(&mut part, message);
                vec![part]
            } else {
                content
                    .array()
                    .iter()
                    .filter(|p| p.get("type").str() == "text")
                    .map(|p| {
                        let mut part = text_part(p.get("text").str());
                        json::cache_control(&mut part, p);
                        part
                    })
                    .collect()
            };
            attach_last_cache(&mut blocks, message);
            system.extend(blocks);
            continue;
        }
        let (role, mut blocks) = match role.as_str() {
            "user" | "assistant" => {
                let mut blocks = parts(&content);
                if role == "assistant" {
                    let reasoning = message.get("reasoning_content");
                    if preserve_thinking && reasoning.kind() == Kind::String && !reasoning.str().trim().is_empty() {
                        let mut part = r#"{"type":"thinking","thinking":"","signature":""}"#.into();
                        set_string(&mut part, "thinking", reasoning.str());
                        blocks.insert(0, part);
                    }
                    for call in message.get("tool_calls").array() {
                        if call.get("type").str() != "function" {
                            continue;
                        }
                        let mut part = r#"{"type":"tool_use","id":"","name":"","input":{}}"#.into();
                        set_string(&mut part, "id", &tool_id(call.get("id").str()));
                        set_string(&mut part, "name", &function_name(call.get("function.name").str()));
                        let args = call.get("function.arguments");
                        if gjson::valid(args.str()) && gjson::parse(args.str()).kind() == Kind::Object {
                            set(&mut part, "input", gjson::parse(args.str()).json());
                        }
                        blocks.push(part);
                    }
                }
                attach_last_cache(&mut blocks, message);
                (role, blocks)
            }
            "tool" => {
                let id = message.get("tool_call_id").str().to_owned();
                if !id.is_empty() && !emitted.insert(id.clone()) {
                    continue;
                }
                let message = last_results.get(&id).copied().unwrap_or(message);
                let mut part = r#"{"type":"tool_result","tool_use_id":"","content":""}"#.into();
                set_string(&mut part, "tool_use_id", &tool_result_id(&id));
                let content = message.get("content");
                set(&mut part, "content", &tool_result(&content));
                let content_parts = if content.kind() == Kind::Object {
                    vec![content]
                } else {
                    content.array()
                };
                for p in content_parts {
                    json::cache_control(&mut part, &p);
                    if gjson::get(&part, "cache_control").exists() {
                        break;
                    }
                }
                if !gjson::get(&part, "cache_control").exists() {
                    json::cache_control(&mut part, message);
                }
                ("user".into(), vec![part])
            }
            _ => continue,
        };
        if blocks.is_empty() {
            continue;
        }
        if let Some((_, last_blocks)) = turns.last_mut().filter(|(r, _)| r == &role) {
            last_blocks.append(&mut blocks);
        } else {
            turns.push((role, blocks));
        }
    }
    let instruction = structured_output(&root.get("response_format"));
    if !instruction.is_empty() {
        system.push(text_part(&instruction));
    }
    if turns.is_empty() && !system.is_empty() {
        turns.push(("user".into(), vec![text_part("")]));
    }
    if !system.is_empty() {
        set(&mut out, "system", &json::array(&system));
    }
    let messages: Vec<_> = turns
        .into_iter()
        .map(|(role, mut blocks)| {
            if role == "assistant" {
                blocks.sort_by_key(|b| gjson::get(b, "type").str() == "tool_use");
            }
            format!(
                "{{\"role\":{},\"content\":{}}}",
                json::string(&role),
                json::array(&blocks)
            )
        })
        .collect();
    if !messages.is_empty() {
        set(&mut out, "messages", &json::array(&messages));
    }

    let choice = root.get("tool_choice");
    let allowed = choice.get("type").str() == "allowed_tools" && choice.kind() == Kind::Object;
    let mut allowed_names = HashSet::new();
    let mut mode = "auto".to_owned();
    if allowed {
        let list = choice.get("allowed_tools.tools");
        let list = if list.array().is_empty() {
            choice.get("tools")
        } else {
            list
        };
        for tool in list.array() {
            let name = tool.get("function.name");
            let name = if name.str().trim().is_empty() {
                tool.get("name")
            } else {
                name
            };
            if !name.str().trim().is_empty() {
                allowed_names.insert(name.str().trim().into());
                allowed_names.insert(function_name(name.str().trim()));
            }
        }
        let value = choice.get("allowed_tools.mode");
        let value = if value.str().trim().is_empty() {
            choice.get("mode")
        } else {
            value
        };
        if !value.str().trim().is_empty() {
            mode = value.str().trim().to_lowercase();
        }
    }
    let mut tools = Vec::new();
    for tool in root.get("tools").array() {
        if tool.get("type").str() != "function" {
            continue;
        }
        let function = tool.get("function");
        let name = function.get("name");
        let sanitized = function_name(name.str());
        if allowed && !allowed_names.contains(name.str()) && !allowed_names.contains(&sanitized) {
            continue;
        }
        let mut out = r#"{"name":"","description":""}"#.into();
        set_string(&mut out, "name", &sanitized);
        set_string(&mut out, "description", function.get("description").str());
        let parameters = function.get("parameters");
        let parameters = if parameters.exists() {
            parameters
        } else {
            function.get("parametersJsonSchema")
        };
        set(&mut out, "input_schema", &schema(parameters.json()));
        json::cache_control(&mut out, &tool);
        if !gjson::get(&out, "cache_control").exists() {
            json::cache_control(&mut out, &function);
        }
        let strict = function.get("strict");
        let strict = if strict.exists() { strict } else { tool.get("strict") };
        if matches!(strict.kind(), Kind::True | Kind::False) {
            set(&mut out, "strict", strict.json());
        }
        tools.push(out);
    }
    if !tools.is_empty() {
        set(&mut out, "tools", &json::array(&tools));
    }
    if allowed {
        set(
            &mut out,
            "tool_choice",
            &format!(
                "{{\"type\":{}}}",
                json::string(if tools.is_empty() {
                    "none"
                } else if mode == "required" {
                    "any"
                } else {
                    "auto"
                })
            ),
        );
    } else {
        let choice_type = if choice.kind() == Kind::String {
            choice.str().to_owned()
        } else {
            choice.get("type").str().to_owned()
        };
        match choice_type.as_str() {
            "none" | "auto" | "required" | "any" if choice_type != "any" || choice.kind() == Kind::Object => set(
                &mut out,
                "tool_choice",
                &format!(
                    "{{\"type\":{}}}",
                    json::string(if choice_type == "required" { "any" } else { &choice_type })
                ),
            ),
            "function" if choice.kind() == Kind::Object => {
                let name = choice.get("function.name");
                let name = if name.str().is_empty() {
                    choice.get("name")
                } else {
                    name
                };
                if name.str().is_empty() {
                    set(&mut out, "tool_choice", r#"{"type":"none"}"#);
                } else {
                    set(
                        &mut out,
                        "tool_choice",
                        &format!(
                            "{{\"type\":\"tool\",\"name\":{}}}",
                            json::string(&function_name(name.str()))
                        ),
                    );
                }
            }
            _ => {}
        }
    }
    if root.get("parallel_tool_calls").kind() == Kind::False {
        let choice = gjson::get(&out, "tool_choice");
        let exists = choice.exists();
        let not_none = choice.get("type").str() != "none";
        if exists && not_none {
            set(&mut out, "tool_choice.disable_parallel_tool_use", "true");
        } else if !exists && !tools.is_empty() {
            set(
                &mut out,
                "tool_choice",
                r#"{"type":"auto","disable_parallel_tool_use":true}"#,
            );
        }
    }
    summary(&mut out, &root, ctx.model);
    Ok(out.into_bytes())
}
