//! Model catalog updaters (Go internal/registry/catalog_sources.go): one updater each
//! for models.json, the Codex client catalog and the Devin catalog. Each reads its
//! effective source ([`effective_sources`]) now and every three hours, validates what it
//! read, and publishes it only while that source is still the configured one. A failed
//! read or an invalid catalog keeps the last valid one. A config reload that changes a
//! source switches the updater at once; a read still running for the old source is
//! dropped.
//!
//! A source is an absolute local path, an http(s) URL, empty for the router-for-me
//! downloads, `embed` (the catalog built into the binary, published once) or `disabled`
//! (no updates; the published catalog stays). Remote reads use Go's `http.DefaultClient`
//! rules (environment proxy, Go's redirects) and are bounded to 30 seconds; Go sets no
//! timeout.
//!
//! The empty source of the Codex client and Devin catalogs starts lazily outside Home
//! mode: with the first Codex or Devin credential, or (Codex) the first request from a
//! Codex client that needs the catalog. Go starts them at launch. The Antigravity client
//! version updater likewise starts with the first Antigravity credential.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::Runtime;

/// Go `modelsURLs`, tried in order.
pub const MODELS_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/models.json",
    "https://models.router-for.me/models.json",
];
/// Go `codexClientModelsURLs`.
const CODEX_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/codex_client_models.json",
    "https://models.router-for.me/codex_client_models.json",
];
/// Go `devinModelsURLs`.
const DEVIN_URLS: [&str; 2] = [
    "https://raw.githubusercontent.com/router-for-me/models/refs/heads/main/devin_models.json",
    "https://models.router-for.me/devin_models.json",
];

/// Go `ModelsRefreshInterval`.
const REFRESH_INTERVAL: Duration = Duration::from_secs(3 * 3600);
/// Go `maxCodexClientModelsSize`, the cap on every catalog read.
const MAX_SIZE: usize = 8 << 20;
// ponytail: Go's DefaultClient has no timeout, so a stalled source holds its updater
// until the next reconfigure; 30 seconds per remote read is the bound here.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Go `embeddedCatalogSource`.
const EMBED: &str = "embed";
const DISABLED: &str = "disabled";

type Fetch = Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>> + Send + Sync>;
type Publish = Box<dyn Fn(&[u8]) -> Result<(), String> + Send + Sync>;
type Validate = fn(&[u8]) -> Result<(), String>;

/// A service lifetime (the context Go hands `StartModelCatalogUpdaters`): cancelling it
/// stops every loop started under it, and a reload after that starts nothing.
#[derive(Clone)]
pub(crate) struct Lifetime {
    id: u64,
    token: CancellationToken,
}

impl Lifetime {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            token: CancellationToken::new(),
        }
    }
}

#[derive(Default)]
struct State {
    source: String,
    generation: u64,
    /// The lifetime the current source runs under, and the token that stops it.
    run: Option<(u64, CancellationToken)>,
}

/// Go `catalogUpdater`: source changes are serialized with publication, not with reads.
pub(crate) struct Updater {
    state: Mutex<State>,
    /// The empty source is configured but waits for [`Updater::wake`].
    pending: AtomicBool,
    fetch: Fetch,
    publish: Publish,
}

impl Updater {
    fn new(fetch: Fetch, publish: Publish) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            pending: AtomicBool::new(false),
            fetch,
            publish,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `catalogUpdater.configure`: nothing when `source` already runs under
    /// `lifetime`; otherwise stops the old loop and starts one for `source` (none for
    /// `disabled`, and none yet when `lazy`).
    fn configure(self: &Arc<Self>, lifetime: &Lifetime, source: &str, lazy: bool) {
        let mut state = self.lock();
        if let Some((id, token)) = &state.run {
            if state.source == source && *id == lifetime.id && !token.is_cancelled() {
                return;
            }
            token.cancel();
        }
        state.generation += 1;
        state.source = source.to_owned();
        let token = lifetime.token.child_token();
        state.run = Some((lifetime.id, token.clone()));
        let pending = lazy && source != DISABLED;
        self.pending.store(pending, Ordering::Release);
        if source == DISABLED || pending {
            return;
        }
        self.spawn(token, source.to_owned(), state.generation);
    }

