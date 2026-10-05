//! MCP tool aliasing for the Claude Code wire (helps/claude_mcp_alias.go and the
//! remap/restore half of claude_executor_request.go).
//!
//! Custom tool names become stable `mcp__<word>_<word>__<word>_<name>` aliases keyed
//! by the downstream caller's key, consistently in declarations, tool_choice and
//! history. Responses are restored with the request-local inverse map, in buffered
//! JSON and per SSE event. Server tools and names already shaped like MCP tools are
//! never renamed.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use cpa_common::json as gj;
use sha2::{Digest, Sha256};

use crate::rawjson;

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| include_str!("bip39_words.txt").split_whitespace().collect())
}

pub(crate) const DEFAULT_SECRET: &str = "cpa-claude-mcp-default-caller";

pub(crate) fn is_mcp_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || !name.starts_with("mcp__") {
        return false;
    }
    let rest = &name[5..];
    let Some(sep) = rest.find("__") else { return false };
    if sep == 0 || sep + 2 >= rest.len() {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn is_server_tool_type(kind: &str) -> bool {
    let kind = kind.trim().to_lowercase();
    [
        "advisor_",
        "agent_toolset_",
        "bash_",
        "code_execution_",
        "computer_",
        "memory_",
        "text_editor_",
        "tool_search_tool_",
        "web_fetch_",
        "web_search_",
    ]
    .iter()
    .any(|p| kind.starts_with(p))
}

/// HMAC-SHA256(secret, "cpa-claude-mcp-alias-v2\0" + purpose + "\0" + original).
fn digest(secret: &str, purpose: &str, original: &str) -> [u8; 32] {
    let mut key = [0u8; 64];
    if secret.len() > 64 {
        key[..32].copy_from_slice(&Sha256::digest(secret.as_bytes()));
    } else {
        key[..secret.len()].copy_from_slice(secret.as_bytes());
    }
    let pad = |byte: u8| key.map(|k| k ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(b"cpa-claude-mcp-alias-v2\0")
        .chain_update(purpose.as_bytes())
        .chain_update([0])
        .chain_update(original.as_bytes())
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

fn word(digest: &[u8], offset: usize) -> &'static str {
    let base = usize::from(u16::from_be_bytes([digest[offset], digest[offset + 1]]));
    words()[base % words().len()]
}

fn server(secret: &str) -> String {
    let d = digest(secret, "server", "");
    format!("{}_{}", word(&d, 0), word(&d, 2))
}

fn semantic_suffix(original: &str, max: usize) -> String {
    let mut out = String::new();
    let mut pending = false;
    for c in original.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            pending = !out.is_empty();
            continue;
        }
        if pending && out.len() + 1 < max {
            out.push('_');
        }
        pending = false;
        if out.len() >= max {
            break;
        }
        out.push(c);
    }
    let trimmed = out.trim_matches(['_', '-']);
    if trimmed.is_empty() {
        "tool".into()
    } else {
        trimmed.into()
    }
}

fn alias_for(server: &str, tool_word: &str, original: &str) -> String {
    let prefix = format!("mcp__{server}__{tool_word}_");
    let max = 64usize.saturating_sub(prefix.len()).max(1);
    format!("{prefix}{}", semantic_suffix(original, max))
}

/// `AllocateClaudeMCPToolAlias`: the first free word from the tool digest onward.
fn allocate(secret: &str, original: &str, reserved: &HashSet<String>) -> Option<String> {
    let server = server(secret);
    let d = digest(secret, "tool", original);
    let base = usize::from(u16::from_be_bytes([d[0], d[1]])) % words().len();
    (0..words().len())
        .map(|attempt| alias_for(&server, words()[(base + attempt) % words().len()], original))
        .find(|alias| !reserved.contains(alias))
}

/// Inverse map: alias → original (passthrough MCP names map to themselves).
pub(crate) type Reverse = HashMap<String, String>;

/// One rename: the JSON path and either a raw replacement (the rebuilt `tools` array) or
/// an alias string.
enum Edit {
    Raw(String, String),
    Alias(String, String),
}

