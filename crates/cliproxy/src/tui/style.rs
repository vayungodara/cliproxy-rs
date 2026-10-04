//! Go's lipgloss palette and styles (internal/tui/styles.go) as ratatui styles, and
//! the text helpers the tabs share. Content is built as styled lines, as Go builds
//! styled strings.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};

/// Display columns, as lipgloss measures them.
pub fn width(s: &str) -> usize {
    Span::raw(s).width()
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const PRIMARY: Color = rgb(0x7C3AED);
pub const SUCCESS: Color = rgb(0x22C55E);
pub const WARNING: Color = rgb(0xEAB308);
pub const ERROR: Color = rgb(0xEF4444);
pub const INFO: Color = rgb(0x3B82F6);
pub const MUTED: Color = rgb(0x6B7280);
pub const SURFACE: Color = rgb(0x313244);
pub const TEXT: Color = rgb(0xCDD6F4);
pub const SUBTEXT: Color = rgb(0xA6ADC8);
pub const BORDER: Color = rgb(0x45475A);
pub const HIGHLIGHT: Color = rgb(0xF5C2E7);
pub const WHITE: Color = rgb(0xFFFFFF);

pub fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

pub fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

pub fn title() -> Style {
    bold(HIGHLIGHT)
}
pub fn help() -> Style {
    fg(MUTED)
}
pub fn subtitle() -> Style {
    fg(SUBTEXT).add_modifier(Modifier::ITALIC)
}
pub fn error() -> Style {
    bold(ERROR)
}
pub fn success() -> Style {
    fg(SUCCESS)
}
pub fn warning() -> Style {
    fg(WARNING)
}
pub fn value() -> Style {
    fg(TEXT)
}

/// Lines a tab renders; each `push*` call is one Go `sb.WriteString(...)` line.
#[derive(Default)]
pub struct Doc(pub Vec<Line<'static>>);

impl Doc {
    pub fn line(&mut self, spans: Vec<Span<'static>>) {
        self.0.push(Line::from(spans));
    }

    pub fn text(&mut self, text: impl Into<String>, style: Style) {
        self.0.push(Line::from(Span::styled(text.into(), style)));
    }

    pub fn blank(&mut self) {
        self.0.push(Line::default());
    }

    /// Go's `titleStyle.Render(x) + "\n"`: the title, then its one-line bottom margin.
    pub fn title(&mut self, text: impl Into<String>) {
        self.text(text, title());
        self.blank();
    }

    /// Go's `tableHeaderStyle`: bold highlight text over a bottom border as wide.
    pub fn table_header(&mut self, text: String) {
        let columns = width(&text);
        self.text(text, title());
        self.text("─".repeat(columns), fg(BORDER));
    }
}

pub fn span(text: impl Into<String>, style: Style) -> Span<'static> {
    Span::styled(text.into(), style)
}

pub fn plain(text: impl Into<String>) -> Span<'static> {
    Span::raw(text.into())
}

/// lipgloss `Width(n)` for one short line: padded with spaces to `n` columns.
pub fn pad(text: &str, columns: usize) -> String {
    let w = width(text);
    if w >= columns {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(columns - w))
    }
}

/// Go `fitStringWidth`: the longest prefix of whole characters within `max` columns.
pub fn fit_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let mut next = out.clone();
        next.push(c);
        if width(&next) > max {
            break;
        }
        out = next;
    }
    out
}

/// charmbracelet/x/ansi `Truncate`: text wider than `max` columns is cut to leave
/// room for `tail`, which is appended.
pub fn truncate_width(text: &str, max: usize, tail: &str) -> String {
    if width(text) <= max {
        return text.to_owned();
    }
    format!("{}{tail}", fit_width(text, max.saturating_sub(width(tail))))
}