    /// Starts a source [`Updater::configure`] left waiting. Cost when none waits: one
    /// atomic load.
    fn wake(self: &Arc<Self>) {
        if !self.pending.load(Ordering::Acquire) {
            return;
        }
        let state = self.lock();
        if !self.pending.swap(false, Ordering::AcqRel) {
            return;
        }
        if let Some((_, token)) = &state.run {
            self.spawn(token.clone(), state.source.clone(), state.generation);
        }
    }

    fn spawn(self: &Arc<Self>, token: CancellationToken, source: String, generation: u64) {
        let this = self.clone();
        tokio::spawn(async move {
            let run = async {
                this.refresh(&token, &source, generation).await;
                if source == EMBED {
                    return;
                }
                let start = tokio::time::Instant::now() + REFRESH_INTERVAL;
                let mut ticker = tokio::time::interval_at(start, REFRESH_INTERVAL);
                loop {
                    ticker.tick().await;
                    this.refresh(&token, &source, generation).await;
                }
            };
            token.run_until_cancelled(run).await;
        });
    }

    /// Go `catalogUpdater.refresh`: publishes only while `generation` is current.
    async fn refresh(&self, token: &CancellationToken, source: &str, generation: u64) {
        let data = match (self.fetch)(source.to_owned()).await {
            Ok(data) => data,
            Err(_) => {
                if !token.is_cancelled() {
                    tracing::warn!("model catalog refresh failed; keeping last valid catalog");
                }
                return;
            }
        };
        let state = self.lock();
        if state.generation != generation || token.is_cancelled() {
            return;
        }
        let published = (self.publish)(&data);
        drop(state);
        if published.is_err() {
            tracing::warn!("model catalog rejected; keeping last valid catalog");
        }
    }
}

/// Why one catalog source failed. Only this category is logged: the source, an error
/// text or a redirect target can carry credentials (Go logs nothing more either).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadError {
    File,
    Request,
    /// A source Go reads that would not go out as Go sends it
    /// ([`cpa_exec::proxy::go_target`]).
    Rewritten,
    Status(u16),
    Body,
    TooLarge,
    Invalid,
}

/// The first request Go sends for `source`, when it would go out as Go sends it
/// ([`cpa_exec::proxy::go_target`]); http.NewRequest drops the first hop's empty port.
fn go_target_of(source: &str) -> Option<axum::http::Uri> {
    let mut url = cpa_core::config::go_url::Escaped::parse(source)?;
    url.remove_empty_port();
    cpa_exec::proxy::go_target(&url)
}

/// Go `readCatalogSource`: an absolute path is read from disk, anything else is fetched
/// with Go's request target (`URL.RequestURI`) and must answer 200. Either is capped at
/// 8 MiB.
async fn read_source(source: &str) -> Result<Vec<u8>, ReadError> {
    let data = if cpa_core::config::go_is_abs(source, cfg!(windows)) {
        use tokio::io::AsyncReadExt;
        let file = tokio::fs::File::open(source).await.map_err(|_| ReadError::File)?;
        let mut data = Vec::new();
        file.take(MAX_SIZE as u64 + 1)
            .read_to_end(&mut data)
            .await
            .map_err(|_| ReadError::File)?;
        data
    } else {
        if go_target_of(source).is_none() {
            return Err(ReadError::Rewritten);
        }
        // A client per read: reads are hours apart and the client is tied to no runtime.
        let client = cpa_exec::proxy::default_client();
        let mut headers = cpa_exec::proxy::GoHeaders::new();
        headers.exact_target();
        let response = cpa_exec::proxy::request(&client, wreq::Method::GET, source, headers, None, Some(FETCH_TIMEOUT))
            .await
            .map_err(|_| ReadError::Request)?;
        if response.status != 200 {
            return Err(ReadError::Status(response.status));
        }
        cpa_exec::proxy::read_all(response.body, MAX_SIZE + 1, false)
            .await
            .map_err(|_| ReadError::Body)?
            .to_vec()
    };
    if data.len() > MAX_SIZE {
        return Err(ReadError::TooLarge);
    }
    Ok(data)
}

