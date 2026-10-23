//! Tagged scalars as yaml.v3 reads them, where serde_yaml_ng reads them differently.
//! yaml.v3 base64-decodes a scalar whose tag resolves to `!!binary` into any string
//! field (decode.go `scalar`); serde_yaml_ng keeps the base64 text. yaml.v3 reads a
//! scalar with an unknown local tag (`!keep x`) as its text, resolves a plain scalar
//! with the non-specific `!` tag implicitly, and treats the literal short tags
//! `!!binary`, `!!null` and `!!str` that its `shortTag` keeps (`!<!!null>`,
//! `!%21binary`) as the standard ones; serde_yaml_ng wraps all of those as tagged
//! values. The tags come from libyaml's own events, so handles, `%TAG` directives and
//! `%XX` escapes resolve as they do for yaml.v3.
//!
//! What this does not follow is refused rather than read differently: other literal
//! short tags, a non-specific tag that resolves to a number or boolean, tagged or
//! aliased mapping keys and collection keys, a repeated anchor name (serde_yaml_ng binds a redefined
//! anchor's aliases to a different node than libyaml does), and a `%00` escape in a
//! tag (libyaml cuts the tag there; Go keeps the NUL).

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::mem::MaybeUninit;

use anyhow::{Context, bail, ensure};
use serde_yaml_ng::Value;

const UNSUPPORTED_TAG: &str = "a YAML tag in the config file is not supported";
const UNSUPPORTED_KEY: &str = "a tagged or aliased mapping key in the config file is not supported";
const REPEATED_ANCHOR: &str = "a YAML anchor name is defined more than once in the config file";
const PLUGIN_PIN: &str =
    "a plugin's store.version or store.release-tag in the config file is a !!binary value, which is not supported";
/// More rewritten scalars than a config has; alias chains past it are refused.
const MAX_REWRITES: usize = 1 << 20;

/// Go's `base64.StdEncoding` as yaml.v3 applies it: padding required, unused bits
/// ignored, CR and LF skipped. `None` for invalid data.
pub(super) fn go_base64(text: &[u8]) -> Option<Vec<u8>> {
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    use base64::{Engine as _, alphabet};
    let engine = GeneralPurpose::new(
        &alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_allow_trailing_bits(true)
            .with_decode_padding_mode(DecodePaddingMode::RequireCanonical),
    );
    let payload: Vec<u8> = text.iter().copied().filter(|b| !matches!(b, b'\r' | b'\n')).collect();
    engine.decode(payload).ok()
}

/// Rewrites the tagged scalars in `value`, which serde_yaml_ng parsed from `text` with
/// merges not yet expanded, to what yaml.v3 reads, aliases of them included. Fails on
/// invalid base64 as Go does, and refuses what it does not follow (see the module
/// docs) and binary bytes that are not UTF-8, which a setting cannot hold.
pub(super) fn decode(text: &str, value: &mut Value) -> anyhow::Result<()> {
    // Without a tag or an anchor, serde_yaml_ng reads the file as yaml.v3 does here.
    if !text.contains(['!', '&']) {
        return Ok(());
    }
    let mut walk = Walk {
        events: events(text)?.into_iter(),
        defined: HashSet::new(),
        anchors: HashMap::new(),
        found: Vec::new(),
    };
    if let Some(root) = walk.events.next() {
        walk.node(root, &mut Vec::new(), false)?;
    }
    // Go reads a plugin's pin from the raw node (pluginhost `yamlScalarString`), so a
    // `!!binary` pin, aliased or merged in too, keeps its base64 text there and fails
    // the version grammar; refused rather than read decoded.
    ensure!(
        !walk.found.iter().any(|(p, _, binary)| *binary && plugin_pin(value, p)),
        PLUGIN_PIN
    );
    // Paths are entry or item positions; patch them in one ordered pass.
    let mut found = walk.found;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    apply(value, 0, &found)
}

