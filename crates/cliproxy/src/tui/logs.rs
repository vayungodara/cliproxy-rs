//! Go's logs tab (internal/tui/logs_tab.go): the embedded server's log hook in
//! standalone mode, else `GET /logs` every two seconds.
use std::sync::Arc;
use std::time::Duration;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::client::Client;
use super::i18n::t;
use super::style::{self as s, Doc};
use super::widgets::Viewport;
use super::{Cmd, LogHook, Msg, cmd};

const MAX_LINES: usize = 5000;

pub struct LogsTab {
    client: Arc<Client>,
    hook: Option<Arc<LogHook>>,
    pub vp: Viewport,
    pub lines: Vec<String>,
    pub auto_scroll: bool,
    width: usize,
    pub filter: &'static str,
    after: i64,
    last_err: Option<String>,
}

impl LogsTab {
    pub fn new(client: Arc<Client>, hook: Option<Arc<LogHook>>) -> Self {
        LogsTab {
            client,
            hook,
            vp: Viewport::default(),
            lines: Vec::new(),
            auto_scroll: true,
            width: 0,
            filter: "",
            after: 0,
            last_err: None,
        }
    }

    /// Go `Init`: wait on the hook, else fetch.
    pub fn init(&self) -> Cmd {
        match &self.hook {
            Some(hook) => {
                let hook = hook.clone();
                cmd(async move { Msg::LogLine(hook.next().await) })
            }
            None => self.fetch(),
        }
    }

    fn fetch(&self) -> Cmd {
        let (client, after) = (self.client.clone(), self.after);
        cmd(async move { Msg::LogsPoll(client.get_logs(after, 200).await) })
    }

    pub fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.vp.height = height;
        self.refresh();
    }

    fn refresh(&mut self) {
        let doc = self.render();
        self.vp.set_content(doc);
    }

    pub fn locale(&mut self) {
        self.refresh();
    }

    fn append(&mut self, lines: impl IntoIterator<Item = String>) {
        self.lines.extend(lines);
        if self.lines.len() > MAX_LINES {
            self.lines.drain(..self.lines.len() - MAX_LINES);
        }
    }

    pub fn message(&mut self, msg: Msg) -> Vec<Cmd> {
        match msg {
            Msg::LogsTick if self.hook.is_none() => vec![self.fetch()],
            Msg::LogsPoll(result) if self.hook.is_none() => {
                match result {
                    Ok((lines, latest)) => {
                        self.last_err = None;
                        self.after = latest;
                        self.append(lines);
                    }
                    Err(e) => self.last_err = Some(e),
                }
                self.refresh();
                if self.auto_scroll {
                    self.vp.goto_bottom();
                }
                vec![cmd(async {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Msg::LogsTick
                })]
            }
            Msg::LogLine(line) => {
                self.append([line]);
                self.refresh();
                if self.auto_scroll {
                    self.vp.goto_bottom();
                }
                vec![self.init()]
            }
            _ => Vec::new(),
        }
    }

    pub fn key(&mut self, key: &str) -> Vec<Cmd> {
        match key {
            "a" => {
                self.auto_scroll = !self.auto_scroll;
                self.refresh();
                if self.auto_scroll {
                    self.vp.goto_bottom();
                }
            }
            "c" => {
                self.lines.clear();
                self.last_err = None;
                self.refresh();
            }
            "1" | "2" | "3" | "4" => {
                self.filter = ["", "info", "warn", "error"][usize::from(key.as_bytes()[0] - b'1')];
                self.refresh();
            }
            _ => {
                let was_at_bottom = self.vp.at_bottom();
                self.vp.key(key);
                if !self.vp.at_bottom() && was_at_bottom {
                    self.auto_scroll = false;
                }
                if self.vp.at_bottom() {
                    self.auto_scroll = true;
                }
                self.refresh();
            }
        }
        Vec::new()
    }

    /// Go `matchLevel`.
    fn matches(&self, line: &str) -> bool {
        match self.filter {
            "error" => line.contains("[error]") || line.contains("[fatal]") || line.contains("[panic]"),
            "warn" => line.contains("[warn") || line.contains("[error]") || line.contains("[fatal]"),
            "info" => !line.contains("[debug]"),
            _ => true,
        }
    }

    pub fn render_into(&self, area: Rect, buf: &mut Buffer) {
        self.vp.render(area, buf);
    }

    /// Go `renderLogs`.
    fn render(&self) -> Doc {
        let mut doc = Doc::default();
        let scroll = if self.auto_scroll {
            s::span(t("logs_auto_scroll"), s::success())
        } else {
            s::span(t("logs_paused"), s::warning())
        };
        let filter = if self.filter.is_empty() {
            "ALL".to_owned()
        } else {
            format!("{}+", self.filter.to_uppercase())
        };
        doc.line(vec![
            s::span(format!(" {}  ", t("logs_title")), s::title()),
            scroll.patch_style(Style::new().add_modifier(ratatui::style::Modifier::BOLD)),
            s::span(
                format!(
                    "  {}: {filter}  {}: {}",
                    t("logs_filter"),
                    t("logs_lines"),
                    self.lines.len()
                ),
                s::title(),
            ),
        ]);
        doc.blank();
        doc.text(t("logs_help"), s::help());
        doc.text("─".repeat(self.width), Style::new());
        if let Some(err) = &self.last_err {
            doc.text(format!("⚠ Error: {err}"), s::error());
        }
        if self.lines.is_empty() {
            doc.text(t("logs_waiting"), s::subtitle());
            return doc;
        }
        for line in self.lines.iter().filter(|l| self.matches(l)) {
            // Go `styleLine`.
            let style = if line.contains("[error]") || line.contains("[fatal]") {
                s::fg(s::ERROR)
            } else if line.contains("[warn") {
                s::fg(s::WARNING)
            } else if line.contains("[info") {
                s::fg(s::INFO)
            } else if line.contains("[debug]") {
                s::fg(s::MUTED)
            } else {
                Style::new()
            };
            doc.text(line.clone(), style);
        }
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_follow_go_match_level() {
        let mut tab = LogsTab::new(Arc::new(Client::new("", "")), None);
        let lines = [
            "[debug] d",
            "[info ] i",
            "[warn ] w",
            "[error] e",
            "[fatal] f",
            "[panic] p",
        ];
        let shown = |tab: &LogsTab| lines.iter().filter(|l| tab.matches(l)).count();
        assert_eq!(shown(&tab), 6);
        tab.filter = "info";
        assert_eq!(shown(&tab), 5);
        tab.filter = "warn";
        assert_eq!(shown(&tab), 3);
        tab.filter = "error";
        assert_eq!(shown(&tab), 3);
    }
}
