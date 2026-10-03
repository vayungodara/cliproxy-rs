//! OpenAI Responses request -> OpenAI Chat Completions request
//! (internal/translator/openai/openai/responses: openai_openai-responses_request.go,
//! openai_openai-responses_tools.go, responses_tool_index.go).

use std::collections::{HashMap, HashSet};

use cpa_common::json::{self as gj, AnyValue, Kind, Res};

use crate::claude_responses::{extract_call_id, normalize_tool_call_outputs};
use crate::common::{align_openai_tool_call_messages_with, go_lower, trim_space};
use crate::responses_tools::{tool_description, tool_parameters};

// ---------------------------------------------------------------------------------------
// Tool declarations and Chat names (openai_openai-responses_tools.go)

const NAME_LIMIT: usize = 64;

/// One function or custom tool declaration and its Chat Completions name.
#[derive(Clone, Debug)]
pub(crate) struct Declaration {
    pub tool: Vec<u8>,
    pub chat_name: Vec<u8>,
    pub local_name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub custom: bool,
}

/// rawResponsesNamespaceQualifiedName (the namespace is not trimmed).
fn raw_qualified(namespace: &[u8], child: &[u8]) -> Vec<u8> {
    let child = trim_space(child);
    if child.is_empty() || namespace.is_empty() || child.starts_with(b"mcp__") {
        return child.to_vec();
    }
    if child == namespace || child.starts_with(&[namespace, b"__"].concat()) {
        return child.to_vec();
    }
    if namespace.ends_with(b"__") {
        return [namespace, child].concat();
    }
    [namespace, b"__", child].concat()
}

/// capResponsesChatToolName: the last 64 bytes, without leading `_` or `-`.
fn cap_name(name: &[u8]) -> Vec<u8> {
    if name.len() <= NAME_LIMIT {
        return name.to_vec();
    }
    let truncated = &name[name.len() - NAME_LIMIT..];
    let start = truncated.iter().position(|&c| c != b'_' && c != b'-');
    match start {
        Some(start) => truncated[start..].to_vec(),
        None => truncated.to_vec(),
    }
}

fn qualify(namespace: &[u8], child: &[u8]) -> Vec<u8> {
    cap_name(&raw_qualified(namespace, child))
}

fn tool_name(tool: &Res<'_>) -> Vec<u8> {
    crate::claude_responses::tool_name(tool)
}

/// walkResponsesToolDeclarations: function and custom tools from `tools` and
/// `additional_tools`, namespace children qualified, long names disambiguated.
fn declarations(root: &Res<'_>) -> Vec<Declaration> {
    let mut out: Vec<Declaration> = vec![];
    let mut emit = |tool: &Res<'_>, namespace: &[u8]| {
        let custom = match trim_space(&tool.get("type").bytes()) {
            b"" | b"function" => false,
            b"custom" => true,
            _ => return,
        };
        let local = tool_name(tool);
        if local.is_empty() {
            return;
        }
        out.push(Declaration {
            tool: tool.raw.to_vec(),
            chat_name: qualify(namespace, &local),
            local_name: local,
            namespace: namespace.to_vec(),
            custom,
        });
    };
    let mut scan = |tools: Res<'_>| {
        if !tools.is_array() {
            return;
        }
        tools.each(|_, tool| {
            if trim_space(&tool.get("type").bytes()) == b"namespace" {
                let children = tool.get("tools");
                if children.is_array() {
                    let namespace = trim_space(&tool.get("name").bytes()).to_vec();
                    children.each(|_, child| {
                        emit(&child, &namespace);
                        true
                    });
                }
                return true;
            }
            emit(&tool, b"");
            true
        });
    };
    scan(root.get("tools"));
    let input = root.get("input");
    if input.is_array() {
        input.each(|_, item| {
            if item.get("type").bytes().as_ref() == b"additional_tools" {
                scan(item.get("tools"));
            }
            true
        });
    }
    disambiguate(&mut out);
    out
}

