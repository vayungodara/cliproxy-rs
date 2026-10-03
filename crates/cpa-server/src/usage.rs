//! Go `internal/redisqueue`: the in-memory usage queue that management's
//! `GET /observability/usage/queue` drains. The request path enqueues one JSON record
//! per upstream request (Go `queuedUsageDetail`); records older than the retention
//! window are pruned on every access.
//!
//! RESP clients on the main listener (crate::resp) subscribe to the `usage` and
//! `errors` channels; while any usage subscriber is connected, records go to them
//! instead of the queue, as in Go.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use cpa_core::config::Config;

const DEFAULT_RETENTION_SECONDS: i64 = 60;
const MAX_RETENTION_SECONDS: i64 = 3600;

pub struct UsageQueue {
    /// Go `redisqueue.Enabled`: management routes are enabled.
    enabled: AtomicBool,
    /// Go `UsageStatisticsEnabled`: `observability.usage.usage-statistics-enabled`.
    statistics: AtomicBool,
    retention_seconds: AtomicI64,
    items: Mutex<VecDeque<(Instant, Vec<u8>)>>,
    usage_subscribers: std::sync::Arc<Subscribers>,
    error_subscribers: std::sync::Arc<Subscribers>,
}

/// Go `usageSubscriberBuffer` / `errorSubscriberBuffer`.
const SUBSCRIBER_BUFFER: usize = 256;
/// Go `usageSupportRefreshPayload`: the first message every usage subscriber gets.
const SUPPORT_REFRESH: &[u8] = br#"{"support_refresh":true}"#;
/// Go `usageRefreshPayload`.
const REFRESH: &[u8] = br#"{"refresh":true}"#;

/// The last subscription ID and the live subscribers by ID.
type SubscriberMap = (u64, std::collections::HashMap<u64, tokio::sync::mpsc::Sender<Vec<u8>>>);

/// Go `queue.subscribers`: bounded channels keyed by subscription.
#[derive(Default)]
struct Subscribers(Mutex<SubscriberMap>);

impl Subscribers {
    fn lock(&self) -> std::sync::MutexGuard<'_, SubscriberMap> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `publishToSubscribers`: whether anyone listened. A subscriber whose buffer
    /// is full is dropped, which ends its subscription.
    fn publish(&self, payload: &[u8]) -> bool {
        let mut guard = self.lock();
        if guard.1.is_empty() {
            return false;
        }
        guard.1.retain(|_, tx| tx.try_send(payload.to_vec()).is_ok());
        true
    }

    fn subscribe(self: &std::sync::Arc<Self>, greeting: Option<&[u8]>) -> Subscription {
        let (tx, rx) = tokio::sync::mpsc::channel(SUBSCRIBER_BUFFER);
        if let Some(greeting) = greeting {
            let _ = tx.try_send(greeting.to_vec());
        }
        let mut guard = self.lock();
        guard.0 += 1;
        let id = guard.0;
        guard.1.insert(id, tx);
        Subscription {
            messages: rx,
            owner: std::sync::Arc::downgrade(self),
            id,
        }
    }

    fn clear(&self) {
        self.lock().1.clear();
    }
}

/// One channel subscription; dropping it unsubscribes (Go's `unsubscribe`). `messages`
/// ends when the queue drops the subscriber (full buffer, or the queue was disabled).
pub struct Subscription {
    pub messages: tokio::sync::mpsc::Receiver<Vec<u8>>,
    owner: std::sync::Weak<Subscribers>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner.lock().1.remove(&self.id);
        }
    }
}

impl Default for UsageQueue {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            // Go's package default; startup immediately applies the config value.
            statistics: AtomicBool::new(true),
            retention_seconds: AtomicI64::new(DEFAULT_RETENTION_SECONDS),
            items: Mutex::default(),
            usage_subscribers: std::sync::Arc::default(),
            error_subscribers: std::sync::Arc::default(),
        }
    }
}

impl UsageQueue {
    /// Applies one published config (Go server_reload.go): disabling the queue drops
    /// everything queued; retention is clamped to 1..=3600 seconds.
    pub fn configure(&self, management_enabled: bool, cfg: &Config) {
        let usage = cfg.document.get("observability").and_then(|o| o.get("usage"));
        let statistics = usage
            .and_then(|u| u.get("usage-statistics-enabled"))
            .and_then(serde_yaml_ng::Value::as_bool)
            .unwrap_or(false);
        let retention = usage
            .and_then(|u| u.get("redis-usage-queue-retention-seconds"))
            .and_then(serde_yaml_ng::Value::as_i64)
            .unwrap_or(0);
        self.statistics.store(statistics, Ordering::SeqCst);
        self.retention_seconds.store(
            if retention <= 0 {
                DEFAULT_RETENTION_SECONDS
            } else {
                retention.min(MAX_RETENTION_SECONDS)
            },
            Ordering::SeqCst,
        );
        self.enabled.store(management_enabled, Ordering::SeqCst);
        if !management_enabled {
            self.lock().clear();
            self.usage_subscribers.clear();
            self.error_subscribers.clear();
        }
    }

