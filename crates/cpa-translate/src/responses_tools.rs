//! Responses tool declarations shared by the non-Claude Responses translators
//! (internal/util/responses_tools.go). The Claude Responses translator keeps its own
//! variant in `claude_responses` because Go's Claude package collects built-in tools too.

use std::collections::HashMap;

use cpa_common::json::{self as gj, Kind, Res};
use sha2::{Digest, Sha256};

use crate::claude_responses::{qualify_namespace_name, tool_name};
use crate::common::{sanitize_function_name, trim_space};

/// util.ResponsesToolIdentity: the client-side identity behind an upstream function name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    pub name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub custom: bool,
    /// From the winning original declaration, never from the upstream name.
    pub apply_patch: bool,
}

/// util.ResponsesToolDescriptor.
pub(crate) struct Descriptor<'a> {
    pub name: Vec<u8>,
    pub local_name: Vec<u8>,
    pub namespace: Vec<u8>,
    pub custom: bool,
    pub tool: Res<'a>,
    priority: u8,
    direct: bool,
    pub order: usize,
}

/// util.ResponsesToolDescription.
pub(crate) fn tool_description(tool: &Res<'_>) -> Vec<u8> {
    let description = tool.get("description").bytes();
    if !description.is_empty() {
        return description.into_owned();
    }
    tool.get("function.description").bytes().into_owned()
}

/// util.ResponsesToolParameters.
pub(crate) fn tool_parameters<'a>(tool: &Res<'a>) -> Option<Res<'a>> {
    [
        "parameters",
        "parametersJsonSchema",
        "input_schema",
        "function.parameters",
        "function.parametersJsonSchema",
    ]
    .iter()
    .map(|path| tool.get(*path))
    .find(Res::exists)
}

/// util.CollectResponsesToolDescriptors: function and custom tools from `tools` and from
/// `additional_tools` input items, with namespace children qualified.
pub(crate) fn descriptors<'a>(root: &Res<'a>) -> Vec<Descriptor<'a>> {
    let mut sources: Vec<(Res<'a>, u8)> = vec![];
    let tools = root.get("tools");
    if tools.is_array() {
        sources.push((tools, 0));
    }
    let input = root.get("input");
    if input.is_array() {
        input.each(|_, item| {
            if item.get("type").bytes().as_ref() == b"additional_tools" {
                let tools = item.get("tools");
                if tools.is_array() {
                    sources.push((tools, 1));
                }
            }
            true
        });
    }
    let mut out: Vec<Descriptor<'a>> = vec![];
    let mut add = |tool: &Res<'a>,
                   name: Vec<u8>,
                   local: Vec<u8>,
                   namespace: Vec<u8>,
                   custom: bool,
                   priority: u8,
                   direct: bool| {
        if name.is_empty() {
            return;
        }
        let order = out.len();
        out.push(Descriptor {
            name,
            local_name: local,
            namespace,
            custom,
            tool: tool.clone(),
            priority,
            direct,
            order,
        });
    };
    for (tools, priority) in sources {
        tools.each(|_, tool| {
            match trim_space(&tool.get("type").bytes()) {
                kind @ (b"" | b"function" | b"custom") => {
                    let name = tool_name(&tool);
                    add(&tool, name.clone(), name, vec![], kind == b"custom", priority, true);
                }
                b"namespace" => {
                    let namespace = trim_space(&tool.get("name").bytes()).to_vec();
                    let mut children = tool.get("tools");
                    if !children.is_array() {
                        children = tool.get("children");
                    }
                    if children.is_array() {
                        children.each(|_, child| {
                            let child_name = tool_name(&child);
                            if child_name.is_empty() {
                                return true;
                            }
                            let qualified = qualify_namespace_name(&namespace, &child_name);
                            if let kind @ (b"" | b"function" | b"custom") = trim_space(&child.get("type").bytes()) {
                                add(
                                    &child,
                                    qualified,
                                    child_name,
                                    namespace.clone(),
                                    kind == b"custom",
                                    priority,
                                    false,
                                );
                            }
                            true
                        });
                    }
                }
                _ => {}
            }
            true
        });
    }
    out
}

