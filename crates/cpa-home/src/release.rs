//! Reliable credential concurrency release (Go internal/home/concurrency_release.go).
//!
//! Releases are cumulative per (credential, model): Home only needs the latest sequence,
//! so a burst of ends collapses into one frame. Frames go out every flush interval; a
//! failed round doubles the delay up to the max backoff. Each dirty mark returns a ticket
//! that completes when Home has acknowledged that sequence.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use crate::client::{Client, ReleaseFrame};
use crate::config::CredentialConcurrency;
use crate::error::Result;
use crate::registry::{ReleaseGroup, ReleaseSink, ReleaseTicket};

/// Where release frames go: one Home lifetime.
pub trait ReleaseSender: Send + Sync {
    fn send<'a>(&'a self, frame: &'a ReleaseFrame) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
    /// The current limiter settings (flush interval and backoff).
    fn limiter(&self) -> CredentialConcurrency;
}

impl ReleaseSender for Client {
    fn send<'a>(&'a self, frame: &'a ReleaseFrame) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(self.push_concurrency_release(frame))
    }

    fn limiter(&self) -> CredentialConcurrency {
        self.limiter_config()
    }
}

struct Group {
    latest: i64,
    acked: watch::Sender<i64>,
}

#[derive(Default)]
struct Shared {
    groups: Mutex<HashMap<ReleaseGroup, Group>>,
    sender: Mutex<Option<Arc<dyn ReleaseSender>>>,
    wake: Notify,
    force: Notify,
    /// The latest forced round's deadline (Go passes the `Flush` context).
    force_deadline: Mutex<Option<tokio::time::Instant>>,
    acked: Notify,
}

/// Go `releaseFlusher`. Cloning shares it.
#[derive(Clone, Default)]
pub struct ReleaseFlusher(Arc<Shared>);

