//! Go's root model (internal/tui/app.go): the password gate, the tab bar, lazy
//! per-tab loading, the locale toggle and the status bar.
//!
//! Deliberate differences from Go, both presentation:
//! - each message goes to the tab that asked for it; Go hands non-key messages to the
//!   active tab only, so a result that lands after a tab switch was dropped (and a
//!   pending OAuth poll never finished);
//! - the status bar sits on the last terminal row; Go leaves two empty rows under it;
//! - while a text field has focus, keys go to it (only Ctrl+C stays global); Go still
//!   quits on `q`, switches locale on `L` and changes tab on Tab while you type, so a
//!   value containing `q` could not be entered. On the password gate `q` and `L` keep
//!   their meaning while the field is empty.
use std::sync::Arc;

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use serde_json::Value;

use super::auth::AuthTab;
use super::client::{Client, Object};
use super::config::ConfigTab;
use super::dashboard::Dashboard;
use super::i18n::{self, t, tf};
use super::keys::KeysTab;
use super::logs::LogsTab;
use super::oauth::OAuthTab;
use super::style::{self as s, Doc, plain, span};
use super::widgets::TextInput;
use super::{Cmd, LogHook, Msg, cmd};

pub const DASHBOARD: usize = 0;
pub const CONFIG: usize = 1;
pub const AUTH_FILES: usize = 2;
pub const API_KEYS: usize = 3;
pub const OAUTH: usize = 4;
pub const LOGS: usize = 5;

/// What one update asks the loop to do.
#[derive(Default)]
pub struct Step {
    pub cmds: Vec<Cmd>,
    pub quit: bool,
}

impl From<Vec<Cmd>> for Step {
    fn from(cmds: Vec<Cmd>) -> Self {
        Step { cmds, quit: false }
    }
}

fn quit() -> Step {
    Step {
        cmds: Vec::new(),
        quit: true,
    }
}

/// Go `isLogsEnabledFromConfig`: on unless `logging-to-file` is a false bool.
fn logs_enabled_from(cfg: &Object) -> bool {
    cfg.get("logging-to-file").and_then(Value::as_bool).unwrap_or(true)
}

pub struct App {
    pub active: usize,
    pub tabs: Vec<&'static str>,
    standalone: bool,
    pub logs_enabled: bool,
    pub authenticated: bool,
    pub auth_input: TextInput,
    pub auth_error: String,
    auth_connecting: bool,
    pub dashboard: Dashboard,
    pub config: ConfigTab,
    pub auth: AuthTab,
    pub keys: KeysTab,
    pub oauth: OAuthTab,
    pub logs: LogsTab,
    client: Arc<Client>,
    width: u16,
    pub initialized: [bool; 6],
}

impl App {
    /// Go `NewAppWithBaseURL`: standalone (with a log hook) skips the password gate.
    pub fn new(base_url: &str, secret: &str, hook: Option<Arc<LogHook>>) -> Self {
        let standalone = hook.is_some();
        let client = Arc::new(Client::new(base_url, secret));
        let mut auth_input = TextInput::new(512);
        auth_input.password = true;
        auth_input.set_value(secret.trim());
        auth_input.focus();
        let mut app = App {
            active: DASHBOARD,
            tabs: Vec::new(),
            standalone,
            logs_enabled: true,
            authenticated: standalone,
            auth_input,
            auth_error: String::new(),
            auth_connecting: false,
            dashboard: Dashboard::new(client.clone()),
            config: ConfigTab::new(client.clone()),
            auth: AuthTab::new(client.clone()),
            keys: KeysTab::new(client.clone()),
            oauth: OAuthTab::new(Some(client.clone())),
            logs: LogsTab::new(client.clone(), hook),
            client,
            width: 0,
            initialized: [true, false, false, false, false, true],
        };
        app.refresh_tabs();
        if !standalone {
            app.initialized = [false; 6];
        }
        app.set_auth_prompt();
        app
    }

    #[cfg(test)]
    pub fn base_url(&self) -> &str {
        self.client.base_url()
    }

    /// Go `Init`.
    pub fn init(&self) -> Vec<Cmd> {
        if !self.authenticated {
            return Vec::new();
        }
        let mut cmds = vec![self.dashboard.fetch()];
        if self.logs_enabled {
            cmds.push(self.logs.init());
        }
        cmds
    }

    fn set_auth_prompt(&mut self) {
        self.auth_input.prompt = format!("  {}: ", t("auth_gate_password"));
    }

    /// Go `refreshTabs`.
    pub fn refresh_tabs(&mut self) {
        let names = i18n::tab_names();
        self.tabs = names
            .iter()
            .enumerate()
            .filter(|(i, _)| self.logs_enabled || *i != LOGS)
            .map(|(_, n)| *n)
            .collect();
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    /// Go's `WindowSizeMsg`: the content area is the screen less the tab and status bars.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.auth_input.width = usize::from(width).saturating_sub(6);
        let (w, h) = (usize::from(width), usize::from(height).saturating_sub(2).max(1));
        self.dashboard.set_size(w, h);
        self.config.set_size(w, h);
        self.auth.set_size(w, h);
        self.keys.set_size(w, h);
        self.oauth.set_size(w, h);
        self.logs.set_size(w, h);
    }

