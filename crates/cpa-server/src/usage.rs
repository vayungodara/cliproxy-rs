//! Go `internal/redisqueue`: the in-memory usage queue that management's
//! `GET /observability/usage/queue` drains. The request path enqueues one JSON record
//! per upstream request (Go `queuedUsageDetail`); records older than the retention
//! window are pruned on every access.
//!
//! ponytail: no RESP subscribers (Go publishes to Redis-protocol clients instead of
//! queueing when any are connected) and no error queue; cliproxy-rs has no RESP
//! listener.

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
}

impl Default for UsageQueue {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            // Go's package default; startup immediately applies the config value.
            statistics: AtomicBool::new(true),
            retention_seconds: AtomicI64::new(DEFAULT_RETENTION_SECONDS),
            items: Mutex::default(),
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
        let now = Instant::now();
        let mut items = self.lock();
        self.prune(&mut items, now);
        items.push_back((now, payload));
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
}
