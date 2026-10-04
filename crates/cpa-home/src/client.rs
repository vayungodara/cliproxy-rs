//! The Home control-plane client (Go internal/home/client.go).
//!
//! One `Client` is one lifetime. Its connections hang off a fence token: `close` and
//! `abort_ambiguous_dispatch` cancel it, which fails every in-flight command on every
//! connection of the lifetime at once, and no new connection can be opened afterwards.
//! A replacement lifetime comes from [`Client::new_lifetime`], which keeps the cluster
//! failover state and the membership identity.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::config::{CredentialConcurrency, HomeConfig};
use crate::error::{Error, Result};
use crate::gojson::Object;
use crate::resp::{Conn, Value};
use crate::tls::{Dialer, join_host_port};

pub const KEY_CONFIG: &str = "config";
pub const CHANNEL_CONFIG: &str = "config";
pub const CHANNEL_CLUSTER: &str = "cluster";
pub const KEY_USAGE: &str = "usage";
pub const KEY_IN_FLIGHT_SNAPSHOT: &str = "in-flight-snapshot";
pub const KEY_CONCURRENCY_RELEASE: &str = "concurrency-release";
pub const KEY_REQUEST_LOG: &str = "request-log";
pub const KEY_APP_LOG: &str = "app-log";
pub const KEY_PLUGIN_STATUS: &str = "plugin-status";
pub const KEY_PLUGIN_TASKS: &str = "plugin-tasks";
pub const KEY_PLUGIN_SYNC: &str = "plugin-sync";

pub(crate) const OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(35);
const PLUGIN_SYNC_TIMEOUT: Duration = Duration::from_secs(120);
const RECONNECT_FAILOVER_THRESHOLD: u32 = 3;
// ponytail: a small idle pool with an age cap instead of go-redis's health-checked pool;
// a stale idle connection costs one failed command, as with MaxRetries -1 in Go.
const MAX_IDLE: usize = 4;
const MAX_IDLE_AGE: Duration = Duration::from_secs(60);

fn closed_error() -> Error {
    Error::Transport("redis: client is closed".into())
}

/// Connections to one Home address.
pub(crate) struct Pool {
    dialer: Dialer,
    idle: Mutex<Vec<(Conn, Instant)>>,
    closed: CancellationToken,
}

impl Pool {
    fn new(dialer: Dialer, fence: &CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            dialer,
            idle: Mutex::default(),
            closed: fence.child_token(),
        })
    }

    pub(crate) fn close(&self) {
        self.closed.cancel();
        self.idle.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }

    pub(crate) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    pub(crate) async fn checkout(&self) -> Result<Conn> {
        if self.closed.is_cancelled() {
            return Err(closed_error());
        }
        let reusable = {
            let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
            idle.retain(|(_, since)| since.elapsed() < MAX_IDLE_AGE);
            idle.pop()
        };
        if let Some((conn, _)) = reusable {
            return Ok(conn);
        }
        tokio::select! {
            _ = self.closed.cancelled() => Err(closed_error()),
            conn = self.dialer.dial() => conn,
        }
    }

    pub(crate) fn checkin(&self, conn: Conn) {
        if self.closed.is_cancelled() {
            return;
        }
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        if idle.len() < MAX_IDLE {
            idle.push((conn, Instant::now()));
        }
    }

    /// One request and its reply on `conn`, bounded by `timeout` and the pool's lifetime.
    pub(crate) async fn exchange<A: AsRef<[u8]>>(
        &self,
        conn: &mut Conn,
        args: &[A],
        timeout: Duration,
    ) -> Result<Value> {
        let work = async {
            conn.send(args).await?;
            conn.recv().await
        };
        tokio::select! {
            _ = self.closed.cancelled() => Err(closed_error()),
            reply = tokio::time::timeout(timeout, work) => match reply {
                Ok(reply) => reply.map_err(Error::from),
                Err(_) => Err(Error::Timeout),
            },
        }
    }

    /// A pooled request: the connection returns to the pool only after a clean exchange.
    async fn call<A: AsRef<[u8]>>(&self, args: &[A], timeout: Duration) -> Result<Value> {
        let mut conn = self.checkout().await?;
        let reply = self.exchange(&mut conn, args, timeout).await?;
        self.checkin(conn);
        Ok(reply)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Recovery {
    Stable = 0,
    TakeoverEligible = 1,
    Switching = 2,
    SwitchingTakeover = 3,
}

impl Recovery {
    fn from_u32(value: u32) -> Self {
        match value {
            1 => Recovery::TakeoverEligible,
            2 => Recovery::Switching,
            3 => Recovery::SwitchingTakeover,
            _ => Recovery::Stable,
        }
    }
}

/// Go `clusterNode`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ClusterNode {
    pub ip: String,
    pub port: i64,
    pub client_count: i64,
    pub is_master: bool,
}

/// Go `parseClusterNodesPayload` and `normalizeClusterNodes`: usable nodes, fewest
/// clients first, ties in Home's order.
pub fn parse_cluster_nodes(raw: &[u8]) -> Result<Vec<ClusterNode>> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Envelope {
        nodes: Vec<ClusterNode>,
    }
    let envelope: Envelope = serde_json::from_slice(raw).map_err(|e| Error::Other(e.to_string()))?;
    let mut nodes: Vec<ClusterNode> = envelope
        .nodes
        .into_iter()
        .filter_map(|mut node| {
            node.ip = node.ip.trim().to_owned();
            if node.ip.is_empty() || node.port <= 0 || node.port > i64::from(u16::MAX) {
                return None;
            }
            node.client_count = node.client_count.max(0);
            Some(node)
        })
        .collect();
    nodes.sort_by_key(|node| node.client_count);
    Ok(nodes)
}

