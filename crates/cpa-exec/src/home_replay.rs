//! Go's Home KV backend for the snapshot replay caches (internal/cache
//! kimi_thinking_replay_cache.go and claude_thinking_replay_cache.go): one JSON envelope
//! per session, `{"generation","deleted","content"|"contents"}`, reserved with a CAS
//! tombstone when absent, and replaced or deleted only by compare-and-swap against the
//! exact bytes the request read, so a slower request never overwrites newer state.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use cpa_core::exec::ExecStream;
use futures_util::StreamExt;

/// `KimiThinkingReplayCacheTTL` / `ClaudeThinkingReplayCacheTTL`.
pub(crate) const TTL: Duration = Duration::from_secs(3600);

/// The exact value one request read or reserved (Go's snapshot `raw`; `found` is always
/// true after a reservation).
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub(crate) raw: Vec<u8>,
}

/// The replay payload of Go's envelope struct.
pub(crate) enum Payload<'a> {
    /// `Content json.RawMessage \`json:"content,omitempty"\`` (Kimi).
    Content(&'a [u8]),
    /// `Contents []json.RawMessage \`json:"contents,omitempty"\`` (Claude).
    Contents(&'a [Vec<u8>]),
}

/// The Home replay writes one request started. Go mutates the cache before the response
/// reaches the client's end (the stream's last chunk or the buffered body), so the next
/// turn, on any node, reads it: the response waits for these writes.
#[derive(Clone, Default)]
pub(crate) struct Writes(Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>);

impl std::fmt::Debug for Writes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Writes")
    }
}

/// A scope's identity is its keys; its pending writes do not distinguish it.
impl PartialEq for Writes {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Writes {}

impl Writes {
    pub(crate) fn spawn(&self, write: impl std::future::Future<Output = ()> + Send + 'static) {
        let handle = tokio::spawn(write);
        self.0.lock().unwrap_or_else(PoisonError::into_inner).push(handle);
    }

    /// Waits for every write started so far, including ones started meanwhile.
    pub(crate) async fn settle(&self) {
        loop {
            let pending = std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner));
            if pending.is_empty() {
                return;
            }
            for handle in pending {
                let _ = handle.await;
            }
        }
    }

    /// `stream`, yielding each item and its end only after the writes started before
    /// them finished.
    pub(crate) fn gate(&self, stream: ExecStream) -> ExecStream {
        let (each, end) = (self.clone(), self.clone());
        let tail = futures_util::stream::once(async move { end.settle().await })
            .filter_map(|()| async { None::<<ExecStream as futures_util::Stream>::Item> });
        stream
            .then(move |item| {
                let writes = each.clone();
                async move {
                    writes.settle().await;
                    item
                }
            })
            .chain(tail)
            .boxed()
    }
}

/// Go `uuid.NewString()`.
pub(crate) fn generation() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Go `json.Marshal` of the envelope: fields in declaration order, `deleted` and an
/// empty payload omitted, raw content compacted and HTML-escaped as encoding/json does.
pub(crate) fn envelope(generation: &str, payload: Option<Payload<'_>>) -> Vec<u8> {
    let mut out = b"{\"generation\":".to_vec();
    cpa_common::json::marshal_str(&mut out, generation.as_bytes(), true);
    match payload {
        None => out.extend_from_slice(b",\"deleted\":true"),
        Some(Payload::Content(content)) if !content.is_empty() => {
            out.extend_from_slice(b",\"content\":");
            out.extend_from_slice(&cpa_common::json::compact(content, true));
        }
        Some(Payload::Contents(contents)) if !contents.is_empty() => {
            out.extend_from_slice(b",\"contents\":[");
            for (i, content) in contents.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&cpa_common::json::compact(content, true));
            }
            out.push(b']');
        }
        Some(_) => {}
    }
    out.push(b'}');
    out
}

/// Go `readOrReserve*HomeValue`: the stored value, or a tombstone this request
/// reserved for an absent key (four attempts against racing writers).
pub(crate) async fn read_or_reserve(
    client: &cpa_home::Client,
    key: &str,
    max_serialized: usize,
    what: &str,
) -> Result<Snapshot, String> {
    for _ in 0..4 {
        if let Some(raw) = client.kv_get(key).await.map_err(|e| e.to_string())? {
            if raw.len() > max_serialized {
                return Err(format!("{what} value exceeds size limit"));
            }
            return Ok(Snapshot { raw });
        }
        let tombstone = envelope(&generation(), None);
        if client
            .kv_compare_and_swap(key, None, &tombstone, TTL)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(Snapshot { raw: tombstone });
        }
    }
    Err(format!("could not reserve absent {what} state"))
}

/// Go `KVCompareAndSwap(key, snapshot.raw, true, value, ttl)`.
pub(crate) async fn swap(
    client: &cpa_home::Client,
    key: &str,
    snapshot: &Snapshot,
    value: &[u8],
) -> Result<bool, String> {
    client
        .kv_compare_and_swap(key, Some(&snapshot.raw), value, TTL)
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A gated response neither yields its next item nor ends while a replay write it
    /// started is pending.
    #[tokio::test]
    async fn gated_streams_wait_for_their_writes() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        let writes = Writes::default();
        let slow_write = |done: Arc<AtomicBool>| {
            let (release, wait) = tokio::sync::oneshot::channel::<()>();
            writes.spawn(async move {
                let _ = wait.await;
                done.store(true, Ordering::SeqCst);
            });
            release
        };
        let first = Arc::new(AtomicBool::new(false));
        let release_first = slow_write(first.clone());
        let mut stream = writes.gate(futures_util::stream::iter([Ok(bytes::Bytes::from_static(b"a"))]).boxed());
        let pending = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(pending.is_err(), "the item waits for the write");
        release_first.send(()).unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        assert!(first.load(Ordering::SeqCst));
        // A write started after the last item still holds the end.
        let second = Arc::new(AtomicBool::new(false));
        let release_second = slow_write(second.clone());
        let pending = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
        assert!(pending.is_err(), "the end waits for the write");
        release_second.send(()).unwrap();
        assert!(stream.next().await.is_none());
        assert!(second.load(Ordering::SeqCst));
    }

    #[test]
    fn envelopes_marshal_like_go() {
        assert_eq!(envelope("g", None), br#"{"generation":"g","deleted":true}"#);
        assert_eq!(
            envelope("g", Some(Payload::Content(br#"[ {"a":"<&>"} ]"#))),
            br#"{"generation":"g","content":[{"a":"\u003c\u0026\u003e"}]}"#
        );
        assert_eq!(
            envelope("g", Some(Payload::Contents(&[b"[1]".to_vec(), b"[ 2 ]".to_vec()]))),
            br#"{"generation":"g","contents":[[1],[2]]}"#
        );
        assert_eq!(envelope("g", Some(Payload::Contents(&[]))), br#"{"generation":"g"}"#);
    }
}
