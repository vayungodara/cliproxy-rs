//! Go's auth files tab (internal/tui/auth_tab.go): list, expand, enable or disable,
//! delete, refresh, and edit prefix, proxy URL and priority.
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;

use super::client::{Client, Object};
use super::i18n::{t, tf};
use super::style::{self as s, Doc, plain, span};
use super::widgets::{TextInput, Viewport};
use super::{Cmd, Msg, cmd};

/// Go's `authEditableFields`.
const EDITABLE: [(&str, &str); 3] = [
    ("Prefix", "prefix"),
    ("Proxy URL", "proxy_url"),
    ("Priority", "priority"),
];

/// Go `getAnyString`: `%v` of a member, empty when absent or null.
fn any_string(f: &Object, key: &str) -> String {
    match f.get(key) {
        None | Some(Value::Null) => String::new(),
        Some(v) => s::go_v(v),
    }
}

pub struct AuthTab {
    client: Arc<Client>,
    pub vp: Viewport,
    pub files: Vec<Object>,
    err: Option<String>,
    width: usize,
    pub cursor: usize,
    pub expanded: Option<usize>,
    /// The file awaiting delete confirmation, by name: a refresh can reorder the list.
    pub confirm: Option<String>,
    status: Option<(String, Style)>,
    pub editing: bool,
    edit_field: usize,
    pub input: TextInput,
    edit_file: String,
}

impl AuthTab {
    pub fn new(client: Arc<Client>) -> Self {
        AuthTab {
            client,
            vp: Viewport::default(),
            files: Vec::new(),
            err: None,
            width: 0,
            cursor: 0,
            expanded: None,
            confirm: None,
            status: None,
            editing: false,
            edit_field: 0,
            input: TextInput::new(256),
            edit_file: String::new(),
        }
    }

    pub fn fetch(&self) -> Cmd {
        let client = self.client.clone();
        cmd(async move { Msg::AuthFiles(client.get_auth_files().await) })
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.vp.height = height;
        self.input.width = width.saturating_sub(20);
    }

    fn refresh(&mut self) {
        let doc = self.render();
        self.vp.set_content(doc);
    }

    pub fn locale(&mut self) {
        self.refresh();
    }

    pub fn files(&mut self, result: Result<Vec<Object>, String>) {
        match result {
            Ok(files) => {
                self.err = None;
                self.files = files;
                if let Some(name) = &self.confirm
                    && !self.files.iter().any(|f| s::get_string(f, "name") == *name)
                {
                    self.confirm = None;
                }
                if self.cursor >= self.files.len() {
                    self.cursor = self.files.len().saturating_sub(1);
                }
                self.status = None;
            }
            Err(e) => self.err = Some(e),
        }
        self.refresh();
    }

    /// Go's `authActionMsg`: the result line, then a fresh list.
    pub fn action(&mut self, result: Result<String, String>) -> Vec<Cmd> {
        self.status = Some(match result {
            Ok(action) => (format!("✓ {action}"), s::success()),
            Err(e) => (format!("✗ {e}"), s::error()),
        });
        self.confirm = None;
        self.refresh();
        vec![self.fetch()]
    }

    fn name(&self, index: usize) -> String {
        self.files
            .get(index)
            .map(|f| s::get_string(f, "name"))
            .unwrap_or_default()
    }

    fn act(&self, work: impl std::future::Future<Output = Result<String, String>> + Send + 'static) -> Vec<Cmd> {
        vec![cmd(async move { Msg::AuthAction(work.await) })]
    }

