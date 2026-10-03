//! One config subscriber lifetime (Go `RunConfigSubscriberLifetime`): GET the
//! authoritative config, SUBSCRIBE with the membership arguments, check the ACK, rebuild
//! the command pool and probe it, then receive config and cluster updates until the
//! heartbeat is lost. Reconnection belongs to the caller, which starts each retry on a
//! fresh [`Client::new_lifetime`].

use tokio_util::sync::CancellationToken;

use crate::client::{CHANNEL_CLUSTER, CHANNEL_CONFIG, Client, Recovery};
use crate::error::{Error, Result};
use crate::resp::{Conn, Value};

/// A pub/sub reply, as go-redis decodes it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Subscription { kind: String, channel: String, count: i64 },
    Message { channel: String, payload: Vec<u8> },
    Pong,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Bulk(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::Simple(text) => Some(text.clone()),
        _ => None,
    }
}

/// go-redis `PubSub.newMessage`.
pub(crate) fn parse_event(reply: Value) -> Result<Event> {
    let items = match reply {
        Value::Array(items) => items,
        Value::Error(message) => return Err(Error::Server(message)),
        other => {
            return Err(Error::Transport(format!(
                "redis: unsupported pubsub message payload: {other:?}"
            )));
        }
    };
    let kind = items.first().and_then(text).unwrap_or_default();
    let field = |i: usize| items.get(i).and_then(text).unwrap_or_default();
    match kind.as_str() {
        "subscribe" | "unsubscribe" | "psubscribe" | "punsubscribe" | "ssubscribe" | "sunsubscribe" => {
            let count = match items.get(2) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            Ok(Event::Subscription {
                channel: field(1),
                kind,
                count,
            })
        }
        "message" | "smessage" if items.len() == 3 => Ok(Event::Message {
            channel: field(1),
            payload: match &items[2] {
                Value::Bulk(bytes) => bytes.clone(),
                other => text(other).unwrap_or_default().into_bytes(),
            },
        }),
        "pmessage" if items.len() == 4 => Ok(Event::Message {
            channel: field(2),
            payload: match &items[3] {
                Value::Bulk(bytes) => bytes.clone(),
                other => text(other).unwrap_or_default().into_bytes(),
            },
        }),
        "pong" => Ok(Event::Pong),
        _ => Err(Error::Transport(format!("redis: unsupported pubsub message: {kind:?}"))),
    }
}

/// A trimmed payload, as Go's `strings.TrimSpace` sees it.
fn trim_ascii_space(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}

impl Client {
    async fn receive(
        &self,
        conn: &mut Conn,
        closed: &CancellationToken,
        timeout: std::time::Duration,
    ) -> Result<Event> {
        tokio::select! {
            _ = closed.cancelled() => Err(Error::Transport("redis: client is closed".into())),
            reply = tokio::time::timeout(timeout, conn.recv()) => match reply {
                Ok(reply) => parse_event(reply.map_err(Error::from)?),
                Err(_) => Err(Error::Timeout),
            },
        }
    }

    fn end_lifetime(&self, error: Error) -> Result<()> {
        self.set_heartbeat(false);
        if !self.managed() {
            self.close();
        }
        Err(error)
    }

