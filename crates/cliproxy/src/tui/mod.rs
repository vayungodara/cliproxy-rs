//! Go's `-tui` management client (internal/tui), a Bubble Tea program ported to
//! ratatui. The [`app::App`] model receives keys and messages and returns commands
//! (futures run on the tokio runtime whose results come back as messages), as Bubble
//! Tea's `Update` returns `tea.Cmd`s. Everything goes through the management API.
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};

mod app;
mod auth;
mod client;
mod config;
mod dashboard;
#[cfg(test)]
mod e2e;
#[cfg(test)]
mod golden;
mod i18n;
mod keys;
mod logs;
mod oauth;
mod style;
mod sys;
mod widgets;

pub use client::{Client, Object};
pub use cpa_server::logging::LogHook;

/// Bubble Tea `tea.Cmd`: work whose result comes back as a message.
pub type Cmd = Pin<Box<dyn Future<Output = Msg> + Send>>;

pub fn cmd(f: impl Future<Output = Msg> + Send + 'static) -> Cmd {
    Box::pin(f)
}

/// The tabs' messages (Go's `*Msg` types).
pub enum Msg {
    None,
    AuthConnect(Result<Object, String>),
    Dashboard(dashboard::Data),
    ConfigData(Result<Object, String>),
    ConfigUpdate {
        path: String,
        value: serde_json::Value,
        err: Option<String>,
    },
    AuthFiles(Result<Vec<Object>, String>),
    AuthAction(Result<String, String>),
    KeysData(Result<keys::Data, String>),
    KeyAction(Result<String, String>),
    OAuthStart(oauth::Start),
    OAuthPoll(oauth::Poll),
    OAuthCallback(Option<String>),
    LogsPoll(Result<(Vec<String>, i64), String>),
    LogsTick,
    LogLine(String),
}

enum Event {
    Term(TermEvent),
    Msg(Msg),
    /// SIGTERM (Unix; Windows has only the console's Ctrl+C).
    #[cfg_attr(not(unix), allow(dead_code))]
    Quit,
    /// SIGINT (Ctrl+C itself is a key while the terminal is raw).
    Interrupted,
}

/// Bubble Tea's `KeyMsg.String()` for a key event.
pub fn key_name(key: &KeyEvent) -> Option<String> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let base = match key.code {
        KeyCode::Char(c) if ctrl => format!("ctrl+{}", c.to_ascii_lowercase()),
        KeyCode::Char(' ') => " ".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left if ctrl => "ctrl+left".into(),
        KeyCode::Right if ctrl => "ctrl+right".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pgup".into(),
        KeyCode::PageDown => "pgdown".into(),
        _ => return None,
    };
    Some(if alt { format!("alt+{base}") } else { base })
}

/// The terminal while the TUI owns it. Dropping it restores raw mode, bracketed paste,
/// the alternate screen and the cursor, on a normal quit, an error or a panic, as Bubble
/// Tea's `restoreTerminalState` does. It exists before any mode changes, so a failure
/// part way through [`Screen::enter`] is undone too.
struct Screen<W: Write> {
    terminal: Terminal<CrosstermBackend<W>>,
}

impl<W: Write> Screen<W> {
    fn enter(out: W) -> io::Result<Self> {
        // Building the terminal reads its size and writes nothing.
        let mut screen = Screen {
            terminal: Terminal::new(CrosstermBackend::new(out))?,
        };
        terminal::enable_raw_mode()?;
        execute!(
            screen.terminal.backend_mut(),
            terminal::EnterAlternateScreen,
            event::EnableBracketedPaste
        )?;
        Ok(screen)
    }
}

impl<W: Write> Drop for Screen<W> {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            event::DisableBracketedPaste,
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
    }
}

