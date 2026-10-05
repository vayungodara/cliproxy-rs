//! The server's LAN advertisement (Go `discoveryAdvertiserManager`): applied at start,
//! on every published config and every 15 s while enabled, stopped with a goodbye on
//! shutdown. One task owns the state, so Go's generation fencing is not needed. While
//! discovery is off the task sleeps until the next config publish and sets no timer.
use std::sync::Arc;
use std::time::Duration;

use cpa_core::config::Config;
use tokio::sync::oneshot;

use super::mdns::Responder;
use super::{DiscoveryConfig, ServiceSpec, build_service_spec};

const REFRESH: Duration = Duration::from_secs(15);

#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return String::new();
    }
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

#[cfg(not(unix))]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

/// Go `extractCleanHost`: the host name without a `.local` suffix.
fn clean_host() -> String {
    let raw = hostname();
    let h = super::go_trim(&raw);
    let h = h.strip_suffix('.').unwrap_or(h);
    let h = h.strip_suffix(".local").unwrap_or(h);
    let h = h.strip_suffix('.').unwrap_or(h);
    if h.is_empty() { "localhost".into() } else { h.to_owned() }
}

/// Go `ZeroconfAdvertiser.Start` checks before `RegisterProxy`.
fn start(spec: &ServiceSpec) -> Result<Responder, String> {
    if spec.interfaces.is_empty() {
        return Err(
            "discovery: cannot start advertiser with empty interface list (refusing fallback to all interfaces)".into(),
        );
    }
    if spec.port == 0 {
        return Err(format!(
            "discovery: invalid service port {} (must be between 1 and 65535)",
            spec.port
        ));
    }
    let mut spec = spec.clone();
    if spec.advertised_ips.is_empty() {
        spec.advertised_ips = super::iface::usable_ips(&spec.interfaces);
    }
    if spec.advertised_ips.is_empty() {
        return Err("discovery: no usable IP addresses found on specified interfaces".into());
    }
    spec.instance_name = super::sanitize_instance_name(&spec.instance_name);
    if spec.instance_name.is_empty() {
        spec.instance_name = "CPA-0001".into();
    }
    let primary = std::iter::once(spec.service_type.clone())
        .chain(
            spec.subtypes
                .iter()
                .map(|s| super::sanitize_subtype(s))
                .filter(|s| !s.is_empty()),
        )
        .collect::<Vec<_>>()
        .join(",");
    Responder::register(&spec, &clean_host())
        .map_err(|e| format!("discovery: failed to register primary service {primary}: {e}"))
}

#[derive(Default)]
struct State {
    /// Whether the listener actually serves TLS; the TXT `tls` record and the scheme
    /// clients build follow the real transport, not `server.tls.enable` alone.
    tls: bool,
    running: Option<(Responder, ServiceSpec)>,
    /// Host, port and TLS of the first apply; the listener never changes on reload.
    bound: Option<(String, u16, bool)>,
    last_error: Option<String>,
}

impl State {
    async fn stop(&mut self, why: Option<&str>) {
        if let Some((responder, _)) = self.running.take() {
            if let Some(why) = why {
                tracing::info!("{why}");
            }
            responder.shutdown().await;
        }
    }

    /// Go `applyContext`; returns whether refreshing should continue.
    async fn apply(&mut self, cfg: &Config, changed: bool) -> bool {
        let tls = self.tls;
        let (host, port, tls) = self.bound.get_or_insert((cfg.host.clone(), cfg.port, tls)).clone();
        if changed {
            self.last_error = None;
        }
        let d = DiscoveryConfig::from_document(&cfg.document);
        if !d.enabled {
            self.stop(Some("discovery: stopping mDNS advertisement (disabled by config)"))
                .await;
            self.last_error = None;
            return false;
        }
        let spec = match build_service_spec(
            &d,
            &host,
            port,
            tls,
            || super::instance_id(super::state_dir().as_deref()),
            super::iface::filter,
        ) {
            Ok(spec) => spec,
            Err(e) => {
                self.stop(Some(
                    "discovery: stopping stale mDNS advertisement after spec build failure",
                ))
                .await;
                if self.last_error.as_ref() != Some(&e) {
                    tracing::warn!("discovery: failed to build service spec: {e}");
                }
                self.last_error = Some(e);
                return true;
            }
        };
        self.last_error = None;
        if self.running.as_ref().is_some_and(|(_, running)| *running == spec) {
            return true;
        }
        self.stop(None).await;
        match start(&spec) {
            Ok(responder) => {
                tracing::info!(
                    "discovery: advertising as '{}.{}' on port {port}",
                    spec.instance_name,
                    spec.service_type
                );
                self.running = Some((responder, spec));
            }
            Err(e) => tracing::warn!("discovery: failed to start mDNS advertiser: {e} (degraded, HTTP intact)"),
        }
        true
    }
}

/// The advertisement task; drop or [`Advertiser::shutdown`] to stop it.
pub struct Advertiser {
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Advertiser {
    /// Starts advertising for whatever `config` publishes; `tls` says whether the
    /// listener serves TLS (Go passes `cfg.TLS.Enable`, which its listener honours).
    /// `published` resolves after each config publish.
    pub fn spawn(
        config: impl Fn() -> Arc<Config> + Send + 'static,
        mut published: tokio::sync::watch::Receiver<()>,
        tls: bool,
    ) -> Self {
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut state = State {
                tls,
                ..State::default()
            };
            published.borrow_and_update();
            let mut current = config();
            let mut refreshing = state.apply(&current, true).await;
            let mut next_refresh = tokio::time::Instant::now() + REFRESH;
            loop {
                let (on, at) = (refreshing, next_refresh);
                let refresh = async move {
                    if on {
                        tokio::time::sleep_until(at).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                tokio::select! {
                    _ = &mut stopped => break,
                    changed = published.changed() => {
                        if changed.is_err() {
                            // The runtime is gone; wait for shutdown without spinning.
                            let _ = (&mut stopped).await;
                            break;
                        }
                        published.borrow_and_update();
                        let latest = config();
                        if Arc::ptr_eq(&latest, &current) {
                            continue;
                        }
                        current = latest;
                        refreshing = state.apply(&current, true).await;
                    }
                    () = refresh => refreshing = state.apply(&current, false).await,
                }
                next_refresh = tokio::time::Instant::now() + REFRESH;
            }
            state.stop(None).await;
        });
        Self {
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// Sends the goodbye packets and waits for the task (Go `shutdownDiscovery`).
    pub async fn shutdown(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
