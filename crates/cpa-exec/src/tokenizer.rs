//! The tiktoken encodings behind Go's `tokenizer.Get`, built on first use and shared by
//! every local token count in the process: Claude count_tokens, the Claude
//! `message_start` estimate for non-Claude upstreams, and the Codex, Meta, xAI and
//! OpenAI-compatible counts.
//!
//! Cost: the first count that needs an encoding builds it on its own request (about
//! 0.15 s for o200k_base and 0.06 s for cl100k_base, release build), and the encoder
//! then stays for the life of the process: 48 MB of resident heap for o200k_base and
//! 25 MB for cl100k_base, measured as the growth of `RssAnon` after a trim. Nothing is
//! built until a count needs it. Before these were shared, each call site built its own,
//! so one process could hold five o200k_base copies and two cl100k_base copies: 172 MB
//! of anonymous memory once the Claude count, the Codex counts (o200k_base and
//! cl100k_base) and the Claude `message_start` estimate for Codex had each run, against
//! 77 MB now.
//!
//! ponytail: tiktoken-rs's `CoreBPE` keeps every token three times to serve decoding,
//! which these counts never do, and its regex once per thread slot. Of the 33.4 MB it
//! allocates for o200k_base (heaptrack), the encoder map and its keys are 10.1 MB, a
//! decoder map and its values 10.1 MB, a sorted token list 6.2 MB and the regexes 6.1 MB
//! (128 per-thread clones); the 600,000 short token allocations at malloc's 32-byte
//! minimum bring that to 48 MB resident. An encode-only table with one regex would keep
//! about a third of it; the upgrade is our own byte-pair merge over the encoder map,
//! checked against `CoreBPE` on the Go fixtures.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

/// The two encodings Go's `tokenizer.ForModel` picks from for the models counted locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoding {
    O200kBase,
    Cl100kBase,
}

/// The process-wide encoder for `encoding`, built the first time it is asked for. The
/// error is tiktoken's, as text; a failed build is kept and not retried, like Go's
/// `sync.Once` around `tokenizer.Get`.
pub(crate) fn encoder(encoding: Encoding) -> Result<&'static CoreBPE, String> {
    type Slot = OnceLock<Result<CoreBPE, String>>;
    static O200K: Slot = Slot::new();
    static CL100K: Slot = Slot::new();
    let loaded = match encoding {
        Encoding::O200kBase => O200K.get_or_init(|| tiktoken_rs::o200k_base().map_err(|e| e.to_string())),
        Encoding::Cl100kBase => CL100K.get_or_init(|| tiktoken_rs::cl100k_base().map_err(|e| e.to_string())),
    };
    loaded.as_ref().map_err(Clone::clone)
}
