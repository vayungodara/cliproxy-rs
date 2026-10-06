//! The tiktoken encodings behind Go's `tokenizer.Get`, built on first use and shared by
//! every local token count in the process: Claude count_tokens, the Claude
//! `message_start` estimate for non-Claude upstreams, and the Codex, Meta, xAI and
//! OpenAI-compatible counts.
//!
//! Cost: the first count that needs an encoding builds it on its own request (about
//! 0.15 s for o200k_base and 0.06 s for cl100k_base, release build), and the encoder
//! then stays for the life of the process: 47 MB of resident heap for o200k_base and
//! 24 MB for cl100k_base, measured as the growth of `RssAnon` after a trim (MB of 1,024
//! kB). Nothing is built until a count needs it. Before these were shared, each call
//! site built its own, so one process could hold five o200k_base copies and two
//! cl100k_base copies: the tokenizers took 164 MB once the Claude count, the Codex
//! counts (o200k_base and cl100k_base) and the Claude `message_start` estimate for Codex
//! had each run, and take 71 MB now.
//!
//! ponytail: tiktoken-rs's `CoreBPE` keeps every token three times to serve decoding,
//! which these counts never do, and its regex once per thread slot. Of the 31.9 MB it
//! allocates for o200k_base (heaptrack), the encoder map and its keys are 9.6 MB, a
//! decoder map and its values 9.6 MB, a sorted token list 5.9 MB and the regexes 6.8 MB
//! (5.9 MB of it 128 per-thread clones of the main one); the 600,000 short token
//! allocations at malloc's 32-byte minimum chunk bring that to 47 MB resident. An
//! encode-only table with one regex would keep about a third of it; the upgrade is our own
//! byte-pair merge over the encoder map, checked against `CoreBPE` on the Go fixtures.

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiktoken constructor anywhere else in this crate would build a second encoder
    /// and keep it for the life of the process (47 MB for o200k_base).
    #[test]
    fn encoders_are_built_only_here() {
        fn visit(dir: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, found);
                } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with("tokenizer.rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if ["o200k_base(", "cl100k_base(", "CoreBPE::new("]
                        .iter()
                        .any(|c| text.contains(c))
                    {
                        found.push(path.display().to_string());
                    }
                }
            }
        }
        let mut found = vec![];
        visit(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut found,
        );
        assert!(found.is_empty(), "build encoders through tokenizer::encoder: {found:?}");
    }

    #[test]
    fn each_encoding_is_built_once_and_shared() {
        let o200k = encoder(Encoding::O200kBase).unwrap();
        let cl100k = encoder(Encoding::Cl100kBase).unwrap();
        assert!(std::ptr::eq(o200k, encoder(Encoding::O200kBase).unwrap()));
        assert!(std::ptr::eq(cl100k, encoder(Encoding::Cl100kBase).unwrap()));
        // Two slots, two vocabularies.
        let text = "Ünïcödé façade naïve résumé — こんにちは世界";
        assert_ne!(o200k.encode_ordinary(text), cl100k.encode_ordinary(text));
    }
}