/// disambiguateResponsesChatToolNames: capped long names that collide with another
/// identity (or with an ambiguous local name) get `_1`, `_2`, ... suffixes.
fn disambiguate(declarations: &mut [Declaration]) {
    let mut claimed: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let claim =
        |claimed: &mut HashMap<Vec<u8>, Vec<u8>>, candidate: &[u8], identity: &[u8]| match claimed.get(candidate) {
            None => {
                claimed.insert(candidate.to_vec(), identity.to_vec());
                true
            }
            Some(owner) => owner == identity,
        };
    let mut long = vec![];
    let mut identities = vec![];
    // Local name -> owning identity; empty: several identities.
    let mut local_owners: Vec<(Vec<u8>, Vec<u8>)> = vec![];
    for (i, d) in declarations.iter().enumerate() {
        let identity = raw_qualified(&d.namespace, &d.local_name);
        if identity.len() > NAME_LIMIT {
            long.push(i);
        } else {
            claim(&mut claimed, &identity, &identity);
        }
        let local = &d.local_name;
        if !(local.is_empty() || *local == identity || local.len() > NAME_LIMIT) {
            match local_owners.iter_mut().find(|(l, _)| l == local) {
                None => local_owners.push((local.clone(), identity.clone())),
                Some((_, owner)) if !owner.is_empty() && *owner != identity => owner.clear(),
                _ => {}
            }
        }
        identities.push(identity);
    }
    let mut ambiguous: HashSet<Vec<u8>> = HashSet::new();
    for (local, owner) in &local_owners {
        claim(&mut claimed, local, owner);
        if owner.is_empty() {
            ambiguous.insert(local.clone());
        }
    }
    for i in long {
        let identity = &identities[i];
        let name = declarations[i].chat_name.clone();
        if !ambiguous.contains(&name) && claim(&mut claimed, &name, identity) {
            continue;
        }
        for suffix in 1.. {
            let candidate = cap_name(&[&name[..], format!("_{suffix}").as_bytes()].concat());
            if ambiguous.contains(&candidate) {
                continue;
            }
            if claim(&mut claimed, &candidate, identity) {
                declarations[i].chat_name = candidate;
                break;
            }
        }
    }
}

/// responsesToolIndex: Chat names for declared tools and their reverse identities.
pub(crate) struct ToolIndex {
    declarations: Vec<Declaration>,
    pub by_chat: HashMap<Vec<u8>, Declaration>,
    by_identity: HashMap<(Vec<u8>, Vec<u8>), Vec<u8>>,
    by_raw: HashMap<Vec<u8>, Vec<u8>>,
    /// Empty: several distinct emitted tools share the local name.
    by_local: HashMap<Vec<u8>, Vec<u8>>,
    pub custom: HashSet<Vec<u8>>,
}

impl ToolIndex {
    pub(crate) fn new(root: &Res<'_>) -> Self {
        let mut index = ToolIndex {
            declarations: vec![],
            by_chat: HashMap::new(),
            by_identity: HashMap::new(),
            by_raw: HashMap::new(),
            by_local: HashMap::new(),
            custom: HashSet::new(),
        };
        for d in declarations(root) {
            index.declarations.push(d.clone());
            index
                .by_identity
                .entry((d.namespace.clone(), d.local_name.clone()))
                .or_insert_with(|| d.chat_name.clone());
            index
                .by_raw
                .entry(raw_qualified(&d.namespace, &d.local_name))
                .or_insert_with(|| d.chat_name.clone());
            if index.by_chat.contains_key(&d.chat_name) {
                continue;
            }
            match index.by_local.get_mut(&d.local_name) {
                Some(existing) => existing.clear(),
                None => {
                    index.by_local.insert(d.local_name.clone(), d.chat_name.clone());
                }
            }
            if d.custom {
                index.custom.insert(d.chat_name.clone());
            }
            index.by_chat.insert(d.chat_name.clone(), d);
        }
        index
    }

    pub(crate) fn from_raw(raw: &[u8]) -> Self {
        Self::new(&gj::parse(raw))
    }

    pub(crate) fn namespace_name(&self, namespace: &[u8], name: &[u8]) -> Vec<u8> {
        if let Some(chat) = self.by_identity.get(&(namespace.to_vec(), name.to_vec())) {
            return chat.clone();
        }
        self.avoid_alias(qualify(namespace, name))
    }

    pub(crate) fn canonical_name(&self, name: &[u8]) -> Vec<u8> {
        if self.by_chat.contains_key(name) {
            return name.to_vec();
        }
        if let Some(chat) = self.by_raw.get(name) {
            return chat.clone();
        }
        if let Some(chat) = self.by_local.get(name).filter(|c| !c.is_empty()) {
            return chat.clone();
        }
        self.avoid_alias(cap_name(name))
    }

    fn avoid_alias(&self, candidate: Vec<u8>) -> Vec<u8> {
        if !self.by_chat.contains_key(&candidate) {
            return candidate;
        }
        (1..)
            .map(|suffix| cap_name(&[&candidate[..], format!("_{suffix}").as_bytes()].concat()))
            .find(|variant| !self.by_chat.contains_key(variant))
            .unwrap()
    }

