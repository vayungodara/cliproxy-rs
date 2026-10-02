//! Line-level YAML edits that keep untouched bytes stable.
//!
//! Go's saver re-encodes the whole file. The owner requires management writes to
//! leave untouched keys byte for byte, so legacy-to-v8 migration moves each legacy
//! root entry's original lines (head comments, value, inline comments) under its v8
//! parent, re-indented. Only block-style documents are handled; anything else
//! (flow roots, anchors, tabs, multi-document files, nested legacy sources) returns
//! `None` and the caller falls back to the value-level writer. Callers must also
//! check that the result parses to the migrated value.
use serde_yaml_ng::Value;

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with('#')
}

/// The plain key of a block mapping entry starting at `indent`, if `line` is one.
fn key_at(line: &str, indent: usize) -> Option<&str> {
    if indent_of(line) != indent {
        return None;
    }
    let rest = &line[indent..];
    if rest.starts_with([
        '#', '-', '{', '[', '?', '&', '*', '!', '"', '\'', '|', '>', '%', '@', '`',
    ]) {
        return None;
    }
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..];
    (after.is_empty() || after.starts_with(' ')).then_some(rest[..colon].trim_end())
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    /// First line, including directly attached head comments.
    start: usize,
    key_line: usize,
    /// Exclusive end: the last non-blank line of the entry's value plus one.
    end: usize,
}

/// Entries of the block mapping whose keys sit at `indent` within `lines[from..to]`.
fn entries(lines: &[String], indent: usize, from: usize, to: usize) -> Vec<(String, Entry)> {
    let mut out: Vec<(String, Entry)> = Vec::new();
    let mut i = from;
    while i < to {
        let Some(key) = key_at(&lines[i], indent) else {
            i += 1;
            continue;
        };
        let mut start = i;
        while start > from && is_comment(&lines[start - 1]) && indent_of(&lines[start - 1]) == indent {
            start -= 1;
        }
        let mut end = i + 1;
        let mut j = i + 1;
        while j < to {
            let line = &lines[j];
            if is_blank(line) {
                j += 1;
                continue;
            }
            let deeper = indent_of(line) > indent;
            let indentless_item = indent_of(line) == indent && line[indent..].starts_with('-');
            if !(deeper || indentless_item) {
                break;
            }
            j += 1;
            end = j;
        }
        out.push((
            key.to_owned(),
            Entry {
                start,
                key_line: i,
                end,
            },
        ));
        i = end.max(i + 1);
    }
    out
}

/// Child indentation of a block mapping value, or `None` when it is not one.
fn child_indent(lines: &[String], e: &Entry, parent: usize) -> Option<usize> {
    let after = lines[e.key_line][lines[e.key_line].find(':')? + 1..].trim();
    if !(after.is_empty() || after.starts_with('#')) {
        return None;
    }
    let body = lines[e.key_line + 1..e.end]
        .iter()
        .find(|l| !is_blank(l) && !is_comment(l));
    match body {
        None => Some(parent + 2),
        Some(l) if l[indent_of(l)..].starts_with('-') => None,
        Some(l) => Some(indent_of(l)),
    }
}

fn reindent(lines: &[String], by: usize) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            if is_blank(l) {
                String::new()
            } else {
                format!("{}{l}", " ".repeat(by))
            }
        })
        .collect()
}

/// Inserts `block` (lines at column 0) as the last entry of the mapping at `path`,
/// creating missing parents at the end of their own parent.
fn insert(lines: &mut Vec<String>, path: &[&str], block: Vec<String>) -> Option<()> {
    let (mut from, mut to, mut indent) = (0, lines.len(), 0);
    for part in path {
        let found = entries(lines, indent, from, to).into_iter().find(|(k, _)| k == part);
        match found {
            Some((_, e)) => {
                let child = child_indent(lines, &e, indent)?;
                let line = &lines[e.key_line];
                if line[line.find(':')? + 1..].trim().starts_with('#') && e.end == e.key_line + 1 {
                    return None; // a commented null: keep it simple, fall back
                }
                (from, to, indent) = (e.key_line + 1, e.end, child);
            }
            None => {
                let at = last_content(lines, from, to);
                lines.insert(at, format!("{}{part}:", " ".repeat(indent)));
                (from, to, indent) = (at + 1, at + 1, indent + 2);
            }
        }
    }
    let at = last_content(lines, from, to);
    let block = reindent(&block, indent);
    lines.splice(at..at, block);
    Some(())
}

/// Index just after the last non-blank line in `from..to` (or `from`).
fn last_content(lines: &[String], from: usize, to: usize) -> usize {
    (from..to).rev().find(|&i| !is_blank(&lines[i])).map_or(from, |i| i + 1)
}

