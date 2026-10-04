//! The optional WebRTC media relay seam (live.go `mediaRelayFactory`,
//! `mediaRelaySession`, `Handler.UpdateConfig`): with `codex.live-media-relay.enabled`,
//! the proxy terminates the client's WebRTC session and opens its own to the upstream, so
//! media flows through this host instead of straight to OpenAI.
//!
//! The relay itself (`media`, feature `media-relay`) is behind this interface so the call
//! handler and the call store do not depend on a WebRTC stack; default builds have none.

use std::sync::{Arc, Mutex, PoisonError};

use cpa_core::config::Config;
use futures_util::future::BoxFuture;
use serde_yaml_ng::Value;

/// A relay failure: Go's `clienterror.HTTPStatusFromErrorOr(err, 502)` and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelayError {
    pub status: u16,
    pub message: String,
}

#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
impl RelayError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
        }
    }
}

/// Where the upstream half of a media session goes (`mediaSessionRoute`).
#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct Route {
    /// The selected credential's effective proxy setting as written (Go `proxyURLForAuth`);
    /// the relay parses it and reports Go's message when it is invalid.
    pub proxy_url: String,
    /// For logs only: label, file name or auth index (`mediaCredentialName`).
    pub credential: String,
    pub auth_index: String,
}

/// One relayed call (`mediaRelaySession`).
pub(crate) trait MediaSession: Send + Sync {
    /// Applies the upstream's SDP answer and returns the answer for the client.
    fn accept_upstream_answer(&self, answer: String) -> BoxFuture<'_, Result<String, RelayError>>;
    fn set_call_id(&self, call_id: &str);
    /// Called once when the media fails or closes on its own; at once if it already did.
    fn set_close_handler(&self, handler: CloseHandler);
    /// Idempotent.
    fn close(&self, reason: &str);
    /// Runs `then` once a started close finished (Go's `CloseWithReason` returns after
    /// the peers closed); at once when the session has nothing left to close.
    fn after_close(&self, then: Box<dyn FnOnce() + Send>) {
        then();
    }
}

/// The relay for new calls: `None` when disabled, `Err` with Go's message when it could
/// not be built.
pub(crate) type CurrentRelay = Result<Option<Arc<dyn MediaRelay>>, String>;

/// Runs once when a media session ends on its own, with the reason.
pub(crate) type CloseHandler = Box<dyn FnOnce(String) + Send>;

/// A started session and the offer to send upstream instead of the client's.
pub(crate) type NewSession = Result<(Arc<dyn MediaSession>, String), RelayError>;

/// What a session's setup keeps until it no longer needs closing (the caller's credential
/// lease): dropped once setup started the session, or once a session whose setup failed
/// or was cancelled finished closing.
pub(crate) type Hold = Box<dyn Send>;

/// Builds media sessions (`mediaRelayFactory`).
pub(crate) trait MediaRelay: Send + Sync {
    /// Starts a session from the client's offer and returns it with the offer to send
    /// upstream instead.
    fn new_session(&self, offer: String, route: Route, hold: Hold) -> BoxFuture<'_, NewSession>;
}

/// `config.CodexLiveMediaRelayConfig`, as `oauth.providers.codex.live-media-relay` holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelayConfig {
    pub enabled: bool,
    pub max_sessions: i64,
    pub disable_private_remote_ips: bool,
    pub public_ip: String,
    pub udp_port_min: u16,
    pub udp_port_max: u16,
    pub ice_servers: Vec<IceServer>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IceServer {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
}

#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
impl RelayConfig {
    /// `EffectiveMaxSessions`: 32 unless set.
    pub fn max_sessions(&self) -> usize {
        if self.max_sessions > 0 {
            self.max_sessions as usize
        } else {
            32
        }
    }

