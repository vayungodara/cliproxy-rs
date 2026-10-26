//! Auth files a plugin parses: the `PluginAuthParser` branch of Go's file synthesizer
//! (internal/watcher/synthesizer/file.go) and `FileTokenStore.readAuthFiles`
//! (sdk/auth/filestore.go). The plugin host installs a parser; while no loaded plugin
//! offers an auth provider, loading reads one atomic flag and takes the built-in path.
//!
//! Plugins are asked outside every lock: [`preparse_auth_dir`] asks them about the
//! folder's files and caches each answer under the file's path and SHA-256. A scan
//! ([`super::scan_auth_dir`]), which callers run under the management disk lock, only
//! reads that cache; a file whose current bytes have no answer is deferred, keeping its
//! last credentials until the next watcher reload parses it. So a plugin can call
//! `host.auth.save`, or wait on its own locks, while it parses.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use sha2::{Digest, Sha256};

use serde_json::{Map, Value as Json};

use super::{excluded_models_from_metadata, excluded_union, oauth_aliases, parse_weight};
use crate::credential::{Credential, Source};
use crate::gostr::GoStr;

/// Go `pluginapi.AuthParseRequest` for one auth file.
pub struct ParseRequest<'a> {
    /// The file's lowercased `type` (`gemini` asks as `gemini-cli`); empty asks every
    /// auth provider in order.
    pub provider: &'a str,
    pub path: &'a Path,
    pub file_name: &'a str,
    pub raw_json: &'a [u8],
}

/// One auth a plugin parsed (Go `pluginAuthDataToCoreAuth`). `metadata` is the JSON
/// document the credential keeps and persists.
#[derive(Clone, Default)]
pub struct ParsedAuth {
    pub provider: String,
    pub id: String,
    pub file_name: String,
    pub label: String,
    pub prefix: String,
    pub proxy_url: String,
    pub disabled: bool,
    pub metadata: Map<String, Json>,
    pub attributes: BTreeMap<String, String>,
    /// Go `NextRefreshAfter`: the credential registers with it ([`Credential::next_refresh_after`]).
    pub next_refresh_after: Option<std::time::SystemTime>,
}

/// Go `ParseAuths`' outcome.
#[derive(Clone)]
pub enum Parsed {
    /// No plugin took the file, or the call failed: the built-in synthesizer runs.
    NotHandled,
    /// A plugin took the file. No auths suppresses the built-in synthesizer.
    Handled(Vec<ParsedAuth>),
}

