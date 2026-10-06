//! A process-wide "work happened" signal for background tasks that should sleep while
//! the server is idle (the heap trim). A finished response body or WebSocket turn calls
//! [`activity`]; a task that has nothing to do awaits [`park`].
//!
//! Cost per call to [`activity`]: one atomic load, plus one wakeup when a task is parked.

use std::sync::atomic::{AtomicBool, Ordering};

static PARKED: AtomicBool = AtomicBool::new(false);
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Wakes a parked task, if one is parked.
pub fn activity() {
    if PARKED.load(Ordering::Relaxed) && PARKED.swap(false, Ordering::AcqRel) {
        WAKE.notify_one();
    }
}

/// Waits for the next [`activity`]. An activity between marking the task parked and
/// awaiting is kept: `notify_one` stores a permit.
pub async fn park() {
    PARKED.store(true, Ordering::Release);
    WAKE.notified().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn activity_wakes_a_parked_task_once() {
        // Nothing parked: no permit is stored.
        activity();
        let parked = tokio::spawn(park());
        tokio::task::yield_now().await;
        assert!(!parked.is_finished(), "woke without activity");
        while !PARKED.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        activity();
        tokio::time::timeout(std::time::Duration::from_secs(1), parked)
            .await
            .expect("activity wakes the parked task")
            .unwrap();
    }
}