/// `remapOAuthToolNamesWithOptions`: every declared client tool gets an MCP alias in
/// declarations, `tool_choice` and history. Valid bodies are edited in one copy at the
/// values' offsets (`remapOAuthToolNamesWithBatchedEdits`); a body whose offsets are not
/// usable, such as malformed JSON, gets the same renames by path through sjson
/// (`remapOAuthToolNamesWithOptionsLegacy`), as Go does.
pub(crate) fn remap(body: &str, secret: &str) -> (String, Reverse) {
    let (edits, reverse) = plan(body.as_bytes(), secret);
    if gj::valid(body.as_bytes())
        && let Some(out) = apply_at_offsets(body, &edits)
    {
        return (out, reverse);
    }
    let mut out = body.as_bytes().to_vec();
    for edit in &edits {
        let updated = match edit {
            Edit::Raw(path, raw) => gj::try_set_raw(&out, path.as_str(), raw),
            Edit::Alias(path, alias) => gj::try_set_str(&out, path.as_str(), alias),
        };
        if let Ok(updated) = updated {
            out = updated;
        }
    }
    let out = String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
    (out, reverse)
}

/// The renames of [`remap`] and the inverse map, read with Go's gjson semantics (which
/// also read malformed JSON the way Go does).
fn plan(body: &[u8], secret: &str) -> (Vec<Edit>, Reverse) {
    let mut reverse = Reverse::new();
    let record = |reverse: &mut Reverse, original: &str, renamed: &str| {
        reverse.entry(renamed.to_owned()).or_insert_with(|| original.to_owned());
    };
    let tools = gj::get(body, "tools");
    let tool_list = if tools.exists() && tools.is_array() {
        tools.array()
    } else {
        Vec::new()
    };
    let name_of = |tool: &gj::Res<'_>| tool.get("name").str().into_owned();
    let type_of = |tool: &gj::Res<'_>| tool.get("type").str().into_owned();
    // AugmentClaudeBuiltinToolRegistry, then every declared name.
    let mut reserved: HashSet<String> = ["web_search", "code_execution", "text_editor", "computer"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut protected = HashSet::new();
    for tool in &tool_list {
        let name = name_of(tool);
        if !name.is_empty() {
            reserved.insert(name.clone());
        }
        if is_server_tool_type(&type_of(tool)) {
            protected.insert(name);
        }
    }
    let mut forward: HashMap<String, String> = HashMap::new();
    let mut passthrough = Vec::new();
    for tool in &tool_list {
        let name = name_of(tool);
        if is_server_tool_type(&type_of(tool)) || name.is_empty() {
            continue;
        }
        if is_mcp_name(&name) {
            passthrough.push(name);
            continue;
        }
        if forward.contains_key(&name) {
            continue;
        }
        if let Some(alias) = allocate(secret, &name, &reserved) {
            reserved.insert(alias.clone());
            forward.insert(name, alias);
        }
    }
    // recordPassthroughMCPTools: only when something was aliased.
    if !forward.is_empty() {
        for name in &passthrough {
            record(&mut reverse, name, name);
        }
    }
    let rewrite = |name: &str| -> Option<String> {
        if name.is_empty() || protected.contains(name) || is_mcp_name(name) {
            return None;
        }
        forward.get(name).filter(|a| a.as_str() != name).cloned()
    };
    let mut edits = Vec::new();
    // 1. tools[]: typed declarations lose `type`, client names become aliases.
    let needs_tools_rewrite = tool_list.iter().any(|t| {
        let kind = type_of(t);
        !is_server_tool_type(&kind) && (!kind.trim().is_empty() || rewrite(&name_of(t)).is_some())
    });
    if needs_tools_rewrite {
        let mut rebuilt = Vec::new();
        for tool in &tool_list {
            let raw = String::from_utf8_lossy(tool.raw()).into_owned();
            if is_server_tool_type(&type_of(tool)) {
                rebuilt.push(raw);
                continue;
            }
            let mut raw = raw;
            if !type_of(tool).trim().is_empty() {
                raw = rawjson::delete(&raw, "type");
            }
            let name = name_of(tool);
            if let Some(alias) = rewrite(&name) {
                raw = rawjson::set_str(&raw, "name", &alias);
                record(&mut reverse, &name, &alias);
            }
            rebuilt.push(raw);
        }
        edits.push(Edit::Raw("tools".into(), format!("[{}]", rebuilt.join(","))));
    }
    // 2. tool_choice naming a declared client tool.
    if gj::get(body, "tool_choice.type").str() == "tool" {
        let name = gj::get(body, "tool_choice.name").str().into_owned();
        if let Some(alias) = rewrite(&name) {
            record(&mut reverse, &name, &alias);
            edits.push(Edit::Alias("tool_choice.name".into(), alias));
        }
    }
    // 3. History references.
    let messages = gj::get(body, "messages");
    if messages.exists() && messages.is_array() {
        for (m, msg) in messages.array().iter().enumerate() {
            let content = msg.get("content");
            if !content.exists() || !content.is_array() {
                continue;
            }
            for (i, part) in content.array().iter().enumerate() {
                for path in reference_paths(part) {
                    let name = part.get(path.as_str()).str().into_owned();
                    if let Some(alias) = rewrite(&name) {
                        record(&mut reverse, &name, &alias);
                        edits.push(Edit::Alias(format!("messages.{m}.content.{i}.{path}"), alias));
                    }
                }
            }
        }
    }
    (edits, reverse)
}

/// Paths, relative to one message content part, of the tool names it references.
fn reference_paths(part: &gj::Res<'_>) -> Vec<String> {
    let mut paths = Vec::new();
    match &*part.get("type").str() {
        "tool_use" => paths.push("name".into()),
        "tool_reference" => paths.push("tool_name".into()),
        "tool_result" => {
            let nested = part.get("content");
            if nested.exists() && nested.is_array() {
                for (n, np) in nested.array().iter().enumerate() {
                    if np.get("type").str() == "tool_reference" {
                        paths.push(format!("content.{n}.tool_name"));
                    }
                }
            }
        }
        "tool_search_tool_result" => {
            let refs = part.get("content.tool_references");
            if refs.exists() && refs.is_array() {
                for (n, rp) in refs.array().iter().enumerate() {
                    if rp.get("type").str() == "tool_reference" {
                        paths.push(format!("content.tool_references.{n}.tool_name"));
                    }
                }
            }
        }
        // claudeToolChangeNamePath.
        "tool_addition" | "tool_removal" => match &*part.get("tool.type").str() {
            "tool_reference" => paths.push("tool.name".into()),
            "tool_definition"
                if part.get("type").str() == "tool_addition"
                    && !is_server_tool_type(&part.get("tool.definition.type").str()) =>
            {
                paths.push("tool.definition.name".into());
            }
            _ => {}
        },
        _ => {}
    }
    paths
}

/// `applyClaudeRawJSONEdits` at the values' gjson offsets; `None` when an offset does
/// not point at the value or edits overlap (Go then takes the legacy path).
fn apply_at_offsets(body: &str, edits: &[Edit]) -> Option<String> {
    let bytes = body.as_bytes();
    let mut spans = Vec::with_capacity(edits.len());
    for edit in edits {
        let (path, replacement) = match edit {
            Edit::Raw(path, raw) => (path, raw.clone()),
            // Generated aliases only use [A-Za-z0-9_-]: quoting is sjson's encoding.
            Edit::Alias(path, alias) => (path, format!("\"{alias}\"")),
        };
        let value = gj::get(bytes, path.as_str());
        let (start, raw) = (value.index, value.raw());
        let end = start + raw.len();
        if raw.is_empty() || end > bytes.len() || &bytes[start..end] != raw || !body.is_char_boundary(start) {
            return None;
        }
        spans.push((start, end, replacement));
    }
    spans.sort_by_key(|span| span.0);
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0;
    for (start, end, replacement) in spans {
        if start < cursor || !body.is_char_boundary(end) {
            return None;
        }
        out.push_str(&body[cursor..start]);
        out.push_str(&replacement);
        cursor = end;
    }
    out.push_str(&body[cursor..]);
    Some(out)
}

struct Parts {
    server: String,
    semantic: String,
}

/// `parseClaudeMCPAlias`: (server, tool ID, semantic suffix).
pub(super) fn alias_parts(name: &str) -> Option<(&str, &str, &str)> {
    if !is_mcp_name(name) {
        return None;
    }
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    let (tool_id, semantic) = tool.split_once('_')?;
    (!server.is_empty() && !tool_id.is_empty() && !semantic.is_empty()).then_some((server, tool_id, semantic))
}

fn parse_alias(name: &str) -> Option<Parts> {
    alias_parts(name).map(|(server, _, semantic)| Parts {
        server: server.into(),
        semantic: semantic.into(),
    })
}

fn alias_server(name: &str) -> String {
    name.strip_prefix("mcp__")
        .and_then(|r| r.split_once("__"))
        .map(|(s, _)| s.to_owned())
        .unwrap_or_default()
}

/// Restores aliases the model may echo exactly, with a repeated server prefix, or
/// with a drifted tool word. Ambiguity is a request error, never a guess.
pub(crate) struct Resolver<'a> {
    exact: &'a Reverse,
    aliases: Vec<(String, String, Parts)>,
    servers: HashSet<String>,
    passthroughs: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(reverse: &'a Reverse) -> Self {
        let mut r = Self {
            exact: reverse,
            aliases: Vec::new(),
            servers: HashSet::new(),
            passthroughs: Vec::new(),
        };
        for (alias, original) in reverse {
            if alias == original {
                r.passthroughs.push(original.clone());
            } else if let Some(parts) = parse_alias(alias) {
                r.servers.insert(parts.server.clone());
                r.aliases.push((alias.clone(), original.clone(), parts));
            }
        }
        r
    }

    /// `Ok(Some(original))` to rewrite, `Ok(None)` to leave unchanged.
    pub fn resolve(&self, name: &str) -> Result<Option<String>, String> {
        if let Some(original) = self.exact.get(name) {
            return Ok((original != name).then(|| original.clone()));
        }
        let server = alias_server(name);
        if !self.servers.contains(&server) {
            return Ok(None);
        }
        let prefix = format!("mcp__{server}__");
        let mut normalized = name.to_owned();
        let mut suffix = name.strip_prefix(&prefix).unwrap_or(name).to_owned();
        while let Some(stripped) = suffix.strip_prefix(&format!("{server}__")) {
            suffix = stripped.to_owned();
            normalized = format!("{prefix}{suffix}");
            if let Some(original) = self.exact.get(&normalized) {
                return Ok(Some(original.clone()));
            }
        }
        let matches: Vec<_> = self
            .aliases
            .iter()
            .filter(|(alias, _, p)| p.server == server && name.ends_with(alias.as_str()))
            .collect();
        match matches.len() {
            1 => return Ok(Some(matches[0].1.clone())),
            n if n > 1 => {
                return Err(format!(
                    "cannot restore Claude OAuth MCP tool alias {name:?}: matched multiple declared aliases"
                ));
            }
            _ => {}
        }
        let mut matched: Vec<&String> = Vec::new();
        if let Some(parts) = parse_alias(&normalized) {
            matched.extend(
                self.aliases
                    .iter()
                    .filter(|(_, _, p)| p.server == parts.server && p.semantic == parts.semantic)
                    .map(|(_, o, _)| o),
            );
        }
        if matched.is_empty() {
            let suffix_matches: Vec<_> = self
                .aliases
                .iter()
                .filter(|(_, _, p)| p.server == server && normalized.ends_with(&format!("_{}", p.semantic)))
                .collect();
            if suffix_matches.len() == 1 {
                matched.push(&suffix_matches[0].1);
            } else if suffix_matches.len() > 1 {
                let mut longest = suffix_matches[0];
                let mut tie = false;
                for c in &suffix_matches[1..] {
                    if c.2.semantic.len() > longest.2.semantic.len() {
                        longest = c;
                        tie = false;
                    } else if c.2.semantic.len() == longest.2.semantic.len() {
                        tie = true;
                    }
                }
                if tie {
                    return Err(format!(
                        "cannot restore Claude OAuth MCP tool alias {name:?}: semantic suffix matches multiple declared tools"
                    ));
                }
                matched.push(&longest.1);
            }
        }
        match matched.len() {
            1 => return Ok(Some(matched[0].clone())),
            n if n > 1 => {
                return Err(format!(
                    "cannot restore Claude OAuth MCP tool alias {name:?}: semantic suffix matches multiple declared tools"
                ));
            }
            _ => {}
        }
        if !self.passthroughs.is_empty() {
            let reprefixed = format!("mcp__{suffix}");
            if self.exact.get(&reprefixed).is_some_and(|o| *o == reprefixed) {
                return Ok(Some(reprefixed));
            }
            let found: Vec<_> = self
                .passthroughs
                .iter()
                .filter(|pt| {
                    let tool = pt
                        .strip_prefix("mcp__")
                        .and_then(|r| r.split_once("__"))
                        .map_or(pt.as_str(), |(_, t)| t);
                    tool == suffix
                })
                .collect();
            match found.len() {
                1 => return Ok(Some(found[0].clone())),
                n if n > 1 => {
                    return Err(format!(
                        "cannot restore Claude OAuth MCP tool alias {name:?}: passthrough tool suffix matches multiple declared tools"
                    ));
                }
                _ => {}
            }
        }
        Ok(None)
    }
}