    pub fn key(&mut self, key: &str) -> Vec<Cmd> {
        if self.editing {
            return self.edit_key(key);
        }
        if self.confirm.is_some() {
            return match key {
                "y" | "Y" => {
                    let Some(name) = self.confirm.take() else {
                        return Vec::new();
                    };
                    let client = self.client.clone();
                    self.act(async move {
                        client.delete_auth_file(&name).await?;
                        Ok(tf("deleted", &name))
                    })
                }
                "n" | "N" | "esc" => {
                    self.confirm = None;
                    self.refresh();
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }
        let count = self.files.len();
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
            "enter" | " " => {
                self.expanded = if self.expanded == Some(self.cursor) {
                    None
                } else {
                    Some(self.cursor)
                };
                self.refresh();
            }
            "d" | "D" => {
                if self.cursor < count {
                    self.confirm = Some(self.name(self.cursor));
                    self.refresh();
                }
            }
            "e" | "E" => {
                if let Some(f) = self.files.get(self.cursor) {
                    let (client, name) = (self.client.clone(), s::get_string(f, "name"));
                    let disabled = !s::get_bool(f, "disabled");
                    return self.act(async move {
                        client.toggle_auth_file(&name, disabled).await?;
                        let action = if disabled { t("disabled") } else { t("enabled") };
                        Ok(format!("{action} {name}"))
                    });
                }
            }
            "1" | "2" | "3" => self.start_edit(usize::from(key.as_bytes()[0] - b'1')),
            "r" => {
                self.status = None;
                return vec![self.fetch()];
            }
            "R" => {
                if self.cursor < count {
                    let (client, name) = (self.client.clone(), self.name(self.cursor));
                    return self.act(async move {
                        client.refresh_auth_file(&name).await?;
                        Ok(tf("refreshed_auth", &name))
                    });
                }
            }
            _ => self.vp.key(key),
        }
        Vec::new()
    }

    /// Go `startEdit`.
    fn start_edit(&mut self, field: usize) {
        let Some(f) = self.files.get(self.cursor) else { return };
        self.edit_file = s::get_string(f, "name");
        self.edit_field = field;
        self.editing = true;
        let (label, key) = EDITABLE[field];
        self.input.set_value(&any_string(f, key));
        self.input.focus();
        self.input.prompt = format!("  {label}: ");
        self.refresh();
    }

    fn edit_key(&mut self, key: &str) -> Vec<Cmd> {
        match key {
            "enter" => {
                let value = self.input.value();
                let field = EDITABLE[self.edit_field].1;
                let name = self.edit_file.clone();
                self.editing = false;
                self.input.blur();
                let parsed: Value = if field == "priority" {
                    match super::config::atoi(&value) {
                        Some(p) => p.into(),
                        None => {
                            let err = format!("{}: {value}", t("invalid_int"));
                            return self.act(async move { Err(err) });
                        }
                    }
                } else {
                    value.into()
                };
                let client = self.client.clone();
                self.act(async move {
                    client.patch_auth_file_fields(&name, vec![(field, parsed)]).await?;
                    Ok(updated_field(field, &name))
                })
            }
            "esc" => {
                self.editing = false;
                self.input.blur();
                self.refresh();
                Vec::new()
            }
            _ => {
                self.input.key(key);
                self.refresh();
                Vec::new()
            }
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.editing {
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
        doc.title(t("auth_title"));
        doc.text(t("auth_help1"), s::help());
        doc.text(t("auth_help2"), s::help());
        doc.text("─".repeat(self.width), Style::new());
        if let Some(err) = &self.err {
            doc.text(format!("⚠ Error: {err}"), s::error());
            return doc;
        }
        if self.files.is_empty() {
            doc.text(t("no_auth_files"), s::subtitle());
            return doc;
        }
        for (i, f) in self.files.iter().enumerate() {
            let name = s::get_string(f, "name");
            let disabled = s::get_bool(f, "disabled");
            let (icon, status) = if disabled {
                (span("○", s::fg(s::MUTED)), t("status_disabled"))
            } else {
                (span("●", s::success()), t("status_active"))
            };
            let row_style = if i == self.cursor {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Style::new()
            };
            let rest = format!(
                " {} {} {} {status}",
                go_pad(&s::truncate_bytes(&name, 24), 24),
                go_pad(&s::get_string(f, "channel"), 12),
                go_pad(&s::truncate_bytes(&s::get_string(f, "email"), 28), 28),
            );
            doc.line(vec![
                span(if i == self.cursor { "▸ " } else { "  " }, row_style),
                icon.patch_style(row_style),
                span(rest, row_style),
            ]);
            if self.confirm.as_deref() == Some(name.as_str()) {
                doc.text(format!("    {}", tf("confirm_delete", &name)), s::warning());
            }
            if self.editing && i == self.cursor {
                doc.0.push(self.input.view());
                doc.text(format!("    {} • {}", t("enter_save"), t("esc_cancel")), s::help());
            }
            if self.expanded == Some(i) {
                self.render_detail(&mut doc, f);
            }
        }
        if let Some((status, style)) = &self.status {
            doc.blank();
            doc.text(status.clone(), *style);
        }
        doc
    }

    /// Go `renderDetail`.
    fn render_detail(&self, doc: &mut Doc, f: &Object) {
        const FIELDS: [(&str, &str, bool); 14] = [
            ("Name", "name", false),
            ("Channel", "channel", false),
            ("Email", "email", false),
            ("Status", "status", false),
            ("Status Msg", "status_message", false),
            ("File Name", "file_name", false),
            ("Auth Type", "auth_type", false),
            ("Prefix", "prefix", true),
            ("Proxy URL", "proxy_url", true),
            ("Priority", "priority", true),
            ("Project ID", "project_id", false),
            ("Disabled", "disabled", false),
            ("Created", "created_at", false),
            ("Updated", "updated_at", false),
        ];
        let rule = "─────────────────────────────────────────────";
        doc.text(format!("    ┌{rule}"), Style::new());
        for (label, key, editable) in FIELDS {
            let mut value = any_string(f, key);
            if value.is_empty() || value == "<nil>" {
                if !editable {
                    continue;
                }
                value = t("not_set").to_owned();
            }
            let mut spans = vec![
                plain("    │ "),
                span(format!("{label:<12}:"), s::bold(Color::Indexed(111))),
                plain(" "),
                span(value, s::fg(Color::Indexed(252))),
            ];
            if editable {
                spans.push(span(" ✎", s::fg(Color::Indexed(214))));
            }
            doc.line(spans);
        }
        doc.text(format!("    └{rule}"), Style::new());
    }
}

/// Go `%-Ns`: pads to N runes (not columns).
fn go_pad(text: &str, n: usize) -> String {
    let count = text.chars().count();
    if count >= n {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(n - count))
    }
}

/// Go `fmt.Sprintf(T("updated_field"), fieldKey, fileName)`.
fn updated_field(field: &str, name: &str) -> String {
    let template = t("updated_field");
    let mut parts = template.splitn(3, "%s");
    let (a, b, c) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    format!("{a}{field}{b}{name}{c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(names: &[&str]) -> Result<Vec<Object>, String> {
        Ok(names
            .iter()
            .map(|n| serde_json::json!({ "name": n }).as_object().cloned().unwrap())
            .collect())
    }

    /// A refresh between `d` and `y` must not move the delete to another file.
    #[test]
    fn delete_targets_the_file_chosen_before_a_refresh() {
        let mut tab = AuthTab::new(Arc::new(Client::new("http://127.0.0.1:1", "")));
        tab.files(files(&["a.json", "b.json", "c.json"]));
        tab.key("down");
        tab.key("d");
        tab.files(files(&["c.json", "b.json"]));
        assert_eq!(tab.confirm.as_deref(), Some("b.json"));
        tab.files(files(&["b.json", "a.json"]));
        assert_eq!(tab.confirm.as_deref(), Some("b.json"));
        tab.files(files(&["a.json", "c.json"]));
        assert_eq!(tab.confirm, None);
        assert!(tab.key("y").is_empty());
    }
}