/// Go `catalogFetcher`: `embed` is the built-in catalog, the empty source tries
/// `defaults` in order, and any other source is the only one tried. A candidate must
/// pass `validate`.
fn fetcher(embedded: &'static [u8], defaults: Vec<String>, validate: Validate) -> Fetch {
    let defaults: Arc<[String]> = defaults.into();
    Arc::new(move |source: String| {
        let defaults = defaults.clone();
        Box::pin(async move {
            if source == EMBED {
                return Ok(embedded.to_vec());
            }
            let candidates = if source.is_empty() {
                defaults.to_vec()
            } else {
                vec![source]
            };
            for candidate in candidates {
                let read = read_source(&candidate).await;
                match read.and_then(|data| validate(&data).map(|()| data).map_err(|_| ReadError::Invalid)) {
                    Ok(data) => return Ok(data),
                    Err(reason) => tracing::debug!(?reason, "model catalog source failed"),
                }
            }
            Err("no valid model catalog source".to_owned())
        })
    })
}

fn owned(urls: &[&str]) -> Vec<String> {
    urls.iter().map(|u| (*u).to_owned()).collect()
}

fn general_fetcher(defaults: Vec<String>) -> Fetch {
    fetcher(cpa_core::registry::MODELS_JSON.as_bytes(), defaults, |data| {
        cpa_core::registry::validate_catalog_json(&String::from_utf8_lossy(data))
    })
}

/// Go `publishCatalogBytes`: a changed section rebuilds the registries that use it.
fn publish_general(data: &[u8]) -> Result<(), String> {
    let changed = cpa_core::registry::refresh_catalog(&String::from_utf8_lossy(data))?;
    if !changed.is_empty() {
        tracing::info!("model catalog updated, changes detected for providers: {changed:?}");
    }
    Ok(())
}

/// The process-wide updaters: models.json, the Codex client catalog, the Devin catalog.
static CATALOGS: LazyLock<[Arc<Updater>; 3]> = LazyLock::new(|| {
    [
        Updater::new(general_fetcher(owned(&MODELS_URLS)), Box::new(publish_general)),
        Updater::new(
            fetcher(
                cpa_common::codex_client::CLIENT_MODELS_JSON.as_bytes(),
                owned(&CODEX_URLS),
                cpa_common::codex_catalog::validate,
            ),
            Box::new(|data| cpa_common::codex_catalog::load(data).map(drop)),
        ),
        Updater::new(
            fetcher(cpa_core::registry::devin::EMBEDDED, owned(&DEVIN_URLS), |data| {
                cpa_core::registry::devin::validate(data).map(drop)
            }),
            Box::new(|data| cpa_core::registry::devin::global().load(data, "catalog").map(drop)),
        ),
    ]
});

/// Go `effectiveCatalogSources` over `[catalog, codex-catalog, devin-catalog]`: an
/// empty source is `embed` under `--local-model`, and Home mode disables models.json and
/// the Devin catalog whatever their source.
pub(crate) fn effective_sources(sources: &[String; 3], local: bool, home: bool) -> [String; 3] {
    let mut effective = sources
        .clone()
        .map(|s| if s.is_empty() && local { EMBED.to_owned() } else { s });
    if home {
        effective[0] = DISABLED.to_owned();
        effective[2] = DISABLED.to_owned();
    }
    effective
}

#[derive(Clone, Copy)]
struct Policy {
    local: bool,
    home: bool,
    /// A test runtime: the empty source never downloads (see [`test_runtime`]).
    deny_default: bool,
}

/// Go `configureCatalogs` for sources the config load already validated. At startup the
/// empty Codex and Devin sources wait outside Home mode.
fn configure(updaters: &[Arc<Updater>; 3], lifetime: &Lifetime, sources: &[String; 3], policy: Policy, startup: bool) {
    // Go `UpdateModelCatalogSources`: nothing after shutdown.
    if lifetime.token.is_cancelled() {
        return;
    }
    let effective = effective_sources(sources, policy.local, policy.home);
    for (index, (updater, source)) in updaters.iter().zip(effective).enumerate() {
        let source = if policy.deny_default && source.is_empty() {
            DISABLED.to_owned()
        } else {
            source
        };
        let lazy = startup && !policy.home && index > 0 && source.is_empty();
        updater.configure(lifetime, &source, lazy);
    }
}