    /// Whether a usage record would be kept; producers can skip building one.
    pub fn accepts(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) && self.statistics.load(Ordering::SeqCst)
    }

    /// Queues one serialized usage record (Go `usageQueuePlugin.HandleUsage` +
    /// `Enqueue`).
    pub fn enqueue(&self, payload: Vec<u8>) {
        if !self.accepts() || payload.is_empty() {
            return;
        }
        // Go `Enqueue`: connected RESP subscribers take the record instead of the queue.
        if self.usage_subscribers.publish(&payload) {
            return;
        }
        let now = Instant::now();
        let mut items = self.lock();
        self.prune(&mut items, now);
        items.push_back((now, payload));
    }

    /// Go `SubscribeUsage`: usage records as they are produced, after a first
    /// `{"support_refresh":true}`.
    pub fn subscribe_usage(&self) -> Subscription {
        self.usage_subscribers.subscribe(Some(SUPPORT_REFRESH))
    }

    /// Go `SubscribeErrors`.
    pub fn subscribe_errors(&self) -> Subscription {
        self.error_subscribers.subscribe(None)
    }

    /// Go `EnqueueError`: an error event for `errors` subscribers; nothing is queued.
    pub fn enqueue_error(&self, payload: &[u8]) {
        if self.enabled.load(Ordering::SeqCst) && !payload.is_empty() {
            self.error_subscribers.publish(payload);
        }
    }

    /// Whether an error event would reach anyone; producers can skip building one.
    pub fn wants_errors(&self) -> bool {
        self.enabled.load(Ordering::SeqCst) && !self.error_subscribers.lock().1.is_empty()
    }

    /// Go `NotifyUsageRefresh`: tells usage subscribers the credential set changed.
    pub fn notify_usage_refresh(&self) {
        self.usage_subscribers.publish(REFRESH);
    }

    /// Go `PopOldest`: removes and returns up to `count` unexpired records, oldest first.
    pub fn pop_oldest(&self, count: usize) -> Vec<Vec<u8>> {
        if !self.enabled.load(Ordering::SeqCst) || count == 0 {
            return Vec::new();
        }
        let mut items = self.lock();
        self.prune(&mut items, Instant::now());
        let n = count.min(items.len());
        items.drain(..n).map(|(_, payload)| payload).collect()
    }

    fn prune(&self, items: &mut VecDeque<(Instant, Vec<u8>)>, now: Instant) {
        let window = Duration::from_secs(self.retention_seconds.load(Ordering::SeqCst).max(1) as u64);
        while items
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > window)
        {
            items.pop_front();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<(Instant, Vec<u8>)>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::parse(yaml).unwrap()
    }

    #[test]
    fn queues_only_when_management_and_statistics_are_enabled() {
        let q = UsageQueue::default();
        let on = cfg("observability: {usage: {usage-statistics-enabled: true}}\n");
        q.enqueue(b"{}".to_vec());
        assert!(q.pop_oldest(5).is_empty(), "management disabled");
        q.configure(true, &cfg("{}\n"));
        q.enqueue(b"{}".to_vec());
        assert!(q.pop_oldest(5).is_empty(), "statistics default off");
        q.configure(true, &on);
        for i in 0..3 {
            q.enqueue(format!("{{\"n\":{i}}}").into_bytes());
        }
        assert_eq!(q.pop_oldest(2), [b"{\"n\":0}".to_vec(), b"{\"n\":1}".to_vec()]);
        q.configure(false, &on);
        q.configure(true, &on);
        assert!(q.pop_oldest(5).is_empty(), "disabling clears the queue");
    }

    #[test]
    fn retention_prunes_expired_records() {
        let q = UsageQueue::default();
        q.configure(
            true,
            &cfg("observability: {usage: {usage-statistics-enabled: true, redis-usage-queue-retention-seconds: 1}}\n"),
        );
        q.lock()
            .push_back((Instant::now() - Duration::from_secs(5), b"old".to_vec()));
        q.enqueue(b"new".to_vec());
        assert_eq!(q.pop_oldest(5), [b"new".to_vec()]);
        q.configure(true, &cfg("observability: {usage: {usage-statistics-enabled: true, redis-usage-queue-retention-seconds: 99999}}\n"));
        assert_eq!(q.retention_seconds.load(Ordering::SeqCst), 3600);
    }

    /// Go redisqueue `Enqueue`, `publishToSubscribers`, `subscribe`, `clear` and
    /// `NotifyUsageRefresh`.
    #[test]
    fn subscribers_take_records_and_are_dropped_when_full_or_disabled() {
        let on = cfg("observability: {usage: {usage-statistics-enabled: true}}\n");
        let q = UsageQueue::default();
        q.configure(true, &on);
        let mut usage = q.subscribe_usage();
        assert_eq!(usage.messages.try_recv().unwrap(), SUPPORT_REFRESH);
        q.enqueue(b"r1".to_vec());
        assert_eq!(usage.messages.try_recv().unwrap(), b"r1");
        assert!(q.pop_oldest(5).is_empty(), "delivered, not queued");
        q.notify_usage_refresh();
        assert_eq!(usage.messages.try_recv().unwrap(), REFRESH);
        drop(usage);
        q.enqueue(b"r2".to_vec());
        assert_eq!(q.pop_oldest(5), [b"r2".to_vec()], "no subscriber: queued again");

        // A subscriber that stops reading is dropped once its buffer is full.
        let mut slow = q.subscribe_usage();
        for i in 0..SUBSCRIBER_BUFFER {
            q.enqueue(format!("{i}").into_bytes());
        }
        assert_eq!(q.usage_subscribers.lock().1.len(), 0, "dropped at the 256th record");
        let mut received = 0;
        while slow.messages.try_recv().is_ok() {
            received += 1;
        }
        assert_eq!(received, SUBSCRIBER_BUFFER, "the greeting and 255 records, then closed");

        // Errors reach only subscribers; disabling the queue ends every subscription.
        let mut errors = q.subscribe_errors();
        q.enqueue_error(b"e1");
        assert_eq!(errors.messages.try_recv().unwrap(), b"e1");
        q.configure(false, &on);
        assert!(matches!(
            errors.messages.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