/// util.CollectResponsesToolWinners: per name, top-level beats additional_tools, direct
/// beats namespaced, then first wins. Values are descriptor orders.
pub(crate) fn winners(descriptors: &[Descriptor<'_>]) -> HashMap<Vec<u8>, usize> {
    let mut winners: HashMap<Vec<u8>, usize> = HashMap::new();
    for d in descriptors {
        let better = winners.get(&d.name).is_none_or(|&w| {
            let c = &descriptors[w];
            if d.priority != c.priority {
                d.priority < c.priority
            } else if d.direct != c.direct {
                d.direct
            } else {
                d.order < c.order
            }
        });
        if better {
            winners.insert(d.name.clone(), d.order);
        }
    }
    winners
}

/// sanitizeResponsesToolNames: Gemini-safe names; names whose sanitized forms collide get
/// a sha256-derived suffix, assigned in sorted name order.
fn sanitize_names(names: &[&[u8]]) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut unique: Vec<&[u8]> = vec![];
    let mut base_counts: HashMap<Vec<u8>, usize> = HashMap::new();
    for &name in names {
        if name.is_empty() || unique.contains(&name) {
            continue;
        }
        unique.push(name);
        *base_counts.entry(sanitize_function_name(name)).or_default() += 1;
    }
    unique.sort();
    let mut out = HashMap::new();
    let mut used: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    for name in unique {
        let base = sanitize_function_name(name);
        let mapped = if base_counts[&base] > 1 || used.contains_key(&base) {
            disambiguate(&base, name, &used)
        } else {
            base
        };
        used.insert(mapped.clone(), name.to_vec());
        out.insert(name.to_vec(), mapped);
    }
    out
}

/// util.functionNamesFromRequest: declared function names of a request's `tools`
/// (nested `tools`, Gemini declarations, OpenAI `function.name`, then `name`).
fn function_names_from_request(raw: &[u8]) -> Vec<Vec<u8>> {
    fn collect(tool: &Res<'_>, names: &mut Vec<Vec<u8>>) {
        let nested = tool.get("tools");
        if nested.is_array() {
            nested.each(|_, t| {
                collect(&t, names);
                true
            });
            return;
        }
        let mut declared = false;
        for key in ["functionDeclarations", "function_declarations"] {
            let declarations = tool.get(key);
            if declarations.is_array() {
                declared = true;
                declarations.each(|_, d| {
                    let name = d.get("name").bytes();
                    if !name.is_empty() {
                        names.push(name.into_owned());
                    }
                    true
                });
            }
        }
        if declared {
            return;
        }
        for path in ["function.name", "name"] {
            let name = tool.get(path).bytes();
            if !name.is_empty() {
                names.push(name.into_owned());
                return;
            }
        }
    }
    if raw.is_empty() || !gj::valid(raw) {
        return vec![];
    }
    let tools = gj::get(raw, "tools");
    let mut names = vec![];
    if tools.is_array() {
        tools.each(|_, tool| {
            collect(&tool, &mut names);
            true
        });
    }
    names
}

/// util.SanitizedFunctionNameMap: declared name -> collision-free Gemini-safe name.
pub(crate) fn sanitized_function_name_map(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    let names = function_names_from_request(raw);
    let names: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
    sanitize_names(&names)
}

/// util.MapSanitizedFunctionName: the request-specific name, else the sanitized name.
pub(crate) fn map_sanitized_function_name(map: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    match map.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_function_name(name),
    }
}

/// util.DisambiguatedToolNameMap: sanitized name -> declared name, for names that changed.
pub(crate) fn disambiguated_tool_name_map(raw: &[u8]) -> HashMap<Vec<u8>, Vec<u8>> {
    sanitized_function_name_map(raw)
        .into_iter()
        .filter(|(original, sanitized)| original != sanitized)
        .map(|(original, sanitized)| (sanitized, original))
        .collect()
}