/// Go `KVSetOptions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetOptions {
    pub ex: Duration,
    pub px: Duration,
    pub nx: bool,
    pub xx: bool,
}

/// Go `durationCeil`.
fn ceil_units(value: Duration, unit: Duration) -> u128 {
    value.as_nanos().div_ceil(unit.as_nanos())
}

/// Go `buildKVSetArgs`: `key value [EX s|PX ms] [NX|XX]`, TTLs rounded up.
pub fn set_args(key: &str, value: &[u8], opts: SetOptions) -> Result<Vec<Vec<u8>>> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Other("home kv: key is empty".into()));
    }
    if !opts.ex.is_zero() && !opts.px.is_zero() {
        return Err(Error::Other("home kv: EX and PX are mutually exclusive".into()));
    }
    if opts.nx && opts.xx {
        return Err(Error::Other("home kv: NX and XX are mutually exclusive".into()));
    }
    let mut args = vec![key.as_bytes().to_vec(), value.to_vec()];
    if !opts.ex.is_zero() {
        args.push(b"EX".to_vec());
        args.push(ceil_units(opts.ex, Duration::from_secs(1)).to_string().into_bytes());
    }
    if !opts.px.is_zero() {
        args.push(b"PX".to_vec());
        args.push(ceil_units(opts.px, Duration::from_millis(1)).to_string().into_bytes());
    }
    if opts.nx {
        args.push(b"NX".to_vec());
    }
    if opts.xx {
        args.push(b"XX".to_vec());
    }
    Ok(args)
}

/// Go `headersToLowerMap` / `queryToLowerMap`: lowercase trimmed names, trimmed
/// values joined with `", "`.
pub fn lower_map<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> BTreeMap<String, String> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, value) in pairs {
        let name = name.trim().to_lowercase();
        if name.is_empty() {
            continue;
        }
        grouped.entry(name).or_default().push(value.trim().to_owned());
    }
    grouped.into_iter().map(|(k, v)| (k, v.join(", "))).collect()
}

/// One credential request (Go `authDispatchRequest`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchRequest {
    pub model: String,
    pub session_id: String,
    pub parent_session_id: String,
    /// Downstream request headers as (name, value) pairs; names are case-insensitive.
    pub headers: Vec<(String, String)>,
    pub count: i64,
    pub credential_policy: String,
    /// The retry-round protocol: `Some` sends `retry_round`.
    pub retry_round: Option<i64>,
    /// Credentials already tried this round; `Some(empty)` still sends `[]`.
    pub excluded_auth_ids: Option<Vec<String>>,
    pub pinned_auth_id: String,
}

impl DispatchRequest {
    /// Go `newAuthDispatchRequest` marshalled: the exact RPOP key.
    pub fn to_json(&self) -> Vec<u8> {
        let mut count = if self.count <= 0 { 1 } else { self.count };
        // Older Home servers ignore exclusions and cap retries by count.
        if self.excluded_auth_ids.is_some() {
            count = 1;
        }
        let node_kind = self
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-node-kind"))
            .map_or("", |(_, value)| value.trim());
        let headers = lower_map(self.headers.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let mut json = Object::new()
            .str("type", "auth")
            .str("model", &self.model)
            .int("count", count)
            .int("concurrency_protocol", 1)
            .str_opt("session_id", self.session_id.trim())
            .str_opt("parent_session_id", self.parent_session_id.trim())
            .str_opt("node_kind", node_kind)
            .map_opt("headers", &headers)
            .str_opt("credential_policy", self.credential_policy.trim());
        if let Some(round) = self.retry_round {
            json = json.int("retry_round", round.max(0));
        }
        if let Some(excluded) = &self.excluded_auth_ids {
            json = json.strs("excluded_auth_ids", excluded);
        }
        json.str_opt("pinned_auth_id", self.pinned_auth_id.trim()).finish()
    }
}

/// Go `ConcurrencyReleaseFrame`: the cumulative release for one credential and model.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReleaseFrame {
    pub credential_id: String,
    pub model: String,
    pub release_seq: i64,
}

impl ReleaseFrame {
    pub fn to_json(&self) -> Vec<u8> {
        Object::new()
            .str("credential_id", &self.credential_id)
            .str("model", &self.model)
            .int("release_seq", self.release_seq)
            .finish()
    }
}

/// Go `PluginTask`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct PluginTask {
    pub id: u64,
    pub operation: String,
    pub plugin_id: String,
    pub target_node_type: String,
    pub target_node_id: String,
    pub created_at: String,
    pub updated_at: String,
}

struct State {
    cfg: HomeConfig,
    seed_host: String,
    seed_port: u16,
    cmd: Option<Arc<Pool>>,
    sub: Option<Arc<Pool>>,
    release: Option<Arc<Pool>>,
    lifecycle: CredentialConcurrency,
    managed: bool,
    legacy_membership: bool,
    cluster_nodes: Vec<ClusterNode>,
    reconnect_failures: u32,
}

