//! Go's API keys tab (internal/tui/keys_tab.go): the client access keys (add, edit,
//! delete, copy) and the provider key lists, read only.
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::client::{Client, Object};
use super::i18n::{t, tf};
use super::style::{self as s, Doc, span};
use super::widgets::{TextInput, Viewport};
use super::{Cmd, Msg, cmd};

/// Go's provider sections, in its order (no Meta list, as in Go).
pub(super) const PROVIDERS: [(&str, &str); 6] = [
    ("Gemini API Keys", "gemini-api-key"),
    ("Interactions API Keys", "interactions-api-key"),
    ("Claude API Keys", "claude-api-key"),
    ("Codex API Keys", "codex-api-key"),
    ("xAI API Keys", "xai-api-key"),
    ("Vertex API Keys", "vertex-api-key"),
];

/// Go's `keysDataMsg`.
pub struct Data {
    pub(super) api_keys: Vec<String>,
    /// One list per [`PROVIDERS`] entry, in order.
    pub(super) providers: Vec<Vec<Object>>,
    pub(super) openai: Vec<Object>,
}

pub struct KeysTab {
    client: Arc<Client>,
    pub vp: Viewport,
    pub keys: Vec<String>,
    providers: Vec<Vec<Object>>,
    openai: Vec<Object>,
    err: Option<String>,
    width: usize,
    pub cursor: usize,
    pub confirm: Option<usize>,
    status: Option<(String, Style)>,
    pub editing: bool,
    pub adding: bool,
    edit_index: usize,
    /// The key being edited or awaiting delete confirmation. The API addresses keys by
    /// index, so a refresh that moves it cancels the action.
    target: String,
    pub input: TextInput,
}

impl KeysTab {
    pub fn new(client: Arc<Client>) -> Self {
        let mut input = TextInput::new(512);
        input.prompt = "  Key: ".into();
        KeysTab {
            client,
            vp: Viewport::default(),
            keys: Vec::new(),
            providers: Vec::new(),
            openai: Vec::new(),
            err: None,
            width: 0,
            cursor: 0,
            confirm: None,
            status: None,
            editing: false,
            adding: false,
            edit_index: 0,
            target: String::new(),
            input,
        }
    }

    /// Go `fetchKeys`: the access keys decide success; provider lists that fail are empty.
    pub fn fetch(&self) -> Cmd {
        let client = self.client.clone();
        cmd(async move {
            let api_keys = match client.get_api_keys().await {
                Ok(keys) => keys,
                Err(e) => return Msg::KeysData(Err(e)),
            };
            let mut providers = Vec::new();
            for (_, route) in PROVIDERS {
                providers.push(client.get_key_list(route).await.unwrap_or_default());
            }
            let openai = client.get_key_list("openai-compatibility").await.unwrap_or_default();
            Msg::KeysData(Ok(Data {
                api_keys,
                providers,
                openai,
            }))
        })
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.vp.height = height;
        self.input.width = width.saturating_sub(16);
    }

    fn refresh(&mut self) {
        let doc = self.render();
        self.vp.set_content(doc);
    }

    pub fn locale(&mut self) {
        self.refresh();
    }

    pub fn data(&mut self, result: Result<Data, String>) {
        match result {
            Ok(data) => {
                self.err = None;
                self.keys = data.api_keys;
                self.providers = data.providers;
                self.openai = data.openai;
                if self.cursor >= self.keys.len() {
                    self.cursor = self.keys.len().saturating_sub(1);
                }
                let index = self.confirm.or(self.editing.then_some(self.edit_index));
                if index.is_some_and(|i| self.keys.get(i) != Some(&self.target)) {
                    self.confirm = None;
                    self.editing = false;
                    self.input.blur();
                    self.status = Some((t("key_changed").to_owned(), s::error()));
                }
            }
            Err(e) => self.err = Some(e),
        }
        self.refresh();
    }

