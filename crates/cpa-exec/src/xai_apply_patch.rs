//! SEAM for Go's apply_patch Responses helpers on the xAI executor:
//! helps.NormalizeApplyPatchResponsesRequest and helps.ApplyPatchResponsesState (with
//! translator/common ApplyPatchResponsesBridge).
//!
//! The translator thread owns the real port and will publish it from cpa-translate.
//! Until then this module reproduces Go exactly for requests whose tools declare no
//! winning custom `apply_patch` (the bridge is then inactive), and passes everything
//! through when one is declared.
// ponytail: owner translator thread (cpa-translate public apply_patch Responses API).
// Integration replaces `normalize_request` and `State` with that API; the call sites in
// xai.rs and xai_request.rs keep their shape. Missing until then for a client that
// declares a custom `apply_patch`: the declaration rewrite to a function with the wrapped
// input schema, folded-namespace dispatcher expansion and the response-side
// function-to-custom bridge.

use std::collections::HashSet;

use cpa_common::json::{self as gj, Kind, Res};
use cpa_core::exec::ExecError;

/// `applypatch.IsCustomTool`.
fn is_custom_apply_patch(tool: &Res<'_>) -> bool {
    &*tool.get("type").bytes() == b"custom" && tool.get("name").str().trim() == "apply_patch"
}

/// `preferChatFunctionPatchTools`: when the client's original request declares Chat
/// function tools, the translated tools are rebuilt without custom apply_patch tools
/// that shadow one of them (an absent array becomes `[]`).
fn prefer_chat_function_tools(original: &[u8], mut declarations: Vec<u8>) -> Vec<u8> {
    let ordinary: HashSet<Vec<u8>> = gj::get(original, "tools")
        .array()
        .iter()
        .filter(|t| &*t.get("type").bytes() == b"function")
        .map(|t| t.get("function.name").bytes().into_owned())
        .collect();
    if ordinary.is_empty() {
        return declarations;
    }
    let tools = gj::get(&declarations, "tools").array();
    let available: HashSet<Vec<u8>> = tools
        .iter()
        .filter(|t| &*t.get("type").bytes() == b"function")
        .map(|t| t.get("name").bytes().into_owned())
        .collect();
    let kept: Vec<Vec<u8>> = tools
        .iter()
        .filter(|t| {
            let name = t.get("name").bytes();
            !(is_custom_apply_patch(t) && ordinary.contains(&*name) && available.contains(&*name))
        })
        .map(|t| t.raw().to_vec())
        .collect();
    gj::set_raw(&mut declarations, "tools", gj::join(&kept));
    declarations
}

/// `helps.NormalizeApplyPatchResponsesRequest(body, original)` without winning custom
/// apply_patch declarations: tool arrays (namespace children and `additional_tools`
/// included) and `tool_choice` are rebuilt item by item, and apply_patch history
/// becomes function calls with `applypatch.WrapInput` arguments.
pub(crate) fn normalize_request(raw: Vec<u8>, original: &[u8]) -> Result<Vec<u8>, ExecError> {
    let raw = prefer_chat_function_tools(original, raw);
    if !gj::valid(&raw) {
        return Err(crate::openai_compat::plain_err("invalid Responses request JSON"));
    }
    fn tools_array(tools: &Res<'_>) -> Vec<u8> {
        let mut items = Vec::new();
        for tool in tools.array() {
            let mut item = tool.raw().to_vec();
            if &*tool.get("type").bytes() == b"namespace" {
                for key in ["tools", "children"] {
                    let children = tool.get(key);
                    if children.is_array() {
                        gj::set_raw(&mut item, key, tools_array(&children));
                        break;
                    }
                }
            }
            items.push(item);
        }
        gj::join(&items)
    }
    fn choice(c: &Res<'_>) -> Vec<u8> {
        let mut out = c.raw().to_vec();
        for (i, child) in c.get("tools").array().iter().enumerate() {
            gj::set_raw(&mut out, format!("tools.{i}").as_str(), choice(child));
        }
        out
    }
    let root = gj::parse(&raw).into_owned();
    let mut raw = raw;
    let tools = root.get("tools");
    if tools.is_array() {
        gj::set_raw(&mut raw, "tools", tools_array(&tools));
    }
    let input = root.get("input").array();
    let history: HashSet<Vec<u8>> = input
        .iter()
        .filter(|i| &*i.get("type").bytes() == b"custom_tool_call" && i.get("name").str().trim() == "apply_patch")
        .map(|i| i.get("call_id").bytes().into_owned())
        .collect();
    for (i, item) in input.iter().enumerate() {
        let path = format!("input.{i}");
        match &*item.get("type").bytes() {
            b"additional_tools" => {
                let tools = item.get("tools");
                if tools.is_array() {
                    gj::set_raw(&mut raw, format!("{path}.tools").as_str(), tools_array(&tools));
                }
            }
            b"custom_tool_call" if item.get("name").str().trim() == "apply_patch" => {
                let patch = item.get("input");
                if patch.kind != Kind::String {
                    return Err(crate::openai_compat::plain_err(
                        "apply_patch history input must be a string",
                    ));
                }
                gj::set_str(&mut raw, format!("{path}.type").as_str(), "function_call");
                // applypatch.WrapInput: json.Marshal of {"input": patch}.
                let wrapped = [&b"{\"input\":"[..], &gj::quote(&*patch.bytes()), b"}"].concat();
                gj::set_str(&mut raw, format!("{path}.arguments").as_str(), wrapped);
                gj::delete(&mut raw, format!("{path}.input").as_str());
            }
            b"custom_tool_call_output" if history.contains(&*item.get("call_id").bytes()) => {
                gj::set_str(&mut raw, format!("{path}.type").as_str(), "function_call_output");
            }
            _ => {}
        }
    }
    let tool_choice = root.get("tool_choice");
    if tool_choice.is_object() {
        gj::set_raw(&mut raw, "tool_choice", choice(&tool_choice));
    }
    Ok(raw)
}