fn apply(node: &mut Value, depth: usize, patches: &[Found]) -> anyhow::Result<()> {
    let Some((path, replacement, _)) = patches.first() else {
        return Ok(());
    };
    if path.len() == depth {
        *node = replacement.clone();
        return Ok(());
    }
    let mut node = node;
    while let Value::Tagged(tagged) = node {
        node = &mut tagged.value;
    }
    let children: Box<dyn Iterator<Item = &mut Value>> = match node {
        Value::Mapping(m) => Box::new(m.values_mut()),
        Value::Sequence(items) => Box::new(items.iter_mut()),
        _ => bail!("config YAML changed while it was read"),
    };
    let mut rest = patches;
    for (i, child) in children.enumerate() {
        if rest.is_empty() {
            break;
        }
        let n = rest.iter().take_while(|(p, _, _)| p[depth] == i).count();
        apply(child, depth + 1, &rest[..n])?;
        rest = &rest[n..];
    }
    ensure!(rest.is_empty(), "config YAML changed while it was read");
    Ok(())
}

/// Whether `path` (entry or item positions in `root`) is a plugin's `store.version` or
/// `store.release-tag`, merge keys skipped.
fn plugin_pin(root: &Value, path: &[usize]) -> bool {
    let mut keys = Vec::new();
    let mut node = root;
    for &i in path {
        while let Value::Tagged(tagged) = node {
            node = &tagged.value;
        }
        match node {
            Value::Mapping(m) => {
                let Some((key, value)) = m.iter().nth(i) else {
                    return false;
                };
                keys.push(key.as_str());
                node = value;
            }
            Value::Sequence(items) => {
                let Some(item) = items.get(i) else { return false };
                keys.push(None);
                node = item;
            }
            _ => return false,
        }
    }
    let mut named = Vec::new();
    let mut keys = keys.into_iter().peekable();
    while let Some(key) = keys.next() {
        if key == Some("<<") {
            // `<<: [*a, *b]`: the list item is part of the merge too.
            keys.next_if(Option::is_none);
        } else {
            named.push(key);
        }
    }
    matches!(
        named.as_slice(),
        [
            Some("plugins"),
            Some("configs"),
            Some(_),
            Some("store"),
            Some("version" | "release-tag")
        ]
    )
}

/// The document's node events, in order, without stream and document markers.
enum Event {
    Scalar {
        anchor: Option<Vec<u8>>,
        tag: Option<Vec<u8>>,
        plain: bool,
        value: Vec<u8>,
    },
    Alias(Vec<u8>),
    Start {
        anchor: Option<Vec<u8>>,
        mapping: bool,
    },
    End,
}