fn disambiguate(base: &[u8], original: &[u8], used: &HashMap<Vec<u8>, Vec<u8>>) -> Vec<u8> {
    for attempt in 0u64.. {
        let mut hasher = Sha256::new();
        hasher.update(original);
        hasher.update(format!("\x00{attempt}"));
        let digest = hasher.finalize();
        let suffix = format!("_{}", crate::common::hex(&digest[..6]));
        let prefix = &base[..base.len().min(64 - suffix.len())];
        let candidate = [prefix, suffix.as_bytes()].concat();
        if !used.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!("an unused suffix always exists")
}

/// util.BuildGeminiFunctionDeclarations: Gemini `functionDeclarations`, the forward map
/// (Responses name to Gemini name) and the reverse identity map.
pub(crate) type Declarations = (Vec<Vec<u8>>, HashMap<Vec<u8>, Vec<u8>>, HashMap<Vec<u8>, Identity>);

pub(crate) fn gemini_function_declarations(root: &Res<'_>) -> Declarations {
    let descriptors = descriptors(root);
    let winners = winners(&descriptors);
    let winning: Vec<&Descriptor<'_>> = descriptors.iter().filter(|d| winners[&d.name] == d.order).collect();
    let mut declarations = vec![];
    let mut forward: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut reverse: HashMap<Vec<u8>, Identity> = HashMap::new();
    if winning.is_empty() {
        return (declarations, forward, reverse);
    }
    let names: Vec<&[u8]> = winning.iter().map(|d| d.name.as_slice()).collect();
    let sanitized = sanitize_names(&names);
    for d in winning {
        let gemini_name = match sanitized.get(&d.name) {
            Some(mapped) if !mapped.is_empty() => mapped.clone(),
            _ => sanitize_function_name(&d.name),
        };
        forward.insert(d.name.clone(), gemini_name.clone());
        if !d.local_name.is_empty() && d.local_name != d.name {
            forward
                .entry(d.local_name.clone())
                .or_insert_with(|| gemini_name.clone());
        }
        let apply_patch = crate::apply_patch::is_custom_tool(&d.tool);
        let identity = Identity {
            name: d.local_name.clone(),
            namespace: d.namespace.clone(),
            custom: d.custom,
            apply_patch,
        };
        if d.name != gemini_name {
            reverse.insert(d.name.clone(), identity.clone());
        }
        reverse.insert(gemini_name.clone(), identity);

        let mut decl = br#"{"name":"","description":"","parametersJsonSchema":{}}"#.to_vec();
        gj::set_str(&mut decl, "name", &gemini_name);
        let description = tool_description(&d.tool);
        if !description.is_empty() {
            gj::set_str(&mut decl, "description", &description);
        }
        if apply_patch {
            gj::set_str(&mut decl, "description", crate::apply_patch::description(&d.tool));
            gj::set_raw(&mut decl, "parametersJsonSchema", crate::apply_patch::PARAMETERS);
        } else if d.custom {
            gj::set_raw(
                &mut decl,
                "parametersJsonSchema",
                br#"{"type":"object","properties":{"input":{"type":"string"}},"required":["input"]}"#,
            );
        } else if let Some(params) = tool_parameters(&d.tool) {
            gj::set_raw(
                &mut decl,
                "parametersJsonSchema",
                cpa_common::gemini_schema::for_gemini_json_schema(&params.raw),
            );
        }
        declarations.push(decl);
    }
    (declarations, forward, reverse)
}

/// util.ResponsesToolReverseIdentityMap over a raw Responses request (or a `request`
/// wrapper holding one).
pub(crate) fn reverse_identity_map(raw: &[u8]) -> HashMap<Vec<u8>, Identity> {
    if raw.is_empty() || !gj::valid(raw) {
        return HashMap::new();
    }
    let mut root = gj::parse(raw);
    let req = root.get("request");
    if req.exists() && (req.get("model").exists() || req.get("input").exists() || req.get("tools").exists()) {
        root = req;
    }
    gemini_function_declarations(&root).2
}

/// util.MapResponsesToolName.
pub(crate) fn map_tool_name(forward: &HashMap<Vec<u8>, Vec<u8>>, name: &[u8]) -> Vec<u8> {
    match forward.get(name) {
        Some(mapped) if !mapped.is_empty() => mapped.clone(),
        _ => sanitize_function_name(name),
    }
}

/// util.ConvertResponsesToolChoiceToGemini: the `functionCallingConfig` object.
pub(crate) fn tool_choice_to_gemini(choice: &Res<'_>, forward: &HashMap<Vec<u8>, Vec<u8>>) -> Option<Vec<u8>> {
    if !choice.exists() {
        return None;
    }
    let mode_of = |s: &[u8]| -> Option<&'static str> {
        match crate::common::go_lower(trim_space(s)).as_slice() {
            b"none" => Some("NONE"),
            b"auto" => Some("AUTO"),
            b"required" | b"any" => Some("ANY"),
            _ => None,
        }
    };
    let mut mode = None;
    let mut allowed: Vec<Vec<u8>> = vec![];
    if choice.kind == Kind::String {
        mode = mode_of(&choice.s);
    } else if choice.is_object() {
        let kind = crate::common::go_lower(trim_space(&choice.get("type").bytes()));
        match kind.as_slice() {
            b"function" | b"custom" | b"tool" | b"" => {
                mode = Some("ANY");
                let first = |paths: [&str; 3]| -> Vec<u8> {
                    paths
                        .iter()
                        .map(|p| trim_space(&choice.get(*p).bytes()).to_vec())
                        .find(|v| !v.is_empty())
                        .unwrap_or_default()
                };
                let mut name = first(["name", "function.name", "custom.name"]);
                let namespace = first(["namespace", "function.namespace", "custom.namespace"]);
                if !namespace.is_empty() {
                    name = qualify_namespace_name(&namespace, &name);
                }
                if !name.is_empty() {
                    allowed.push(map_tool_name(forward, &name));
                }
            }
            other => mode = mode_of(other),
        }
    }
    let mode = mode?;
    let mut cfg = br#"{"mode":""}"#.to_vec();
    gj::set_str(&mut cfg, "mode", mode);
    if !allowed.is_empty() {
        gj::set_strs(&mut cfg, "allowedFunctionNames", &allowed);
    }
    Some(cfg)
}