    /// Runs until the subscription fails or `shutdown` fires. `on_config` receives every
    /// config payload (the initial GET and each published update); an error from the
    /// initial apply ends the lifetime, later errors are logged and ignored. `on_ready`
    /// runs once the subscription is acknowledged and the command pool is live.
    pub async fn run_config_subscriber_lifetime(
        &self,
        shutdown: &CancellationToken,
        mut on_config: impl FnMut(&[u8]) -> std::result::Result<(), String>,
        on_ready: impl FnOnce(),
    ) -> Result<()> {
        if !self.enabled() {
            return Err(Error::Disabled);
        }
        if shutdown.is_cancelled() {
            return self.end_lifetime(Error::Cancelled);
        }
        let live = || !shutdown.is_cancelled();
        self.close_bootstrap_pools();
        if let Err(error) = self.ensure_pools() {
            if live() {
                self.mark_reconnect_failure("connect");
            }
            return self.end_lifetime(error);
        }

        let raw = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            raw = self.get_config() => raw,
        };
        let raw = match raw {
            Ok(raw) => raw,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("config fetch");
                }
                return self.end_lifetime(error);
            }
        };
        if let Err(message) = on_config(&raw) {
            return self.end_lifetime(Error::Other(message));
        }

        let pool = match self.subscription_pool() {
            Ok(pool) => pool,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("subscribe client");
                }
                return self.end_lifetime(error);
            }
        };
        let (args, receive_timeout) = self.subscription_parameters();
        let subscribed = async {
            let mut conn = pool.checkout().await?;
            let mut command = vec!["subscribe".to_owned()];
            command.extend(args.iter().cloned());
            tokio::time::timeout(self.op_timeout(), conn.send(&command))
                .await
                .map_err(|_| Error::Timeout)??;
            // Home acknowledges only the config channel; the rest are membership args.
            match self.receive(&mut conn, pool.closed(), receive_timeout).await? {
                Event::Subscription { kind, channel, count }
                    if kind == "subscribe" && channel == args[0] && count == 1 => {}
                _ => return Err(Error::Other("invalid Home subscription ACK".into())),
            }
            Ok::<_, Error>(conn)
        };
        let conn = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            conn = subscribed => conn,
        };
        let mut conn = match conn {
            Ok(conn) => conn,
            Err(error) => {
                if live() {
                    self.mark_reconnect_failure("subscribe");
                }
                return self.end_lifetime(error);
            }
        };
        // A protocol-one ACK means Home committed this membership already.
        if args.len() > 1 {
            self.mark_takeover_eligible();
        }

        self.promote_subscription();
        let probe = tokio::select! {
            _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
            probe = self.ping() => probe,
        };
        if let Err(error) = probe {
            if live() {
                self.mark_reconnect_failure("command probe");
            }
            return self.end_lifetime(error);
        }
        self.set_recovery(Recovery::Stable);
        self.reset_reconnect_failures();
        self.set_heartbeat(true);
        on_ready();

        loop {
            let (_, receive_timeout) = self.subscription_parameters();
            let event = tokio::select! {
                _ = shutdown.cancelled() => return self.end_lifetime(Error::Cancelled),
                event = self.receive(&mut conn, pool.closed(), receive_timeout) => event,
            };
            let event = match event {
                Ok(event) => event,
                Err(error) => {
                    if live() {
                        if self.heartbeat_ok() {
                            self.mark_takeover_eligible();
                        }
                        if error.is_timeout() {
                            self.mark_subscription_timeout();
                        } else {
                            self.mark_reconnect_failure("subscription");
                        }
                    }
                    return self.end_lifetime(error);
                }
            };
            match event {
                Event::Message { channel, payload } => {
                    let payload = trim_ascii_space(&payload);
                    if payload.is_empty() {
                        continue;
                    }
                    let applied = match channel.trim().to_lowercase().as_str() {
                        CHANNEL_CONFIG => on_config(payload).is_ok(),
                        CHANNEL_CLUSTER => self.update_cluster_nodes(payload).is_ok(),
                        _ => true,
                    };
                    if !applied {
                        let kind = if channel.trim().eq_ignore_ascii_case(CHANNEL_CLUSTER) {
                            "cluster"
                        } else {
                            "config"
                        };
                        tracing::warn!("failed to apply {kind} update from home control center, ignoring");
                    }
                }
                Event::Pong => self.reset_reconnect_failures(),
                Event::Subscription { .. } => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::Bulk(s.as_bytes().to_vec())
    }

    #[test]
    fn decodes_go_redis_pubsub_shapes() {
        assert_eq!(
            parse_event(Value::Array(vec![bulk("subscribe"), bulk("config"), Value::Int(1)])).unwrap(),
            Event::Subscription {
                kind: "subscribe".into(),
                channel: "config".into(),
                count: 1
            }
        );
        assert_eq!(
            parse_event(Value::Array(vec![bulk("message"), bulk("cluster"), bulk("{}")])).unwrap(),
            Event::Message {
                channel: "cluster".into(),
                payload: b"{}".to_vec()
            }
        );
        assert_eq!(
            parse_event(Value::Array(vec![bulk("pong"), bulk("")])).unwrap(),
            Event::Pong
        );
        assert_eq!(
            parse_event(Value::Error(
                "ERR wrong number of arguments for 'subscribe' command".into()
            ))
            .unwrap_err(),
            Error::Server("ERR wrong number of arguments for 'subscribe' command".into())
        );
        assert!(parse_event(Value::Simple("OK".into())).is_err());
        assert!(parse_event(Value::Array(vec![bulk("message"), bulk("only-two")])).is_err());
    }
}