/// Removes a root entry and returns its lines (head comments included).
fn take_root(lines: &mut Vec<String>, key: &str) -> Option<Vec<String>> {
    let (_, e) = entries(lines, 0, 0, lines.len()).into_iter().find(|(k, _)| k == key)?;
    Some(lines.drain(e.start..e.end).collect())
}

/// Renames the key on the entry's key line, keeping the value and inline comment.
fn rename(mut block: Vec<String>, old: &str, new: &str) -> Vec<String> {
    if let Some(line) = block.iter_mut().find(|l| key_at(l, 0) == Some(old)) {
        *line = format!("{new}{}", &line[old.len()..]);
    }
    block
}

/// One legacy root key and its v8 path, in the order `ConfigDocument` applies them.
pub(super) struct Move<'a> {
    pub old: &'a str,
    pub new: &'a str,
}

fn unsupported(text: &str) -> bool {
    text.contains('\t')
        || text.lines().any(|l| {
            let t = l.trim_start();
            t.starts_with("---") && l.len() == t.len() && !l.trim_end().eq("---")
                || t.starts_with("...")
                || t.starts_with("<<")
                || t.contains(" &")
                || t.contains(": *")
                || t.contains("- *")
                || t.starts_with('&')
                || t.starts_with('{')
                || t.starts_with('[')
        })
        || text.lines().filter(|l| l.trim_end() == "---").count() > 1
}

/// Text-level migration of legacy root entries. `families` lists legacy key-family
/// roots that the caller regroups; they are removed here and re-emitted from
/// `regrouped` (the migrated `api-keys` value).
pub(super) fn migrate(original: &str, moves: &[Move<'_>], families: &[&str], migrated: &Value) -> Option<String> {
    if unsupported(original) {
        return None;
    }
    let trailing_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_owned).collect();
    let root_keys =
        |lines: &[String]| -> Vec<String> { entries(lines, 0, 0, lines.len()).into_iter().map(|(k, _)| k).collect() };
    // Every root line must belong to a recognised entry, a comment or a blank line.
    let covered = {
        let all = entries(&lines, 0, 0, lines.len());
        lines.iter().enumerate().all(|(i, l)| {
            is_blank(l)
                || is_comment(l)
                || l.trim_end() == "---"
                || all.iter().any(|(_, e)| i >= e.key_line && i < e.end)
        })
    };
    if !covered {
        return None;
    }
    for m in moves {
        if !root_keys(&lines).iter().any(|k| k == m.old) {
            continue;
        }
        let present = |lines: &[String], path: &[&str]| -> bool {
            let (mut from, mut to, mut indent) = (0, lines.len(), 0);
            for part in path {
                let Some((_, e)) = entries(lines, indent, from, to).into_iter().find(|(k, _)| k == part) else {
                    return false;
                };
                match child_indent(lines, &e, indent) {
                    Some(child) => (from, to, indent) = (e.key_line + 1, e.end, child),
                    None => return part == path.last().unwrap(),
                }
            }
            true
        };
        let target: Vec<&str> = m.new.split('.').collect();
        let block = take_root(&mut lines, m.old)?;
        if present(&lines, &target) {
            continue; // v8 wins by presence; the legacy entry is dropped
        }
        let (parents, leaf) = target.split_at(target.len() - 1);
        let mut block = rename(block, m.old, leaf[0]);
        // A null legacy struct migrates to an empty mapping.
        let null_struct = block_lines_after_key(&block).is_empty() && super::schema::is_struct(m.new);
        if let Some(line) = block.iter_mut().find(|l| key_at(l, 0) == Some(leaf[0]))
            && line.trim_end().ends_with(':')
            && null_struct
        {
            line.push_str(" {}");
        }
        if parents.is_empty() {
            let at = lines.len();
            lines.splice(at..at, block);
        } else {
            insert(&mut lines, parents, block)?;
        }
    }
    for family in families {
        if take_root(&mut lines, family).is_none() {
            continue;
        }
    }
    let groups = migrated.get("api-keys").and_then(Value::as_mapping);
    let have_api_keys = root_keys(&lines).iter().any(|k| k == "api-keys");
    for family in families_v8(families) {
        let Some(value) = groups.and_then(|g| g.get(family)) else {
            continue;
        };
        let block = emit_entry(family, value);
        if have_api_keys {
            insert(&mut lines, &["api-keys"], block)?;
        } else {
            let mut section = vec!["api-keys:".to_owned()];
            section.extend(reindent(&block, 2));
            let at = lines.len();
            lines.splice(at..at, section);
        }
    }
    if !root_keys(&lines).iter().any(|k| k == "config-version") {
        lines.push("config-version: 8".into());
    }
    let mut text = lines.join("\n");
    if trailing_newline || !text.is_empty() {
        text.push('\n');
    }
    Some(text)
}