    /// Go `initTabIfNeeded`.
    fn init_tab_if_needed(&mut self) -> Vec<Cmd> {
        if self.initialized[self.active] {
            return Vec::new();
        }
        self.initialized[self.active] = true;
        match self.active {
            DASHBOARD => vec![self.dashboard.fetch()],
            CONFIG => vec![self.config.fetch()],
            AUTH_FILES => vec![self.auth.fetch()],
            API_KEYS => vec![self.keys.fetch()],
            LOGS if self.logs_enabled => vec![self.logs.init()],
            _ => Vec::new(),
        }
    }

    /// Whether a tab's text field has focus.
    fn typing(&self) -> bool {
        match self.active {
            CONFIG => self.config.editing,
            AUTH_FILES => self.auth.editing,
            API_KEYS => self.keys.editing || self.keys.adding,
            OAUTH => self.oauth.input_active && !self.oauth.device,
            _ => false,
        }
    }

    pub fn key(&mut self, key: &str) -> Step {
        if !self.authenticated {
            let empty = self.auth_input.value().is_empty();
            match key {
                "ctrl+c" => return quit(),
                "q" if empty => return quit(),
                "L" if empty => {
                    i18n::toggle_locale();
                    self.refresh_tabs();
                    self.set_auth_prompt();
                }
                "enter" => {
                    if self.auth_connecting {
                        return Step::default();
                    }
                    let password = self.auth_input.value().trim().to_owned();
                    if password.is_empty() {
                        self.auth_error = t("auth_gate_password_required").to_owned();
                        return Step::default();
                    }
                    self.auth_error.clear();
                    self.auth_connecting = true;
                    let client = self.client.clone();
                    return vec![cmd(async move {
                        client.set_secret_key(&password);
                        Msg::AuthConnect(client.get_config().await)
                    })]
                    .into();
                }
                _ => self.auth_input.key(key),
            }
            return Step::default();
        }
        if key == "ctrl+c" {
            return quit();
        }
        if self.typing() {
            return match self.active {
                CONFIG => self.config.key(key),
                AUTH_FILES => self.auth.key(key),
                API_KEYS => self.keys.key(key),
                _ => self.oauth.key(key),
            }
            .into();
        }
        match key {
            // On the logs tab `q` goes to the tab instead.
            "q" if !self.logs_enabled || self.active != LOGS => return quit(),
            "L" => {
                i18n::toggle_locale();
                self.refresh_tabs();
                let cmds = self.dashboard.locale();
                self.config.locale();
                self.auth.locale();
                self.keys.locale();
                self.oauth.locale();
                self.logs.locale();
                return cmds.into();
            }
            "tab" | "shift+tab" => {
                let n = self.tabs.len();
                self.active = if key == "tab" {
                    (self.active + 1) % n
                } else {
                    (self.active + n - 1) % n
                };
                return self.init_tab_if_needed().into();
            }
            _ => {}
        }
        match self.active {
            DASHBOARD => self.dashboard.key(key),
            CONFIG => self.config.key(key),
            AUTH_FILES => self.auth.key(key),
            API_KEYS => self.keys.key(key),
            OAUTH => self.oauth.key(key),
            _ => self.logs.key(key),
        }
        .into()
    }

    /// A terminal paste: Bubble Tea delivers it as runes to the focused input.
    pub fn paste(&mut self, text: &str) -> Step {
        if !self.authenticated {
            self.auth_input.insert(text);
            return Step::default();
        }
        match self.active {
            CONFIG => self.config.paste(text),
            AUTH_FILES => self.auth.paste(text),
            API_KEYS => self.keys.paste(text),
            OAUTH => self.oauth.paste(text),
            _ => {}
        }
        Step::default()
    }