pub(crate) struct Inner {
    state: Mutex<State>,
    heartbeat_ok: AtomicBool,
    fenced: AtomicBool,
    ambiguous: AtomicBool,
    /// Latched per lifetime: a Home upgrade takes effect at the next reconnect.
    cas_unsupported: AtomicBool,
    recovery: AtomicU32,
    limiter: RwLock<CredentialConcurrency>,
    instance_id: String,
    op_timeout: Duration,
    fence: CancellationToken,
}

/// A Home client lifetime. Cloning shares the lifetime.
#[derive(Clone)]
pub struct Client(pub(crate) Arc<Inner>);

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("addr", &self.addr())
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(cfg: HomeConfig) -> Self {
        Self::with_options(cfg, OPERATION_TIMEOUT, uuid::Uuid::new_v4().to_string())
    }

    /// A client with a custom per-operation timeout (tests use short ones).
    pub fn with_options(cfg: HomeConfig, op_timeout: Duration, instance_id: String) -> Self {
        let seed_host = cfg.host.trim().to_owned();
        let seed_port = cfg.port;
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                cfg,
                seed_host,
                seed_port,
                cmd: None,
                sub: None,
                release: None,
                lifecycle: CredentialConcurrency::default(),
                managed: false,
                legacy_membership: false,
                cluster_nodes: Vec::new(),
                reconnect_failures: 0,
            }),
            heartbeat_ok: AtomicBool::new(false),
            fenced: AtomicBool::new(false),
            ambiguous: AtomicBool::new(false),
            cas_unsupported: AtomicBool::new(false),
            recovery: AtomicU32::new(Recovery::Stable as u32),
            limiter: RwLock::new(CredentialConcurrency::default().with_defaults()),
            instance_id,
            op_timeout,
            fence: CancellationToken::new(),
        }))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `NewLifetime`: a fresh client keeping failover state, membership identity
    /// and the legacy-protocol downgrade. Lifecycle config is re-read from Home.
    pub fn new_lifetime(&self) -> Client {
        let state = self.state();
        let next = Client::with_options(state.cfg.clone(), self.0.op_timeout, self.0.instance_id.clone());
        {
            let mut next_state = next.state();
            next_state.seed_host.clone_from(&state.seed_host);
            next_state.seed_port = state.seed_port;
            next_state.cluster_nodes.clone_from(&state.cluster_nodes);
            next_state.reconnect_failures = state.reconnect_failures;
            next_state.legacy_membership = state.legacy_membership;
        }
        next.0
            .recovery
            .store(self.0.recovery.load(Ordering::SeqCst), Ordering::SeqCst);
        next
    }

    pub fn ptr_eq(&self, other: &Client) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub fn membership_instance_id(&self) -> &str {
        &self.0.instance_id
    }

    pub fn legacy_membership(&self) -> bool {
        self.state().legacy_membership
    }

    /// Go `EnableLegacyMembership`: permanent for this lifetime chain.
    pub fn enable_legacy_membership(&self) {
        self.state().legacy_membership = true;
        self.suppress_takeover();
    }

    pub fn enabled(&self) -> bool {
        self.state().cfg.enabled
    }

    pub fn node_id(&self) -> String {
        self.state().cfg.node_id.clone()
    }

    pub fn heartbeat_ok(&self) -> bool {
        self.enabled() && self.0.heartbeat_ok.load(Ordering::SeqCst)
    }

    pub(crate) fn set_heartbeat(&self, ok: bool) {
        self.0.heartbeat_ok.store(ok, Ordering::SeqCst);
    }

    pub fn fenced(&self) -> bool {
        self.0.fenced.load(Ordering::SeqCst)
    }

    /// The current target, `host:port`.
    pub fn addr(&self) -> Option<String> {
        let state = self.state();
        let host = state.cfg.host.trim();
        (!host.is_empty() && state.cfg.port > 0).then(|| join_host_port(host, state.cfg.port))
    }

    fn detach(state: &mut State, release: bool) -> Vec<Arc<Pool>> {
        let mut pools: Vec<Arc<Pool>> = [state.cmd.take(), state.sub.take()].into_iter().flatten().collect();
        if release {
            pools.extend(state.release.take());
        }
        pools
    }

    /// Go `Close`: permanently ends this lifetime's dispatch and connections.
    pub fn close(&self) {
        self.0.fenced.store(true, Ordering::SeqCst);
        self.set_heartbeat(false);
        let pools = Self::detach(&mut self.state(), true);
        self.0.fence.cancel();
        for pool in pools {
            pool.close();
        }
    }

    /// Go `AbortAmbiguousDispatch`: an issued dispatch has an unknown outcome, so this
    /// lifetime stops at once; in-flight requests on every connection fail now.
    pub fn abort_ambiguous_dispatch(&self) {
        self.0.ambiguous.store(true, Ordering::SeqCst);
        self.close();
    }

    pub fn ambiguous_dispatch(&self) -> bool {
        self.0.ambiguous.load(Ordering::SeqCst)
    }

    /// Go `SuppressTakeover`: the next subscriber goes through normal membership.
    pub fn suppress_takeover(&self) {
        let r = &self.0.recovery;
        if r.compare_exchange(1, 0, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            let _ = r.compare_exchange(3, 2, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    pub(crate) fn recovery(&self) -> Recovery {
        Recovery::from_u32(self.0.recovery.load(Ordering::SeqCst))
    }

    pub(crate) fn set_recovery(&self, state: Recovery) {
        self.0.recovery.store(state as u32, Ordering::SeqCst);
    }

    pub(crate) fn mark_takeover_eligible(&self) {
        let r = &self.0.recovery;
        if r.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            let _ = r.compare_exchange(2, 3, Ordering::SeqCst, Ordering::SeqCst);
        }
    }

    /// Go `SetManagedLifetime`: the service, not the subscriber, closes this client.
    pub fn set_managed_lifetime(&self, managed: bool) {
        self.state().managed = managed;
    }

    pub(crate) fn managed(&self) -> bool {
        self.state().managed
    }

    pub(crate) fn op_timeout(&self) -> Duration {
        self.0.op_timeout
    }

    fn dialer(&self, state: &State, timeout: Duration) -> Result<Dialer> {
        let host = state.cfg.host.trim();
        if host.is_empty() || state.cfg.port == 0 {
            return Err(Error::Other(format!(
                "home: invalid address (host={:?} port={})",
                state.cfg.host, state.cfg.port
            )));
        }
        let mut server_name = if state.cfg.tls.use_target_server_name {
            host.to_owned()
        } else {
            state.seed_host.clone()
        };
        if server_name.trim().is_empty() {
            server_name = host.to_owned();
        }
        Dialer::new(&state.cfg.tls, host, state.cfg.port, &server_name, timeout)
    }

    /// The TLS name the next dial verifies (`None` without TLS), after moving the
    /// target to `host` as a cluster failover would.
    #[cfg(test)]
    pub(crate) fn dial_server_name(&self, host: &str) -> Result<Option<String>> {
        let mut state = self.state();
        state.cfg.host = host.to_owned();
        Ok(self.dialer(&state, self.0.op_timeout)?.server_name().map(str::to_owned))
    }

    /// Go `ensureClients`.
    pub(crate) fn ensure_pools(&self) -> Result<()> {
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        let mut state = self.state();
        if !state.cfg.enabled {
            return Err(Error::Disabled);
        }
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        if state.cmd.is_none() {
            let dialer = self.dialer(&state, self.0.op_timeout)?;
            state.cmd = Some(Pool::new(dialer, &self.0.fence));
        }
        if state.sub.is_none() {
            let dialer = self.dialer(&state, self.0.op_timeout)?;
            state.sub = Some(Pool::new(dialer, &self.0.fence));
        }
        Ok(())
    }

    pub(crate) fn command_pool(&self) -> Result<Arc<Pool>> {
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        self.ensure_pools()?;
        let state = self.state();
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        state.cmd.clone().ok_or(Error::NotConnected)
    }

    pub(crate) fn subscription_pool(&self) -> Result<Arc<Pool>> {
        self.ensure_pools()?;
        self.state().sub.clone().ok_or(Error::NotConnected)
    }

    /// Go `closeBootstrapPools`: drop pools without ending the lifetime.
    pub(crate) fn close_bootstrap_pools(&self) {
        self.set_heartbeat(false);
        for pool in Self::detach(&mut self.state(), false) {
            pool.close();
        }
    }

    /// Go `promoteSubscription`: the bootstrap command pool is replaced after the
    /// subscription is acknowledged.
    pub(crate) fn promote_subscription(&self) {
        let pool = self.state().cmd.take();
        if let Some(pool) = pool {
            pool.close();
        }
    }

    async fn command_with<A: AsRef<[u8]>>(&self, args: &[A], timeout: Duration) -> Result<Value> {
        let pool = self.command_pool()?;
        match pool.call(args, timeout).await? {
            Value::Error(message) => Err(Error::Server(message)),
            reply => Ok(reply),
        }
    }

    pub(crate) async fn command<A: AsRef<[u8]>>(&self, args: &[A]) -> Result<Value> {
        self.command_with(args, self.0.op_timeout).await
    }

    pub async fn ping(&self) -> Result<()> {
        self.command(&["ping"]).await.map(|_| ())
    }

    /// go-redis `StringCmd`: bulk, status and integer replies read as bytes.
    fn bytes(reply: Value) -> Result<Option<Vec<u8>>> {
        match reply {
            Value::Bulk(bytes) => Ok(Some(bytes)),
            Value::Simple(text) => Ok(Some(text.into_bytes())),
            Value::Int(n) => Ok(Some(n.to_string().into_bytes())),
            Value::Nil => Ok(None),
            Value::Error(message) => Err(Error::Server(message)),
            Value::Array(_) => Err(Error::Transport("redis: can't parse array reply as string".into())),
        }
    }

    fn int(reply: Value) -> Result<i64> {
        match reply {
            Value::Int(n) => Ok(n),
            Value::Error(message) => Err(Error::Server(message)),
            other => Err(Error::Transport(format!("redis: unexpected reply {other:?}"))),
        }
    }

    /// GET returning Go's not-found and empty errors.
    async fn get_required(&self, key: &[u8], timeout: Duration, missing: Error) -> Result<Vec<u8>> {
        match Self::bytes(self.command_with(&[b"get".as_slice(), key], timeout).await?)? {
            None => Err(missing),
            Some(bytes) if bytes.is_empty() => Err(Error::EmptyResponse),
            Some(bytes) => Ok(bytes),
        }
    }

    /// Go `GetConfig`: the authoritative config YAML, after cluster discovery.
    pub async fn get_config(&self) -> Result<Vec<u8>> {
        if let Err(Discovery::Transport(error)) = self.refresh_best_cluster_node().await {
            return Err(Error::Other(format!(
                "home cluster discovery transport failed: {error}"
            )));
        }
        self.get_required(KEY_CONFIG.as_bytes(), self.0.op_timeout, Error::ConfigNotFound)
            .await
    }

    /// Go `GetModels`: Home answers the model catalog for these request credentials.
    pub async fn get_models(
        &self,
        headers: &BTreeMap<String, String>,
        query: &BTreeMap<String, String>,
    ) -> Result<Vec<u8>> {
        // Validate the lifetime before encoding, as Go does.
        self.command_pool()?;
        let key = Object::new()
            .str("type", "models")
            .map_opt("headers", headers)
            .map_opt("query", query)
            .finish();
        self.get_required(&key, self.0.op_timeout, Error::ModelsNotFound).await
    }

    /// Go `GetRefreshAuth`: Home refreshes one credential (35 s budget).
    pub async fn get_refresh_auth(&self, auth_index: &str, access_token_sha256: &str) -> Result<Vec<u8>> {
        self.command_pool()?;
        let auth_index = auth_index.trim();
        if auth_index.is_empty() {
            return Err(Error::Other("home: auth_index is empty".into()));
        }
        let key = Object::new()
            .str("type", "refresh")
            .str("auth_index", auth_index)
            .str_opt("access_token_sha256", access_token_sha256.trim())
            .finish();
        self.get_required(&key, REFRESH_TIMEOUT, Error::AuthNotFound).await
    }

    /// Go `rPopAuth`. Failures before `RPOP` is written are deterministic. Afterwards
    /// any failure other than a Home error reply is ambiguous: Home may have issued a
    /// lease, so this lifetime is fenced. Dropping the future while the reply is
    /// outstanding fences too.
    pub async fn rpop_auth(&self, request: &DispatchRequest) -> Result<Vec<u8>> {
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        if request.model.trim().is_empty() {
            return Err(Error::Other("home: requested model is empty".into()));
        }
        let mut request = request.clone();
        request.model = request.model.trim().to_owned();
        let key = request.to_json();
        let pool = self.command_pool()?;
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        let mut conn = pool.checkout().await?;
        if let Value::Error(message) = pool.exchange(&mut conn, &["ping"], self.0.op_timeout).await? {
            return Err(Error::Server(message));
        }
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        let guard = FenceOnDrop(Some(self));
        let reply = pool
            .exchange(&mut conn, &[b"rpop".as_slice(), &key], self.0.op_timeout)
            .await;
        guard.disarm();
        let bytes = match reply {
            Ok(Value::Error(message)) => {
                pool.checkin(conn);
                return Err(Error::Server(message));
            }
            Ok(Value::Nil) => {
                pool.checkin(conn);
                return Err(Error::AuthNotFound);
            }
            Ok(Value::Bulk(bytes)) => bytes,
            Ok(Value::Simple(text)) => text.into_bytes(),
            Ok(other) => {
                return Err(self.ambiguous(Error::Transport(format!("redis: unexpected reply {other:?}"))));
            }
            Err(error) => return Err(self.ambiguous(error)),
        };
        pool.checkin(conn);
        if bytes.is_empty() {
            return Err(Error::EmptyResponse);
        }
        Ok(bytes)
    }

    fn ambiguous(&self, error: Error) -> Error {
        self.abort_ambiguous_dispatch();
        Error::Ambiguous(Box::new(error))
    }

    /// Go `KVGet`.
    pub async fn kv_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Self::bytes(self.command(&[b"get".as_slice(), key.as_bytes()]).await?)
    }

    /// Go `KVSet`: false when an NX/XX condition was not met.
    pub async fn kv_set(&self, key: &str, value: &[u8], opts: SetOptions) -> Result<bool> {
        self.command_pool()?;
        let mut args = vec![b"SET".to_vec()];
        args.extend(set_args(key, value, opts)?);
        Ok(!matches!(self.command(&args).await?, Value::Nil))
    }

    /// Go `KVSetNX`.
    pub async fn kv_set_nx(&self, key: &str, value: &[u8], ttl: Duration) -> Result<bool> {
        self.kv_set(
            key,
            value,
            SetOptions {
                ex: ttl,
                nx: true,
                ..SetOptions::default()
            },
        )
        .await
    }

    /// Go `KVCompareAndSwap` over Home's `CAS <key> <0|1> <expected> <new> [PX ms]`.
    /// `expected: None` means the key must be absent.
    pub async fn kv_compare_and_swap(
        &self,
        key: &str,
        expected: Option<&[u8]>,
        value: &[u8],
        ttl: Duration,
    ) -> Result<bool> {
        if self.0.cas_unsupported.load(Ordering::SeqCst) {
            return Err(Error::CompareAndSwapUnsupported);
        }
        let mut args: Vec<Vec<u8>> = vec![
            b"CAS".to_vec(),
            key.as_bytes().to_vec(),
            if expected.is_some() {
                b"1".to_vec()
            } else {
                b"0".to_vec()
            },
            expected.unwrap_or_default().to_vec(),
            value.to_vec(),
        ];
        let millis = ceil_units(ttl, Duration::from_millis(1));
        if millis > 0 {
            args.push(b"PX".to_vec());
            args.push(millis.to_string().into_bytes());
        }
        match self.command(&args).await.and_then(Self::int) {
            Ok(n) => Ok(n == 1),
            Err(error) if error.is_command_unsupported() => {
                if !self.0.cas_unsupported.swap(true, Ordering::SeqCst) {
                    tracing::warn!(
                        "home kv: this Home does not implement the CAS command; Antigravity and Codex reasoning replay are disabled until Home is upgraded"
                    );
                }
                Err(Error::CompareAndSwapUnsupported)
            }
            Err(error) => Err(error),
        }
    }

    /// Go `KVDel`.
    pub async fn kv_del(&self, keys: &[&str]) -> Result<i64> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut args = vec!["del"];
        args.extend_from_slice(keys);
        Self::int(self.command(&args).await?)
    }

    /// Go `KVExpire` (go-redis `formatSec`: a positive sub-second TTL is one second).
    pub async fn kv_expire(&self, key: &str, ttl: Duration) -> Result<bool> {
        let seconds = if !ttl.is_zero() && ttl < Duration::from_secs(1) {
            1
        } else {
            ttl.as_secs()
        };
        let reply = self.command(&["expire", key, &seconds.to_string()]).await?;
        Ok(Self::int(reply)? == 1)
    }

    /// Go `KVIncrBy`.
    pub async fn kv_incr_by(&self, key: &str, delta: i64) -> Result<i64> {
        Self::int(self.command(&["incrby", key, &delta.to_string()]).await?)
    }

    /// Go `KVMGet`: one entry per key, `None` for misses.
    pub async fn kv_mget(&self, keys: &[&str]) -> Result<Vec<Option<Vec<u8>>>> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["mget"];
        args.extend_from_slice(keys);
        match self.command(&args).await? {
            Value::Array(items) => items
                .into_iter()
                .map(|item| match item {
                    Value::Array(_) | Value::Int(_) => {
                        Err(Error::Other(format!("home kv: unsupported MGET item type {item:?}")))
                    }
                    item => Self::bytes(item),
                })
                .collect(),
            other => Err(Error::Transport(format!("redis: unexpected reply {other:?}"))),
        }
    }

    /// Go `KVMSet`: keys in sorted order.
    pub async fn kv_mset(&self, pairs: &BTreeMap<String, Vec<u8>>) -> Result<()> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut args = vec![b"MSET".to_vec()];
        for (key, value) in pairs {
            args.push(key.as_bytes().to_vec());
            args.push(value.clone());
        }
        self.command(&args).await.map(|_| ())
    }

    async fn push(&self, command: &str, key: &str, payload: &[u8], skip_empty: bool) -> Result<()> {
        self.command_pool()?;
        if skip_empty && payload.is_empty() {
            return Ok(());
        }
        self.command(&[command.as_bytes(), key.as_bytes(), payload])
            .await
            .map(|_| ())
    }

    /// Go `LPushUsage`.
    pub async fn lpush_usage(&self, payload: &[u8]) -> Result<()> {
        self.push("lpush", KEY_USAGE, payload, true).await
    }

    /// Go `LPushInFlightSnapshot`.
    pub async fn lpush_in_flight_snapshot(&self, payload: &[u8]) -> Result<()> {
        self.push("lpush", KEY_IN_FLIGHT_SNAPSHOT, payload, false).await
    }

    /// Go `RPushRequestLog`.
    pub async fn rpush_request_log(&self, payload: &[u8]) -> Result<()> {
        self.push("rpush", KEY_REQUEST_LOG, payload, true).await
    }

    /// Go `RPushAppLog`.
    pub async fn rpush_app_log(&self, payload: &[u8]) -> Result<()> {
        self.push("rpush", KEY_APP_LOG, payload, true).await
    }

    /// Go `RPushPluginStatus`.
    pub async fn rpush_plugin_status(&self, payload: &[u8]) -> Result<()> {
        self.push("rpush", KEY_PLUGIN_STATUS, payload, true).await
    }

    /// Go `GetPluginTasks`.
    pub async fn get_plugin_tasks(&self) -> Result<Vec<PluginTask>> {
        match Self::bytes(self.command(&["get", KEY_PLUGIN_TASKS]).await?)? {
            None => Ok(Vec::new()),
            Some(bytes) if bytes.is_empty() => Ok(Vec::new()),
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|e| Error::Other(e.to_string())),
        }
    }

    /// Go `GetPluginSync` transport: `GET plugin-sync <request>` on a dedicated
    /// connection with a two-minute budget. Returns the raw response; the plugin layer
    /// decodes and validates it. Homes without plugin sync yield `PluginSyncUnsupported`.
    pub async fn get_plugin_sync(&self, request: &[u8]) -> Result<Vec<u8>> {
        let pool = self.command_pool()?;
        let mut conn = pool.checkout().await?;
        let reply = pool
            .exchange(
                &mut conn,
                &[b"get".as_slice(), KEY_PLUGIN_SYNC.as_bytes(), request],
                PLUGIN_SYNC_TIMEOUT,
            )
            .await?;
        let bytes = match reply {
            Value::Error(message) => {
                return Err(match plugin_sync_unsupported_message(&message) {
                    Some(m) => Error::PluginSyncUnsupported(m),
                    None => Error::Server(message),
                });
            }
            reply => Self::bytes(reply)?.unwrap_or_default(),
        };
        if bytes.is_empty() {
            return Err(Error::EmptyResponse);
        }
        if let Some(message) = plugin_sync_unsupported_response(&bytes) {
            return Err(Error::PluginSyncUnsupported(message));
        }
        Ok(bytes)
    }

    /// Go `concurrencyReleaseClient`: an independent connection, refused while
    /// membership recovery is in progress.
    fn release_pool(&self) -> Result<Arc<Pool>> {
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        if self.recovery() != Recovery::Stable {
            return Err(Error::NotConnected);
        }
        let mut state = self.state();
        if !state.cfg.enabled {
            return Err(Error::Disabled);
        }
        if self.fenced() {
            return Err(Error::DispatchFenced);
        }
        if self.recovery() != Recovery::Stable {
            return Err(Error::NotConnected);
        }
        if let Some(pool) = &state.release {
            return Ok(pool.clone());
        }
        let pool = Pool::new(self.dialer(&state, self.0.op_timeout)?, &self.0.fence);
        state.release = Some(pool.clone());
        Ok(pool)
    }

    /// Go `PushConcurrencyRelease`.
    pub async fn push_concurrency_release(&self, frame: &ReleaseFrame) -> Result<()> {
        if frame.credential_id.is_empty() || frame.model.is_empty() || frame.release_seq <= 0 {
            return Err(Error::Other("invalid concurrency release frame".into()));
        }
        let pool = self.release_pool()?;
        let args = [
            b"LPUSH".as_slice(),
            KEY_CONCURRENCY_RELEASE.as_bytes(),
            &frame.to_json(),
        ];
        match pool.call(&args, self.0.op_timeout).await? {
            Value::Error(message) => Err(Error::Server(message)),
            _ => Ok(()),
        }
    }

    /// Go `SetLifecycleConfig`: Home's lifecycle settings for this lifetime.
    pub fn set_lifecycle_config(&self, cfg: CredentialConcurrency) -> Result<()> {
        let cfg = cfg.with_defaults();
        cfg.validate()
            .map_err(|e| Error::Other(format!("validate credential concurrency lifecycle config: {e}")))?;
        self.state().lifecycle = cfg;
        *self.0.limiter.write().unwrap_or_else(PoisonError::into_inner) = cfg;
        Ok(())
    }

    /// Go `LimiterConfig`: the latest validated limiter settings.
    pub fn limiter_config(&self) -> CredentialConcurrency {
        *self.0.limiter.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Go `subscriptionParameters`: `SUBSCRIBE` arguments and the heartbeat timeout.
    pub(crate) fn subscription_parameters(&self) -> (Vec<String>, Duration) {
        let state = self.state();
        let cfg = state.lifecycle.with_defaults();
        let mut args = vec![CHANNEL_CONFIG.to_owned()];
        if cfg.lifecycle_config_revision > 0 {
            args.push(cfg.lifecycle_config_revision.to_string());
            if !state.legacy_membership {
                if matches!(
                    self.recovery(),
                    Recovery::TakeoverEligible | Recovery::SwitchingTakeover
                ) {
                    args.push("takeover".into());
                }
                args.push(self.0.instance_id.clone());
            }
        }
        (args, cfg.heartbeat_timeout())
    }

    fn discovery_enabled(&self) -> bool {
        !self.state().cfg.disable_cluster_discovery
    }

    async fn refresh_best_cluster_node(&self) -> std::result::Result<(), Discovery> {
        if !self.discovery_enabled() {
            return Ok(());
        }
        match self.refresh_cluster_nodes().await {
            Ok(true) => {
                if let Some(addr) = self.addr() {
                    tracing::info!("home cluster target switched to {addr}");
                }
                Ok(())
            }
            Ok(false) => Ok(()),
            Err(error) => {
                let (Discovery::Transport(inner) | Discovery::Other(inner)) = &error;
                tracing::debug!("home cluster nodes unavailable: {inner}");
                Err(error)
            }
        }
    }

    /// Go `refreshClusterNodes`: switch to the least loaded node.
    async fn refresh_cluster_nodes(&self) -> std::result::Result<bool, Discovery> {
        let pool = self.command_pool().map_err(Discovery::Transport)?;
        let raw = match pool.call(&["CLUSTER", "NODES"], self.0.op_timeout).await {
            Ok(Value::Error(message)) => return Err(Discovery::Other(Error::Server(message))),
            Ok(reply) => Self::bytes(reply).map_err(Discovery::Other)?.unwrap_or_default(),
            Err(error) => return Err(Discovery::Transport(error)),
        };
        let nodes = parse_cluster_nodes(&raw).map_err(Discovery::Other)?;
        let Some(first) = nodes.first().cloned() else {
            return Ok(false);
        };
        let mut state = self.state();
        state.cluster_nodes = nodes;
        state.reconnect_failures = 0;
        Ok(self.switch_to_node(&mut state, &first.ip, first.port as u16))
    }

    /// Go `updateClusterNodesFromPayload` (the `cluster` channel).
    pub(crate) fn update_cluster_nodes(&self, raw: &[u8]) -> Result<()> {
        if !self.discovery_enabled() {
            return Ok(());
        }
        let nodes = parse_cluster_nodes(raw)?;
        self.state().cluster_nodes = nodes;
        Ok(())
    }

    fn switch_to_node(&self, state: &mut State, host: &str, port: u16) -> bool {
        let host = host.trim();
        if host.is_empty() || port == 0 || (state.cfg.host.trim() == host && state.cfg.port == port) {
            return false;
        }
        state.cfg.host = host.to_owned();
        state.cfg.port = port;
        let r = &self.0.recovery;
        if r.compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            let _ = r.compare_exchange(1, 3, Ordering::SeqCst, Ordering::SeqCst);
        }
        for pool in Self::detach(state, true) {
            pool.close();
        }
        true
    }

    fn switch_to_next_node(&self, state: &mut State) -> Option<String> {
        let current = (state.cfg.host.trim().to_owned(), state.cfg.port);
        let mut candidates: Vec<(String, i64)> = state
            .cluster_nodes
            .iter()
            .map(|n| (n.ip.trim().to_owned(), n.port))
            .collect();
        if !state.seed_host.trim().is_empty() && state.seed_port > 0 {
            candidates.push((state.seed_host.clone(), i64::from(state.seed_port)));
        }
        for (host, port) in candidates {
            let Ok(port) = u16::try_from(port) else { continue };
            if host.is_empty() || port == 0 || (host == current.0 && port == current.1) {
                continue;
            }
            if self.switch_to_node(state, &host, port) {
                return Some(join_host_port(&host, port));
            }
        }
        None
    }

    /// Go `markReconnectFailure`: fail over after three consecutive failures.
    pub(crate) fn mark_reconnect_failure(&self, reason: &str) {
        let switched = {
            let mut state = self.state();
            if state.cfg.disable_cluster_discovery {
                state.reconnect_failures = 0;
                None
            } else {
                state.reconnect_failures += 1;
                if state.reconnect_failures < RECONNECT_FAILOVER_THRESHOLD {
                    None
                } else {
                    state.reconnect_failures = 0;
                    self.switch_to_next_node(&mut state)
                }
            }
        };
        if let Some(addr) = switched {
            tracing::warn!("home control center unavailable after repeated {reason} failures; switching to {addr}");
        }
    }

    /// Go `markSubscriptionTimeout`: a heartbeat loss fails over at once.
    pub(crate) fn mark_subscription_timeout(&self) {
        let switched = {
            let mut state = self.state();
            state.reconnect_failures = 0;
            if state.cfg.disable_cluster_discovery {
                None
            } else {
                self.switch_to_next_node(&mut state)
            }
        };
        if let Some(addr) = switched {
            tracing::warn!("home subscription heartbeat timeout; switching to {addr}");
        }
    }

    pub(crate) fn reset_reconnect_failures(&self) {
        self.state().reconnect_failures = 0;
    }

    #[cfg(test)]
    pub(crate) fn reconnect_failures(&self) -> u32 {
        self.state().reconnect_failures
    }

    #[cfg(test)]
    pub(crate) fn cluster_nodes(&self) -> Vec<ClusterNode> {
        self.state().cluster_nodes.clone()
    }
}