/// lipgloss `Width(n)` wrapping (charmbracelet/x/ansi `Wrap`): words fill each line up
/// to `max` columns; a word wider than a line is broken.
pub fn wrap_words(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let mut word = word.to_owned();
        let needed = if line.is_empty() {
            width(&word)
        } else {
            width(&line) + 1 + width(&word)
        };
        if needed <= max {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(&word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A single character wider than the line still takes a line of its own.
        while width(&word) > max && word.chars().nth(1).is_some() {
            let mut head = fit_width(&word, max);
            if head.is_empty() {
                head = word.chars().next().map(String::from).unwrap_or_default();
            }
            word = word[head.len()..].to_owned();
            lines.push(head);
        }
        line = word;
    }
    lines.push(line);
    lines
}

/// Go `getString`: a string member, else empty.
pub fn get_string(m: &Map<String, Value>, key: &str) -> String {
    m.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Go `getFloat`: a number member, else 0.
pub fn get_float(m: &Map<String, Value>, key: &str) -> f64 {
    m.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Go `getBool`: a bool member, else false.
pub fn get_bool(m: &Map<String, Value>, key: &str) -> bool {
    m.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Go `fmt.Sprintf("%.0f", f)` (ties round to even, as Rust's formatting does).
pub fn int_text(f: f64) -> String {
    let text = format!("{f:.0}");
    if text == "-0" { "0".into() } else { text }
}

/// Go `fmt.Sprintf("%v", v)` of a value `encoding/json` decoded into `any`.
pub fn go_v(v: &Value) -> String {
    match v {
        Value::Null => "<nil>".into(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => go_g(n.as_f64().unwrap_or(0.0)),
        Value::Array(a) => format!("[{}]", a.iter().map(go_v).collect::<Vec<_>>().join(" ")),
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let inner: Vec<String> = entries.iter().map(|(k, v)| format!("{k}:{}", go_v(v))).collect();
            format!("map[{}]", inner.join(" "))
        }
    }
}

/// Go `%v` of a float64: shortest digits, exponent form when the exponent is below -4
/// or at least 6 (`strconv.FormatFloat(f, 'g', -1, 64)`).
fn go_g(f: f64) -> String {
    if f == 0.0 {
        return if f.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if (-4..6).contains(&exp) {
        return format!("{f}");
    }
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exp.abs())
}

/// Go `maskKey`.
pub fn mask_key(key: &str) -> String {
    let b = key.as_bytes();
    if b.len() <= 8 {
        return "*".repeat(b.len());
    }
    format!(
        "{}{}{}",
        String::from_utf8_lossy(&b[..4]),
        "*".repeat(b.len() - 8),
        String::from_utf8_lossy(&b[b.len() - 4..])
    )
}

/// Go's byte-length truncation with `...` (auth file names and emails), which may cut a
/// multi-byte character; the replacement character stands in for the cut bytes.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    format!("{}...", String::from_utf8_lossy(&s.as_bytes()[..max - 3]))
}

/// Go `wrapText`: byte-wise slices of at most `max` bytes.
pub fn wrap_text(s: &str, max: usize) -> Vec<String> {
    if max == 0 {
        return vec![s.to_owned()];
    }
    s.as_bytes()
        .chunks(max)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Expected values: Go 1.26 fmt and the internal/tui helpers.
    #[test]
    fn formats_like_go() {
        let cases = [
            (json!(1), "1"),
            (json!(123456), "123456"),
            (json!(1000000), "1e+06"),
            (json!(1234567), "1.234567e+06"),
            (json!(1e21), "1e+21"),
            (json!(1.5), "1.5"),
            (json!(0.0001), "0.0001"),
            (json!(0.00001), "1e-05"),
            (json!(-3), "-3"),
            (json!(true), "true"),
            (json!({"b": 1, "a": [1, "x"]}), "map[a:[1 x] b:1]"),
            (json!(["a", 2]), "[a 2]"),
            (Value::Null, "<nil>"),
        ];
        for (v, want) in cases {
            assert_eq!(go_v(&v), want, "{v}");
        }
        assert_eq!(int_text(2.5), "2");
        assert_eq!(int_text(3.5), "4");
        assert_eq!(mask_key("12345678"), "********");
        assert_eq!(mask_key("sk-abcdefghij"), "sk-a*****ghij");
        assert_eq!(wrap_text("abcdefg", 3), ["abc", "def", "g"]);
        assert_eq!(fit_width("日本語", 5), "日本");
        assert_eq!(truncate_bytes("abcdefghij", 8), "abcde...");
        assert_eq!(wrap_words("Auth Files (1 active)", 16), ["Auth Files (1", "active)"]);
        assert_eq!(wrap_words("abcdefgh ij", 3), ["abc", "def", "gh", "ij"]);
        // Two-column characters on a one-column line: one per line, no endless loop.
        assert_eq!(wrap_words("日本", 1), ["日", "本"]);
        assert_eq!(wrap_words("a 日本語", 3), ["a", "日", "本", "語"]);
        assert_eq!(truncate_width("abcdef", 4, "…"), "abc…");
        assert_eq!(truncate_width("abc", 4, "…"), "abc");
    }
}