impl ReleaseFlusher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Go `SetSender` (and `SetConfigProvider`): the lifetime used from now on.
    pub fn set_sender(&self, sender: Option<Arc<dyn ReleaseSender>>) {
        *self.0.sender.lock().unwrap_or_else(PoisonError::into_inner) = sender;
        self.0.wake.notify_one();
    }

    /// A registry release sink feeding this flusher.
    pub fn sink(&self) -> ReleaseSink {
        let flusher = self.clone();
        Arc::new(move |group, sequence| flusher.mark_dirty(group, sequence))
    }

    /// Go `MarkDirty`: records the latest sequence of `group`.
    pub fn mark_dirty(&self, group: ReleaseGroup, sequence: i64) -> Option<ReleaseTicket> {
        if sequence <= 0 || group.credential_id.is_empty() || group.model.is_empty() {
            return None;
        }
        let acked = {
            let mut groups = self.0.groups.lock().unwrap_or_else(PoisonError::into_inner);
            let state = groups.entry(group.clone()).or_insert_with(|| Group {
                latest: 0,
                acked: watch::channel(0).0,
            });
            state.latest = state.latest.max(sequence);
            state.acked.subscribe()
        };
        self.0.wake.notify_one();
        Some(ReleaseTicket::new(group, sequence, acked))
    }

    fn timings(&self) -> (Duration, Duration) {
        let sender = self.0.sender.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let cfg = sender.map(|s| s.limiter()).unwrap_or_default().with_defaults();
        let defaults = CredentialConcurrency::default().with_defaults();
        let mut flush = cfg.flush_interval();
        if flush.is_zero() {
            flush = defaults.flush_interval();
        }
        (flush, cfg.max_backoff().max(flush))
    }

    /// Go `nextDelay`.
    fn next_delay(&self, delay: Duration, failed: bool) -> (Duration, bool) {
        let (flush, max_backoff) = self.timings();
        if !failed {
            return (flush, false);
        }
        ((delay * 2).clamp(flush, max_backoff), true)
    }

    /// Sends every dirty group once; true when any send failed.
    async fn flush(&self) -> bool {
        let sender = self.0.sender.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let Some(sender) = sender else {
            return false;
        };
        let pending: Vec<(ReleaseGroup, i64)> = self
            .0
            .groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, g)| g.latest > *g.acked.borrow())
            .map(|(k, g)| (k.clone(), g.latest))
            .collect();
        let mut failed = false;
        for (group, sequence) in pending {
            let frame = ReleaseFrame {
                credential_id: group.credential_id.clone(),
                model: group.model.clone(),
                release_seq: sequence,
            };
            if sender.send(&frame).await.is_err() {
                failed = true;
                continue;
            }
            if let Some(state) = self.0.groups.lock().unwrap_or_else(PoisonError::into_inner).get(&group) {
                state.acked.send_if_modified(|acked| {
                    let raised = sequence > *acked;
                    if raised {
                        *acked = sequence;
                    }
                    raised
                });
            }
            self.0.acked.notify_waiters();
        }
        failed
    }

    fn idle(&self) -> bool {
        self.0
            .groups
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .all(|g| g.latest <= *g.acked.borrow())
    }

    /// Go `Run`: sends dirty groups until `shutdown`.
    pub async fn run(&self, shutdown: CancellationToken) {
        let (mut delay, _) = self.timings();
        let mut backing_off = false;
        let mut next = tokio::time::Instant::now();
        loop {
            // A round in progress stops at shutdown or at the forced deadline; groups
            // it did not get acknowledged stay dirty.
            let deadline = tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = self.0.wake.notified() => {
                    if !backing_off {
                        next = tokio::time::Instant::now();
                    }
                    continue;
                }
                _ = self.0.force.notified() => self.0.force_deadline.lock().unwrap_or_else(PoisonError::into_inner).take(),
                _ = tokio::time::sleep_until(next) => None,
            };
            let round = async {
                match deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, self.flush()).await.unwrap_or(true),
                    None => self.flush().await,
                }
            };
            let failed = tokio::select! {
                _ = shutdown.cancelled() => return,
                failed = round => failed,
            };
            (delay, backing_off) = self.next_delay(delay, failed);
            next = tokio::time::Instant::now() + delay;
        }
    }

    /// Go `Flush`: forces a round and waits until every dirty group is acknowledged.
    pub async fn flush_all(&self, bound: Duration) -> std::result::Result<(), crate::registry::RegistryError> {
        *self.0.force_deadline.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(tokio::time::Instant::now() + bound);
        self.0.force.notify_one();
        let wait = async {
            loop {
                let acked = self.0.acked.notified();
                tokio::pin!(acked);
                acked.as_mut().enable();
                if self.idle() {
                    return;
                }
                // Re-check periodically too: a round may finish between checks.
                tokio::select! {
                    _ = acked => {}
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            }
        };
        tokio::time::timeout(bound, wait)
            .await
            .map_err(|_| crate::registry::RegistryError::Timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Recorder {
        frames: Mutex<Vec<ReleaseFrame>>,
        fail: AtomicBool,
        cfg: CredentialConcurrency,
    }

    impl ReleaseSender for Recorder {
        fn send<'a>(&'a self, frame: &'a ReleaseFrame) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                self.frames.lock().unwrap().push(frame.clone());
                if self.fail.load(Ordering::SeqCst) {
                    Err(Error::Timeout)
                } else {
                    Ok(())
                }
            })
        }

        fn limiter(&self) -> CredentialConcurrency {
            self.cfg
        }
    }

    fn group(id: &str) -> ReleaseGroup {
        ReleaseGroup {
            credential_id: id.into(),
            model: "gpt".into(),
        }
    }

    /// Go `TestConcurrencyReleaseFrameFixture`.
    #[test]
    fn frame_matches_go_fixture() {
        // internal/home/testdata/concurrency_release.json
        let frame = ReleaseFrame {
            credential_id: "cred-1".into(),
            model: "gpt".into(),
            release_seq: 1,
        };
        let fixture = include_str!("../tests/fixtures/concurrency_release.json");
        assert_eq!(String::from_utf8(frame.to_json()).unwrap(), fixture.trim());
    }

    #[test]
    fn invalid_marks_have_no_ticket() {
        let flusher = ReleaseFlusher::new();
        assert!(flusher.mark_dirty(group("a"), 0).is_none());
        assert!(flusher.mark_dirty(group(""), 1).is_none());
    }

    /// Go `TestReleaseFlusherRetriesLatestCumulativeSequence` (the retry after a failure
    /// is `failures_back_off_exponentially_up_to_the_max`).
    #[tokio::test(start_paused = true)]
    async fn bursts_collapse_to_the_latest_sequence_and_tickets_complete() {
        let recorder = Arc::new(Recorder::default());
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(recorder.clone()));
        let first = flusher.mark_dirty(group("a"), 1).unwrap();
        let latest = flusher.mark_dirty(group("a"), 3).unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        latest.wait(Duration::from_secs(1)).await.unwrap();
        first.wait(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            *recorder.frames.lock().unwrap(),
            vec![ReleaseFrame {
                credential_id: "a".into(),
                model: "gpt".into(),
                release_seq: 3
            }]
        );
        // An already acknowledged sequence completes at once and sends nothing.
        flusher
            .mark_dirty(group("a"), 2)
            .unwrap()
            .wait(Duration::ZERO)
            .await
            .unwrap();
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestReleaseFlusherCoalescesDirtyWakesDuringFailureBackoff`.
    #[tokio::test(start_paused = true)]
    async fn failures_back_off_exponentially_up_to_the_max() {
        let recorder = Arc::new(Recorder::default());
        recorder.fail.store(true, Ordering::SeqCst);
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(recorder.clone()));
        let ticket = flusher.mark_dirty(group("a"), 1).unwrap();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        // Defaults: 250ms flush, 2s max. Attempts at 0, 500ms, 1.5s, 3.5s, 5.5s.
        let start = tokio::time::Instant::now();
        let mut times = Vec::new();
        for _ in 0..5 {
            while recorder.frames.lock().unwrap().len() == times.len() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            times.push(start.elapsed().as_millis() / 10 * 10);
        }
        assert_eq!(times, vec![0, 500, 1500, 3500, 5500]);
        // A new mark while backing off does not short-circuit the backoff.
        flusher.mark_dirty(group("b"), 1);
        recorder.fail.store(false, Ordering::SeqCst);
        ticket.wait(Duration::from_secs(3)).await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(7500));
        shutdown.cancel();
        task.await.unwrap();
    }

    /// A sender whose sends never complete.
    struct Stuck(CredentialConcurrency);

    impl ReleaseSender for Stuck {
        fn send<'a>(&'a self, _frame: &'a ReleaseFrame) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(std::future::pending())
        }

        fn limiter(&self) -> CredentialConcurrency {
            self.0
        }
    }

    /// Go `TestReleaseFlusherStopsWithLifetime`.
    #[tokio::test(start_paused = true)]
    async fn shutdown_interrupts_a_stuck_round_and_flush_all_honours_its_bound() {
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(Arc::new(Stuck(CredentialConcurrency::default()))));
        flusher.mark_dirty(group("a"), 1);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        // As in Go, a round already in progress ignores later force requests; the
        // caller's bound still holds and the sequence stays dirty.
        assert!(flusher.flush_all(Duration::from_millis(100)).await.is_err());
        assert!(!flusher.idle());
        tokio::time::sleep(Duration::from_millis(10)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_millis(50), task)
            .await
            .expect("run returns while a send is stuck")
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn flush_all_forces_a_round_and_waits_for_acks() {
        let recorder = Arc::new(Recorder::default());
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(recorder.clone()));
        flusher.mark_dirty(group("a"), 2);
        flusher.mark_dirty(group("b"), 1);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        flusher.flush_all(Duration::from_secs(1)).await.unwrap();
        assert!(flusher.idle());
        shutdown.cancel();
        task.await.unwrap();
        // Without a sender nothing is acknowledged.
        let lonely = ReleaseFlusher::new();
        lonely.mark_dirty(group("a"), 1);
        assert!(lonely.flush_all(Duration::from_millis(50)).await.is_err());
    }

    /// A sender whose first send waits for `release`, recording every frame.
    struct Gated {
        started: Notify,
        release: Notify,
        frames: Mutex<Vec<i64>>,
    }

    impl ReleaseSender for Gated {
        fn send<'a>(&'a self, frame: &'a ReleaseFrame) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
            Box::pin(async move {
                let first = {
                    let mut frames = self.frames.lock().unwrap();
                    frames.push(frame.release_seq);
                    frames.len() == 1
                };
                if first {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok(())
            })
        }

        fn limiter(&self) -> CredentialConcurrency {
            CredentialConcurrency::default()
        }
    }

    /// Go `TestReleaseFlusherDoesNotLoseASequenceMarkedDuringSend`.
    #[tokio::test(start_paused = true)]
    async fn a_sequence_marked_during_a_send_is_sent_next() {
        let sender = Arc::new(Gated {
            started: Notify::new(),
            release: Notify::new(),
            frames: Mutex::default(),
        });
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(sender.clone()));
        flusher.mark_dirty(group("a"), 1);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        sender.started.notified().await;
        let latest = flusher.mark_dirty(group("a"), 2).unwrap();
        sender.release.notify_one();
        latest.wait(Duration::from_secs(1)).await.unwrap();
        assert_eq!(*sender.frames.lock().unwrap(), vec![1, 2]);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestReleaseFlusherUsesCurrentLimiterConfig`: the timings come from the
    /// current sender's limiter settings.
    #[test]
    fn timings_follow_the_current_limiter_config() {
        let flusher = ReleaseFlusher::new();
        let defaults = CredentialConcurrency::default().with_defaults();
        assert_eq!(flusher.timings(), (defaults.flush_interval(), defaults.max_backoff()));
        let cfg = CredentialConcurrency {
            release_flush_interval: 5_000_000,
            release_max_backoff: 25_000_000,
            ..CredentialConcurrency::default()
        };
        flusher.set_sender(Some(Arc::new(Recorder {
            cfg,
            ..Recorder::default()
        })));
        assert_eq!(flusher.timings(), (Duration::from_millis(5), Duration::from_millis(25)));
    }

    /// Go `TestReleaseFlusherFlushForceUsesBoundedContext`: after a failed round, a
    /// forced flush runs at once instead of waiting out the backoff.
    #[tokio::test(start_paused = true)]
    async fn a_forced_flush_bypasses_the_backoff() {
        let recorder = Arc::new(Recorder {
            cfg: CredentialConcurrency {
                release_flush_interval: 1_000_000_000,
                release_max_backoff: 1_000_000_000,
                ..CredentialConcurrency::default()
            },
            ..Recorder::default()
        });
        recorder.fail.store(true, Ordering::SeqCst);
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(recorder.clone()));
        flusher.mark_dirty(group("a"), 1);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        while recorder.frames.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        recorder.fail.store(false, Ordering::SeqCst);
        let start = tokio::time::Instant::now();
        flusher.flush_all(Duration::from_millis(40)).await.unwrap();
        assert!(
            start.elapsed() < Duration::from_millis(40),
            "no wait for the 1s backoff"
        );
        assert_eq!(recorder.frames.lock().unwrap().len(), 2);
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestScopeEndBlocksDrainUntilReleaseSinkFlushesFinalSequence`: a drain waits
    /// for a scope's release sink, which runs outside the registry lock, and the final
    /// sequence then flushes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_waits_for_the_sink_and_the_final_sequence_flushes() {
        let recorder = Arc::new(Recorder::default());
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(recorder.clone()));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn({
            let flusher = flusher.clone();
            let shutdown = shutdown.clone();
            async move { flusher.run(shutdown).await }
        });
        let registry = crate::registry::Registry::new();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let (leave_tx, leave_rx) = std::sync::mpsc::channel::<()>();
        let leave_rx = Mutex::new(leave_rx);
        let sink = flusher.sink();
        registry.set_release_sink(Some(Arc::new(move |group, sequence| {
            let _ = entered_tx.send(());
            let _ = leave_rx.lock().unwrap().recv();
            sink(group, sequence)
        })));
        let spec = crate::registry::ScopeSpec {
            request_id: "req".into(),
            credential_id: "cred-1".into(),
            model: "gpt".into(),
            kind: "http".into(),
            started_at: std::time::SystemTime::now(),
            accounted: true,
        };
        let scope = registry.install(registry.begin_dispatch().unwrap(), spec).unwrap();
        let ender = std::thread::spawn(move || scope.end());
        entered_rx.recv().unwrap();
        let drain = tokio::spawn({
            let registry = registry.clone();
            async move { registry.drain(Duration::from_secs(1)).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!drain.is_finished(), "the sink is still running");
        let (replaced_tx, replaced_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn({
            let registry = registry.clone();
            move || {
                registry.set_release_sink(None);
                let _ = replaced_tx.send(());
            }
        });
        replaced_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("the sink does not hold the registry lock");
        assert_eq!(
            registry.begin_dispatch().err(),
            Some(crate::registry::RegistryError::NotAccepting)
        );
        leave_tx.send(()).unwrap();
        ender.join().unwrap();
        drain.await.unwrap().unwrap();
        flusher.flush_all(Duration::from_secs(1)).await.unwrap();
        assert_eq!(recorder.frames.lock().unwrap().last().map(|f| f.release_seq), Some(1));
        shutdown.cancel();
        task.await.unwrap();
    }

    /// Go `TestReleaseFlusherSenderReplacementPreservesTicket`: a ticket from a failed
    /// lifetime completes once the next lifetime's sender acknowledges the sequence.
    #[tokio::test]
    async fn a_ticket_survives_a_sender_replacement() {
        let old = Arc::new(Recorder::default());
        old.fail.store(true, Ordering::SeqCst);
        let flusher = ReleaseFlusher::new();
        flusher.set_sender(Some(old.clone()));
        let ticket = flusher.mark_dirty(group("cred-1"), 1).unwrap();
        assert!(flusher.flush().await, "the old lifetime failed");
        let new = Arc::new(Recorder::default());
        flusher.set_sender(Some(new.clone()));
        assert!(!flusher.flush().await);
        assert_eq!(
            *new.frames.lock().unwrap(),
            vec![ReleaseFrame {
                credential_id: "cred-1".into(),
                model: "gpt".into(),
                release_seq: 1
            }]
        );
        ticket.wait(Duration::from_secs(1)).await.unwrap();
    }
}
