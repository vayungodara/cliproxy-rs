//! The two Bubble Tea components the tabs use: bubbles v1 `viewport` (scrolling and
//! its key map) and `textinput` (editing keys, password echo, placeholder).
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::style::{self, Doc};

/// bubbles `viewport.Model`: lines, a vertical offset and its height.
#[derive(Default)]
pub struct Viewport {
    lines: Vec<Line<'static>>,
    pub offset: usize,
    pub height: usize,
}

impl Viewport {
    /// `SetContent`: an offset past the new last line moves to the bottom.
    pub fn set_content(&mut self, doc: Doc) {
        self.lines = doc.0;
        if self.offset > self.lines.len().saturating_sub(1) {
            self.goto_bottom();
        }
    }

    fn max_offset(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    pub fn goto_bottom(&mut self) {
        self.offset = self.max_offset();
    }

    pub fn at_bottom(&self) -> bool {
        self.offset >= self.max_offset()
    }

    /// `SetYOffset`, clamped.
    pub fn set_offset(&mut self, offset: usize) {
        self.offset = offset.min(self.max_offset());
    }

    fn down(&mut self, n: usize) {
        if self.at_bottom() || n == 0 || self.lines.is_empty() {
            return;
        }
        self.set_offset(self.offset + n);
    }

    fn up(&mut self, n: usize) {
        if self.offset == 0 || n == 0 {
            return;
        }
        self.set_offset(self.offset.saturating_sub(n));
    }

    /// The viewport `DefaultKeyMap`. ponytail: no horizontal scrolling (`h`/`l`); lines
    /// wider than the terminal are cut.
    pub fn key(&mut self, key: &str) {
        let page = self.height;
        match key {
            "pgdown" | " " | "f" => self.down(page),
            "pgup" | "b" => self.up(page),
            "d" | "ctrl+d" => self.down(page / 2),
            "u" | "ctrl+u" => self.up(page / 2),
            "down" | "j" => self.down(1),
            "up" | "k" => self.up(1),
            _ => {}
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        for (row, line) in self
            .lines
            .iter()
            .skip(self.offset)
            .take(area.height as usize)
            .enumerate()
        {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }
}

/// bubbles `textinput.Model`, the parts the TUI uses. ponytail: the cursor does not
/// blink and `ctrl+v` does not read the system clipboard (a terminal paste does work).
pub struct TextInput {
    value: Vec<char>,
    pos: usize,
    pub prompt: String,
    pub placeholder: String,
    pub password: bool,
    pub limit: usize,
    pub width: usize,
    focused: bool,
}

impl TextInput {
    pub fn new(limit: usize) -> Self {
        TextInput {
            value: Vec::new(),
            pos: 0,
            prompt: "> ".into(),
            placeholder: String::new(),
            password: false,
            limit,
            width: 0,
            focused: false,
        }
    }

    pub fn value(&self) -> String {
        self.value.iter().collect()
    }

    /// `SetValue`: cut to the character limit; the cursor moves to the end only from an
    /// empty field or when it would be past the end (Go keeps it otherwise).
    pub fn set_value(&mut self, value: &str) {
        let was_empty = self.value.is_empty();
        self.value = value.chars().take(self.limit_or_max()).collect();
        if (self.pos == 0 && was_empty) || self.pos > self.value.len() {
            self.pos = self.value.len();
        }
    }

    fn limit_or_max(&self) -> usize {
        if self.limit == 0 { usize::MAX } else { self.limit }
    }

    pub fn focus(&mut self) {
        self.focused = true;
    }

    pub fn blur(&mut self) {
        self.focused = false;
    }

    /// `insertRunesFromUserInput`: tabs and newlines become spaces; the limit holds.
    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", " ").replace(['\r', '\n', '\t'], " ");
        for c in text.chars() {
            if self.value.len() >= self.limit_or_max() {
                break;
            }
            self.value.insert(self.pos, c);
            self.pos += 1;
        }
    }

    /// Word motions on a masked field reach the start or end, as bubbles does, so the
    /// word boundaries of a password stay hidden.
    fn word_start(&self) -> usize {
        if self.password {
            return 0;
        }
        let mut i = self.pos;
        while i > 0 && self.value[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.value[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    fn word_end(&self) -> usize {
        if self.password {
            return self.value.len();
        }
        let mut i = self.pos;
        while i < self.value.len() && self.value[i].is_whitespace() {
            i += 1;
        }
        while i < self.value.len() && !self.value[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// The textinput `DefaultKeyMap`; any other key inserts its characters.
    pub fn key(&mut self, key: &str) {
        if !self.focused {
            return;
        }
        match key {
            "right" | "ctrl+f" => self.pos = (self.pos + 1).min(self.value.len()),
            "left" | "ctrl+b" => self.pos = self.pos.saturating_sub(1),
            "alt+right" | "ctrl+right" | "alt+f" => self.pos = self.word_end(),
            "alt+left" | "ctrl+left" | "alt+b" => self.pos = self.word_start(),
            "alt+backspace" | "ctrl+w" => {
                let start = self.word_start();
                self.value.drain(start..self.pos);
                self.pos = start;
            }
            "alt+delete" | "alt+d" => {
                let end = self.word_end();
                self.value.drain(self.pos..end);
            }
            "ctrl+k" => self.value.truncate(self.pos),
            "ctrl+u" => {
                self.value.drain(..self.pos);
                self.pos = 0;
            }
            "backspace" | "ctrl+h" => {
                if self.pos > 0 {
                    self.pos -= 1;
                    self.value.remove(self.pos);
                }
            }
            "delete" | "ctrl+d" => {
                if self.pos < self.value.len() {
                    self.value.remove(self.pos);
                }
            }
            "home" | "ctrl+a" => self.pos = 0,
            "end" | "ctrl+e" => self.pos = self.value.len(),
            k if k.chars().count() == 1 => self.insert(k),
            _ => {}
        }
    }

    /// `View`: prompt, text (masked for passwords) and a reverse-video cursor, scrolled
    /// to keep the cursor inside `width` columns.
    pub fn view(&self) -> Line<'static> {
        let mut spans = vec![Span::raw(self.prompt.clone())];
        let cursor = Style::new().add_modifier(Modifier::REVERSED);
        if self.value.is_empty() && !self.placeholder.is_empty() {
            let mut chars = self.placeholder.chars();
            let first = chars.next().map(String::from).unwrap_or_default();
            let mut rest: String = chars.collect();
            if self.width > 0 {
                // bubbles `placeholderView`: the rest fits the width, cut with "…".
                let room = self
                    .width
                    .saturating_sub(style::width(&self.prompt) + style::width(&first));
                rest = style::truncate_width(&rest, room, "…");
            }
            let faint = style::fg(style::MUTED);
            spans.push(Span::styled(first, if self.focused { cursor } else { faint }));
            spans.push(Span::styled(rest, faint));
            return Line::from(spans);
        }
        let shown: Vec<char> = if self.password {
            vec!['*'; self.value.len()]
        } else {
            self.value.clone()
        };
        let (mut start, mut end) = (0, shown.len());
        if self.width > 0 && shown.len() >= self.width {
            start = self.pos.saturating_sub(self.width.saturating_sub(1));
            end = (start + self.width).min(shown.len());
        }
        spans.push(Span::raw(shown[start..self.pos.min(end)].iter().collect::<String>()));
        if self.focused {
            let at = shown.get(self.pos).copied().unwrap_or(' ');
            spans.push(Span::styled(at.to_string(), cursor));
            if self.pos < end {
                spans.push(Span::raw(shown[self.pos + 1..end].iter().collect::<String>()));
            }
        } else {
            spans.push(Span::raw(shown[self.pos.min(end)..end].iter().collect::<String>()));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_scrolls_like_bubbles() {
        let mut vp = Viewport {
            height: 3,
            ..Default::default()
        };
        let mut doc = Doc::default();
        for i in 0..10 {
            doc.text(i.to_string(), Style::new());
        }
        vp.set_content(doc);
        vp.key(" ");
        assert_eq!(vp.offset, 3);
        vp.key("d");
        assert_eq!(vp.offset, 4);
        vp.key("pgdown");
        vp.key("pgdown");
        assert_eq!(vp.offset, 7);
        assert!(vp.at_bottom());
        vp.key("k");
        assert_eq!(vp.offset, 6);
        // New content shorter than the offset moves to the bottom.
        let mut short = Doc::default();
        short.text("a", Style::new());
        vp.set_content(short);
        assert_eq!(vp.offset, 0);
    }

    #[test]
    fn text_input_edits_like_bubbles() {
        let mut input = TextInput::new(5);
        input.focus();
        input.insert("ab\tcdef");
        assert_eq!(input.value(), "ab cd");
        input.key("left");
        input.key("backspace");
        assert_eq!(input.value(), "ab d");
        input.key("ctrl+a");
        input.key("x");
        assert_eq!(input.value(), "xab d");
        input.key("ctrl+k");
        assert_eq!(input.value(), "x");
        input.set_value("hello world");
        assert_eq!(input.value(), "hello");
        input.password = true;
        // The cursor stayed after the first character (Go SetValue), on the second star.
        let text: String = input.view().spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "> *****");
        // Masked: ctrl+w clears everything before the cursor.
        input.limit = 0;
        input.set_value("foo bar");
        input.key("end");
        input.key("ctrl+w");
        assert_eq!(input.value(), "");
        // Go's SetValue keeps a cursor that is still inside the new value.
        let mut edit = TextInput::new(0);
        edit.focus();
        edit.set_value("abcd");
        edit.key("home");
        edit.set_value("abcd");
        edit.key("x");
        assert_eq!(edit.value(), "xabcd");
    }
}