/// The signal task and the terminal reader thread, stopped however the loop ends (a
/// panic included); dropped before the [`Screen`].
struct Workers {
    signals: tokio::task::JoinHandle<()>,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.signals.abort();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Bubble Tea's signal handling: SIGTERM quits, SIGINT interrupts (`ErrInterrupted`).
/// The handlers are installed by [`signals`] before the terminal changes.
#[cfg(unix)]
type Signals = (tokio::signal::unix::Signal, tokio::signal::unix::Signal);
#[cfg(not(unix))]
type Signals = ();

#[cfg(unix)]
fn signals() -> io::Result<Signals> {
    use tokio::signal::unix::{SignalKind, signal};
    Ok((signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?))
}

#[cfg(not(unix))]
fn signals() -> io::Result<Signals> {
    Ok(())
}

#[cfg(unix)]
async fn signal(mut signals: Signals) -> Event {
    tokio::select! {
        _ = signals.0.recv() => Event::Quit,
        _ = signals.1.recv() => Event::Interrupted,
    }
}

#[cfg(not(unix))]
async fn signal(_: Signals) -> Event {
    let _ = tokio::signal::ctrl_c().await;
    Event::Interrupted
}

/// Go `RunWithBaseURL`: runs the TUI against `base_url` on the alternate screen of
/// `out` until the user quits. `hook` is the standalone mode's log source (Go's
/// `LogHook`); without it the TUI asks for the management password. Call from a
/// blocking thread inside a tokio runtime.
pub fn run<W: Write + Send + 'static>(
    base_url: &str,
    secret: &str,
    hook: Option<Arc<LogHook>>,
    out: W,
) -> io::Result<()> {
    let runtime = tokio::runtime::Handle::current();
    let mut app = app::App::new(base_url, secret, hook);
    // Unit outside Unix, where Ctrl+C needs no handler set up in advance.
    #[cfg_attr(not(unix), allow(clippy::let_unit_value))]
    let handlers = {
        let _context = runtime.enter();
        signals()?
    };
    let mut screen = Screen::enter(out)?;
    let (tx, rx) = mpsc::channel::<Event>();
    let signals = {
        let tx = tx.clone();
        runtime.spawn(async move {
            let _ = tx.send(signal(handlers).await);
        })
    };
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (tx, stop) = (tx.clone(), stop.clone());
        Some(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(100)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(Event::Term(ev)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        }))
    };
    let workers = Workers { signals, stop, reader };
    let spawn = |cmds: Vec<Cmd>| {
        for c in cmds {
            let tx = tx.clone();
            runtime.spawn(async move {
                let msg = c.await;
                if !matches!(msg, Msg::None) {
                    let _ = tx.send(Event::Msg(msg));
                }
            });
        }
    };
    let result = (|| -> io::Result<()> {
        let size = screen.terminal.size()?;
        app.resize(size.width, size.height);
        spawn(app.init());
        loop {
            screen.terminal.draw(|frame| app.draw(frame))?;
            let mut next = rx.recv().map_err(io::Error::other)?;
            loop {
                let step = match next {
                    Event::Term(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => match key_name(&key) {
                        Some(name) => app.key(&name),
                        None => app::Step::default(),
                    },
                    Event::Term(TermEvent::Paste(text)) => app.paste(&text),
                    Event::Term(TermEvent::Resize(w, h)) => {
                        app.resize(w, h);
                        app::Step::default()
                    }
                    Event::Term(_) => app::Step::default(),
                    Event::Msg(msg) => app.message(msg),
                    Event::Quit => return Ok(()),
                    Event::Interrupted => return Err(io::Error::other("program was interrupted")),
                };
                spawn(step.cmds);
                if step.quit {
                    return Ok(());
                }
                // Apply whatever else is queued before drawing again.
                match rx.try_recv() {
                    Ok(ev) => next = ev,
                    Err(_) => break,
                }
            }
        }
    })();
    drop(workers);
    drop(screen);
    result
}

/// Go's standalone readiness check: up to 30 `GET /config` tries, 100 ms apart and
/// growing by half up to one second.
pub async fn wait_ready(base_url: &str, secret: &str) -> bool {
    let client = Client::new(base_url, secret);
    let mut backoff = Duration::from_millis(100);
    for _ in 0..30 {
        if client.get_config().await.is_ok() {
            return true;
        }
        tokio::time::sleep(backoff).await;
        if backoff < Duration::from_secs(1) {
            backoff = backoff.mul_f64(1.5);
        }
    }
    false
}
