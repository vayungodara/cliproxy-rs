//! Go's OAuth tab (internal/tui/oauth_tab.go): starts a provider login through the v0
//! `*-auth-url` routes, then polls `get-auth-status`; web flows take a pasted callback
//! URL, device flows show the user code.
use std::sync::Arc;
use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::client::Client;
use super::i18n::{t, tf};
use super::style::{self as s, Doc, plain, span};
use super::widgets::{TextInput, Viewport};
use super::{Cmd, Msg, cmd};

/// Go's `oauthProviders`: name, v0 route, emoji, device flow, callback provider key.
pub const PROVIDERS: [(&str, &str, &str, bool, &str); 7] = [
    ("Claude (Anthropic)", "anthropic-auth-url", "🟧", false, "anthropic"),
    ("Codex (OpenAI)", "codex-auth-url", "🟩", false, "codex"),
    ("Antigravity", "antigravity-auth-url", "🟪", false, "antigravity"),
    ("Kimi (kimi.com)", "kimi-auth-url", "🟫", true, "kimi"),
    ("Kimi (kimi.ai)", "kimi-ai-auth-url", "🟫", true, "kimi-ai"),
    ("xAI", "xai-auth-url", "⬛", true, "xai"),
    ("Meta", "meta-auth-url", "🔵", true, "meta"),
];

const DEFAULT_POLL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const DEVICE_POLL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_POLL_ERRORS: u32 = 5;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Go `shouldFailOAuthStatusPoll`.
fn should_fail_poll(consecutive_errors: u32, max_errors: u32) -> bool {
    if max_errors == 0 {
        return consecutive_errors > 0;
    }
    consecutive_errors >= max_errors
}

/// When polling gives up: `expires_in` seconds from the server, else Go's default for
/// the flow. A value too large for `Instant` (Go's `Time.Add` cannot overflow) falls
/// back to the device timeout instead of panicking.
fn poll_deadline(now: Instant, expires_in: i64, device: bool) -> Instant {
    let timeout = if expires_in > 0 {
        Duration::from_secs(expires_in as u64)
    } else if device {
        DEVICE_POLL_TIMEOUT
    } else {
        DEFAULT_POLL_TIMEOUT
    };
    now.checked_add(timeout).unwrap_or(now + DEVICE_POLL_TIMEOUT)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    Idle,
    Pending,
    Remote,
    Success,
    Error,
}

/// Go's `oauthStartMsg`.
#[derive(Default)]
pub struct Start {
    pub(super) url: String,
    pub(super) state: String,
    pub(super) provider: String,
    pub(super) user_code: String,
    pub(super) device: bool,
    pub(super) expires_in: i64,
    pub(super) generation: u64,
    pub(super) err: Option<String>,
}

/// Go's `oauthPollMsg`.
pub struct Poll {
    pub(super) state: String,
    pub(super) generation: u64,
    pub(super) done: bool,
    pub(super) message: String,
    pub(super) err: Option<String>,
}

pub struct OAuthTab {
    client: Option<Arc<Client>>,
    pub vp: Viewport,
    pub cursor: usize,
    pub state: State,
    message: Option<(String, Style)>,
    width: usize,
    pub auth_url: String,
    pub auth_state: String,
    pub(super) provider: String,
    pub(super) user_code: String,
    pub device: bool,
    pub(super) expires_in: i64,
    pub input: TextInput,
    pub input_active: bool,
    pub generation: u64,
}

impl OAuthTab {
    pub fn new(client: Option<Arc<Client>>) -> Self {
        let mut input = TextInput::new(2048);
        input.placeholder = "http://localhost:.../auth/callback?code=...&state=...".into();
        input.prompt = "  回调 URL: ".into();
        OAuthTab {
            client,
            vp: Viewport::default(),
            cursor: 0,
            state: State::Idle,
            message: None,
            width: 0,
            auth_url: String::new(),
            auth_state: String::new(),
            provider: String::new(),
            user_code: String::new(),
            device: false,
            expires_in: 0,
            input,
            input_active: false,
            generation: 0,
        }
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.vp.height = height;
        self.input.width = width.saturating_sub(16);
        self.refresh();
    }

    fn refresh(&mut self) {
        let doc = self.render();
        self.vp.set_content(doc);
    }

    pub fn locale(&mut self) {
        self.refresh();
    }