    /// Go's `keyActionMsg`: the result line, then a fresh read.
    pub fn action(&mut self, result: Result<String, String>) -> Vec<Cmd> {
        self.status = Some(match result {
            Ok(action) => (format!("✓ {action}"), s::success()),
            Err(e) => (format!("✗ {e}"), s::error()),
        });
        self.confirm = None;
        self.refresh();
        vec![self.fetch()]
    }

    fn act(&self, work: impl std::future::Future<Output = Result<String, String>> + Send + 'static) -> Vec<Cmd> {
        vec![cmd(async move { Msg::KeyAction(work.await) })]
    }

    pub fn key(&mut self, key: &str) -> Vec<Cmd> {
        if self.editing || self.adding {
            match key {
                "enter" => {
                    let value = self.input.value().trim().to_owned();
                    let (adding, index) = (self.adding, self.edit_index);
                    self.editing = false;
                    self.adding = false;
                    self.input.blur();
                    if value.is_empty() {
                        self.refresh();
                        return Vec::new();
                    }
                    let client = self.client.clone();
                    return if adding {
                        self.act(async move {
                            client.add_api_key(&value).await?;
                            Ok(t("key_added").to_owned())
                        })
                    } else {
                        self.act(async move {
                            client.edit_api_key(index, &value).await?;
                            Ok(t("key_updated").to_owned())
                        })
                    };
                }
                "esc" => {
                    self.editing = false;
                    self.adding = false;
                    self.input.blur();
                }
                _ => self.input.key(key),
            }
            self.refresh();
            return Vec::new();
        }
        if let Some(index) = self.confirm {
            match key {
                "y" | "Y" => {
                    self.confirm = None;
                    let client = self.client.clone();
                    return self.act(async move {
                        client.delete_api_key(index).await?;
                        Ok(t("key_deleted").to_owned())
                    });
                }
                "n" | "N" | "esc" => {
                    self.confirm = None;
                    self.refresh();
                }
                _ => {}
            }
            return Vec::new();
        }
        let count = self.keys.len();
        match key {
            "j" | "down" => {
                if count > 0 {
                    self.cursor = (self.cursor + 1) % count;
                    self.refresh();
                }
            }
            "k" | "up" => {
                if count > 0 {
                    self.cursor = (self.cursor + count - 1) % count;
                    self.refresh();
                }
            }
            "a" => {
                self.adding = true;
                self.editing = false;
                self.input.set_value("");
                self.input.prompt = t("new_key_prompt").to_owned();
                self.input.focus();
                self.refresh();
            }
            "e" => {
                if self.cursor < count {
                    self.editing = true;
                    self.adding = false;
                    self.edit_index = self.cursor;
                    self.target = self.keys[self.cursor].clone();
                    self.input.set_value(&self.target);
                    self.input.prompt = t("edit_key_prompt").to_owned();
                    self.input.focus();
                    self.refresh();
                }
            }
            "d" => {
                if self.cursor < count {
                    self.confirm = Some(self.cursor);
                    self.target = self.keys[self.cursor].clone();
                    self.refresh();
                }
            }
            "c" => {
                if let Some(key) = self.keys.get(self.cursor) {
                    self.status = Some(match super::sys::write_clipboard(key) {
                        Ok(()) => (t("copied").to_owned(), s::success()),
                        Err(e) => (format!("{}: {e}", t("copy_failed")), s::error()),
                    });
                    self.refresh();
                }
            }
            "r" => {
                self.status = None;
                return vec![self.fetch()];
            }
            _ => self.vp.key(key),
        }
        Vec::new()
    }

    pub fn paste(&mut self, text: &str) {
        if self.editing || self.adding {
            self.input.insert(text);
            self.refresh();
        }
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        self.vp.render(area, buf);
    }