/// `reverseRemapOAuthToolNames` on a buffered Messages response.
pub(crate) fn restore_response(body: &str, reverse: &Reverse) -> Result<String, String> {
    if reverse.is_empty() {
        return Ok(body.to_owned());
    }
    let content = rawjson::get(body, "content");
    if content.kind() != gjson::Kind::Array {
        return Ok(body.to_owned());
    }
    let resolver = Resolver::new(reverse);
    let mut paths = Vec::new();
    for (i, part) in content.array().iter().enumerate() {
        match part.get("type").str() {
            "tool_use" => paths.push(format!("content.{i}.name")),
            "tool_reference" => paths.push(format!("content.{i}.tool_name")),
            "tool_result" => {
                let nested = part.get("content");
                if nested.kind() == gjson::Kind::Array {
                    for (n, np) in nested.array().iter().enumerate() {
                        if np.get("type").str() == "tool_reference" {
                            paths.push(format!("content.{i}.content.{n}.tool_name"));
                        }
                    }
                }
            }
            "tool_search_tool_result" => {
                let refs = part.get("content.tool_references");
                if refs.kind() == gjson::Kind::Array {
                    for (n, rp) in refs.array().iter().enumerate() {
                        if rp.get("type").str() == "tool_reference" {
                            paths.push(format!("content.{i}.content.tool_references.{n}.tool_name"));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = body.to_owned();
    for path in paths {
        let name = rawjson::string(&out, &path);
        if let Some(original) = resolver.resolve(&name)? {
            out = rawjson::set_str(&out, &path, &original);
        }
    }
    Ok(out)
}

/// `reverseRemapOAuthToolNamesFromStreamLine` for one `data:` payload. Returns the
/// rewritten JSON payload when a name changed.
pub(crate) fn restore_event_payload(payload: &str, reverse: &Reverse) -> Result<Option<String>, String> {
    if reverse.is_empty() || !gjson::valid(payload) {
        return Ok(None);
    }
    let block = rawjson::get(payload, "content_block");
    if !block.exists() {
        return Ok(None);
    }
    let resolver = Resolver::new(reverse);
    let paths: Vec<String> = match block.get("type").str() {
        "tool_use" => vec!["content_block.name".into()],
        "tool_reference" => vec!["content_block.tool_name".into()],
        "tool_search_tool_result" => {
            let refs = block.get("content.tool_references");
            if refs.kind() != gjson::Kind::Array {
                return Ok(None);
            }
            refs.array()
                .iter()
                .enumerate()
                .filter(|(_, r)| r.get("type").str() == "tool_reference")
                .map(|(n, _)| format!("content_block.content.tool_references.{n}.tool_name"))
                .collect()
        }
        _ => return Ok(None),
    };
    let mut out = payload.to_owned();
    let mut changed = false;
    for path in paths {
        let name = rawjson::string(&out, &path);
        if let Some(original) = resolver.resolve(&name)? {
            out = rawjson::set_str(&out, &path, &original);
            changed = true;
        }
    }
    Ok(changed.then_some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_alias_matches_clean_go_capture() {
        let body = r#"{"tools":[{"name":"fixture_lookup","input_schema":{"type":"object"}},{"name":"mcp__fixture__native","input_schema":{}}],"tool_choice":{"type":"tool","name":"fixture_lookup"},"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"fixture_lookup","input":{}}]}]}"#;
        let (out, reverse) = remap(body, "fixture-client-key");
        let alias = "mcp__poem_real__leisure_fixture_lookup";
        assert_eq!(out, body.replace("\"fixture_lookup\"", &format!("\"{alias}\"")));
        assert_eq!(reverse.get(alias).map(String::as_str), Some("fixture_lookup"));
        assert_eq!(
            reverse.get("mcp__fixture__native").map(String::as_str),
            Some("mcp__fixture__native")
        );
        let resolver = Resolver::new(&reverse);
        assert_eq!(resolver.resolve(alias).unwrap().as_deref(), Some("fixture_lookup"));
        assert_eq!(resolver.resolve("mcp__fixture__native").unwrap(), None);
        // Drifted tool word with the same server and semantic suffix still restores.
        assert_eq!(
            resolver
                .resolve("mcp__poem_real__abandon_fixture_lookup")
                .unwrap()
                .as_deref(),
            Some("fixture_lookup")
        );
        assert_eq!(resolver.resolve("mcp__other__x_fixture_lookup").unwrap(), None);
    }
}
