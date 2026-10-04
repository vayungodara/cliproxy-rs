//! Plugin-fed streams (internal/pluginhost/stream_bridge.go).
//!
//! `executor.execute_stream` hands the plugin a `stream_id`; the plugin pushes chunks
//! with `host.stream.emit` (blocking while 16 are queued) and ends with
//! `host.stream.close`, optionally with an error that becomes the final chunk. The host
//! side reads chunks asynchronously; dropping the reader aborts the stream so a pending
//! emit returns "stream is not open".

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use bytes::Bytes;

use crate::host::lock;

const BUFFER: usize = 16;

/// One chunk (`pluginapi.ExecutorStreamChunk`): a payload and, when set, an error that
/// ends the stream after it. A plugin may send both in one emit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Chunk {
    pub payload: Bytes,
    pub error: Option<String>,
}

impl Chunk {
    pub fn data(payload: Bytes) -> Self {
        Self { payload, error: None }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            payload: Bytes::new(),
            error: Some(message.into()),
        }
    }
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Chunk>,
    closed: bool,
    aborted: bool,
}

struct Stream {
    queue: Mutex<Queue>,
    space: Condvar,
    ready: tokio::sync::Notify,
}

impl Stream {
    fn emit(&self, chunk: Chunk) -> Result<(), ()> {
        let mut q = lock(&self.queue);
        loop {
            if q.closed || q.aborted {
                return Err(());
            }
            if q.items.len() < BUFFER {
                q.items.push_back(chunk);
                self.ready.notify_one();
                return Ok(());
            }
            q = self.space.wait(q).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn close(&self, error: &str) {
        let mut q = lock(&self.queue);
        if q.closed || q.aborted {
            return;
        }
        q.closed = true;
        if !error.is_empty() {
            q.items.push_back(Chunk::error(error));
        }
        // Terminal: wake every pending reader, as closing Go's channel does.
        self.ready.notify_waiters();
        self.space.notify_all();
    }

    fn abort(&self) {
        let mut q = lock(&self.queue);
        q.aborted = true;
        q.items.clear();
        self.ready.notify_waiters();
        self.space.notify_all();
    }

    async fn next(&self) -> Option<Chunk> {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut q = lock(&self.queue);
                if let Some(chunk) = q.items.pop_front() {
                    self.space.notify_one();
                    return Some(chunk);
                }
                if q.closed || q.aborted {
                    return None;
                }
            }
            notified.await;
        }
    }
}

/// Go `streamBridge`.
#[derive(Default)]
pub struct StreamBridge {
    next: AtomicU64,
    streams: Mutex<HashMap<String, Arc<Stream>>>,
}

/// The reading side of one bridged stream. Dropping it aborts the stream.
pub struct Reader {
    id: String,
    stream: Arc<Stream>,
    bridge: std::sync::Weak<StreamBridge>,
}

impl Reader {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The next chunk; `None` once the plugin closed the stream and it drained.
    pub async fn next(&self) -> Option<Chunk> {
        self.stream.next().await
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        if let Some(bridge) = self.bridge.upgrade() {
            let mut streams = lock(&bridge.streams);
            if streams.get(&self.id).is_some_and(|s| Arc::ptr_eq(s, &self.stream)) {
                streams.remove(&self.id);
            }
        }
        self.stream.abort();
    }
}

impl StreamBridge {
    /// Go `streamBridge.open`: IDs are decimal counters from 1.
    pub fn open(self: &Arc<Self>) -> Reader {
        let id = (self.next.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let stream = Arc::new(Stream {
            queue: Mutex::default(),
            space: Condvar::new(),
            ready: tokio::sync::Notify::new(),
        });
        lock(&self.streams).insert(id.clone(), stream.clone());
        Reader {
            id,
            stream,
            bridge: Arc::downgrade(self),
        }
    }

    /// Go `streamBridge.emit`. Blocks while the stream is full.
    pub fn emit(&self, id: &str, chunk: Chunk) -> Result<(), String> {
        if id.is_empty() {
            return Err("stream id is required".into());
        }
        let stream = lock(&self.streams).get(id).cloned();
        let Some(stream) = stream else {
            return Err(format!("stream {id} is not open"));
        };
        stream.emit(chunk).map_err(|()| format!("stream {id} is not open"))
    }

    /// Go `streamBridge.close`.
    pub fn close(&self, id: &str, error: &str) {
        if id.is_empty() {
            return;
        }
        let stream = lock(&self.streams).remove(id);
        if let Some(stream) = stream {
            stream.close(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go `TestStreamBridgeCloseUnblocksPendingEmit` and ordering: a full stream blocks
    /// the emitter until the reader takes a chunk; close appends the error chunk; a
    /// dropped reader makes pending emits fail.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backpressure_close_and_abort() {
        let bridge = Arc::new(StreamBridge::default());
        let reader = bridge.open();
        let id = reader.id().to_owned();
        let emitter = {
            let bridge = bridge.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || {
                for i in 0..20u8 {
                    bridge.emit(&id, Chunk::data(Bytes::from(vec![i]))).unwrap();
                }
                bridge.close(&id, "boom");
            })
        };
        let mut got = Vec::new();
        while let Some(chunk) = reader.next().await {
            got.push(chunk);
        }
        emitter.await.unwrap();
        assert_eq!(got.len(), 21);
        assert_eq!(got[0], Chunk::data(Bytes::from_static(&[0])));
        assert_eq!(got[20], Chunk::error("boom"));
        assert_eq!(
            bridge.emit(&id, Chunk::default()),
            Err(format!("stream {id} is not open"))
        );

        let reader = bridge.open();
        let id = reader.id().to_owned();
        for _ in 0..BUFFER {
            bridge.emit(&id, Chunk::default()).unwrap();
        }
        let blocked = {
            let bridge = bridge.clone();
            let id = id.clone();
            tokio::task::spawn_blocking(move || bridge.emit(&id, Chunk::default()))
        };
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        drop(reader);
        assert_eq!(blocked.await.unwrap(), Err(format!("stream {id} is not open")));

        // Closing wakes every pending reader; notify_one would leave one asleep.
        let reader = Arc::new(bridge.open());
        let id = reader.id().to_owned();
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let reader = reader.clone();
                tokio::spawn(async move { reader.next().await })
            })
            .collect();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        bridge.close(&id, "");
        for waiter in waiters {
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), waiter).await;
            assert_eq!(got.expect("reader woke").unwrap(), None);
        }
    }
}