    /// Go `renderContent`.
    fn render(&self) -> Doc {
        let mut doc = Doc::default();
        doc.title(t("keys_title"));
        doc.text(t("keys_help"), s::help());
        doc.text("─".repeat(self.width), Style::new());
        if let Some(err) = &self.err {
            doc.text(format!("{}{err}", t("error_prefix")), s::error());
            return doc;
        }
        doc.table_header(format!("  {} ({})", t("access_keys"), self.keys.len()));
        if self.keys.is_empty() {
            doc.text(t("no_keys"), s::subtitle());
        }
        for (i, key) in self.keys.iter().enumerate() {
            let style = if i == self.cursor {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let cursor = if i == self.cursor { "▸ " } else { "  " };
            doc.line(vec![span(format!("{cursor}{}. {}", i + 1, s::mask_key(key)), style)]);
            if self.confirm == Some(i) {
                doc.text(
                    format!("    {}", tf("confirm_delete_key", s::mask_key(key))),
                    s::warning(),
                );
            }
            if self.editing && self.edit_index == i {
                doc.0.push(self.input.view());
                doc.text(t("enter_save_esc"), s::help());
            }
        }
        if self.adding {
            doc.blank();
            doc.0.push(self.input.view());
            doc.text(t("enter_add"), s::help());
        }
        doc.blank();
        for ((title, _), keys) in PROVIDERS.iter().zip(&self.providers) {
            if keys.is_empty() {
                continue;
            }
            doc.table_header(format!("  {title} ({})", keys.len()));
            for (i, key) in keys.iter().enumerate() {
                let mut info = s::mask_key(&s::get_string(key, "api-key"));
                append_details(&mut info, key);
                doc.text(format!("  {}. {info}", i + 1), Style::new());
            }
            doc.blank();
        }
        if !self.openai.is_empty() {
            doc.table_header(format!("  OpenAI Compatibility ({})", self.openai.len()));
            for (i, entry) in self.openai.iter().enumerate() {
                let mut info = s::get_string(entry, "name");
                append_details(&mut info, entry);
                doc.text(format!("  {}. {info}", i + 1), Style::new());
            }
            doc.blank();
        }
        if let Some((status, style)) = &self.status {
            doc.text(status.clone(), *style);
        }
        doc
    }
}

/// Go's ` (prefix: p)` and ` → base-url` suffixes.
fn append_details(info: &mut String, entry: &Object) {
    let prefix = s::get_string(entry, "prefix");
    if !prefix.is_empty() {
        info.push_str(&format!(" (prefix: {prefix})"));
    }
    let base = s::get_string(entry, "base-url");
    if !base.is_empty() {
        info.push_str(&format!(" → {base}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(keys: &[&str]) -> Result<Data, String> {
        Ok(Data {
            api_keys: keys.iter().map(|k| (*k).to_owned()).collect(),
            providers: Vec::new(),
            openai: Vec::new(),
        })
    }

    /// Keys are deleted and edited by index; a refresh that moves the chosen key
    /// cancels the action instead of hitting its neighbour.
    #[test]
    fn a_refresh_that_moves_the_key_cancels_delete_and_edit() {
        let mut tab = KeysTab::new(Arc::new(Client::new("http://127.0.0.1:1", "")));
        tab.data(data(&["k1", "k2", "k3"]));
        tab.key("down");
        tab.key("d");
        tab.data(data(&["k1", "k2", "k4"]));
        assert_eq!(tab.confirm, Some(1), "unmoved key keeps the prompt");
        tab.data(data(&["k1", "k3", "k2"]));
        assert_eq!(tab.confirm, None);
        assert!(tab.key("y").is_empty());

        tab.key("e");
        assert!(tab.editing);
        tab.data(data(&["k2"]));
        assert!(!tab.editing);
        assert!(tab.key("enter").is_empty());
        let _locale = super::super::i18n::LOCALE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tab.locale();
        assert!(tab.vp.text().iter().any(|l| l.contains(t("key_changed"))));
    }
}