/// Fails on a tagged node or a document start whose source contains `%00`: libyaml
/// cuts a tag at the NUL, so its tag would not be the one Go reads.
fn events(text: &str) -> anyhow::Result<Vec<Event>> {
    let nul_escape = text.contains("%00");
    struct Parser(Box<MaybeUninit<unsafe_libyaml::yaml_parser_t>>);
    impl Drop for Parser {
        fn drop(&mut self) {
            // SAFETY: only built after a successful yaml_parser_initialize.
            unsafe { unsafe_libyaml::yaml_parser_delete(self.0.as_mut_ptr()) }
        }
    }
    fn bytes(ptr: *const u8) -> Option<Vec<u8>> {
        // SAFETY: libyaml's anchors and tags are NUL-terminated or null.
        (!ptr.is_null()).then(|| unsafe { CStr::from_ptr(ptr.cast()) }.to_bytes().to_vec())
    }
    let mut out = Vec::new();
    // SAFETY: the parser is boxed, so it does not move while libyaml holds pointers
    // into it; `text` outlives it; each event is deleted once its data is copied.
    unsafe {
        let mut raw = Box::new(MaybeUninit::<unsafe_libyaml::yaml_parser_t>::uninit());
        ensure!(
            !unsafe_libyaml::yaml_parser_initialize(raw.as_mut_ptr()).fail,
            "failed to start the YAML parser"
        );
        let mut parser = Parser(raw);
        let p = parser.0.as_mut_ptr();
        unsafe_libyaml::yaml_parser_set_encoding(p, unsafe_libyaml::YAML_UTF8_ENCODING);
        unsafe_libyaml::yaml_parser_set_input_string(p, text.as_ptr(), text.len() as _);
        loop {
            let mut event = MaybeUninit::<unsafe_libyaml::yaml_event_t>::uninit();
            // serde_yaml_ng parsed the same text, so this does not fail.
            ensure!(
                !unsafe_libyaml::yaml_parser_parse(p, event.as_mut_ptr()).fail,
                "failed to parse config file"
            );
            let event = event.assume_init_mut();
            let kind = event.type_;
            if nul_escape {
                let tagged = match kind {
                    unsafe_libyaml::YAML_SCALAR_EVENT => !event.data.scalar.tag.is_null(),
                    unsafe_libyaml::YAML_SEQUENCE_START_EVENT => !event.data.sequence_start.tag.is_null(),
                    unsafe_libyaml::YAML_MAPPING_START_EVENT => !event.data.mapping_start.tag.is_null(),
                    // Its span holds the document's directives.
                    unsafe_libyaml::YAML_DOCUMENT_START_EVENT => true,
                    _ => false,
                };
                // libyaml skips a leading BOM without counting it in its marks.
                let bom = if text.starts_with('\u{feff}') { 3 } else { 0 };
                let (start, end) = (
                    event.start_mark.index as usize + bom,
                    event.end_mark.index as usize + bom,
                );
                let span = text.get(start..end);
                if tagged && span.is_none_or(|s| s.contains("%00")) {
                    unsafe_libyaml::yaml_event_delete(event);
                    bail!(UNSUPPORTED_TAG);
                }
            }
            let next = match kind {
                unsafe_libyaml::YAML_SCALAR_EVENT => {
                    let s = &event.data.scalar;
                    Some(Event::Scalar {
                        anchor: bytes(s.anchor),
                        tag: bytes(s.tag),
                        plain: s.style == unsafe_libyaml::YAML_PLAIN_SCALAR_STYLE,
                        value: std::slice::from_raw_parts(s.value, s.length as usize).to_vec(),
                    })
                }
                unsafe_libyaml::YAML_ALIAS_EVENT => bytes(event.data.alias.anchor).map(Event::Alias),
                unsafe_libyaml::YAML_SEQUENCE_START_EVENT => Some(Event::Start {
                    anchor: bytes(event.data.sequence_start.anchor),
                    mapping: false,
                }),
                unsafe_libyaml::YAML_MAPPING_START_EVENT => Some(Event::Start {
                    anchor: bytes(event.data.mapping_start.anchor),
                    mapping: true,
                }),
                unsafe_libyaml::YAML_SEQUENCE_END_EVENT | unsafe_libyaml::YAML_MAPPING_END_EVENT => Some(Event::End),
                _ => None,
            };
            unsafe_libyaml::yaml_event_delete(event);
            out.extend(next);
            if kind == unsafe_libyaml::YAML_STREAM_END_EVENT {
                return Ok(out);
            }
        }
    }
}

/// yaml.v3's null spellings.
fn is_null(value: &[u8]) -> bool {
    matches!(value, b"" | b"~" | b"null" | b"Null" | b"NULL")
}

/// A plain scalar that yaml.v3 resolves to a string, judged without resolving it: text
/// that starts with a letter and is not a boolean word. Numbers, timestamps, `.inf`,
/// `<<` and the like start with something else.
fn resolves_to_text(value: &[u8]) -> bool {
    const WORDS: &[&[u8]] = &[b"true", b"false", b"yes", b"no", b"on", b"off", b"y", b"n"];
    value.first().is_some_and(u8::is_ascii_alphabetic) && !WORDS.iter().any(|w| value.eq_ignore_ascii_case(w))
}