/// Go `synthesizer.PluginAuthParser` (`ParseAuths`).
pub trait PluginAuthParser: Send + Sync {
    fn parse_auths(&self, req: &ParseRequest<'_>) -> Parsed;
}

/// Whether a loaded plugin offers an auth provider; the owner of the installed parser
/// keeps it current. One plugin host owns it.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static PARSER: RwLock<Option<Arc<dyn PluginAuthParser>>> = RwLock::new(None);

/// One cached answer: for the file bytes with this SHA-256, asked at this epoch.
/// `own_writes` are the SHA-256 of what `host.auth.save` wrote to the file during the
/// parse ([`note_plugin_auth_write`]); the answer is not cached for them.
struct Answer {
    hash: [u8; 32],
    own_writes: Vec<[u8; 32]>,
    epoch: u64,
    parsed: Parsed,
}

/// Bumped whenever the parser or its plugins may have changed, retiring every answer.
static EPOCH: AtomicU64 = AtomicU64::new(0);
static ANSWERS: Mutex<Option<HashMap<PathBuf, Answer>>> = Mutex::new(None);

fn answers() -> std::sync::MutexGuard<'static, Option<HashMap<PathBuf, Answer>>> {
    ANSWERS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Called after answers are retired, so auth files are loaded again.
static ON_CHANGE: RwLock<Option<Arc<dyn Fn() + Send + Sync>>> = RwLock::new(None);

/// Retires every cached answer (the parser or a plugin changed) and tells the
/// [`on_plugin_auth_change`] callback.
fn retire() {
    EPOCH.fetch_add(1, Ordering::AcqRel);
    *answers() = None;
    let wake = ON_CHANGE.read().unwrap_or_else(PoisonError::into_inner).clone();
    if let Some(wake) = wake {
        wake();
    }
}

/// Installs the callback told whenever plugin auth providers may answer differently (a
/// plugin offering one was loaded, unloaded or fused, or the parser changed). It runs
/// on the thread making the change, which may hold plugin host locks: it should only
/// schedule an auth-folder reload.
pub fn on_plugin_auth_change(wake: Option<Arc<dyn Fn() + Send + Sync>>) {
    *ON_CHANGE.write().unwrap_or_else(PoisonError::into_inner) = wake;
}

/// Counts the changes [`on_plugin_auth_change`] is told about.
pub fn plugin_auth_epoch() -> u64 {
    EPOCH.load(Ordering::Acquire)
}

/// The [`set_plugin_auth_providers`] signature last set.
static PROVIDERS: Mutex<Option<u64>> = Mutex::new(None);

/// Go `RegisterPluginAuthParser` / `Watcher.SetPluginAuthParser`. Clearing it also
/// clears the auth providers. Setting the parser already installed changes nothing.
pub fn set_plugin_auth_parser(parser: Option<Arc<dyn PluginAuthParser>>) {
    let mut slot = PARSER.write().unwrap_or_else(PoisonError::into_inner);
    let same = match (&*slot, &parser) {
        (Some(old), Some(new)) => std::ptr::addr_eq(Arc::as_ptr(old), Arc::as_ptr(new)),
        (None, None) => true,
        _ => false,
    };
    if same {
        return;
    }
    let was = ACTIVE.load(Ordering::Acquire);
    if parser.is_none() {
        ACTIVE.store(false, Ordering::Release);
        *PROVIDERS.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
    *slot = parser;
    drop(slot);
    if was {
        retire();
    }
}

/// Set by the parser's owner whenever its plugins change: `providers` identifies the
/// loaded, unfused plugins offering an auth provider (`None`: there is none), and
/// answers cached before are asked again only when it differs from the last one set.
/// A sync that changes nothing therefore changes nothing here.
pub fn set_plugin_auth_providers(providers: Option<u64>) {
    let mut last = PROVIDERS.lock().unwrap_or_else(PoisonError::into_inner);
    ACTIVE.store(providers.is_some(), Ordering::Release);
    if *last == providers {
        return;
    }
    *last = providers;
    drop(last);
    retire();
}

/// One parse in progress: its ID, the file (see [`clean`]) and the SHA-256 of the bytes
/// `host.auth.save` wrote to that file meanwhile ([`note_plugin_auth_write`]).
type Parsing = (u64, PathBuf, Vec<[u8; 32]>);
static PARSING: Mutex<Vec<Parsing>> = Mutex::new(Vec::new());
static NEXT_PARSE: AtomicU64 = AtomicU64::new(0);

fn parsing() -> std::sync::MutexGuard<'static, Vec<Parsing>> {
    PARSING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `path` absolute and lexically cleaned (Go `filepath.Abs`), so the scan's spelling
/// and the save's match.
pub fn clean(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Tells credential loading that a plugin's `host.auth.save` wrote `data` to `path`.
/// When an auth provider is parsing that file meanwhile, a scan that finds exactly
/// these bytes publishes that parse's answer once (Go publishes every parse's answer)
/// and leaves the file for the watcher to parse again. Nothing else is cached for them.
pub fn note_plugin_auth_write(path: &Path, data: &[u8]) {
    if !ACTIVE.load(Ordering::Acquire) && !(cfg!(feature = "test-support") && thread_parser().is_some()) {
        return;
    }
    let path = clean(path);
    let mut parsing = parsing();
    if parsing.is_empty() {
        return;
    }
    let hash: [u8; 32] = Sha256::digest(data).into();
    for (_, parsed, writes) in parsing.iter_mut() {
        if *parsed == path {
            writes.push(hash);
        }
    }
}

#[cfg(feature = "test-support")]
thread_local! {
    static THREAD_PARSER: std::cell::RefCell<Option<Arc<dyn PluginAuthParser>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(feature = "test-support")]
fn thread_parser() -> Option<Arc<dyn PluginAuthParser>> {
    THREAD_PARSER.with(|p| p.borrow().clone())
}

#[cfg(not(feature = "test-support"))]
fn thread_parser() -> Option<Arc<dyn PluginAuthParser>> {
    None
}

/// Runs `f` with `parser` installed, as if a plugin offered an auth provider, for this
/// thread only: tests sharing a process (`cargo test`) never see it.
#[cfg(feature = "test-support")]
pub fn with_thread_plugin_auth_parser<R>(parser: Arc<dyn PluginAuthParser>, f: impl FnOnce() -> R) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            THREAD_PARSER.with(|p| *p.borrow_mut() = None);
        }
    }
    THREAD_PARSER.with(|p| *p.borrow_mut() = Some(parser));
    let _reset = Reset;
    f()
}

/// Whether auth files go to the installed parser first.
pub fn plugin_auth_parser_active() -> bool {
    if cfg!(feature = "test-support") && thread_parser().is_some() {
        return true;
    }
    ACTIVE.load(Ordering::Acquire)
        && PARSER
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
}