    /// Reads the section; the legacy `allow-private-remote-ips` is the inverse of
    /// `disable-private-remote-ips`, and setting both is Go's config error.
    pub fn from_config(cfg: &Config) -> Result<Self, String> {
        let section = ["oauth", "providers", "codex", "live-media-relay"]
            .iter()
            .try_fold(&cfg.document, |node, key| node.get(*key));
        let Some(section) = section.filter(|s| !s.is_null()) else {
            return Ok(Self::default());
        };
        let get = |key: &str| section.get(key).filter(|v| !v.is_null());
        let boolean = |key: &str| get(key).and_then(Value::as_bool);
        let text = |v: &Value| match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => String::new(),
        };
        let port = |key: &str| {
            get(key)
                .and_then(Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
                .unwrap_or(0)
        };
        let disable_private_remote_ips = match (
            boolean("allow-private-remote-ips"),
            boolean("disable-private-remote-ips"),
        ) {
            (Some(_), Some(_)) => {
                return Err(
                    "codex.live-media-relay cannot set both allow-private-remote-ips and disable-private-remote-ips"
                        .into(),
                );
            }
            (Some(allow), None) => !allow,
            (None, disable) => disable.unwrap_or(false),
        };
        let ice_servers = get("ice-servers")
            .and_then(Value::as_sequence)
            .map(|servers| {
                servers
                    .iter()
                    .map(|s| IceServer {
                        urls: s
                            .get("urls")
                            .and_then(Value::as_sequence)
                            .map(|u| u.iter().map(text).collect())
                            .unwrap_or_default(),
                        username: s.get("username").map(text).unwrap_or_default(),
                        credential: s.get("credential").map(text).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            enabled: boolean("enabled").unwrap_or(false),
            max_sessions: get("max-sessions").and_then(Value::as_i64).unwrap_or(0),
            disable_private_remote_ips,
            public_ip: get("public-ip").map(text).unwrap_or_default(),
            udp_port_min: port("udp-port-min"),
            udp_port_max: port("udp-port-max"),
            ice_servers,
        })
    }
}

/// Media sessions in flight, shared by every relay built over the process lifetime so a
/// reload cannot exceed the limit (`mediaSessionLimiter`).
#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
#[derive(Default)]
pub(crate) struct Limiter(Mutex<(usize, usize)>);

#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
impl Limiter {
    pub fn set_limit(&self, limit: usize) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).0 = limit;
    }

    /// Takes a slot; the returned guard gives it back.
    pub fn acquire(self: &Arc<Self>) -> Option<Slot> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 == 0 || state.1 >= state.0 {
            return None;
        }
        state.1 += 1;
        Some(Slot(self.clone()))
    }
}

#[cfg_attr(not(feature = "media-relay"), allow(dead_code))]
pub(crate) struct Slot(Arc<Limiter>);

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.1 = state.1.saturating_sub(1);
    }
}

/// The relay for the current config, rebuilt only when its section changes
/// (`Handler.UpdateConfig`); sessions keep the relay they started with.
#[derive(Default)]
pub(crate) struct Relays {
    state: Mutex<Option<Current>>,
    limiter: Arc<Limiter>,
    /// Test hook: always this relay.
    #[cfg(test)]
    pub fixed: Option<Arc<dyn MediaRelay>>,
}

#[derive(Clone)]
struct Current {
    config: Result<RelayConfig, String>,
    relay: CurrentRelay,
}

impl Relays {
    /// The relay to use for a new call: `None` when disabled, `Err` with Go's message when
    /// it could not be built (every call then answers 503).
    #[cfg(test)]
    pub fn current(&self, cfg: &Config) -> CurrentRelay {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.current_locked(state, cfg)
    }

    /// The config snapshot and its relay, read together under the relay lock
    /// (`Handler.currentRuntime`): a request holding an older snapshot can no longer
    /// rebuild an older relay over a newer one.
    pub fn snapshot(&self, config: impl FnOnce() -> Arc<Config>) -> (Arc<Config>, CurrentRelay) {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let cfg = config();
        let relay = self.current_locked(state, &cfg);
        (cfg, relay)
    }

    fn current_locked(&self, mut state: std::sync::MutexGuard<'_, Option<Current>>, cfg: &Config) -> CurrentRelay {
        #[cfg(test)]
        if let Some(fixed) = &self.fixed {
            return Ok(Some(fixed.clone()));
        }
        let config = RelayConfig::from_config(cfg);
        if let Some(current) = state.as_ref()
            && current.config == config
        {
            return current.relay.clone();
        }
        let relay = config.clone().and_then(|c| self.build(&c));
        let reloaded = state.is_some();
        if relay.is_ok() && (reloaded || config.as_ref().is_ok_and(|c| c.enabled)) {
            tracing::info!(
                reloaded,
                "codex live media relay configured; changes apply to new sessions"
            );
        }
        *state = Some(Current {
            config,
            relay: relay.clone(),
        });
        relay
    }