/// `helps.ApplyPatchResponsesState` as the xAI executor drives it. Inactive (no custom
/// apply_patch declared) it is Go's pass-through: events flow unchanged until the first
/// terminal event closes the state.
pub(crate) struct State {
    closed: bool,
}

impl State {
    /// `NewApplyPatchResponsesState(from, original, declarations)`.
    pub(crate) fn new(_declarations: &[u8]) -> Self {
        Self { closed: false }
    }

    /// `AddDispatcher`: only folded namespaces holding a custom apply_patch register.
    pub(crate) fn add_dispatcher(&mut self, _name: &str, _namespace: &str) {}

    /// `RememberDispatcherEvent`: a no-op without dispatchers.
    pub(crate) fn remember_dispatcher_event(&mut self, _event: &[u8]) {}

    /// `Transform` for the buffered path: one upstream event in, the events to process
    /// out. `Err` is the 502 apply_patch upstream failure.
    pub(crate) fn transform(&mut self, event: Vec<u8>) -> Result<Vec<Vec<u8>>, ()> {
        if self.closed {
            return Ok(vec![]);
        }
        let kind = gj::get(&event, "type").bytes().into_owned();
        if matches!(
            kind.as_slice(),
            b"response.completed" | b"response.incomplete" | b"response.done" | b"response.failed"
        ) {
            self.closed = true;
        }
        Ok(vec![event])
    }

    /// `Finish`: whether the source response closed validly.
    pub(crate) fn finish(&self) -> Result<(), ()> {
        Ok(())
    }

    /// `Stream`: one translated-side line in, the lines to translate out.
    pub(crate) fn stream(&mut self, line: Vec<u8>) -> Result<Vec<Vec<u8>>, ()> {
        Ok(vec![line])
    }

    /// `FinishStream`: closing events on EOF without a validated completion.
    pub(crate) fn finish_stream(&mut self) -> Result<Vec<Vec<u8>>, ()> {
        Ok(vec![])
    }

    /// `Bridge.TransformNonStream` for the compact response.
    pub(crate) fn transform_non_stream(&mut self, data: Vec<u8>) -> Result<Vec<u8>, ()> {
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_wraps_input_like_json_marshal() {
        let body = br#"{"input":[{"type":"custom_tool_call","name":"apply_patch","call_id":"c1","input":"a<b&c"},{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}]}"#;
        let out = normalize_request(body.to_vec(), b"{}").unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"input":[{"type":"function_call","name":"apply_patch","call_id":"c1","arguments":"{\"input\":\"a\\u003cb\\u0026c\"}"},{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#
        );
    }

    #[test]
    fn chat_function_tools_in_the_original_rebuild_tools() {
        let original = br#"{"tools":[{"type":"function","function":{"name":"f"}}]}"#;
        let out = normalize_request(br#"{"model":"m"}"#.to_vec(), original).unwrap();
        assert_eq!(out, br#"{"model":"m","tools":[]}"#);
    }
}