    /// applyIdentity: the declared local name and namespace for a Chat name.
    pub(crate) fn apply_identity(&self, item: &mut Vec<u8>, qualified: &[u8], path: &str) {
        let mut name = trim_space(qualified).to_vec();
        let mut namespace = vec![];
        if let Some(d) = self.by_chat.get(&name) {
            name = d.local_name.clone();
            namespace = d.namespace.clone();
        }
        let at = |key: &str| {
            if path.is_empty() {
                key.to_owned()
            } else {
                format!("{path}.{key}")
            }
        };
        gj::set_str(item, &at("name"), &name);
        if namespace.is_empty() {
            gj::delete(item, &at("namespace"));
        } else {
            gj::set_str(item, &at("namespace"), &namespace);
        }
    }

    pub(crate) fn single_custom_name(&self) -> (Vec<u8>, bool) {
        if self.custom.len() == 1 {
            let name = self.custom.iter().next().unwrap().clone();
            return (name, self.by_chat.len() == 1);
        }
        (vec![], false)
    }

    pub(crate) fn is_apply_patch(&self, name: &[u8]) -> bool {
        self.by_chat
            .get(name)
            .is_some_and(|d| d.custom && crate::apply_patch::is_custom_tool(&gj::parse(&d.tool)))
    }

    pub(crate) fn has_apply_patch(&self) -> bool {
        self.by_chat.keys().any(|name| self.is_apply_patch(name))
    }

    /// The Chat Completions `tools`, one per distinct Chat name.
    fn chat_tools(&self) -> Vec<Vec<u8>> {
        let mut seen: HashSet<&[u8]> = HashSet::new();
        let mut out = vec![];
        for d in &self.declarations {
            if seen.contains(d.chat_name.as_slice()) {
                continue;
            }
            let tool = gj::parse(&d.tool);
            let converted = if d.custom {
                custom_tool(&tool, &d.chat_name)
            } else {
                function_tool(&tool, &d.chat_name)
            };
            if let Some(converted) = converted {
                out.push(converted);
                seen.insert(&d.chat_name);
            }
        }
        out
    }
}

fn override_name(tool: &Res<'_>, name: &[u8]) -> Vec<u8> {
    let name = trim_space(name);
    if name.is_empty() {
        tool_name(tool)
    } else {
        name.to_vec()
    }
}

/// convertResponsesCustomToolToOpenAIChat.
fn custom_tool(tool: &Res<'_>, name: &[u8]) -> Option<Vec<u8>> {
    let name = override_name(tool, name);
    if name.is_empty() {
        return None;
    }
    let mut out = br#"{"type":"function","function":{"name":"","description":"","parameters":{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}}}"#.to_vec();
    gj::set_str(&mut out, "function.name", &name);
    let description = tool_description(tool);
    if !description.is_empty() {
        gj::set_str(&mut out, "function.description", &description);
    }
    if crate::apply_patch::is_custom_tool(tool) {
        gj::set_str(&mut out, "function.description", crate::apply_patch::description(tool));
        gj::set_raw(&mut out, "function.parameters", crate::apply_patch::PARAMETERS);
    }
    Some(out)
}

/// convertResponsesFunctionToolToOpenAIChat.
fn function_tool(tool: &Res<'_>, name: &[u8]) -> Option<Vec<u8>> {
    let name = override_name(tool, name);
    if name.is_empty() {
        return None;
    }
    let mut out = br#"{"type":"function","function":{"name":"","description":"","parameters":{}}}"#.to_vec();
    gj::set_str(&mut out, "function.name", &name);
    let description = tool_description(tool);
    if !description.is_empty() {
        gj::set_str(&mut out, "function.description", &description);
    }
    if let Some(parameters) = tool_parameters(tool) {
        gj::set_raw(&mut out, "function.parameters", &parameters.raw);
    }
    Some(out)
}

/// responsesToolOutputText.
fn tool_output_text(output: &Res<'_>) -> Vec<u8> {
    if output.kind == Kind::String {
        return output.s.to_vec();
    }
    if output.is_array() {
        let mut out = vec![];
        output.each(|_, part| {
            if part.kind == Kind::String {
                out.extend_from_slice(&part.s);
            } else {
                let text = part.get("text");
                if text.exists() {
                    out.extend_from_slice(&text.bytes());
                }
            }
            true
        });
        return out;
    }
    output.raw.to_vec()
}

/// unwrapCustomToolInput: the `input` of the arguments, else the arguments.
pub(crate) fn unwrap_custom_tool_input(arguments: &[u8]) -> Vec<u8> {
    let v = gj::get(arguments, "input");
    if !v.exists() {
        return arguments.to_vec();
    }
    if v.kind == Kind::String {
        v.s.to_vec()
    } else {
        v.raw.to_vec()
    }
}