/// What yaml.v3 reads for a tagged scalar, where serde_yaml_ng reads something else;
/// `None` where they agree.
fn rewrite(tag: &[u8], plain: bool, value: Vec<u8>) -> anyhow::Result<Option<Value>> {
    let text = || {
        String::from_utf8(value.clone())
            .ok()
            .context("config YAML is not UTF-8")
    };
    Ok(Some(match tag {
        b"tag:yaml.org,2002:binary" | b"!!binary" => {
            let bytes = go_base64(&value).context("yaml: !!binary value contains invalid base64 data")?;
            let text = String::from_utf8(bytes)
                .ok()
                .context("a !!binary value in the config file is not UTF-8 text")?;
            Value::String(text)
        }
        // Non-specific: a plain scalar resolves as if untagged, a quoted one is text.
        b"!" if plain && is_null(&value) => Value::Null,
        b"!" if plain && !resolves_to_text(&value) => bail!(UNSUPPORTED_TAG),
        b"!" | b"!!str" => Value::String(text()?),
        b"!!null" if is_null(&value) => Value::Null,
        t if t.starts_with(b"!!") => bail!(UNSUPPORTED_TAG),
        // Any other local tag is unknown to yaml.v3: the scalar is its text.
        t if t.starts_with(b"!") => Value::String(text()?),
        _ => return Ok(None),
    }))
}

/// A rewritten scalar: its path (entry or item positions), the value yaml.v3 reads, and
/// whether a `!!binary` tag made it.
type Found = (Vec<usize>, Value, bool);

struct Walk {
    events: std::vec::IntoIter<Event>,
    defined: HashSet<Vec<u8>>,
    /// Each anchor's rewritten scalars, by their path below it.
    anchors: HashMap<Vec<u8>, Vec<Found>>,
    /// Rewritten scalars, by their path from the root.
    found: Vec<Found>,
}

impl Walk {
    fn next(&mut self) -> anyhow::Result<Event> {
        self.events.next().context("config YAML ended early")
    }

    fn define(&mut self, anchor: &Option<Vec<u8>>) -> anyhow::Result<()> {
        if let Some(name) = anchor {
            ensure!(self.defined.insert(name.clone()), REPEATED_ANCHOR);
        }
        Ok(())
    }

    fn node(&mut self, event: Event, path: &mut Vec<usize>, key: bool) -> anyhow::Result<()> {
        let start = self.found.len();
        let anchor = match event {
            Event::Scalar {
                anchor,
                tag,
                plain,
                value,
            } => {
                self.define(&anchor)?;
                if let Some(tag) = tag {
                    // yaml.v3's layout code matches keys by their node, tag included
                    // (`!!str <<` is no merge key).
                    ensure!(!key, UNSUPPORTED_KEY);
                    let binary = matches!(tag.as_slice(), b"tag:yaml.org,2002:binary" | b"!!binary");
                    if let Some(v) = rewrite(&tag, plain, value)? {
                        self.found.push((path.clone(), v, binary));
                    }
                }
                anchor
            }
            Event::Alias(name) => {
                ensure!(!key, UNSUPPORTED_KEY);
                let copies = self.anchors.get(&name).cloned().unwrap_or_default();
                ensure!(
                    self.found.len() + copies.len() <= MAX_REWRITES,
                    "config YAML aliases expand too far"
                );
                for (below, v, binary) in copies {
                    self.found.push(([path.as_slice(), &below].concat(), v, binary));
                }
                None
            }
            Event::Start { anchor, mapping } => {
                // Paths address mapping values only; Go rejects collection keys.
                ensure!(!key, UNSUPPORTED_KEY);
                self.define(&anchor)?;
                let mut i = 0;
                loop {
                    let event = self.next()?;
                    if matches!(event, Event::End) {
                        break;
                    }
                    if mapping {
                        self.node(event, path, true)?;
                        let value = self.next()?;
                        path.push(i);
                        self.node(value, path, false)?;
                    } else {
                        path.push(i);
                        self.node(event, path, false)?;
                    }
                    path.pop();
                    i += 1;
                }
                anchor
            }
            Event::End => bail!("config YAML is malformed"),
        };
        if let Some(name) = anchor {
            let below = self.found[start..]
                .iter()
                .map(|(p, v, binary)| (p[path.len()..].to_vec(), v.clone(), *binary))
                .collect();
            self.anchors.insert(name, below);
        }
        Ok(())
    }
}
