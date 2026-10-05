//! Conditional catalog refreshes. Each catalog updater remembers the `ETag` of the last
//! catalog it installed from a source and sends it back as `If-None-Match`, so an
//! unchanged catalog costs a `304` with no body instead of a full download (the Codex
//! catalog is about 600 KB). Go downloads the whole file every time; the installed
//! catalog is the same either way.
//!
//! Cost: one short string per source, held only after a catalog with an `ETag` was
//! installed from it.

use std::sync::{Mutex, PoisonError};

use http::HeaderMap;

/// `ETag`s by source URL.
pub struct Etags(Mutex<Vec<(String, String)>>);

impl Etags {
    pub const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// The `If-None-Match` value for `url`.
    pub fn get(&self, url: &str) -> Option<String> {
        let entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        entries.iter().find(|(u, _)| u == url).map(|(_, tag)| tag.clone())
    }

    /// Records what `url` answered with, after its catalog was installed (or found
    /// identical). Only the latest source is kept: an `ETag` from another source no
    /// longer describes the installed catalog.
    pub fn remember(&self, url: &str, headers: &HeaderMap) {
        let tag = headers
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        entries.clear();
        if let Some(tag) = tag {
            entries.push((url.to_owned(), tag));
        }
    }
}

impl Default for Etags {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_latest_source() {
        let etags = Etags::new();
        let with = |tag: &str| {
            let mut h = HeaderMap::new();
            h.insert(http::header::ETAG, tag.parse().unwrap());
            h
        };
        etags.remember("https://a/models.json", &with("\"1\""));
        assert_eq!(etags.get("https://a/models.json").as_deref(), Some("\"1\""));
        etags.remember("https://b/models.json", &with("\"2\""));
        assert_eq!(etags.get("https://a/models.json"), None);
        assert_eq!(etags.get("https://b/models.json").as_deref(), Some("\"2\""));
        etags.remember("https://b/models.json", &HeaderMap::new());
        assert_eq!(etags.get("https://b/models.json"), None);
    }
}