// ---------------------------------------------------------------------------------------
// Tool outputs (setFunctionCallOutputContent and helpers)

/// normalizeChatImageDetail: (detail, ok).
fn image_detail(detail: &Res<'_>) -> (Vec<u8>, bool) {
    if !detail.exists() {
        return (vec![], true);
    }
    if detail.kind != Kind::String {
        return (vec![], false);
    }
    let normalized = go_lower(trim_space(&detail.s));
    match normalized.as_slice() {
        b"auto" | b"low" | b"high" => (normalized, true),
        b"original" => (b"high".to_vec(), true),
        _ => (vec![], true),
    }
}

/// chatToolOutputImageFields.
fn image_fields(item: &Res<'_>) -> Option<(Vec<u8>, Vec<u8>)> {
    let (url, detail) = match item.get("type").bytes().as_ref() {
        b"image_url" => (item.get("image_url.url"), item.get("image_url.detail")),
        b"input_image" => (item.get("image_url"), item.get("detail")),
        _ => return None,
    };
    if url.kind != Kind::String {
        return None;
    }
    let url = trim_space(&url.s).to_vec();
    if url.is_empty() {
        return None;
    }
    let (detail, ok) = image_detail(&detail);
    ok.then_some((url, detail))
}

/// hasChatToolOutputImagePart.
fn has_image_part(content: &Res<'_>) -> bool {
    if !content.is_array() {
        return false;
    }
    let mut has_image = false;
    for item in content.array() {
        let kind = item.get("type");
        if kind.kind != Kind::String {
            continue;
        }
        match kind.s.as_ref() {
            b"text" | b"input_text" | b"output_text" => {
                if item.get("text").kind != Kind::String {
                    return false;
                }
            }
            b"image_url" | b"input_image" => {
                if image_fields(&item).is_none() {
                    return false;
                }
                has_image = true;
            }
            _ => {}
        }
    }
    has_image
}

fn text_content_part(text: &[u8]) -> Vec<u8> {
    let mut part = br#"{"type":"text","text":""}"#.to_vec();
    gj::set_str(&mut part, "text", text);
    part
}

/// chatToolOutputContentPart.
fn output_content_part(item: &Res<'_>) -> Vec<u8> {
    let fallback = || {
        let text = if item.kind == Kind::String || item.raw.is_empty() {
            item.bytes().into_owned()
        } else {
            item.raw.to_vec()
        };
        text_content_part(&text)
    };
    match item.get("type").bytes().as_ref() {
        b"text" | b"input_text" | b"output_text" => text_content_part(&item.get("text").bytes()),
        b"image_url" | b"input_image" => match image_fields(item) {
            Some((url, detail)) => {
                let mut part = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
                gj::set_str(&mut part, "image_url.url", &url);
                if !detail.is_empty() {
                    gj::set_str(&mut part, "image_url.detail", &detail);
                }
                part
            }
            None => fallback(),
        },
        _ => fallback(),
    }
}

/// setFunctionCallOutputContent: image-bearing outputs become content parts.
fn set_function_output(message: &mut Vec<u8>, output: &Res<'_>) {
    let parsed;
    let structured = if output.kind == Kind::String {
        if !gj::valid(&output.s) {
            gj::set_str(message, "content", &output.s);
            return;
        }
        parsed = gj::parse(&output.s).into_owned();
        &parsed
    } else {
        output
    };
    if has_image_part(structured) {
        let parts: Vec<Vec<u8>> = structured.array().iter().map(output_content_part).collect();
        gj::set_items(message, "content", &parts);
        return;
    }
    gj::set_str(message, "content", output.bytes());
}

/// setCustomToolCallOutputContent.
fn set_custom_output(message: &mut Vec<u8>, output: &Res<'_>) {
    let parsed;
    let structured = if output.kind == Kind::String && gj::valid(&output.s) {
        parsed = gj::parse(&output.s).into_owned();
        &parsed
    } else {
        output
    };
    if has_image_part(structured) {
        set_function_output(message, output);
        return;
    }
    gj::set_str(message, "content", tool_output_text(output));
}

