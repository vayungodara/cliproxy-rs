//! Go's dashboard tab (internal/tui/dashboard.go): connection, key and auth-file
//! cards, and the current config.
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

use super::client::{Client, Object};
use super::i18n::t;
use super::style::{self as s, Doc, plain, span};
use super::widgets::Viewport;
use super::{Cmd, Msg, cmd};

/// Go's `dashboardDataMsg`.
pub struct Data {
    pub(super) config: Option<Object>,
    pub(super) auth_files: Vec<Object>,
    pub(super) api_keys: Vec<String>,
    pub(super) err: Option<String>,
}

pub struct Dashboard {
    client: Arc<Client>,
    pub vp: Viewport,
    width: usize,
    last: Option<(Option<Object>, Vec<Object>, Vec<String>)>,
    /// Sizes the cards to fit the screen. Go's `(width-2)/2` makes the pair three
    /// columns wider than the terminal, which cuts the right card's border; the golden
    /// test turns this off to compare with Go's exact layout.
    pub(super) fit_cards: bool,
}

impl Dashboard {
    pub fn new(client: Arc<Client>) -> Self {
        Dashboard {
            client,
            vp: Viewport::default(),
            width: 0,
            last: None,
            fit_cards: true,
        }
    }

    /// Go `fetchData`: config, auth files and API keys; the first error wins.
    pub fn fetch(&self) -> Cmd {
        let client = self.client.clone();
        cmd(async move {
            let config = client.get_config().await;
            let files = client.get_auth_files().await;
            let keys = client.get_api_keys().await;
            let err = [config.as_ref().err(), files.as_ref().err(), keys.as_ref().err()]
                .into_iter()
                .flatten()
                .next()
                .cloned();
            Msg::Dashboard(Data {
                config: config.ok(),
                auth_files: files.unwrap_or_default(),
                api_keys: keys.unwrap_or_default(),
                err,
            })
        })
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.vp.height = height;
    }

    pub fn data(&mut self, data: Data) {
        match data.err {
            Some(err) => {
                let mut doc = Doc::default();
                doc.text(format!("⚠ Error: {err}"), s::error());
                self.vp.set_content(doc);
            }
            None => {
                self.last = Some((data.config, data.auth_files, data.api_keys));
                self.vp.set_content(self.render());
            }
        }
    }

    /// Go's `localeChangedMsg`: re-render the cached data, then fetch again.
    pub fn locale(&mut self) -> Vec<Cmd> {
        self.vp.set_content(self.render());
        vec![self.fetch()]
    }

    pub fn key(&mut self, key: &str) -> Vec<Cmd> {
        if key == "r" {
            return vec![self.fetch()];
        }
        self.vp.key(key);
        Vec::new()
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        self.vp.render(area, buf);
    }

    /// Go `renderDashboard`.
    fn render(&self) -> Doc {
        let (cfg, files, keys) = match &self.last {
            Some((cfg, files, keys)) => (cfg.as_ref(), files.as_slice(), keys.as_slice()),
            None => (None, &[][..], &[][..]),
        };
        let mut doc = Doc::default();
        doc.title(t("dashboard_title"));
        doc.text(t("dashboard_help"), s::help());
        doc.blank();
        doc.line(vec![
            span(t("connected"), s::bold(s::SUCCESS)),
            plain(format!("  {}", self.client.base_url())),
        ]);
        doc.blank();

        let spare = if self.fit_cards { 5 } else { 2 };
        let card_width = if self.width > 0 {
            ((self.width as i64 - spare) / 2).max(18) as usize
        } else {
            25
        };
        let active = files.iter().filter(|f| !s::get_bool(f, "disabled")).count();
        let cards = [
            (
                format!("🔑 {}", keys.len()),
                Color::Indexed(111),
                t("mgmt_keys").to_owned(),
            ),
            (
                format!("📄 {}", files.len()),
                Color::Indexed(76),
                format!("{} ({active} {})", t("auth_files_label"), t("active_suffix")),
            ),
        ];
        // lipgloss: rounded border (color 240), padding 0 1, Width (with padding),
        // Height 2 and text wrapped to fit; the cards joined top-aligned with a space.
        let border = s::fg(Color::Indexed(240));
        let inner = card_width.saturating_sub(2);
        let card = |(value, color, label): &(String, Color, String)| {
            let mut rows: Vec<(String, Style)> = s::wrap_words(value, inner)
                .into_iter()
                .map(|l| (l, s::bold(*color)))
                .collect();
            rows.extend(s::wrap_words(label, inner).into_iter().map(|l| (l, s::fg(s::MUTED))));
            let mut out = vec![vec![span(format!("╭{}╮", "─".repeat(card_width)), border)]];
            for (text, style) in rows {
                out.push(vec![
                    span("│ ", border),
                    span(s::pad(&text, inner), style),
                    span(" │", border),
                ]);
            }
            out.push(vec![span(format!("╰{}╯", "─".repeat(card_width)), border)]);
            out
        };
        let (left, right) = (card(&cards[0]), card(&cards[1]));
        for row in 0..left.len().max(right.len()) {
            let blank = || vec![plain(" ".repeat(card_width + 2))];
            let mut line = left.get(row).cloned().unwrap_or_else(blank);
            line.push(plain(" "));
            line.extend(right.get(row).cloned().unwrap_or_else(blank));
            doc.line(line);
        }
        doc.blank();

        doc.text(t("current_config"), s::bold(s::HIGHLIGHT));
        doc.text("─".repeat(self.width.min(60)), Style::new());
        if let Some(cfg) = cfg {
            // A missing or non-bool usage-statistics-enabled counts as on.
            let usage = cfg
                .get("usage-statistics-enabled")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            let bool_text = |b: bool| if b { t("bool_yes") } else { t("bool_no") }.to_owned();
            let mut items = vec![
                (t("debug_mode"), bool_text(s::get_bool(cfg, "debug"))),
                (t("usage_stats"), bool_text(usage)),
                (t("log_to_file"), bool_text(s::get_bool(cfg, "logging-to-file"))),
                (t("retry_count"), s::int_text(s::get_float(cfg, "request-retry"))),
            ];
            let proxy = s::get_string(cfg, "proxy-url");
            if !proxy.is_empty() {
                items.push((t("proxy_url"), proxy));
            }
            let strategy = cfg
                .get("routing")
                .and_then(serde_json::Value::as_object)
                .map(|r| s::get_string(r, "strategy"))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "round-robin".into());
            items.push((t("routing_strategy"), strategy));
            for (label, value) in items {
                doc.line(vec![
                    plain("  "),
                    span(s::pad(&format!("{label}:"), 24), s::bold(s::INFO)),
                    plain(" "),
                    span(value, s::value()),
                ]);
            }
        }
        doc.blank();
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_fit_the_screen() {
        let mut tab = Dashboard::new(Arc::new(Client::new("", "")));
        for width in [41, 80, 112, 113] {
            tab.set_size(width, 100);
            tab.data(Data {
                config: None,
                auth_files: Vec::new(),
                api_keys: Vec::new(),
                err: None,
            });
            let cards = tab.vp.text().into_iter().find(|l| l.starts_with('╭')).unwrap();
            let used = s::width(&cards);
            assert!(used <= width && used + 2 >= width, "{width}: {used}");
        }
    }
}