/// The installed parser while a plugin offers an auth provider: one atomic load
/// otherwise.
pub(super) fn current() -> Option<Arc<dyn PluginAuthParser>> {
    if cfg!(feature = "test-support")
        && let Some(parser) = thread_parser()
    {
        return Some(parser);
    }
    if !ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    PARSER.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// How a scan asks plugin auth providers.
#[derive(Clone, Copy)]
pub(super) enum Ask<'a> {
    /// The cached answer for the file's current bytes, else `parser`'s, then cached.
    Fill(&'a dyn PluginAuthParser),
    /// Only the cache: a file without a current answer is deferred. Callers may hold
    /// locks a plugin waits on.
    Cached,
    /// `parser`, uncached.
    #[cfg(test)]
    Live(&'a dyn PluginAuthParser),
}

impl Ask<'_> {
    /// `None`: deferred. `true` with the answer of a parse that wrote these bytes itself,
    /// to publish once without marking the file applied.
    fn parse(self, req: &ParseRequest<'_>) -> Option<(Parsed, bool)> {
        let parser = match self {
            Ask::Fill(parser) => parser,
            Ask::Cached => {
                let hash = Sha256::digest(req.raw_json).into();
                return cached(req.path, &hash)
                    .map(|parsed| (parsed, false))
                    .or_else(|| written_by_parse(req.path, &hash).map(|parsed| (parsed, true)));
            }
            #[cfg(test)]
            Ask::Live(parser) => return Some((parser.parse_auths(req), false)),
        };
        let hash = Sha256::digest(req.raw_json).into();
        if let Some(parsed) = cached(req.path, &hash) {
            return Some((parsed, false));
        }
        // The epoch before asking: a change during the call retires the answer.
        let epoch = EPOCH.load(Ordering::Acquire);
        let id = NEXT_PARSE.fetch_add(1, Ordering::Relaxed);
        parsing().push((id, clean(req.path), Vec::new()));
        let parsed = parser.parse_auths(req);
        let mut parsing = parsing();
        let own_writes = parsing
            .iter()
            .position(|(p, ..)| *p == id)
            .map(|at| parsing.swap_remove(at).2)
            .unwrap_or_default();
        drop(parsing);
        let answer = Answer {
            hash,
            own_writes,
            epoch,
            parsed: parsed.clone(),
        };
        answers().get_or_insert_default().insert(req.path.to_owned(), answer);
        Some((parsed, false))
    }
}

fn cached(path: &Path, hash: &[u8; 32]) -> Option<Parsed> {
    let answers = answers();
    let answer = answers.as_ref()?.get(path)?;
    (answer.hash == *hash && answer.epoch == EPOCH.load(Ordering::Acquire)).then(|| answer.parsed.clone())
}

/// The current answer for `path` whose parse wrote `hash` to it.
fn written_by_parse(path: &Path, hash: &[u8; 32]) -> Option<Parsed> {
    let answers = answers();
    let answer = answers.as_ref()?.get(path)?;
    (answer.own_writes.contains(hash) && answer.epoch == EPOCH.load(Ordering::Acquire)).then(|| answer.parsed.clone())
}

/// Asks plugin auth providers about every auth file in `cfg`'s folder whose current
/// bytes have no cached answer, holding no lock, so that the next scan finds them all.
/// Call it right before a scan that runs under a lock; returns at once while no plugin
/// offers an auth provider.
pub fn preparse_auth_dir(cfg: &crate::config::Config) {
    let Some(parser) = current() else { return };
    let scan = super::scan_with(cfg, Some(Ask::Fill(&*parser)));
    // Forget the folder's files that are gone (an answer holds the plugin's tokens).
    if let Some(answers) = answers().as_mut() {
        answers.retain(|path, _| path.parent() != Some(cfg.auth_dir.as_path()) || scan.read.contains_key(path));
    }
}

/// Go `ValidateAuthWeight` on a parsed auth.
fn weight_valid(auth: &ParsedAuth) -> bool {
    auth.attributes
        .get("weight")
        .is_none_or(|w| parse_weight(&Json::String(w.clone())).is_ok())
        && auth.metadata.get("weight").is_none_or(|w| parse_weight(w).is_ok())
}

/// The plugin branch of Go `synthesizeFileAuths` for a file whose JSON is `meta`
/// (normalized) and lowercased type `provider`. `None`: no plugin took it.
pub(super) fn credentials(
    ask: Ask<'_>,
    meta: &Map<String, Json>,
    provider: &str,
    path: &Path,
    data: &[u8],
) -> Option<Result<super::FileCredentials, String>> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let provider = if provider == "gemini" { "gemini-cli" } else { provider };
    let parsed = ask.parse(&ParseRequest {
        provider,
        path,
        file_name: &file_name,
        raw_json: data,
    });
    let (auths, unapplied) = match parsed {
        None | Some((Parsed::NotHandled, true)) => return Some(Ok(super::FileCredentials::Deferred)),
        Some((Parsed::NotHandled, false)) => return None,
        Some((Parsed::Handled(auths), unapplied)) => (auths, unapplied),
    };
    // Go `compactPluginAuths`: an auth with an invalid weight is dropped.
    let auths: Vec<ParsedAuth> = auths.into_iter().filter(weight_valid).collect();
    let built = build(auths, meta, path, &file_name);
    Some(built.map(|all| match unapplied {
        true => super::FileCredentials::Unapplied(all),
        false => super::FileCredentials::Plugin(all),
    }))
}

