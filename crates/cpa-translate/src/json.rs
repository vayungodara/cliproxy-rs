//! Raw edits preserve untouched JSON, including key order and number spelling.
use gjson::{Kind, Value};

// ponytail: gjson accepts UTF-8 strings. Malformed non-UTF-8 parity needs a
// byte-oriented JSON editor; do not silently replace invalid bytes.
pub fn text(body: &[u8]) -> Result<&str, crate::Error> {
    std::str::from_utf8(body).map_err(|e| crate::Error(e.to_string()))
}

pub fn string(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}

pub fn array(items: &[String]) -> String {
    format!("[{}]", items.join(","))
}

pub fn set(out: &mut String, path: &str, value: &str) {
    let (key, rest) = path.split_once('.').unwrap_or((path, ""));
    let root = gjson::parse(out);
    let current = root.get(key);
    if current.exists() {
        let start = current.json().as_ptr() as usize - out.as_ptr() as usize;
        let end = start + current.json().len();
        let mut replacement = current.json().to_owned();
        if rest.is_empty() {
            replacement = value.to_owned();
        } else {
            set(&mut replacement, rest, value);
        }
        out.replace_range(start..end, &replacement);
        return;
    }
    let mut replacement = value.to_owned();
    if !rest.is_empty() {
        replacement = if rest.split('.').next().unwrap().parse::<usize>().is_ok() {
            "[]".into()
        } else {
            "{}".into()
        };
        set(&mut replacement, rest, value);
    }
    match root.kind() {
        Kind::Object => {
            let members = root.json().strip_prefix('{').unwrap_or("").trim_start();
            let empty = members.is_empty() || members.starts_with('}');
            let comma = if empty { "" } else { "," };
            let addition = format!("{comma}{}:{replacement}", string(key));
            // sjson scans backward for the last literal '}', even inside a string
            // or malformed trailing bytes. Preserve that behavior, not a JSON repair.
            let start = out.find('{').unwrap();
            let prefix = out.rfind('}').map(|i| &out[start..i]).unwrap_or("");
            *out = format!("{prefix}{addition}}}");
        }
        Kind::Array => {
            let count = root.array().len();
            let index = if key == "-1" {
                count
            } else {
                key.parse().unwrap_or(count)
            };
            let mut addition = String::new();
            for i in count..=index {
                if i > 0 {
                    addition.push(',');
                }
                addition.push_str(if i == index { &replacement } else { "null" });
            }
            let pos = out.rfind(']').unwrap();
            out.insert_str(pos, &addition);
        }
        _ => {
            *out = if key.parse::<usize>().is_ok() {
                "[]".into()
            } else {
                "{}".into()
            };
            set(out, path, value);
        }
    }
}

pub fn set_string(out: &mut String, path: &str, value: &str) {
    set(out, path, &string(value));
}

pub fn cache_control(out: &mut String, source: &Value<'_>) {
    let cc = source.get("cache_control");
    if cc.kind() == Kind::Object && cc.get("type").str() == "ephemeral" {
        set(out, "cache_control", cc.json());
    }
}

pub fn data_lines(event: &str) -> impl Iterator<Item = &str> {
    event
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
}

pub fn frame(payload: &str) -> bytes::Bytes {
    bytes::Bytes::from(format!("data: {payload}\n\n"))
}