/// The catalog policy of one runtime's lifetime (Go's `catalogRuntime` and service
/// context). Dropped with the runtime, which stops its readers.
pub(crate) struct Session {
    lifetime: Lifetime,
    policy: Policy,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.lifetime.token.cancel();
    }
}

impl Session {
    /// Go `StartModelCatalogUpdaters` (`startup`) and `UpdateModelCatalogSources`: called
    /// by the runtime under its commit mutex, so commits reconfigure in their order.
    pub(crate) fn configure(&self, config: &cpa_core::config::Config, startup: bool) {
        configure(
            &CATALOGS,
            &self.lifetime,
            &config.catalog_sources(),
            self.policy,
            startup,
        );
    }
}

/// Go `startModelCatalogUpdaters` and `misc.StartAntigravityVersionUpdater`, for the
/// lifetime of `rt`: configures the catalogs from the current config, and every later
/// config commit configures them again ([`Runtime::publish_config_and_policy`]). Call
/// once per runtime. Cost while no Codex, Devin or Antigravity credential exists: one
/// parked task that looks at the credentials' providers after each change to the set.
/// In Home mode the local store stays empty, so every updater starts at once, as in Go.
pub fn start(rt: &Arc<Runtime>, home_enabled: bool) {
    let deny_default = test_runtime(rt);
    let policy = Policy {
        local: rt.local_model(),
        home: home_enabled,
        deny_default,
    };
    rt.start_catalogs(Session {
        lifetime: Lifetime::new(),
        policy,
    });
    if home_enabled {
        tracing::info!(
            "Home mode: models.json and Devin catalog updates disabled; Codex client model list follows Home model IDs"
        );
    }
    if deny_default {
        return;
    }
    // Updaters that serve one provider, each started by that provider's first credential.
    let mut wanted: Vec<(&'static str, fn())> = vec![
        ("codex", || CATALOGS[1].wake()),
        (cpa_exec::devin::PROVIDER, || CATALOGS[2].wake()),
        // The client version Antigravity requests carry; Go refreshes it whatever the plan.
        (cpa_exec::antigravity::PROVIDER, cpa_exec::antigravity_version::start),
    ];
    if home_enabled {
        for (_, start) in wanted {
            start();
        }
        return;
    }
    let weak = Arc::downgrade(rt);
    let mut changes = rt.store().subscribe();
    tokio::spawn(async move {
        loop {
            {
                let Some(rt) = weak.upgrade() else { return };
                changes.borrow_and_update();
                for c in rt.store().snapshot().iter() {
                    wanted.retain(|(provider, start)| {
                        let found = c.provider == *provider;
                        if found {
                            start();
                        }
                        !found
                    });
                }
            }
            if wanted.is_empty() || changes.changed().await.is_err() {
                return;
            }
        }
    });
}

/// A Codex client asked for its model catalog: start the default source if it waits.
pub(crate) fn codex_client_catalog_wanted() {
    CATALOGS[1].wake();
}

/// A runtime built by [`crate::testing::runtime`] never downloads the router-for-me
/// catalogs: tests stay off the network and keep the embedded data. Explicit sources
/// (files and loopback mocks) still run.
fn test_runtime(rt: &Runtime) -> bool {
    rt.deny_external.load(Ordering::Relaxed)
}