/// Go `buildAuthsFromFileData`'s plugin branch for a `host.auth.save`: the parsed auths
/// with the save's own rules (see [`apply_save_rules`]) from `meta`, the saved file's
/// decoded and normalized metadata. The watcher's other file rules (excluded models,
/// model aliases, fingerprint profile, auth kind) apply when it next parses the file.
pub fn plugin_save_credentials(
    path: &Path,
    meta: &Map<String, Json>,
    auths: Vec<ParsedAuth>,
) -> Result<Vec<Credential>, String> {
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let count = auths.len();
    let mut out = Vec::with_capacity(count);
    for (index, mut auth) in auths.into_iter().enumerate() {
        apply_save_rules(&mut auth, meta, path, &file_name, index, count)?;
        out.push(into_credential(auth, path));
    }
    Ok(out)
}

/// The rules Go applies to a plugin-parsed auth from its file both when saving and when
/// the watcher loads it: plugin-virtual marking when the file holds several, the file's
/// path, its disabled state, proxy URL, prefix, weight and priority, and the auth's own
/// header metadata (`buildAuthsFromFileData`, auth_callbacks.go L366-425).
fn apply_save_rules(
    auth: &mut ParsedAuth,
    meta: &Map<String, Json>,
    path: &Path,
    file_name: &str,
    index: usize,
    count: usize,
) -> Result<(), String> {
    let full = path.display().to_string();
    let file_disabled = meta.get("disabled").and_then(Json::as_bool) == Some(true);
    let file_str = |key: &str| meta.get(key).and_then(Json::as_str);
    super::normalize_metadata(&mut auth.metadata);
    let attrs = &mut auth.attributes;
    if count > 1 {
        // Go `MarkPluginVirtualAuth`.
        attrs.insert("plugin_virtual".into(), "true".into());
        attrs.insert("virtual_source".into(), full.clone());
        let seed = [auth.id.trim(), auth.file_name.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or_else(|| index.to_string(), str::to_owned);
        let seed = format!("{}|{full}|{seed}|{index}", auth.provider.trim().go_lower());
        attrs.insert("auth_index_seed".into(), seed);
    }
    for key in ["path", "source"] {
        attrs.insert(key.into(), full.clone());
    }
    attrs.insert("source_backend".into(), "file".into());
    if file_disabled {
        auth.disabled = true;
        auth.metadata.insert("disabled".into(), true.into());
    }
    if auth.proxy_url.is_empty()
        && let Some(proxy) = file_str("proxy_url")
    {
        auth.proxy_url = proxy.trim().to_owned();
    }
    if auth.prefix.is_empty()
        && let Some(prefix) = file_str("prefix")
    {
        auth.prefix = prefix.trim().trim_matches('/').to_owned();
    }
    // Go `ApplyAuthWeightMetadata`: the auth was validated above.
    if let Some(weight) = meta.get("weight") {
        let weight = parse_weight(weight)
            .map_err(|e| format!("invalid plugin auth weight in {file_name}: invalid metadata weight: {e}"))?;
        attrs.insert("weight".into(), weight.to_string());
    }
    // Go `ApplyAuthPriorityMetadata` from the file.
    attrs.remove("file_priority");
    let priority = match meta.get("priority") {
        Some(Json::Number(n)) => Some(n.as_f64().map(|f| f.trunc() as i64).unwrap_or(0).to_string()),
        Some(Json::String(s)) if s.trim().parse::<i64>().is_ok() => Some(s.trim().to_owned()),
        _ => None,
    };
    if let Some(priority) = priority {
        // Go keeps the decoded number in the metadata; only the attribute is an integer.
        auth.metadata.insert("priority".into(), meta["priority"].clone());
        attrs.insert("priority".into(), priority);
        attrs.insert("file_priority".into(), "true".into());
    }
    // Go `ApplyCustomHeadersFromMetadata` reads the parsed auth's metadata.
    if let Some(Json::Object(headers)) = auth.metadata.get("headers") {
        for (k, v) in headers {
            if let (k, Some(v)) = (k.trim(), v.as_str().map(str::trim))
                && !k.is_empty()
                && !v.is_empty()
            {
                attrs.insert(format!("header:{k}"), v.into());
            }
        }
    }
    Ok(())
}

/// The credential of a parsed auth from the file at `path`. Go keeps `Prefix` and
/// `ProxyURL` as fields; here they are attributes, as for every file credential.
fn into_credential(mut auth: ParsedAuth, path: &Path) -> Credential {
    auth.attributes.insert("prefix".into(), auth.prefix);
    if !auth.proxy_url.is_empty() {
        auth.attributes.insert("proxy_url".into(), auth.proxy_url);
    }
    // ponytail: the label and disabled state come from the plugin; a later metadata
    // patch recomputes them from the metadata as for every credential.
    Credential {
        id: auth.id,
        provider: auth.provider,
        source: Source::File(path.to_owned()),
        disabled: auth.disabled,
        label: auth.label,
        attributes: auth.attributes,
        metadata: auth.metadata,
        revision: 0,
        next_refresh_after: auth.next_refresh_after,
    }
}

fn build(
    auths: Vec<ParsedAuth>,
    meta: &Map<String, Json>,
    path: &Path,
    file_name: &str,
) -> Result<Vec<Credential>, String> {
    let (per_account, invalid) = excluded_models_from_metadata(meta);
    if let Some(error) = invalid {
        tracing::warn!(%error, "ignoring invalid excluded models in auth file {file_name}");
    }
    if auths.is_empty() {
        return Ok(Vec::new());
    }
    let excluded = excluded_union(&[&per_account]);
    let aliases = oauth_aliases(meta);
    let fingerprint = meta
        .get("fingerprint_profile")
        .and_then(Json::as_str)
        .map(|s| s.trim().go_lower())
        .filter(|p| !p.is_empty());
    let count = auths.len();
    let mut out = Vec::with_capacity(count);
    for (index, mut auth) in auths.into_iter().enumerate() {
        apply_save_rules(&mut auth, meta, path, file_name, index, count)?;
        let attrs = &mut auth.attributes;
        if let Some(aliases) = &aliases {
            attrs.insert("model_aliases".into(), aliases.clone());
        }
        // Go `ApplyAuthExcludedModelsMeta`: the file's own rules replace the plugin's.
        if !excluded.is_empty() {
            attrs.insert("excluded_models".into(), excluded.join(","));
        }
        attrs.insert("auth_kind".into(), "oauth".into());
        if let Some(profile) = &fingerprint {
            attrs.insert("fingerprint_profile".into(), profile.clone());
        }
        out.push(into_credential(auth, path));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::super::{FileCredentials, auth_index, file_credentials, scan_with};
    use super::*;

    /// Serializes the tests that use the process-wide answer cache and provider
    /// signature: `cargo test` runs tests on parallel threads, and one test's
    /// `set_plugin_auth_providers` retires the answers another is reading.
    fn plugin_auth_state() -> std::sync::MutexGuard<'static, ()> {
        static STATE: Mutex<()> = Mutex::new(());
        STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A parser answering from a closure and recording what it was asked.
    struct Fake<F> {
        answer: F,
        asked: Mutex<Vec<(String, String, String)>>,
    }

    impl<F: Fn(&ParseRequest<'_>) -> Parsed + Send + Sync> PluginAuthParser for Fake<F> {
        fn parse_auths(&self, req: &ParseRequest<'_>) -> Parsed {
            self.asked.lock().unwrap().push((
                req.provider.to_owned(),
                req.path.display().to_string(),
                req.file_name.to_owned(),
            ));
            (self.answer)(req)
        }
    }

    fn fake<F: Fn(&ParseRequest<'_>) -> Parsed + Send + Sync>(answer: F) -> Fake<F> {
        Fake {
            answer,
            asked: Mutex::default(),
        }
    }

    fn auth(id: &str, provider: &str, metadata: Json, attributes: &[(&str, &str)]) -> ParsedAuth {
        ParsedAuth {
            provider: provider.into(),
            id: id.into(),
            metadata: metadata.as_object().cloned().unwrap_or_default(),
            attributes: attributes.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    const DIR: &str = "/auth";

    fn synth(parser: &dyn PluginAuthParser, name: &str, raw: &str) -> Result<Option<FileCredentials>, String> {
        file_credentials(
            Some(Ask::Live(parser)),
            Path::new(DIR),
            &Path::new(DIR).join(name),
            raw.as_bytes(),
        )
    }

    fn plugin_creds(parser: &dyn PluginAuthParser, name: &str, raw: &str) -> Vec<Credential> {
        match synth(parser, name, raw) {
            Ok(Some(FileCredentials::Plugin(all))) => all,
            other => panic!("{raw}: want a plugin result, got {:?}", other.map(|o| o.is_some())),
        }
    }

    /// Go `TestSynthesizeAuthFileExpandsPluginMultiAuths`.
    #[test]
    fn expands_plugin_multi_auths() {
        let parser = fake(|_| {
            let headers = json!({"X-Test": "value"});
            Parsed::Handled(vec![
                auth(
                    "geminicli.json",
                    "gemini-cli",
                    json!({"type": "gemini-cli", "headers": headers}),
                    &[],
                ),
                auth(
                    "geminicli-project-a.json",
                    "gemini-cli",
                    json!({"type": "gemini-cli", "project_id": "project-a", "headers": headers}),
                    &[],
                ),
            ])
        });
        let raw = r#"{"type":"gemini-cli","excluded_models":["model-a"],"headers":{"X-Test":"value"}}"#;
        let auths = plugin_creds(&parser, "geminicli.json", raw);
        let full = "/auth/geminicli.json";
        assert_eq!(
            *parser.asked.lock().unwrap(),
            [("gemini-cli".to_owned(), full.to_owned(), "geminicli.json".to_owned())]
        );
        assert_eq!(auths.len(), 2);
        let (first, second) = (auth_index(&auths[0]), auth_index(&auths[1]));
        assert!(!first.is_empty() && first != second, "auth indexes {first}/{second}");
        for a in &auths {
            let attr = |k: &str| a.attributes.get(k).map(String::as_str);
            assert_eq!(attr("plugin_virtual"), Some("true"));
            assert_eq!(attr("virtual_source"), Some(full));
            assert_eq!((attr("path"), attr("source")), (Some(full), Some(full)));
            assert_eq!(attr("header:X-Test"), Some("value"));
            assert_eq!(attr("auth_kind"), Some("oauth"));
            assert_eq!(attr("excluded_models"), Some("model-a"));
            assert_eq!(a.source, Source::File(full.into()));
        }
        assert_eq!(auths[1].metadata["project_id"], "project-a");
        assert_eq!(
            auths[1].attributes["auth_index_seed"],
            "gemini-cli|/auth/geminicli.json|geminicli-project-a.json|1"
        );
    }

    /// A single parsed auth is not virtual: it owns its file.
    #[test]
    fn a_single_plugin_auth_is_not_virtual() {
        let parser = fake(|_| Parsed::Handled(vec![auth("plugin.json", "acme", json!({}), &[])]));
        let auths = plugin_creds(
            &parser,
            "plugin.json",
            r#"{"type":"acme","prefix":"/team/","proxy_url":" socks5://p "}"#,
        );
        assert_eq!(auths.len(), 1);
        let attrs = &auths[0].attributes;
        assert!(!attrs.contains_key("plugin_virtual") && !attrs.contains_key("auth_index_seed"));
        assert_eq!(attrs["prefix"], "team");
        assert_eq!(attrs["proxy_url"], "socks5://p");
    }

    /// Go `TestSynthesizeAuthFileAppliesSourcePriorityToPluginAuths`.
    #[test]
    fn applies_source_priority_to_plugin_auths() {
        for (raw, want, want_meta) in [
            (r#"{"type":"plugin","priority":1}"#, "1", json!(1)),
            (r#"{"type":"plugin","priority":" 2 "}"#, "2", json!(" 2 ")),
        ] {
            let parser = fake(|_| {
                Parsed::Handled(vec![
                    auth("first", "plugin", json!({"project_id": "first"}), &[]),
                    auth("second", "plugin", json!({"project_id": "second"}), &[]),
                ])
            });
            let auths = plugin_creds(&parser, "plugin.json", raw);
            assert_eq!(auths.len(), 2);
            for a in &auths {
                assert_eq!(a.attributes["priority"], want, "{raw}");
                assert_eq!(a.attributes["file_priority"], "true", "{raw}");
                assert_eq!(a.metadata["priority"], want_meta, "{raw}");
            }
        }
    }

    /// Go `ApplyAuthPriorityMetadata` makes only the attribute an integer: the metadata
    /// keeps the number as the file wrote it, a float stays a float (`-0.0` included).
    #[test]
    fn keeps_a_float_priority_in_the_metadata() {
        for (raw, want, want_meta) in [
            (r#"{"type":"plugin","priority":7.0}"#, "7", "7.0"),
            (r#"{"type":"plugin","priority":7e0}"#, "7", "7.0"),
            (r#"{"type":"plugin","priority":-0.0}"#, "0", "-0.0"),
        ] {
            let parser = fake(|_| Parsed::Handled(vec![auth("a", "plugin", json!({}), &[])]));
            let auths = plugin_creds(&parser, "plugin.json", raw);
            assert_eq!(auths[0].attributes["priority"], want, "{raw}");
            let meta = &auths[0].metadata["priority"];
            assert!(meta.is_f64(), "{raw}: {meta}");
            assert_eq!(serde_json::to_string(meta).unwrap(), want_meta, "{raw}");
        }
    }

    /// Go `TestSynthesizeAuthFileSkipsInvalidPluginAuthWeight`.
    #[test]
    fn skips_invalid_plugin_auth_weight() {
        let parser = fake(|_| {
            Parsed::Handled(vec![
                auth("invalid", "plugin", json!({}), &[("weight", "1.5")]),
                auth("valid", "plugin", json!({}), &[("weight", "0")]),
            ])
        });
        let auths = plugin_creds(&parser, "plugin.json", r#"{"type":"plugin"}"#);
        assert_eq!(auths.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(), ["valid"]);
    }

    /// Go `TestSynthesizeAuthFileAppliesSourceDisabledToPluginMultiAuths`.
    #[test]
    fn applies_source_disabled_to_plugin_multi_auths() {
        let parser = fake(|_| {
            Parsed::Handled(vec![
                auth("geminicli.json", "gemini-cli", json!({"type": "gemini-cli"}), &[]),
                auth(
                    "geminicli-project-a.json",
                    "gemini-cli",
                    json!({"project_id": "project-a"}),
                    &[],
                ),
            ])
        });
        let auths = plugin_creds(&parser, "geminicli.json", r#"{"type":"gemini-cli","disabled":true}"#);
        assert_eq!(auths.len(), 2);
        for a in &auths {
            assert!(a.disabled, "{}", a.id);
            assert_eq!(a.metadata["disabled"], true, "{}", a.id);
        }
    }

    /// Go `TestSynthesizeAuthFilePluginHandledEmptySuppressesBuiltin`.
    #[test]
    fn plugin_handled_empty_suppresses_builtin() {
        let parser = fake(|_| Parsed::Handled(Vec::new()));
        let raw = r#"{"type":"codex","access_token":"token"}"#;
        assert!(plugin_creds(&parser, "codex.json", raw).is_empty());
        let none = fake(|_| Parsed::NotHandled);
        assert!(matches!(
            synth(&none, "codex.json", raw),
            Ok(Some(FileCredentials::One(_)))
        ));
    }

    /// Go `TestSynthesizeAuthFileKeepsPluginExcludedModelsWithoutFileRules`.
    #[test]
    fn keeps_plugin_excluded_models_without_file_rules() {
        let parser = fake(|_| {
            Parsed::Handled(vec![auth(
                "plugin.json",
                "acme",
                json!({}),
                &[
                    ("excluded_models", "plugin-model"),
                    ("excluded_models_hash", "plugin-hash"),
                ],
            )])
        });
        for (raw, want) in [
            (r#"{"type":"acme"}"#, "plugin-model"),
            (r#"{"type":"acme","excluded_models":[]}"#, "plugin-model"),
            (r#"{"type":"acme","excluded_models":["File-Model"]}"#, "file-model"),
        ] {
            let auths = plugin_creds(&parser, "plugin.json", raw);
            assert_eq!(auths.len(), 1, "{raw}");
            assert_eq!(auths[0].attributes["excluded_models"], want, "{raw}");
        }
    }

    /// A file no plugin takes builds exactly what the built-in synthesizer builds; Go
    /// asks with the lowercased type, `gemini` as `gemini-cli`, and an empty type asks
    /// every auth provider. The file's own weight is still checked first.
    #[test]
    fn unhandled_files_take_the_builtin_path() {
        let parser = fake(|_| Parsed::NotHandled);
        for raw in [
            r#"{"type":" Claude ","email":"a@example.test","priority":3,"prefix":"p"}"#,
            r#"{"type":"gemini"}"#,
            r#"{"email":"x@example.test"}"#,
            "not json",
        ] {
            let path = Path::new(DIR).join("a.json");
            let plain = file_credentials(None, Path::new(DIR), &path, raw.as_bytes());
            let asked = synth(&parser, "a.json", raw);
            let show = |r: Result<Option<FileCredentials>, String>| match r {
                Ok(Some(FileCredentials::One(c))) => format!("{c:?}"),
                Ok(Some(FileCredentials::Plugin(all))) => format!("plugin {all:?}"),
                Ok(Some(FileCredentials::Deferred)) => "deferred".into(),
                Ok(Some(FileCredentials::Unapplied(all))) => format!("unapplied {all:?}"),
                Ok(None) => "none".into(),
                Err(e) => e,
            };
            assert_eq!(show(plain), show(asked), "{raw}");
        }
        let providers: Vec<String> = parser.asked.lock().unwrap().iter().map(|a| a.0.clone()).collect();
        assert_eq!(providers, ["claude", "gemini-cli", ""]);
        let err = synth(&parser, "a.json", r#"{"type":"acme","weight":1.5}"#);
        assert!(err.is_err());
        assert_eq!(
            parser.asked.lock().unwrap().len(),
            3,
            "an invalid file weight is never parsed"
        );
    }

    /// Go's scan with a plugin parser: files in name order (`os.ReadDir`), and the last
    /// auth under an ID wins, including several a response leaves without an ID (they
    /// all take the file name).
    #[test]
    fn plugin_scans_read_files_in_name_order_and_the_last_id_wins() {
        let dir = std::env::temp_dir().join(format!("plugin-scan-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Created out of name order.
        for name in ["c.json", "b.json", "a.json"] {
            std::fs::write(dir.join(name), r#"{"type":"acme"}"#).unwrap();
        }
        let parser = fake(|req| {
            let shared = |from: &str| auth("shared", "acme", json!({"from": from}), &[]);
            let unnamed = |n: u64| auth(req.file_name, "acme", json!({"n": n}), &[]);
            Parsed::Handled(match req.file_name {
                "a.json" => vec![shared("a"), unnamed(1), unnamed(2)],
                "b.json" => vec![shared("b")],
                _ => vec![shared("c")],
            })
        });
        let cfg = crate::config::Config {
            auth_dir: dir.clone(),
            ..Default::default()
        };
        let scan = scan_with(&cfg, Some(Ask::Live(&parser)));
        let asked: Vec<String> = parser.asked.lock().unwrap().iter().map(|a| a.2.clone()).collect();
        assert_eq!(asked, ["a.json", "b.json", "c.json"]);
        let got: Vec<(String, Json)> = scan
            .credentials
            .iter()
            .map(|c| (c.id.clone(), Json::Object(c.metadata.clone())))
            .collect();
        assert_eq!(
            got,
            [
                ("a.json".to_owned(), json!({"n": 2})),
                ("shared".to_owned(), json!({"from": "c"})),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan callers run under the disk lock asks no plugin: it takes what a fill
    /// cached for each file's current bytes, and defers a changed or new file (kept out
    /// of `read`, so the watcher reloads it) until a fill answers for it.
    #[test]
    fn a_locked_scan_only_uses_answers_cached_for_the_current_bytes() {
        let _state = plugin_auth_state();
        let dir = std::env::temp_dir().join(format!("plugin-scan-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.json", "b.json"] {
            std::fs::write(dir.join(name), r#"{"type":"acme"}"#).unwrap();
        }
        let parser = fake(|req| Parsed::Handled(vec![auth(req.file_name, "acme", json!({}), &[])]));
        let cfg = crate::config::Config {
            auth_dir: dir.clone(),
            ..Default::default()
        };
        let asked = || -> Vec<String> { parser.asked.lock().unwrap().iter().map(|a| a.2.clone()).collect() };
        let ids =
            |scan: &super::super::AuthScan| -> Vec<String> { scan.credentials.iter().map(|c| c.id.clone()).collect() };
        let names = |paths: Vec<&PathBuf>| -> Vec<String> {
            let mut names: Vec<String> = paths
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into())
                .collect();
            names.sort();
            names
        };
        scan_with(&cfg, Some(Ask::Fill(&parser)));
        let scan = scan_with(&cfg, Some(Ask::Cached));
        assert_eq!(ids(&scan), ["a.json", "b.json"]);
        assert!(scan.deferred.is_empty());
        assert_eq!(asked(), ["a.json", "b.json"]);

        std::fs::write(dir.join("b.json"), r#"{"type":"acme","n":1}"#).unwrap();
        std::fs::write(dir.join("c.json"), r#"{"type":"acme"}"#).unwrap();
        let scan = scan_with(&cfg, Some(Ask::Cached));
        assert_eq!(ids(&scan), ["a.json"]);
        assert_eq!(names(scan.deferred.iter().collect()), ["b.json", "c.json"]);
        assert_eq!(names(scan.read.keys().collect()), ["a.json"]);
        assert_eq!(asked().len(), 2, "the locked scan asked a plugin");

        scan_with(&cfg, Some(Ask::Fill(&parser)));
        assert_eq!(
            asked(),
            ["a.json", "b.json", "b.json", "c.json"],
            "only changed files are asked again"
        );
        assert_eq!(ids(&scan_with(&cfg, Some(Ask::Cached))), ["a.json", "b.json", "c.json"]);

        // A plugin change retires every answer; setting the same providers again does not.
        set_plugin_auth_providers(Some(1));
        let scan = scan_with(&cfg, Some(Ask::Cached));
        assert!(scan.credentials.is_empty());
        scan_with(&cfg, Some(Ask::Fill(&parser)));
        set_plugin_auth_providers(Some(1));
        assert_eq!(scan_with(&cfg, Some(Ask::Cached)).credentials.len(), 3);
        set_plugin_auth_providers(Some(2));
        let scan = scan_with(&cfg, Some(Ask::Cached));
        assert!(scan.credentials.is_empty());
        assert_eq!(scan.deferred.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bytes written to a file while a plugin parses it: what that parse's own
    /// `host.auth.save` wrote (noted under any spelling of the path) publishes the
    /// parse's answer once, unapplied; any other write (a native refresh) defers the
    /// file. Neither caches the answer for the new bytes: the next fill parses them.
    #[test]
    fn only_a_parses_own_write_publishes_its_answer_and_neither_is_cached() {
        let _state = plugin_auth_state();
        let dir = std::env::temp_dir().join(format!("plugin-own-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        for name in ["own.json", "native.json"] {
            std::fs::write(dir.join(name), r#"{"type":"acme","v":0}"#).unwrap();
        }
        set_plugin_auth_providers(Some(7));
        let parser = fake(|req| {
            let v1 = br#"{"type":"acme","v":1}"#;
            if req.raw_json != v1 {
                std::fs::write(req.path, v1).unwrap();
                if req.file_name == "own.json" {
                    let spelled = req.path.parent().unwrap().join("sub/../own.json");
                    note_plugin_auth_write(&spelled, v1);
                }
            }
            let v = if req.raw_json == v1 { "v1" } else { "v0" };
            Parsed::Handled(vec![auth(&format!("{}-{v}", req.file_name), "acme", json!({}), &[])])
        });
        let cfg = crate::config::Config {
            auth_dir: dir.clone(),
            ..Default::default()
        };
        scan_with(&cfg, Some(Ask::Fill(&parser)));
        let scan = scan_with(&cfg, Some(Ask::Cached));
        let ids: Vec<&str> = scan.credentials.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["own.json-v0"], "the parse's answer for its own write");
        assert_eq!(scan.unapplied.iter().collect::<Vec<_>>(), [&dir.join("own.json")]);
        assert!(scan.read.contains_key(&dir.join("own.json")));
        assert_eq!(scan.deferred.iter().collect::<Vec<_>>(), [&dir.join("native.json")]);
        // The next fill parses both files' current bytes.
        scan_with(&cfg, Some(Ask::Fill(&parser)));
        let scan = scan_with(&cfg, Some(Ask::Cached));
        let mut ids: Vec<&str> = scan.credentials.iter().map(|c| c.id.as_str()).collect();
        ids.sort();
        assert_eq!(ids, ["native.json-v1", "own.json-v1"]);
        assert!(scan.unapplied.is_empty() && scan.deferred.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