    pub fn start(&mut self, msg: Start) -> Vec<Cmd> {
        if msg.generation != self.generation {
            // A start that arrives after Esc or a restart: cancel its server session.
            return match msg.err {
                None => self.cancel_session(msg.state),
                Some(_) => Vec::new(),
            };
        }
        if let Some(err) = msg.err {
            self.state = State::Error;
            self.message = Some((format!("✗ {err}"), s::error()));
            self.refresh();
            return Vec::new();
        }
        self.auth_url = msg.url;
        self.auth_state = msg.state.clone();
        self.provider = msg.provider;
        self.user_code = msg.user_code;
        self.device = msg.device;
        self.expires_in = msg.expires_in;
        self.state = State::Remote;
        self.input.set_value("");
        self.message = None;
        if self.device {
            self.input_active = false;
            self.input.blur();
        } else {
            self.input.focus();
            self.input_active = true;
        }
        self.refresh();
        vec![self.poll(msg.state, msg.expires_in, self.device, msg.generation)]
    }

    pub fn polled(&mut self, msg: Poll) {
        // Go `shouldAcceptOAuthPoll`.
        if msg.generation != self.generation
            || msg.state.is_empty()
            || msg.state != self.auth_state
            || self.state != State::Remote
        {
            return;
        }
        if let Some(err) = msg.err {
            self.state = State::Error;
            self.message = Some((format!("✗ {err}"), s::error()));
            self.input_active = false;
            self.input.blur();
        } else if msg.done {
            self.state = State::Success;
            self.message = Some((format!("✓ {}", msg.message), s::success()));
            self.input_active = false;
            self.input.blur();
        } else {
            self.message = Some((format!("⏳ {}", msg.message), s::warning()));
        }
        self.refresh();
    }

    pub fn callback_submitted(&mut self, err: Option<String>) {
        self.message = Some(match err {
            Some(e) => (format!("{}: {e}", t("oauth_submit_fail")), s::error()),
            None => (t("oauth_submit_ok").to_owned(), s::success()),
        });
        self.refresh();
    }