    #[cfg(feature = "media-relay")]
    fn build(&self, config: &RelayConfig) -> CurrentRelay {
        if !config.enabled {
            return Ok(None);
        }
        self.limiter.set_limit(config.max_sessions());
        super::media::Relay::new(config, self.limiter.clone()).map(|r| Some(Arc::new(r) as Arc<dyn MediaRelay>))
    }

    /// Default builds carry no WebRTC stack: an enabled relay is reported once per
    /// config and calls negotiate end to end with the upstream media servers.
    #[cfg(not(feature = "media-relay"))]
    fn build(&self, config: &RelayConfig) -> CurrentRelay {
        if config.enabled {
            tracing::warn!(
                "codex.live-media-relay is enabled, but this build lacks the media relay (cargo feature `media-relay`); Codex Live calls negotiate without it"
            );
        }
        let _ = &self.limiter;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::parse(yaml).unwrap()
    }

    #[test]
    fn config_reads_section_and_legacy_private_ip_setting() {
        let c = RelayConfig::from_config(&cfg(
            "codex:\n  live-media-relay:\n    enabled: true\n    max-sessions: 2\n    public-ip: 203.0.113.9\n    udp-port-min: 40000\n    udp-port-max: 40010\n    allow-private-remote-ips: false\n    ice-servers:\n      - urls: [\"turn:turn.example:3478\"]\n        username: u\n        credential: c\n",
        ))
        .unwrap();
        assert!(c.enabled && c.disable_private_remote_ips);
        assert_eq!((c.max_sessions(), c.udp_port_min, c.udp_port_max), (2, 40000, 40010));
        assert_eq!(c.public_ip, "203.0.113.9");
        assert_eq!(c.ice_servers[0].urls, ["turn:turn.example:3478"]);
        assert_eq!(RelayConfig::from_config(&cfg("{}")).unwrap(), RelayConfig::default());
        assert_eq!(RelayConfig::default().max_sessions(), 32);
        assert!(
            RelayConfig::from_config(&cfg(
                "codex:\n  live-media-relay:\n    allow-private-remote-ips: true\n    disable-private-remote-ips: true\n"
            ))
            .is_err()
        );
    }

    /// Go `TestHandlerUpdatesMediaRelayConfig`: an unrelated config change keeps the relay,
    /// a relay change rebuilds it, disabling removes it.
    #[cfg(feature = "media-relay")]
    #[test]
    fn relay_is_rebuilt_only_when_its_section_changes() {
        let relays = Relays::default();
        let enabled = "codex:\n  live-media-relay:\n    enabled: true\n    max-sessions: 1\n";
        let first = relays.current(&cfg(enabled)).unwrap().expect("enabled");
        let unrelated = format!("{enabled}debug: true\nproxy-url: http://new-proxy.example\n");
        let same = relays.current(&cfg(&unrelated)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&first, &same), "unrelated change kept the relay");
        let changed = relays
            .current(&cfg(
                "codex:\n  live-media-relay:\n    enabled: true\n    max-sessions: 2\n",
            ))
            .unwrap()
            .unwrap();
        assert!(!Arc::ptr_eq(&first, &changed), "relay change rebuilt it");
        assert!(relays.current(&cfg("{}")).unwrap().is_none(), "disabled");
    }

    /// Without the feature an enabled relay is not an error: calls negotiate without it.
    #[cfg(not(feature = "media-relay"))]
    #[test]
    fn builds_without_the_relay_negotiate_without_it() {
        let relays = Relays::default();
        let enabled = cfg("codex:\n  live-media-relay:\n    enabled: true\n");
        assert!(relays.current(&enabled).unwrap().is_none());
    }

    #[test]
    fn limiter_counts_slots_across_rebuilds() {
        let limiter = Arc::new(Limiter::default());
        assert!(limiter.acquire().is_none(), "no limit set yet");
        limiter.set_limit(1);
        let slot = limiter.acquire().unwrap();
        assert!(limiter.acquire().is_none());
        limiter.set_limit(2);
        let second = limiter.acquire().unwrap();
        drop(slot);
        drop(second);
        assert!(limiter.acquire().is_some());
    }
}