#[derive(Debug)]
enum Discovery {
    Transport(Error),
    Other(Error),
}

/// Fences the lifetime when an issued RPOP is abandoned before its reply.
struct FenceOnDrop<'a>(Option<&'a Client>);

impl FenceOnDrop<'_> {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for FenceOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            client.abort_ambiguous_dispatch();
        }
    }
}

const PLUGIN_SYNC_UNSUPPORTED: &str = "plugin_sync_unsupported";

/// Go `pluginSyncUnsupportedMessage`.
fn plugin_sync_unsupported_message(message: &str) -> Option<String> {
    let lowered = message.trim().to_lowercase();
    let lowered = lowered.strip_prefix("err ").unwrap_or(&lowered).trim().to_owned();
    matches!(
        lowered.as_str(),
        PLUGIN_SYNC_UNSUPPORTED | "unsupported key" | "wrong number of arguments for 'get' command"
    )
    .then_some(lowered)
}

/// Go `pluginSyncUnsupportedResponse`.
fn plugin_sync_unsupported_response(raw: &[u8]) -> Option<String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Detail {
        code: String,
        #[serde(rename = "type")]
        kind: String,
        message: String,
    }
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Envelope {
        error: Detail,
    }
    let envelope: Envelope = serde_json::from_slice(raw).ok()?;
    let detail = envelope.error;
    let unsupported = |code: &str| code.trim().eq_ignore_ascii_case(PLUGIN_SYNC_UNSUPPORTED);
    if unsupported(&detail.code) || unsupported(&detail.kind) {
        let message = detail.message.trim();
        return Some(if message.is_empty() {
            PLUGIN_SYNC_UNSUPPORTED.to_owned()
        } else {
            message.to_owned()
        });
    }
    plugin_sync_unsupported_message(&detail.message)
}

static CURRENT: RwLock<Option<Client>> = RwLock::new(None);

/// Go `home.SetCurrent`: the lifetime runtime integrations (KV, refresh) use.
pub fn set_current(client: Option<Client>) {
    *CURRENT.write().unwrap_or_else(PoisonError::into_inner) = client;
}

/// Go `home.Current`.
pub fn current() -> Option<Client> {
    CURRENT.read().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Go `home.ClearCurrentIf`.
pub fn clear_current_if(client: &Client) {
    let mut current = CURRENT.write().unwrap_or_else(PoisonError::into_inner);
    if current.as_ref().is_some_and(|c| c.ptr_eq(client)) {
        *current = None;
    }
}