    pub fn key(&mut self, key: &str) -> Vec<Cmd> {
        if self.input_active && !self.device {
            match key {
                "enter" => {
                    let url = self.input.value();
                    if url.is_empty() {
                        return Vec::new();
                    }
                    self.input_active = false;
                    self.input.blur();
                    self.message = Some((t("oauth_submitting").to_owned(), s::warning()));
                    self.refresh();
                    return vec![self.submit_callback(url)];
                }
                "esc" => return self.cancel_remote(),
                _ => {
                    self.input.key(key);
                    self.refresh();
                    return Vec::new();
                }
            }
        }
        if self.state == State::Remote {
            match key {
                "c" | "C" => {
                    if !self.device {
                        self.input_active = true;
                        self.input.focus();
                        self.refresh();
                    }
                }
                "esc" => return self.cancel_remote(),
                _ => self.vp.key(key),
            }
            return Vec::new();
        }
        if self.state == State::Pending {
            if key == "esc" {
                self.generation += 1;
                self.state = State::Idle;
                self.message = None;
                self.refresh();
            }
            return Vec::new();
        }
        match key {
            "up" | "k" => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.refresh();
                }
            }
            "down" | "j" => {
                if self.cursor + 1 < PROVIDERS.len() {
                    self.cursor += 1;
                    self.refresh();
                }
            }
            "enter" if self.unsupported(self.cursor) => {
                let message = format!("✗ {}: {}", PROVIDERS[self.cursor].0, t("oauth_unsupported"));
                self.message = Some((message, s::error()));
                self.refresh();
            }
            "enter" => {
                self.generation += 1;
                self.state = State::Pending;
                self.message = Some((tf("oauth_initiating", PROVIDERS[self.cursor].0), s::warning()));
                self.refresh();
                return vec![self.start_login(self.cursor, self.generation)];
            }
            "esc" => {
                self.state = State::Idle;
                self.message = None;
                self.refresh();
            }
            _ => self.vp.key(key),
        }
        Vec::new()
    }

    pub fn paste(&mut self, text: &str) {
        if self.input_active && !self.device {
            self.input.insert(text);
            self.refresh();
        }
    }

    /// Antigravity sign-in is not ported, so a cliproxy-rs server always answers 404 to
    /// it; Go servers still offer it.
    fn unsupported(&self, index: usize) -> bool {
        PROVIDERS[index].4 == "antigravity" && self.client.as_ref().is_some_and(|c| c.is_cliproxy_rs())
    }

    /// Go `startOAuth`: the auth URL with `is_webui=true`, then a best-effort browser.
    fn start_login(&self, index: usize, generation: u64) -> Cmd {
        let Some(client) = self.client.clone() else {
            return cmd(async { Msg::None });
        };
        let (name, route, _, device_provider, _) = PROVIDERS[index];
        cmd(async move {
            let fail = |err: String| {
                Msg::OAuthStart(Start {
                    generation,
                    err: Some(err),
                    ..Start::default()
                })
            };
            let data = match client.get_json(&format!("/v0/management/{route}?is_webui=true")).await {
                Ok(d) => d,
                Err(e) => return fail(format!("failed to start {name} login: {e}")),
            };
            let url = s::get_string(&data, "url");
            if url.is_empty() {
                return fail(format!("no auth URL returned for {name}"));
            }
            let user_code = s::get_string(&data, "user_code");
            let flow = s::get_string(&data, "flow").trim().to_lowercase();
            let _ = super::sys::open_browser(&url);
            Msg::OAuthStart(Start {
                device: device_provider || flow == "device" || !user_code.is_empty(),
                state: s::get_string(&data, "state"),
                expires_in: s::get_float(&data, "expires_in") as i64,
                provider: name.to_owned(),
                url,
                user_code,
                generation,
                err: None,
            })
        })
    }

    /// Go `cancelRemoteOAuth`.
    fn cancel_remote(&mut self) -> Vec<Cmd> {
        let state = std::mem::take(&mut self.auth_state);
        self.generation += 1;
        self.state = State::Idle;
        self.message = None;
        self.auth_url.clear();
        self.user_code.clear();
        self.device = false;
        self.expires_in = 0;
        self.input_active = false;
        self.input.blur();
        self.input.set_value("");
        self.refresh();
        self.cancel_session(state)
    }

    /// Go `cancelOAuthSession`.
    fn cancel_session(&self, state: String) -> Vec<Cmd> {
        let Some(client) = self.client.clone().filter(|_| !state.trim().is_empty()) else {
            return Vec::new();
        };
        vec![cmd(async move {
            let _ = client.cancel_auth_session(&state).await;
            Msg::None
        })]
    }

    /// Go `submitCallback`.
    fn submit_callback(&self, url: String) -> Cmd {
        let Some(client) = self.client.clone() else {
            return cmd(async { Msg::None });
        };
        let provider = PROVIDERS
            .iter()
            .find(|p| p.0 == self.provider)
            .map_or("", |p| p.4)
            .to_owned();
        let state = self.auth_state.clone();
        cmd(async move { Msg::OAuthCallback(client.post_oauth_callback(&provider, &url, &state).await.err()) })
    }

    /// Go `pollOAuthStatus`.
    fn poll(&self, state: String, expires_in: i64, device: bool, generation: u64) -> Cmd {
        let Some(client) = self.client.clone() else {
            return cmd(async { Msg::None });
        };
        cmd(async move {
            let deadline = poll_deadline(Instant::now(), expires_in, device);
            let mut errors = 0;
            let poll = |done: bool, message: String, err: Option<String>| {
                Msg::OAuthPoll(Poll {
                    state: state.clone(),
                    generation,
                    done,
                    message,
                    err,
                })
            };
            loop {
                if Instant::now() > deadline {
                    return poll(false, String::new(), Some(t("oauth_timeout").to_owned()));
                }
                tokio::time::sleep(POLL_INTERVAL).await;
                let (status, message) = match client.get_auth_status(&state).await {
                    Ok(r) => r,
                    Err(e) => {
                        errors += 1;
                        if should_fail_poll(errors, MAX_POLL_ERRORS) {
                            return poll(false, String::new(), Some(format!("{}: {e}", t("oauth_status_error"))));
                        }
                        continue;
                    }
                };
                errors = 0;
                match status.as_str() {
                    "ok" => return poll(true, t("oauth_success").to_owned(), None),
                    "error" => return poll(false, String::new(), Some(format!("{}: {message}", t("oauth_failed")))),
                    "wait" => {}
                    _ => return poll(true, t("oauth_completed").to_owned(), None),
                }
            }
        })
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        self.vp.render(area, buf);
    }

    /// Go `renderContent`.
    fn render(&self) -> Doc {
        let mut doc = Doc::default();
        doc.title(t("oauth_title"));
        doc.blank();
        if let Some((message, style)) = &self.message {
            doc.line(vec![plain("  "), span(message.clone(), *style)]);
            doc.blank();
        }
        if self.state == State::Remote {
            self.render_remote(&mut doc);
            return doc;
        }
        if self.state == State::Pending {
            doc.text(t("oauth_press_esc"), s::help());
            return doc;
        }
        doc.text(t("oauth_select"), s::help());
        doc.blank();
        for (i, (name, _, emoji, _, _)) in PROVIDERS.iter().enumerate() {
            let selected = i == self.cursor;
            let unsupported = self.unsupported(i);
            let label = if unsupported {
                format!(" {emoji} {name} ({}) ", t("oauth_unsupported"))
            } else {
                format!(" {emoji} {name} ")
            };
            let style = match (selected, unsupported) {
                (true, _) => s::bold(s::WHITE).bg(s::PRIMARY),
                (false, true) => s::fg(s::MUTED),
                (false, false) => s::fg(s::TEXT),
            };
            doc.line(vec![plain(if selected { "▸ " } else { "  " }), span(label, style)]);
        }
        doc.blank();
        doc.text(t("oauth_help"), s::help());
        doc
    }

    /// Go `renderRemoteMode` and `renderDeviceMode`.
    fn render_remote(&self, doc: &mut Doc) {
        doc.text(format!("  ✦ {} OAuth", self.provider), s::bold(s::HIGHLIGHT));
        doc.blank();
        doc.text(t("oauth_auth_url"), s::bold(s::INFO));
        let max = self.width.saturating_sub(6).max(40);
        for line in s::wrap_text(&self.auth_url, max) {
            doc.line(vec![plain("  "), span(line, s::fg(Color::Indexed(252)))]);
        }
        doc.blank();
        if self.device {
            if !self.user_code.trim().is_empty() {
                doc.text(t("oauth_user_code"), s::bold(s::INFO));
                doc.line(vec![
                    plain("  "),
                    span(format!(" {} ", self.user_code), s::bold(s::WHITE).bg(s::PRIMARY)),
                ]);
                doc.blank();
            }
            doc.text(t("oauth_device_hint"), s::help());
            if self.expires_in > 0 {
                doc.text(tf("oauth_device_expires", self.expires_in), s::help());
            }
            doc.blank();
            doc.text(t("oauth_waiting"), s::warning());
            doc.text(t("oauth_press_esc"), s::help());
            return;
        }
        doc.text(t("oauth_remote_hint"), s::help());
        doc.blank();
        doc.text(t("oauth_callback_url"), s::bold(s::INFO));
        if self.input_active {
            doc.0.push(self.input.view());
            doc.text(format!("  {} • {}", t("enter_submit"), t("esc_cancel")), s::help());
        } else {
            doc.text(t("oauth_press_c"), s::help());
        }
        doc.blank();
        doc.text(t("oauth_waiting"), s::warning());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(generation: u64, state: &str) -> OAuthTab {
        let mut tab = OAuthTab::new(None);
        tab.state = State::Remote;
        tab.auth_state = state.into();
        tab.auth_url = "https://example.invalid/auth".into();
        tab.generation = generation;
        tab
    }

    fn poll(state: &str, generation: u64, done: bool) -> Poll {
        Poll {
            state: state.into(),
            generation,
            done,
            message: "ok".into(),
            err: None,
        }
    }

    // Cases from Go oauth_tab_test.go.
    #[test]
    fn stale_polls_are_ignored() {
        let mut tab = remote(4, "st");
        tab.polled(poll("st", 3, true));
        assert_eq!(tab.state, State::Remote);
        tab.polled(poll("other", 4, true));
        assert_eq!(tab.state, State::Remote);
        tab.polled(poll("", 4, true));
        assert_eq!(tab.state, State::Remote);
        // Not waiting on a remote flow: ignored.
        tab.state = State::Idle;
        tab.polled(poll("st", 4, true));
        assert_eq!(tab.state, State::Idle);
        tab.state = State::Remote;
        tab.polled(poll("st", 4, true));
        assert_eq!(tab.state, State::Success);
    }

    #[test]
    fn poll_deadline_survives_huge_expiry() {
        let now = Instant::now();
        assert_eq!(poll_deadline(now, 90, true), now + Duration::from_secs(90));
        assert_eq!(poll_deadline(now, 0, false), now + DEFAULT_POLL_TIMEOUT);
        assert_eq!(poll_deadline(now, -1, true), now + DEVICE_POLL_TIMEOUT);
        assert_eq!(poll_deadline(now, i64::MAX, false), now + DEVICE_POLL_TIMEOUT);
    }

    // Go TestShouldFailOAuthStatusPoll.
    #[test]
    fn poll_errors_fail_like_go() {
        assert!(!should_fail_poll(4, 5));
        assert!(should_fail_poll(5, 5));
        assert!(should_fail_poll(1, 0));
    }

    #[test]
    fn esc_cancels_the_remote_flow() {
        let mut tab = remote(4, "st");
        tab.device = true;
        assert!(tab.key("esc").is_empty(), "no client, no cancel command");
        assert_eq!((tab.state, tab.generation), (State::Idle, 5));
        assert!(tab.auth_state.is_empty() && tab.auth_url.is_empty() && !tab.device);

        let mut tab = remote(7, "st");
        tab.input_active = true;
        tab.input.focus();
        tab.input.set_value("http://localhost/cb");
        assert!(tab.key("esc").is_empty());
        assert_eq!((tab.state, tab.generation), (State::Idle, 8));
        assert_eq!(tab.input.value(), "");
    }

    #[test]
    fn stale_start_is_ignored() {
        let mut tab = OAuthTab::new(None);
        tab.generation = 2;
        let cmds = tab.start(Start {
            url: "https://example.invalid".into(),
            state: "st".into(),
            generation: 1,
            ..Start::default()
        });
        assert!(cmds.is_empty());
        assert_eq!(tab.state, State::Idle);
        assert!(tab.auth_state.is_empty());
    }
}