fn block_lines_after_key(block: &[String]) -> Vec<String> {
    block
        .iter()
        .skip_while(|l| is_comment(l))
        .skip(1)
        .filter(|l| !is_blank(l))
        .cloned()
        .collect()
}

fn families_v8(legacy: &[&str]) -> Vec<&'static str> {
    const MAP: &[(&str, &str)] = &[
        ("gemini-api-key", "gemini"),
        ("interactions-api-key", "interactions"),
        ("vertex-api-key", "vertex"),
        ("codex-api-key", "codex"),
        ("claude-api-key", "claude"),
        ("xai-api-key", "xai"),
        ("meta-api-key", "meta"),
        ("openai-compatibility", "openai-compatibility"),
    ];
    MAP.iter()
        .filter(|(old, _)| legacy.contains(old))
        .map(|(_, new)| *new)
        .collect()
}

/// A YAML scalar or key in the form serde_yaml_ng would write it.
fn scalar(value: &Value) -> String {
    serde_yaml_ng::to_string(value)
        .unwrap_or_default()
        .trim_end_matches('\n')
        .to_owned()
}

/// Block YAML for `key: value` at column 0, with two-space nesting and indented
/// sequences. Empty collections stay in flow form (`{}`/`[]`) as Go writes them.
pub(super) fn emit_entry(key: &str, value: &Value) -> Vec<String> {
    let key = scalar(&Value::from(key));
    match value {
        Value::Mapping(m) if !m.is_empty() => {
            let mut out = vec![format!("{key}:")];
            out.extend(reindent(&emit_mapping(m), 2));
            out
        }
        Value::Sequence(s) if !s.is_empty() => {
            let mut out = vec![format!("{key}:")];
            out.extend(reindent(&emit_sequence(s), 2));
            out
        }
        other => {
            let text = scalar(other);
            let mut lines = text.lines();
            let first = lines.next().unwrap_or_default();
            let mut out = vec![format!("{key}: {first}")];
            out.extend(lines.map(str::to_owned));
            out
        }
    }
}

fn emit_mapping(m: &serde_yaml_ng::Mapping) -> Vec<String> {
    m.iter()
        .flat_map(|(k, v)| emit_entry(&k.as_str().map(str::to_owned).unwrap_or_else(|| scalar(k)), v))
        .collect()
}

fn emit_sequence(s: &[Value]) -> Vec<String> {
    let mut out = Vec::new();
    for item in s {
        let body = match item {
            Value::Mapping(m) if !m.is_empty() => emit_mapping(m),
            Value::Sequence(inner) if !inner.is_empty() => emit_sequence(inner),
            other => scalar(other).lines().map(str::to_owned).collect(),
        };
        for (i, line) in body.into_iter().enumerate() {
            out.push(if i == 0 {
                format!("- {line}")
            } else {
                format!("  {line}")
            });
        }
    }
    out
}

/// Block text for a non-empty mapping or sequence value (no key), for yaml-edit.
pub(super) fn emit_value(value: &Value) -> String {
    let lines = match value {
        Value::Mapping(m) => emit_mapping(m),
        Value::Sequence(s) => emit_sequence(s),
        other => vec![scalar(other)],
    };
    lines.into_iter().map(|l| l + "\n").collect()
}

/// Adds `key: value` as a new root entry in block style, after the last root entry
/// and before a trailing comment block (Go's root foot comment, which holds archived
/// sections, always stays last).
pub(super) fn append_root(text: &str, key: &str, value: &Value) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let foot = lines
        .iter()
        .rposition(|l| !l.trim().is_empty() && !(l.starts_with('#') && indent_of(l) == 0))
        .map_or(0, |i| i + 1);
    let mut out = String::new();
    for line in &lines[..foot] {
        out.push_str(line);
        out.push('\n');
    }
    for line in emit_entry(key, value) {
        out.push_str(&line);
        out.push('\n');
    }
    for line in &lines[foot..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Go archives unrecognised sections as root foot comments: `# path: value`.
pub fn archive_comments(archived: &[(String, Value)]) -> String {
    let mut out = String::new();
    for (path, value) in archived {
        for line in emit_entry(path, value) {
            out.push_str("# ");
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emitted_blocks_parse_back_to_the_value() {
        let value: Value = serde_yaml_ng::from_str(
            "a: {b: [1, 'two', {c: '', d: [], e: {}}], f: \"multi\\nline\"}\ng: [[x, y], []]\nh: 'true'\n",
        )
        .unwrap();
        let text: String = emit_mapping(value.as_mapping().unwrap())
            .into_iter()
            .map(|l| l + "\n")
            .collect();
        assert_eq!(serde_yaml_ng::from_str::<Value>(&text).unwrap(), value, "{text}");
    }
}