    pub fn message(&mut self, msg: Msg) -> Step {
        if let Msg::AuthConnect(result) = msg {
            self.auth_connecting = false;
            let cfg = match result {
                Ok(cfg) => cfg,
                Err(e) => {
                    self.auth_error = tf("auth_gate_connect_fail", e);
                    return Step::default();
                }
            };
            self.auth_error.clear();
            self.authenticated = true;
            self.logs_enabled = self.standalone || logs_enabled_from(&cfg);
            self.refresh_tabs();
            // The OAuth menu depends on the server kind, known from this reply.
            self.oauth.locale();
            self.initialized = [false; 6];
            self.initialized[DASHBOARD] = true;
            let mut cmds = vec![self.dashboard.fetch()];
            if self.logs_enabled {
                self.initialized[LOGS] = true;
                cmds.push(self.logs.init());
            }
            return cmds.into();
        }
        if !self.authenticated {
            return Step::default();
        }
        let cmds = match msg {
            Msg::ConfigUpdate { path, value, err } => {
                let mut cmds = Vec::new();
                // Go: switching logging-to-file shows or hides the logs tab.
                if let (false, None, "logging-to-file", Some(enabled)) =
                    (self.standalone, &err, path.as_str(), value.as_bool())
                {
                    let before = self.logs_enabled;
                    self.logs_enabled = enabled;
                    if before != enabled {
                        self.refresh_tabs();
                    }
                    if !enabled {
                        self.initialized[LOGS] = false;
                    }
                    if !before && enabled {
                        self.initialized[LOGS] = true;
                        cmds.push(self.logs.init());
                    }
                }
                cmds.extend(self.config.updated(err));
                cmds
            }
            Msg::Dashboard(data) => {
                self.dashboard.data(data);
                Vec::new()
            }
            Msg::ConfigData(result) => {
                self.config.data(result);
                Vec::new()
            }
            Msg::AuthFiles(result) => {
                self.auth.files(result);
                Vec::new()
            }
            Msg::AuthAction(result) => self.auth.action(result),
            Msg::KeysData(result) => {
                self.keys.data(result);
                Vec::new()
            }
            Msg::KeyAction(result) => self.keys.action(result),
            Msg::OAuthStart(start) => self.oauth.start(start),
            Msg::OAuthPoll(poll) => {
                self.oauth.polled(poll);
                Vec::new()
            }
            Msg::OAuthCallback(err) => {
                self.oauth.callback_submitted(err);
                Vec::new()
            }
            // Go drops the poll chain while the logs tab is hidden.
            msg @ (Msg::LogsPoll(_) | Msg::LogsTick | Msg::LogLine(_)) if self.logs_enabled => self.logs.message(msg),
            _ => Vec::new(),
        };
        cmds.into()
    }

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        let buf = frame.buffer_mut();
        if !self.authenticated {
            self.draw_gate(area, buf);
            return;
        }
        if area.height < 3 {
            return;
        }
        let bar = Rect { height: 1, ..area };
        let status = Rect {
            y: area.bottom() - 1,
            height: 1,
            ..area
        };
        let content = Rect {
            y: area.y + 1,
            height: area.height - 2,
            ..area
        };
        self.draw_tab_bar(bar, buf);
        match self.active {
            DASHBOARD => self.dashboard.render_into(content, buf),
            CONFIG => self.config.render_into(content, buf),
            AUTH_FILES => self.auth.render_into(content, buf),
            API_KEYS => self.keys.render_into(content, buf),
            OAUTH => self.oauth.render_into(content, buf),
            _ if self.logs_enabled => self.logs.render_into(content, buf),
            _ => {}
        }
        self.draw_status_bar(status, buf);
    }

    fn draw_gate(&self, area: Rect, buf: &mut Buffer) {
        for (row, line) in self.gate_doc().0.iter().take(area.height as usize).enumerate() {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
    }

    /// Go `renderAuthView`.
    pub fn gate_doc(&self) -> Doc {
        let mut doc = Doc::default();
        doc.title(t("auth_gate_title"));
        doc.text(t("auth_gate_help"), s::help());
        doc.blank();
        if self.auth_connecting {
            doc.text(t("auth_gate_connecting"), s::warning());
            doc.blank();
        }
        if !self.auth_error.trim().is_empty() {
            doc.text(self.auth_error.clone(), s::error());
            doc.blank();
        }
        doc.0.push(self.auth_input.view());
        doc.text(t("auth_gate_enter"), s::help());
        doc
    }

    fn draw_tab_bar(&self, area: Rect, buf: &mut Buffer) {
        buf.set_style(area, Style::new().bg(s::SURFACE));
        buf.set_line(area.x, area.y, &self.tab_bar(), area.width);
    }

    /// Go `renderTabBar`. Where Go's lipgloss wraps tabs that do not fit onto a second
    /// row (pushing the content down), this one line is cut at the screen edge.
    pub fn tab_bar(&self) -> Line<'static> {
        let mut spans = vec![plain(" ")];
        for (i, name) in self.tabs.iter().enumerate() {
            let style = if i == self.active {
                s::bold(s::WHITE).bg(s::PRIMARY)
            } else {
                s::fg(s::SUBTEXT).bg(s::SURFACE)
            };
            spans.push(span(format!("  {name}  "), style));
        }
        Line::from(spans)
    }

    fn draw_status_bar(&self, area: Rect, buf: &mut Buffer) {
        let style = s::fg(s::SUBTEXT).bg(s::SURFACE);
        buf.set_style(area, style);
        buf.set_line(
            area.x,
            area.y,
            &Line::styled(status_text(area.width), style),
            area.width,
        );
    }
}

/// Go `renderStatusBar`: left and right texts, cut to fit the padded width.
pub fn status_text(width: u16) -> String {
    let content = usize::from(width.max(1)).saturating_sub(2);
    let mut left = t("status_left").trim_end_matches(' ').to_owned();
    let mut right = t("status_right").trim_end_matches(' ').to_owned();
    if s::width(&left) > content {
        left = s::fit_width(&left, content);
        right.clear();
    }
    let remaining = content.saturating_sub(s::width(&left));
    if s::width(&right) > remaining {
        right = s::fit_width(&right, remaining);
    }
    let gap = content.saturating_sub(s::width(&left) + s::width(&right));
    format!(" {left}{}{right} ", " ".repeat(gap))
}