/// One models.json refresh from the first of `urls` that serves a valid catalog (the
/// empty source's reader with other defaults). For tests against local mocks.
pub async fn refresh(urls: &[String]) {
    match general_fetcher(urls.to_vec())(String::new()).await {
        Ok(data) => {
            if publish_general(&data).is_err() {
                tracing::warn!("model catalog rejected; keeping last valid catalog");
            }
        }
        Err(_) => tracing::warn!("model catalog refresh failed; keeping last valid catalog"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// An updater whose read returns the source's name and whose publications arrive on
    /// the returned channel.
    fn echo() -> (Arc<Updater>, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        let fetch: Fetch = Arc::new(|source: String| Box::pin(async move { Ok(source.into_bytes()) }));
        let publish: Publish = Box::new(move |data| {
            tx.send(String::from_utf8_lossy(data).into_owned()).unwrap();
            Ok(())
        });
        (Updater::new(fetch, publish), rx)
    }

    async fn next(rx: &mut UnboundedReceiver<String>) -> String {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("catalog source was not updated")
            .unwrap()
    }

    fn source(u: &Updater) -> String {
        u.lock().source.clone()
    }

    fn generation(u: &Updater) -> u64 {
        u.lock().generation
    }

    /// Go TestEffectiveCatalogSources.
    #[test]
    fn effective_sources_match_go() {
        let url = "https://example.com/catalog.json";
        for local in [false, true] {
            for home in [false, true] {
                for mask in 0..8 {
                    let sources: [String; 3] = std::array::from_fn(|i| {
                        if mask & (1 << i) != 0 {
                            url.to_owned()
                        } else {
                            String::new()
                        }
                    });
                    let got = effective_sources(&sources, local, home);
                    for (index, given) in sources.iter().enumerate() {
                        let mut want = if given.is_empty() && local {
                            EMBED
                        } else {
                            given.as_str()
                        };
                        if home && index != 1 {
                            want = DISABLED;
                        }
                        assert_eq!(
                            got[index], want,
                            "local={local} home={home} mask={mask} catalog={index}"
                        );
                    }
                }
            }
        }
    }

    /// Go TestCatalogSourceSwitchRejectsStalePublication: a read that finishes after the
    /// source changed is not published, and the new source does not wait for it.
    #[tokio::test]
    async fn source_switch_rejects_stale_publication() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Mutex::new(Some(started_tx));
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let fetch: Fetch = Arc::new(move |source: String| {
            let started = (source == "old").then(|| started_tx.lock().unwrap().take()).flatten();
            let release = held.clone();
            Box::pin(async move {
                if let Some(started) = started {
                    started.send(()).unwrap();
                    release.notified().await;
                }
                Ok(source.into_bytes())
            })
        });
        let (tx, mut published) = unbounded_channel();
        let u = Updater::new(
            fetch,
            Box::new(move |data| {
                tx.send(String::from_utf8_lossy(data).into_owned()).unwrap();
                Ok(())
            }),
        );
        u.lock().generation = 1;
        let lifetime = Lifetime::new();
        let old = {
            let (u, token) = (u.clone(), lifetime.token.clone());
            tokio::spawn(async move { u.refresh(&token, "old", 1).await })
        };
        started_rx.await.unwrap();
        u.configure(&lifetime, "new", false);
        assert_eq!(next(&mut published).await, "new");
        release.notify_one();
        old.await.unwrap();
        assert!(published.try_recv().is_err(), "stale publication");
        u.configure(&lifetime, "new", false);
        assert_eq!(generation(&u), 2, "unchanged source restarted");
    }

    /// Go TestCatalogRefreshKeepsLastValidData and TestDisabledCatalogDoesNotFetch.
    #[tokio::test]
    async fn failures_keep_the_last_valid_catalog_and_disabled_reads_nothing() {
        let last = Arc::new(Mutex::new("last valid".to_owned()));
        let keep = |fetch: Fetch, ok: bool| {
            let last = last.clone();
            Updater::new(
                fetch,
                Box::new(move |data| {
                    if !ok {
                        return Err("invalid".into());
                    }
                    *last.lock().unwrap() = String::from_utf8_lossy(data).into_owned();
                    Ok(())
                }),
            )
        };
        let token = CancellationToken::new();
        let unavailable: Fetch = Arc::new(|_| Box::pin(async { Err("unavailable".to_owned()) }));
        keep(unavailable, true).refresh(&token, "", 0).await;
        let invalid: Fetch = Arc::new(|_| Box::pin(async { Ok(b"invalid".to_vec()) }));
        keep(invalid, false).refresh(&token, "", 0).await;
        assert_eq!(*last.lock().unwrap(), "last valid");

        let reads = Arc::new(AtomicU64::new(0));
        let counted = reads.clone();
        let fetch: Fetch = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Vec::new()) })
        });
        let u = keep(fetch, true);
        u.configure(&Lifetime::new(), DISABLED, false);
        u.wake();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!((source(&u), reads.load(Ordering::SeqCst)), (DISABLED.to_owned(), 0));
    }

    /// Go TestCatalogFetcherSources over local mocks and files: `embed`, a URL and a
    /// path each serve their catalog; an invalid or missing explicit source fails without
    /// falling back to the defaults; the empty source reads the defaults; a file is
    /// read again each time.
    #[tokio::test]
    async fn fetcher_reads_each_kind_of_source() {
        use axum::routing::get;
        let catalogs: [(&str, &'static [u8], Validate); 3] = [
            ("general", cpa_core::registry::MODELS_JSON.as_bytes(), |d| {
                cpa_core::registry::validate_catalog_json(&String::from_utf8_lossy(d))
            }),
            (
                "codex",
                cpa_common::codex_client::CLIENT_MODELS_JSON.as_bytes(),
                cpa_common::codex_catalog::validate,
            ),
            ("devin", cpa_core::registry::devin::EMBEDDED, |d| {
                cpa_core::registry::devin::validate(d).map(drop)
            }),
        ];
        let dir = std::env::temp_dir().join(format!("cpa-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, data, validate) in catalogs {
            let official_hits = Arc::new(AtomicU64::new(0));
            let hits = official_hits.clone();
            let app = axum::Router::new()
                .route(
                    "/official",
                    get(move || {
                        hits.fetch_add(1, Ordering::SeqCst);
                        async move { data }
                    }),
                )
                .route("/custom", get(move || async move { data }))
                .route("/broken", get(|| async { "invalid" }))
                .route("/missing", get(|| async { (axum::http::StatusCode::NOT_FOUND, "") }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let path = dir.join(format!("{name}.json"));
            std::fs::write(&path, data).unwrap();
            let path = path.to_str().unwrap().to_owned();
            let fetch = fetcher(data, vec![format!("{base}/official")], validate);
            for source in [EMBED.to_owned(), format!("{base}/custom"), path.clone()] {
                assert_eq!(fetch(source.clone()).await.as_deref(), Ok(data), "{name}: {source}");
            }
            for source in [
                format!("{base}/broken"),
                format!("{base}/missing"),
                format!("{path}.missing"),
            ] {
                assert!(fetch(source.clone()).await.is_err(), "{name}: accepted {source}");
            }
            assert_eq!(
                official_hits.load(Ordering::SeqCst),
                0,
                "{name}: fell back to the default"
            );
            assert_eq!(fetch(String::new()).await.as_deref(), Ok(data), "{name}");
            assert_eq!(official_hits.load(Ordering::SeqCst), 1, "{name}: default not read");
            std::fs::write(&path, "invalid").unwrap();
            assert!(fetch(path.clone()).await.is_err(), "{name}: file was not read again");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    async fn serve(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    /// A remote source goes out with Go's request target (`URL.RequestURI`, recorded
    /// with Go 1.26.4): dot segments as written, the path escaped as Go escapes it, and
    /// each redirect resolved and escaped as Go's client does (more cases in cpa-exec's
    /// Go-recorded redirect replay). A source that would not go out so is refused
    /// without a request.
    #[tokio::test]
    async fn sources_are_requested_as_go_requests_them() {
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let record = seen.clone();
        let app = axum::Router::new().fallback(move |request: axum::extract::Request| {
            let uri = request.uri().to_string();
            let auth = request
                .headers()
                .get("authorization")
                .map(|v| v.to_str().unwrap().to_owned());
            record.lock().unwrap().push(match auth {
                Some(auth) => format!("{uri} {auth}"),
                None => uri.clone(),
            });
            async move {
                use axum::response::IntoResponse;
                let location = match uri.as_str() {
                    "/dots" => "/x/%2E%2e/catalog.json",
                    "/backslash" => "/a\\..\\catalog.json",
                    "/tab" => "/a/\t../catalog.json",
                    _ => return cpa_core::registry::devin::EMBEDDED.into_response(),
                };
                (
                    axum::http::StatusCode::FOUND,
                    [(axum::http::header::LOCATION, location)],
                )
                    .into_response()
            }
        });
        let addr = serve(app).await;
        let fetch = fetcher(cpa_core::registry::devin::EMBEDDED, Vec::new(), |d| {
            cpa_core::registry::devin::validate(d).map(drop)
        });
        for path in [
            "/a/../catalog.json",
            "/my catalogs/../catalog.json",
            "/mod\u{e8}les/catalog.json",
            "/a%2Fb/x|y.json",
        ] {
            assert!(fetch(format!("http://{addr}{path}")).await.is_ok(), "{path}");
        }
        assert!(
            fetch(format!("http://u%3A:p@{addr}/a/../catalog.json?v=1"))
                .await
                .is_ok()
        );
        // Go requests these; the URL would not go out as Go sends it.
        for source in [
            "http://b\u{fc}cher.example/a/../catalog.json",
            "http://[fe80::1%25en0]:8080/catalog.json",
            "http://127.0.0.1:1/catalog.json?q=a b",
            // Go sends Host: 127.0.0.1:80 (and :080, :443); wreq would drop the port.
            "http://127.0.0.1:80/catalog.json",
            "http://127.0.0.1:080/catalog.json",
            "https://127.0.0.1:443/catalog.json",
        ] {
            assert_eq!(read_source(source).await, Err(ReadError::Rewritten), "{source}");
        }
        // http.NewRequest drops the first hop's empty port, so Go's Host header has none
        // either: the source is accepted. Not sent: port 80 belongs to no test.
        assert!(go_target_of("http://127.0.0.1:/catalog.json").is_some());
        for path in ["/dots", "/backslash"] {
            assert!(fetch(format!("http://{addr}{path}")).await.is_ok(), "{path}");
        }
        // Go's url.Parse fails on the tab; WHATWG would strip it and request /catalog.json.
        assert!(fetch(format!("http://{addr}/tab")).await.is_err());
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "/a/../catalog.json",
                "/my%20catalogs/../catalog.json",
                "/mod%C3%A8les/catalog.json",
                "/a/b/x%7Cy.json",
                // Basic "u::p", as Go sends the unescaped user info.
                "/a/../catalog.json?v=1 Basic dTo6cA==",
                "/dots",
                "/x/%2E%2e/catalog.json",
                "/backslash",
                "/a%5C..%5Ccatalog.json",
                "/tab",
            ]
        );
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A failed read logs its category only: neither the source's user info and query
    /// nor a malformed redirect target reach a DEBUG log.
    #[tokio::test]
    async fn failed_reads_log_no_url() {
        use axum::http::{StatusCode, header};
        let target = "http://alice:SECRET@[invalid/catalog.json?token=SECRET";
        let app =
            axum::Router::new().fallback(move || async move { (StatusCode::FOUND, [(header::LOCATION, target)]) });
        let addr = serve(app).await;
        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let fetch = fetcher(b"", Vec::new(), |_| Ok(()));
        assert!(fetch(format!("http://bob:SECRET@{addr}/r?token=SECRET")).await.is_err());
        let u = Updater::new(fetch, Box::new(|_| Ok(())));
        u.refresh(&CancellationToken::new(), &format!("http://{addr}/r?token=SECRET"), 0)
            .await;
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("model catalog source failed"), "{logs}");
        assert!(logs.contains("model catalog refresh failed"), "{logs}");
        assert!(!logs.contains("SECRET"), "{logs}");
    }

    /// Go TestCatalogPolicyHotReload, then the lazy default: at startup outside Home the
    /// empty Codex and Devin sources wait for a wake, and a reload that changes one starts
    /// it at once.
    #[tokio::test]
    async fn policy_follows_reloads_and_lifetimes() {
        let (general, mut general_rx) = echo();
        let (codex, mut codex_rx) = echo();
        let (devin, mut devin_rx) = echo();
        let updaters = [general.clone(), codex.clone(), devin.clone()];
        let sources = |g: &str, c: &str, d: &str| [g.to_owned(), c.to_owned(), d.to_owned()];
        let policy = |local, home| Policy {
            local,
            home,
            deny_default: false,
        };
        let lifetime = Lifetime::new();
        let (url, codex_url) = ("https://example.com/general", "https://example.com/codex");
        configure(&updaters, &lifetime, &sources(url, "", ""), policy(true, false), true);
        assert_eq!(next(&mut general_rx).await, url);
        assert_eq!(next(&mut codex_rx).await, EMBED);
        assert_eq!(next(&mut devin_rx).await, EMBED);
        configure(
            &updaters,
            &lifetime,
            &sources("", codex_url, ""),
            policy(true, false),
            false,
        );
        assert_eq!(next(&mut general_rx).await, EMBED);
        assert_eq!(next(&mut codex_rx).await, codex_url);
        let ignored = "https://example.com/ignored";
        configure(
            &updaters,
            &lifetime,
            &sources(ignored, "", ""),
            policy(true, true),
            false,
        );
        assert_eq!(next(&mut codex_rx).await, EMBED);
        assert_eq!((source(&general), source(&devin)), (DISABLED.into(), DISABLED.into()));
        configure(&updaters, &lifetime, &sources("", "", ""), policy(true, false), false);
        assert_eq!(next(&mut general_rx).await, EMBED);
        assert_eq!(next(&mut devin_rx).await, EMBED);

        // Shutdown fences reloads; a new lifetime restarts the same sources.
        lifetime.token.cancel();
        let stopped = generation(&codex);
        configure(
            &updaters,
            &lifetime,
            &sources("", "https://example.com/stopped", ""),
            policy(true, false),
            false,
        );
        assert_eq!(generation(&codex), stopped, "reload after shutdown restarted a reader");
        let lifetime = Lifetime::new();
        configure(
            &updaters,
            &lifetime,
            &sources(ignored, "", ""),
            policy(true, true),
            true,
        );
        assert_eq!(next(&mut codex_rx).await, EMBED);
        assert_eq!(source(&general), DISABLED);

        // The default source: models.json starts, Codex and Devin wait.
        let lifetime = Lifetime::new();
        configure(&updaters, &lifetime, &sources("", "", ""), policy(false, false), true);
        assert_eq!(next(&mut general_rx).await, "");
        configure(&updaters, &lifetime, &sources("", "", ""), policy(false, false), false);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(codex_rx.try_recv().is_err() && devin_rx.try_recv().is_err());
        codex.wake();
        assert_eq!(next(&mut codex_rx).await, "");
        let devin_url = "https://example.com/devin";
        configure(
            &updaters,
            &lifetime,
            &sources("", "", devin_url),
            policy(false, false),
            false,
        );
        assert_eq!(next(&mut devin_rx).await, devin_url);
        // In Home mode the Codex default starts at launch.
        let lifetime = Lifetime::new();
        configure(&updaters, &lifetime, &sources("", "", ""), policy(false, true), true);
        assert_eq!(next(&mut codex_rx).await, "");
    }

    /// A test runtime downloads no default catalog, through either entry point: the
    /// startup plan (Home mode starts every updater at once) or a Codex client asking
    /// for its catalog.
    #[tokio::test]
    async fn test_runtimes_start_no_default_download() {
        let executors = cpa_exec::Executors {
            claude: cpa_exec::claude::ClaudeExecutor::new("http://127.0.0.1:1").unwrap(),
            codex: Default::default(),
            devices: Default::default(),
            openai: Default::default(),
            google: Default::default(),
        };
        let dir = std::env::temp_dir().join(format!("cpa-updater-{}", uuid::Uuid::new_v4()));
        let cfg = cpa_core::config::Config::parse(&format!("auth-dir: {}\n", dir.display())).unwrap();
        let rt = Arc::new(crate::testing::runtime(cfg, Vec::new(), executors));
        start(&rt, false);
        start(&rt, true);
        let _ = crate::codex_models::response(&rt, "0.130.0");
        for updater in CATALOGS.iter() {
            assert_eq!(source(updater), DISABLED);
            assert!(!updater.pending.load(Ordering::SeqCst));
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