type SetOutput = fn(&mut Vec<u8>, &Res<'_>);

/// appendStandaloneResponsesToolOutputAsUser: a tool output with no matching call becomes
/// user content when it has any.
fn standalone_output(output: &Res<'_>, set: SetOutput) -> Option<Vec<u8>> {
    let mut message = br#"{"role":"user","content":""}"#.to_vec();
    if output.exists() {
        set(&mut message, output);
    }
    let content = gj::get(&message, "content");
    if !content.exists()
        || (content.kind == Kind::String && trim_space(&content.s).is_empty())
        || (content.is_array() && !content.get("0").exists())
    {
        return None;
    }
    Some(message)
}

// ---------------------------------------------------------------------------------------
// Reasoning helpers

const UNAVAILABLE: &[u8] = b"[reasoning unavailable]";

/// collectOpenAIResponsesReasoningContent.
fn reasoning_content(item: &Res<'_>) -> Vec<u8> {
    let mut text = vec![];
    let summary = item.get("summary");
    if summary.is_array() {
        summary.each(|_, s| {
            if s.get("type").bytes().as_ref() == b"summary_text" {
                text.extend_from_slice(&s.get("text").bytes());
            }
            true
        });
    }
    if text.is_empty() { UNAVAILABLE.to_vec() } else { text }
}

/// combineOpenAIResponsesReasoning.
fn combine_reasoning(existing: &[u8], incoming: &[u8]) -> Vec<u8> {
    let (e, i) = (trim_space(existing), trim_space(incoming));
    if e.is_empty() || e == UNAVAILABLE {
        return incoming.to_vec();
    }
    if i.is_empty() || i == UNAVAILABLE || e == i {
        return existing.to_vec();
    }
    [existing, b"\n\n", incoming].concat()
}

fn usable_reasoning(reasoning: &[u8]) -> bool {
    let t = trim_space(reasoning);
    !t.is_empty() && t != UNAVAILABLE
}

fn effort_enables_reasoning(value: &Res<'_>) -> bool {
    let effort = go_lower(trim_space(&value.bytes()));
    !matches!(effort.as_slice(), b"" | b"none" | b"0" | b"false")
}

// ---------------------------------------------------------------------------------------
// Request conversion

/// convertResponsesToolChoiceWithIndex.
fn tool_choice(choice: &Res<'_>, index: &ToolIndex) -> Vec<u8> {
    if !choice.is_object() {
        return choice.raw.to_vec();
    }
    let kind = choice.get("type").bytes();
    if kind.as_ref() != b"function" && kind.as_ref() != b"custom" {
        return choice.raw.to_vec();
    }
    let name = ["function.name", "custom.name", "name"]
        .iter()
        .map(|p| choice.get(*p).bytes().into_owned())
        .find(|n| !n.is_empty())
        .unwrap_or_default();
    if name.is_empty() {
        return choice.raw.to_vec();
    }
    let namespace = ["namespace", "function.namespace", "custom.namespace"]
        .iter()
        .map(|p| trim_space(&choice.get(*p).bytes()).to_vec())
        .find(|n| !n.is_empty())
        .unwrap_or_default();
    let name = if namespace.is_empty() {
        index.canonical_name(&name)
    } else {
        index.namespace_name(&namespace, &name)
    };
    let mut out = br#"{"type":"function","function":{"name":""}}"#.to_vec();
    gj::set_str(&mut out, "function.name", &name);
    out
}

/// convertResponsesTextFormatToChatResponseFormat.
fn response_format(format: &Res<'_>) -> Option<Vec<u8>> {
    let kind = format.get("type").bytes();
    match kind.as_ref() {
        b"text" | b"json_object" => {
            let mut out = br#"{"type":""}"#.to_vec();
            gj::set_str(&mut out, "type", &kind);
            Some(out)
        }
        b"json_schema" => {
            let mut out = br#"{"type":"json_schema","json_schema":{}}"#.to_vec();
            for field in ["name", "description", "strict"] {
                let value = format.get(field);
                if value.exists() {
                    AnyValue::from_res(&value).set(&mut out, &format!("json_schema.{field}"));
                }
            }
            let schema = format.get("schema");
            if schema.exists() {
                gj::set_raw(&mut out, "json_schema.schema", &schema.raw);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Message content part of an input message (text, video, image).
fn message_part(item: &Res<'_>) -> Option<Vec<u8>> {
    let kind = item.get("type").bytes();
    let kind: &[u8] = if kind.is_empty() { b"input_text" } else { &kind };
    match kind {
        b"input_text" | b"output_text" => Some(text_content_part(&item.get("text").bytes())),
        b"input_video" | b"video_url" => {
            let mut part = br#"{"type":"video_url","video_url":{}}"#.to_vec();
            let video = item.get("video_url");
            if video.is_object() {
                gj::set_raw(&mut part, "video_url", &video.raw);
            } else if video.exists() {
                gj::set_raw(&mut part, "video_url.url", &video.raw);
            }
            let processing = item.get("processing");
            if processing.exists() {
                gj::set_raw(&mut part, "video_url.processing", &processing.raw);
            }
            Some(part)
        }
        b"input_image" => {
            let mut part = br#"{"type":"image_url","image_url":{"url":""}}"#.to_vec();
            gj::set_str(&mut part, "image_url.url", item.get("image_url").bytes());
            let (detail, ok) = image_detail(&item.get("detail"));
            if ok && !detail.is_empty() {
                gj::set_str(&mut part, "image_url.detail", &detail);
            }
            Some(part)
        }
        _ => None,
    }
}

struct Conversation<'i> {
    index: &'i ToolIndex,
    messages: Vec<Vec<u8>>,
    has_reasoning: bool,
    pending_calls: Vec<AnyValue>,
    pending_call_ids: Vec<Vec<u8>>,
    pending_reasoning: Vec<u8>,
    latest_reasoning: Vec<u8>,
    awaiting_outputs: HashSet<Vec<u8>>,
    output_counts: HashMap<Vec<u8>, usize>,
    duplicate_outputs: Vec<Vec<u8>>,
    mergeable_assistant: Option<usize>,
}

impl Conversation<'_> {
    fn fallback_reasoning(&self) -> Vec<u8> {
        if !self.latest_reasoning.is_empty() {
            return self.latest_reasoning.clone();
        }
        if self.has_reasoning {
            UNAVAILABLE.to_vec()
        } else {
            vec![]
        }
    }

    fn note_usable(&mut self, reasoning: &[u8]) {
        if usable_reasoning(reasoning) {
            self.latest_reasoning = reasoning.to_vec();
        }
    }

    fn flush_pending_calls(&mut self) {
        if self.pending_calls.is_empty() {
            return;
        }
        let reasoning = std::mem::take(&mut self.pending_reasoning);
        let calls = AnyValue::Array(std::mem::take(&mut self.pending_calls));
        let mut merged = false;
        if let Some(index) = self.mergeable_assistant
            && index + 1 == self.messages.len()
        {
            let assistant = gj::parse(&self.messages[index]).into_owned();
            if assistant.get("role").bytes().as_ref() == b"assistant" && !assistant.get("tool_calls").exists() {
                let mut updated = self.messages[index].clone();
                calls.set(&mut updated, "tool_calls");
                let combined = combine_reasoning(&assistant.get("reasoning_content").bytes(), &reasoning);
                if !combined.is_empty() {
                    gj::set_str(&mut updated, "reasoning_content", &combined);
                    self.note_usable(&combined);
                } else {
                    let fallback = self.fallback_reasoning();
                    if !fallback.is_empty() {
                        gj::set_str(&mut updated, "reasoning_content", &fallback);
                    }
                }
                self.messages[index] = updated;
                merged = true;
            }
        }
        if !merged {
            let mut message = br#"{"role":"assistant","tool_calls":[]}"#.to_vec();
            calls.set(&mut message, "tool_calls");
            if !reasoning.is_empty() {
                gj::set_str(&mut message, "reasoning_content", &reasoning);
                self.note_usable(&reasoning);
            } else {
                let fallback = self.fallback_reasoning();
                if !fallback.is_empty() {
                    gj::set_str(&mut message, "reasoning_content", &fallback);
                }
            }
            self.messages.push(message);
        }
        for id in std::mem::take(&mut self.pending_call_ids) {
            let id = trim_space(&id).to_vec();
            if !id.is_empty() {
                self.awaiting_outputs.insert(id);
            }
        }
        self.mergeable_assistant = None;
    }

    fn append_pending_reasoning(&mut self) {
        let reasoning = std::mem::take(&mut self.pending_reasoning);
        if reasoning.is_empty() {
            return;
        }
        self.note_usable(&reasoning);
        let mut message = br#"{"role":"assistant","content":"","reasoning_content":""}"#.to_vec();
        gj::set_str(&mut message, "reasoning_content", &reasoning);
        self.messages.push(message);
    }

    fn message(&mut self, item: &Res<'_>) {
        let mut role = item.get("role").bytes().into_owned();
        if role == b"developer" {
            role = b"user".to_vec();
        }
        self.mergeable_assistant = None;
        if role != b"assistant" {
            self.append_pending_reasoning();
            self.latest_reasoning.clear();
        }
        let mut message = br#"{"role":"","content":[]}"#.to_vec();
        gj::set_str(&mut message, "role", &role);
        let content = item.get("content");
        if content.is_array() {
            let mut parts = vec![];
            content.each(|_, part| {
                parts.extend(message_part(&part));
                true
            });
            gj::set_items(&mut message, "content", &parts);
        } else if content.kind == Kind::String {
            gj::set_str(&mut message, "content", &content.s);
        }
        if role == b"assistant" {
            let pending = std::mem::take(&mut self.pending_reasoning);
            let reasoning = combine_reasoning(&pending, &item.get("reasoning_content").bytes());
            if !reasoning.is_empty() {
                gj::set_str(&mut message, "reasoning_content", &reasoning);
                self.note_usable(&reasoning);
            }
        }
        self.messages.push(message);
        if role == b"assistant" {
            self.mergeable_assistant = Some(self.messages.len() - 1);
        }
    }

    fn tool_call(&mut self, item: &Res<'_>, custom: bool) {
        let rc = item.get("reasoning_content").bytes().into_owned();
        self.pending_reasoning = combine_reasoning(&self.pending_reasoning, &rc);
        self.note_usable(&rc);
        let mut call = br#"{"id":"","type":"function","function":{"name":"","arguments":""}}"#.to_vec();
        let call_id = extract_call_id(item);
        if custom {
            gj::set_str(&mut call, "id", &call_id);
            let name = item.get("name").bytes().into_owned();
            let namespace = item.get("namespace").bytes();
            let name = if namespace.is_empty() {
                self.index.canonical_name(&name)
            } else {
                self.index.namespace_name(&namespace, &name)
            };
            gj::set_str(&mut call, "function.name", &name);
            let mut wrapped = br#"{"input":""}"#.to_vec();
            gj::set_str(&mut wrapped, "input", item.get("input").bytes());
            gj::set_str(&mut call, "function.arguments", &wrapped);
        } else {
            if !call_id.is_empty() {
                gj::set_str(&mut call, "id", &call_id);
            }
            let name = item.get("name");
            if name.exists() {
                let name = name.bytes().into_owned();
                let namespace = trim_space(&item.get("namespace").bytes()).to_vec();
                let name = if namespace.is_empty() {
                    self.index.canonical_name(&name)
                } else {
                    self.index.namespace_name(&namespace, &name)
                };
                gj::set_str(&mut call, "function.name", &name);
            }
            let arguments = item.get("arguments");
            if arguments.exists() {
                gj::set_str(&mut call, "function.arguments", arguments.bytes());
            }
        }
        self.pending_calls.push(AnyValue::from_res(&gj::parse(&call)));
        if !call_id.is_empty() {
            self.pending_call_ids.push(call_id);
        }
    }

    fn tool_output(&mut self, item: &Res<'_>, set: SetOutput) {
        self.mergeable_assistant = None;
        let call_id = extract_call_id(item);
        if !call_id.is_empty() {
            let count = self.output_counts.entry(call_id.clone()).or_default();
            *count += 1;
            if *count > 1 && !self.duplicate_outputs.contains(&call_id) {
                self.duplicate_outputs.push(call_id.clone());
            }
        }
        let output = item.get("output");
        if !self.awaiting_outputs.remove(&call_id) {
            if let Some(message) = standalone_output(&output, set) {
                self.messages.push(message);
            }
            return;
        }
        let mut message = br#"{"role":"tool","tool_call_id":"","content":""}"#.to_vec();
        gj::set_str(&mut message, "tool_call_id", &call_id);
        if output.exists() {
            set(&mut message, &output);
        }
        self.messages.push(message);
    }
}

/// ConvertOpenAIResponsesRequestToOpenAIChatCompletions.
pub(crate) fn convert(model: &str, raw: &[u8], stream: bool) -> Vec<u8> {
    let mut out = br#"{"model":"","messages":[],"stream":false}"#.to_vec();
    let root = gj::parse(raw);
    let index = ToolIndex::new(&root);
    gj::set_str(&mut out, "model", model);
    gj::set_bool(&mut out, "stream", stream);

    let format = root.get("text.format");
    if format.exists()
        && let Some(format) = response_format(&format)
    {
        gj::set_raw(&mut out, "response_format", format);
    }
    let max_tokens = root.get("max_output_tokens");
    if max_tokens.exists() {
        gj::set_raw(&mut out, "max_tokens", &max_tokens.raw);
    }
    let mut conversation = Conversation {
        index: &index,
        messages: vec![],
        has_reasoning: false,
        pending_calls: vec![],
        pending_call_ids: vec![],
        pending_reasoning: vec![],
        latest_reasoning: vec![],
        awaiting_outputs: HashSet::new(),
        output_counts: HashMap::new(),
        duplicate_outputs: vec![],
        mergeable_assistant: None,
    };
    let instructions = root.get("instructions");
    if instructions.exists() {
        let mut system = br#"{"role":"system","content":""}"#.to_vec();
        gj::set_str(&mut system, "content", instructions.bytes());
        conversation.messages.push(system);
    }

    let input = root.get("input");
    if input.is_array() {
        let raw_items = input.array();
        let is_output = |i: &Res<'_>| {
            matches!(
                i.get("type").bytes().as_ref(),
                b"function_call_output" | b"custom_tool_call_output"
            )
        };
        let is_call = |i: &Res<'_>| matches!(i.get("type").bytes().as_ref(), b"function_call" | b"custom_tool_call");
        let mut explicit: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut missing_ids = 0;
        for item in raw_items.iter().filter(|i| is_output(i)) {
            let id = extract_call_id(item);
            if id.is_empty() {
                missing_ids += 1;
            } else {
                *explicit.entry(id).or_default() += 1;
            }
        }
        let unclaimed: HashSet<Vec<u8>> = raw_items
            .iter()
            .filter(|i| is_call(i))
            .map(extract_call_id)
            .filter(|id| !id.is_empty() && !explicit.contains_key(id))
            .collect();
        let mut items = normalize_tool_call_outputs(raw_items.clone());
        if missing_ids > 1 || (missing_ids > 0 && unclaimed.len() > 1) {
            for (i, item) in items.iter_mut().enumerate() {
                if is_output(item) && i < raw_items.len() && extract_call_id(&raw_items[i]).is_empty() {
                    let mut raw = item.raw.to_vec();
                    for key in ["call_id", "tool_call_id", "callId"] {
                        gj::delete(&mut raw, key);
                    }
                    *item = gj::parse(&raw).into_owned();
                }
            }
        }

        let effort = root.get("reasoning.effort");
        let flat_effort = root.get("reasoning_effort");
        let reasoning = root.get("reasoning");
        conversation.has_reasoning = if effort.exists() {
            effort_enables_reasoning(&effort)
        } else if flat_effort.exists() {
            effort_enables_reasoning(&flat_effort)
        } else if reasoning.exists() {
            let raw = go_lower(trim_space(&reasoning.bytes()));
            !matches!(raw.as_slice(), b"" | b"none" | b"false" | b"{}")
        } else {
            false
        };
        if !conversation.has_reasoning {
            conversation.has_reasoning = raw_items
                .iter()
                .any(|i| i.get("type").bytes().as_ref() == b"reasoning" || i.get("reasoning_content").exists());
        }

        for item in &items {
            let mut kind = item.get("type").bytes().into_owned();
            if kind.is_empty() && !item.get("role").bytes().is_empty() {
                kind = b"message".to_vec();
            }
            if kind != b"function_call" && kind != b"custom_tool_call" {
                conversation.flush_pending_calls();
            }
            match kind.as_slice() {
                b"message" | b"" => conversation.message(item),
                b"reasoning" => {
                    let content = reasoning_content(item);
                    conversation.pending_reasoning = combine_reasoning(&conversation.pending_reasoning, &content);
                    conversation.note_usable(&content);
                }
                b"function_call" => conversation.tool_call(item, false),
                b"custom_tool_call" => conversation.tool_call(item, true),
                b"function_call_output" => conversation.tool_output(item, set_function_output),
                b"custom_tool_call_output" => conversation.tool_output(item, set_custom_output),
                _ => conversation.mergeable_assistant = None,
            }
        }
        conversation.flush_pending_calls();
        conversation.append_pending_reasoning();
    } else if input.kind == Kind::String {
        let mut message = b"{}".to_vec();
        gj::set_str(&mut message, "role", "user");
        gj::set_str(&mut message, "content", &input.s);
        conversation.messages.push(message);
    }

    if !conversation.messages.is_empty() {
        let messages = align_openai_tool_call_messages_with(
            std::mem::take(&mut conversation.messages),
            &conversation.duplicate_outputs,
        );
        gj::set_raw(&mut out, "messages", gj::join(&messages));
    }

    let tools: Vec<AnyValue> = index
        .chat_tools()
        .iter()
        .map(|t| AnyValue::from_res(&gj::parse(t)))
        .collect();
    if !tools.is_empty() {
        AnyValue::Array(tools).set(&mut out, "tools");
        let parallel = root.get("parallel_tool_calls");
        if parallel.exists() {
            gj::set_bool(&mut out, "parallel_tool_calls", parallel.bool());
        }
        let choice = root.get("tool_choice");
        if choice.exists() {
            gj::set_raw(&mut out, "tool_choice", tool_choice(&choice, &index));
        }
    }
    let effort = root.get("reasoning.effort");
    if effort.exists() {
        let effort = go_lower(trim_space(&effort.bytes()));
        if !effort.is_empty() {
            gj::set_str(&mut out, "reasoning_effort", &effort);
        }
    }
    out
}