/// util.UnwrapResponsesCustomToolInput: the `input` of `{"input": ...}` arguments, a bare
/// JSON string's value, or the trimmed arguments.
pub(crate) fn unwrap_custom_tool_input(arguments: &[u8]) -> Vec<u8> {
    let arguments = trim_space(arguments);
    if arguments.is_empty() || arguments == b"{}" {
        return vec![];
    }
    if gj::valid(arguments) {
        let parsed = gj::parse(arguments);
        let v = parsed.get("input");
        if v.exists() {
            return if v.kind == Kind::String {
                v.s.to_vec()
            } else {
                v.raw.to_vec()
            };
        }
        if parsed.kind == Kind::String {
            return parsed.s.to_vec();
        }
    }
    arguments.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_sanitized_names_get_go_hash_suffixes() {
        // "a b" and "a_b" both sanitize to "a_b" ("." is allowed and stays); Go suffixes
        // every colliding name with sha256("<name>\x00<attempt>")[:6] in hex.
        let names: [&[u8]; 3] = [b"a_b", b"a b", b"a.b"];
        let out = sanitize_names(&names);
        assert_eq!(out[&b"a.b".to_vec()], b"a.b");
        let expect = |name: &[u8]| {
            let digest = Sha256::digest([name, b"\x000"].concat());
            format!("a_b_{}", crate::common::hex(&digest[..6])).into_bytes()
        };
        assert_eq!(out[&b"a b".to_vec()], expect(b"a b"));
        assert_eq!(out[&b"a_b".to_vec()], expect(b"a_b"));
        assert_ne!(out[&b"a b".to_vec()], out[&b"a_b".to_vec()]);
    }

    #[test]
    fn namespace_children_fall_back_to_children_key() {
        let raw = br#"{"tools":[{"type":"namespace","name":"ns","children":[{"type":"function","name":"run"}]}]}"#;
        let (decls, forward, reverse) = gemini_function_declarations(&gj::parse(raw));
        assert_eq!(decls.len(), 1);
        assert_eq!(forward[&b"run".to_vec()], b"ns__run");
        assert_eq!(reverse[&b"ns__run".to_vec()].namespace, b"ns");
    }
}
