//! The store client (internal/pluginstore/github.go, direct.go,
//! request_identity.go): registry, release and artifact downloads with manual
//! redirects, per-hop store authentication and the GitHub rate limiter.

use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use futures_util::stream::BoxStream;
use sha2::{Digest, Sha256};

use super::auth::{self, AuthConfig, Headers, ResolvedAuthConfig};
use super::rate_limit::{GitHubRateLimiter, RateLimitError, github_rate_limit_key};
use super::registry::{
    Artifact, DEFAULT_REGISTRY_URL, INSTALL_TYPE_DIRECT, InstallPlan, Plugin, Registry, github_repository_parts,
    normalize_install_plan, parse_registry, validate_artifact,
};
use super::url;
use super::{REQUEST_KIND_ARTIFACT, REQUEST_KIND_METADATA, REQUEST_KIND_REGISTRY};
use crate::go_struct;
use crate::gojson;

const USER_AGENT: &str = "CLIProxyAPI";
const MAX_REDIRECTS: usize = 10;
/// The most any store response may hold. Go reads registry, release and asset
/// responses without a bound and direct artifacts up to their declared size; here a
/// declared size can only lower this.
pub const MAX_DOWNLOAD_BYTES: u64 = 256 << 20;

/// One response from a [`Doer`]: status, headers (canonical keys) and the body.
pub struct DoerResponse {
    pub status: u16,
    pub headers: Headers,
    pub body: BoxStream<'static, Result<Bytes, String>>,
}

/// Go `httpfetch.Doer` as the store uses it: one GET without following redirects.
/// The error is the transport's cause (Go's `url.Error.Err` text).
pub trait Doer: Send + Sync {
    fn get(&self, url: &str, headers: &Headers) -> BoxFuture<'_, Result<DoerResponse, String>>;
}

/// Why a store request failed: GitHub's rate limit (the routes answer 429), an install
/// that would overwrite a loaded library (Go `ErrLoadedPluginLocked`, 409), or anything
/// else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    RateLimited(RateLimitError),
    LoadedPluginLocked,
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited(e) => e.fmt(f),
            Self::LoadedPluginLocked => f.write_str(super::LOADED_PLUGIN_LOCKED),
            Self::Other(e) => f.write_str(e),
        }
    }
}

impl From<String> for StoreError {
    fn from(e: String) -> Self {
        Self::Other(e)
    }
}

impl From<RateLimitError> for StoreError {
    fn from(e: RateLimitError) -> Self {
        Self::RateLimited(e)
    }
}

impl StoreError {
    /// Go `fmt.Errorf("<prefix>: %w", err)`: wraps the text, keeps a rate limit
    /// recognisable.
    pub fn context(self, prefix: &str) -> Self {
        match self {
            Self::Other(e) => Self::Other(format!("{prefix}: {e}")),
            // ponytail: Go's wrapped text is not kept; only the kind is ever read.
            typed => typed,
        }
    }

    pub fn rate_limit(&self) -> Option<&RateLimitError> {
        match self {
            Self::RateLimited(e) => Some(e),
            _ => None,
        }
    }
}

go_struct! {
    pub struct ReleaseAsset("pluginstore.ReleaseAsset") {
        "url" => api_url: String,
        "name" => name: String,
        "browser_download_url" => browser_download_url: String,
    }
}

go_struct! {
    pub struct Release("pluginstore.Release") {
        "tag_name" => tag_name: String,
        "assets" => assets: Vec<ReleaseAsset>,
    }
}

#[derive(Clone)]
struct Prepared {
    request_url: String,
    kind: String,
    headers: Headers,
    authenticated: bool,
}

/// Go `pluginstore.Client`.
#[derive(Clone)]
pub struct Client {
    pub http: Arc<dyn Doer>,
    /// Distinguishes request state for different proxy/egress configurations.
    pub network_scope: String,
    pub rate_limiter: Option<Arc<GitHubRateLimiter>>,
    pub registry_url: String,
    pub user_agent: String,
    pub auth: Vec<AuthConfig>,
    pub resolved_auth: Vec<ResolvedAuthConfig>,
    pub resolved_auth_expires_at: Option<SystemTime>,
    prepared: Option<Prepared>,
}

impl Client {
    pub fn new(http: Arc<dyn Doer>) -> Self {
        Self {
            http,
            network_scope: String::new(),
            rate_limiter: None,
            registry_url: String::new(),
            user_agent: String::new(),
            auth: Vec::new(),
            resolved_auth: Vec::new(),
            resolved_auth_expires_at: None,
            prepared: None,
        }
    }

    fn limiter(&self) -> &GitHubRateLimiter {
        match self.rate_limiter.as_deref() {
            Some(limiter) => limiter,
            None => GitHubRateLimiter::shared(),
        }
    }

    /// Go `FetchRegistry`.
    pub async fn fetch_registry(&self) -> Result<Registry, StoreError> {
        let registry_url = match self.registry_url.trim() {
            "" => DEFAULT_REGISTRY_URL,
            u => u,
        };
        let data = self
            .get(registry_url, "application/json", REQUEST_KIND_REGISTRY, 0)
            .await?;
        Ok(parse_registry(&data)?)
    }

    fn release_url(plugin: &Plugin, tail: &str) -> Result<String, String> {
        let (owner, repo) = github_repository_parts(&plugin.repository)?;
        Ok(format!(
            "https://api.github.com/repos/{}/{}/releases/{tail}",
            url::path_escape(&owner),
            url::path_escape(&repo)
        ))
    }

    async fn fetch_release(&self, release_url: &str) -> Result<Release, StoreError> {
        let data = self
            .get(release_url, "application/vnd.github+json", REQUEST_KIND_METADATA, 0)
            .await?;
        gojson::from_slice(&data).map_err(|e| StoreError::Other(format!("decode release: {e}")))
    }

    /// Go `FetchLatestRelease`.
    pub async fn fetch_latest_release(&self, plugin: &Plugin) -> Result<Release, StoreError> {
        let release_url = Self::release_url(plugin, "latest")?;
        self.fetch_release(&release_url).await
    }

    /// Go `FetchReleaseByTag`.
    pub async fn fetch_release_by_tag(&self, plugin: &Plugin, tag: &str) -> Result<Release, StoreError> {
        github_repository_parts(&plugin.repository)?;
        let tag = tag.trim();
        if tag.is_empty() {
            return Err(StoreError::Other("release tag is required".into()));
        }
        let release_url = Self::release_url(plugin, &format!("tags/{}", url::path_escape(tag)))?;
        self.fetch_release(&release_url).await
    }

    /// Go `PrepareLatestRelease`: binds the cache key and the first release request
    /// to one credential snapshot.
    pub fn prepare_latest_release(&self, plugin: &Plugin) -> Result<(Client, String), StoreError> {
        let request_url = Self::release_url(plugin, "latest")?;
        let (headers, authenticated) = self.auth_headers(&request_url, REQUEST_KIND_METADATA)?;
        let key = format!(
            "{}/{}",
            request_url.to_lowercase(),
            request_identity(&self.network_scope, &headers, authenticated)
        );
        let mut client = self.clone();
        client.prepared = Some(Prepared {
            request_url,
            kind: REQUEST_KIND_METADATA.into(),
            headers,
            authenticated,
        });
        Ok((client, key))
    }

    /// Go `LatestReleaseCacheKey`.
    pub fn latest_release_cache_key(&self, plugin: &Plugin) -> Result<String, StoreError> {
        self.prepare_latest_release(plugin).map(|(_, key)| key)
    }

    /// Go `DownloadAsset`: the browser URL, or the API URL when there is none or
    /// credentials apply to it.
    pub async fn download_asset(&self, asset: &ReleaseAsset) -> Result<Bytes, StoreError> {
        let api_url = asset.api_url.trim();
        let mut download_url = asset.browser_download_url.trim();
        if (download_url.is_empty() || self.release_asset_api_authenticated(api_url)) && !api_url.is_empty() {
            download_url = api_url;
        }
        if download_url.is_empty() {
            return Err(StoreError::Other(format!(
                "asset {} missing download url",
                cpa_common::gostr::quote(&asset.name)
            )));
        }
        self.get(download_url, "application/octet-stream", REQUEST_KIND_ARTIFACT, 0)
            .await
    }

    fn release_asset_api_authenticated(&self, api_url: &str) -> bool {
        if api_url.is_empty() {
            return false;
        }
        match auth::matching_resolved_auth_config(&self.resolved_auth, api_url, REQUEST_KIND_ARTIFACT) {
            Some(item) => auth::resolved_auth_configured(item),
            None => auth::auth_configured(&self.auth, api_url, REQUEST_KIND_ARTIFACT),
        }
    }

    /// Go `DownloadArtifact`: bounded by the declared size.
    pub async fn download_artifact(&self, artifact: &Artifact) -> Result<Bytes, StoreError> {
        let artifact = normalize_install_plan(&InstallPlan {
            install_type: INSTALL_TYPE_DIRECT.into(),
            artifacts: vec![artifact.clone()],
        })
        .artifacts
        .remove(0);
        validate_artifact(&artifact)?;
        let max = artifact.size.max(0) as u64;
        let data = self
            .get(&artifact.url, "application/octet-stream", REQUEST_KIND_ARTIFACT, max)
            .await?;
        if max > 0 && data.len() as u64 > max {
            return Err(StoreError::Other("artifact exceeds declared size".into()));
        }
        Ok(data)
    }

    /// Go `authHeaders`: the URL checked, then the prepared or freshly applied
    /// credentials for it.
    fn auth_headers(&self, request_url: &str, kind: &str) -> Result<(Headers, bool), String> {
        auth::validate_request_url(&self.auth, request_url, kind)?;
        auth::validate_resolved_auth_expiry(
            &self.resolved_auth,
            self.resolved_auth_expires_at,
            SystemTime::now(),
            request_url,
            kind,
        )?;
        if let Some(prepared) = &self.prepared
            && prepared.request_url == request_url
            && prepared.kind == kind
        {
            return Ok((prepared.headers.clone(), prepared.authenticated));
        }
        let mut headers = Headers::new();
        let authenticated = auth::apply_auth(&mut headers, &self.resolved_auth, &self.auth, request_url, kind)?;
        Ok((headers, authenticated))
    }

    /// Go `get`: follows up to ten redirects itself, authenticating each hop on its
    /// own URL; GitHub API hops go through the rate limiter.
    pub(super) async fn get(
        &self,
        request_url: &str,
        accept: &str,
        kind: &str,
        max_size: u64,
    ) -> Result<Bytes, StoreError> {
        let mut current = request_url.trim().to_owned();
        let limiter = self.limiter();
        let mut redirects = 0;
        loop {
            let (mut headers, authenticated) = self.auth_headers(&current, kind)?;
            let rate_key = github_rate_limit_key(&current, &self.network_scope, &headers, authenticated);
            limiter.check(&rate_key)?;
            if !headers.contains_key("Accept") {
                headers.insert("Accept".into(), vec![accept.into()]);
            }
            if !headers.contains_key("User-Agent") {
                let ua = match self.user_agent.trim() {
                    "" => USER_AGENT,
                    ua => ua,
                };
                headers.insert("User-Agent".into(), vec![ua.into()]);
            }
            let response = self.http.get(&current, &headers).await;
            if authenticated {
                headers.clear();
            }
            let response = response.map_err(|cause| request_error(&current, &cause))?;
            if matches!(response.status, 301 | 302 | 303 | 307 | 308) {
                let _ = limiter.observe(&rate_key, response.status, &response.headers, None);
                let next = redirect_url(&response.headers, &current)?;
                if redirects >= MAX_REDIRECTS {
                    return Err(StoreError::Other(format!("stopped after {MAX_REDIRECTS} redirects")));
                }
                redirects += 1;
                current = next;
                continue;
            }
            return read_response(response, max_size, authenticated, limiter, &rate_key).await;
        }
    }
}

/// Go `pluginStoreRedirectURL`.
fn redirect_url(headers: &Headers, request_url: &str) -> Result<String, String> {
    let location = headers
        .get("Location")
        .and_then(|v| v.first())
        .map(|v| v.trim().to_owned())
        .unwrap_or_default();
    if location.is_empty() {
        return Err("redirect missing Location header".into());
    }
    let base = url::parse_err(request_url).map_err(|e| format!("parse redirect base: {e}"))?;
    let next = url::resolve(&base, &location).map_err(|e| format!("parse redirect location: {e}"))?;
    if next.scheme.is_empty() || next.host.is_empty() {
        return Err("redirect location is not absolute".into());
    }
    Ok(url::string(&next))
}

/// Go `readPluginStoreResponse`.
async fn read_response(
    response: DoerResponse,
    max_size: u64,
    authenticated: bool,
    limiter: &GitHubRateLimiter,
    rate_key: &str,
) -> Result<Bytes, StoreError> {
    let DoerResponse {
        status,
        headers,
        mut body,
    } = response;
    if !(200..300).contains(&status) {
        limiter.observe(rate_key, status, &headers, None)?;
        if authenticated && (rate_key.is_empty() || status != 403) {
            return Err(StoreError::Other(format!("unexpected status {status}")));
        }
        let mut text = Vec::new();
        while text.len() < 4096 {
            match body.next().await {
                Some(Ok(chunk)) => text.extend_from_slice(&chunk[..chunk.len().min(4096 - text.len())]),
                _ => break,
            }
        }
        limiter.observe(rate_key, status, &headers, Some(&text))?;
        if authenticated {
            return Err(StoreError::Other(format!("unexpected status {status}")));
        }
        return Err(StoreError::Other(format!(
            "unexpected status {status}: {}",
            String::from_utf8_lossy(&text).trim()
        )));
    }
    let _ = limiter.observe(rate_key, status, &headers, None);
    let max_size = match max_size {
        0 => MAX_DOWNLOAD_BYTES,
        declared => declared.min(MAX_DOWNLOAD_BYTES),
    };
    let limit = max_size + 1;
    let mut data = bytes::BytesMut::new();
    while (data.len() as u64) < limit {
        match body.next().await {
            Some(Ok(chunk)) => {
                let room = (limit - data.len() as u64).min(chunk.len() as u64) as usize;
                data.extend_from_slice(&chunk[..room]);
            }
            Some(Err(e)) => return Err(StoreError::Other(format!("read response: {e}"))),
            None => break,
        }
    }
    if data.len() as u64 > max_size {
        return Err(StoreError::Other(format!(
            "response exceeds maximum allowed size of {max_size} bytes"
        )));
    }
    Ok(data.freeze())
}

/// Go `pluginStoreRequestError`: the URL without user info, query or fragment.
fn request_error(request_url: &str, cause: &str) -> String {
    let safe = match url::parse(request_url.trim()) {
        Some(mut u) if !u.scheme.is_empty() && !u.host.is_empty() => {
            u.has_user = false;
            u.raw_query.clear();
            u.force_query = false;
            u.fragment.clear();
            url::string(&u)
        }
        _ => "plugin store url".into(),
    };
    format!("request {safe} failed: {cause}")
}

/// Go `requestIdentity`: SHA-256 of the network scope, whether credentials apply,
/// and (if so) the headers as `Header.Write` renders them.
pub(super) fn request_identity(network_scope: &str, headers: &Headers, authenticated: bool) -> String {
    let mut hash = Sha256::new();
    hash.update(format!("{network_scope}\x00{authenticated}\x00").as_bytes());
    if authenticated {
        for (name, values) in headers {
            for value in values {
                let value = value.replace(['\n', '\r'], " ");
                hash.update(format!("{name}: {}\r\n", value.trim_matches([' ', '\t'])).as_bytes());
            }
        }
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Go `SelectReleaseAssets`: the platform archive and `checksums.txt`.
pub fn select_release_assets(
    release: &Release,
    id: &str,
    version: &str,
    goos: &str,
    goarch: &str,
) -> Result<(ReleaseAsset, ReleaseAsset), String> {
    let archive_name = archive_name(id, version, goos, goarch);
    let mut archive = None;
    let mut checksums = None;
    for asset in &release.assets {
        match asset.name.trim() {
            n if n == archive_name => archive = Some(asset.clone()),
            "checksums.txt" => checksums = Some(asset.clone()),
            _ => {}
        }
    }
    let archive = archive
        .filter(|a| !a.name.trim().is_empty())
        .ok_or_else(|| format!("release asset {archive_name} not found"))?;
    let checksums = checksums
        .filter(|a| !a.name.trim().is_empty())
        .ok_or_else(|| "release asset checksums.txt not found".to_owned())?;
    Ok((archive, checksums))
}

/// Go `ArchiveName`: `{id}_{version}_{goos}_{goarch}.zip`.
pub fn archive_name(id: &str, version: &str, goos: &str, goarch: &str) -> String {
    format!("{}_{}_{}_{}.zip", id.trim(), version.trim(), goos.trim(), goarch.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An endless 200 response; nothing leaves the process.
    struct Endless;

    impl Doer for Endless {
        fn get(&self, _: &str, _: &Headers) -> BoxFuture<'_, Result<DoerResponse, String>> {
            Box::pin(async {
                let chunk = Bytes::from(vec![b'x'; 1 << 20]);
                Ok(DoerResponse {
                    status: 200,
                    headers: Headers::new(),
                    body: futures_util::stream::repeat(Ok(chunk)).boxed(),
                })
            })
        }
    }

    #[tokio::test]
    async fn downloads_stop_at_the_ceiling() {
        let mut client = Client::new(Arc::new(Endless));
        client.registry_url = "https://registry.example.invalid/plugins.json".into();
        let err = client.fetch_registry().await.unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("response exceeds maximum allowed size of {MAX_DOWNLOAD_BYTES} bytes")
        );
        // A declared size lowers the ceiling; one above it does not raise it.
        let url = "https://cdn.example.invalid/p.zip";
        let err = client
            .get(url, "application/octet-stream", REQUEST_KIND_ARTIFACT, 10)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "response exceeds maximum allowed size of 10 bytes");
        let err = client
            .get(url, "application/octet-stream", REQUEST_KIND_ARTIFACT, u64::MAX / 2)
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("response exceeds maximum allowed size of {MAX_DOWNLOAD_BYTES} bytes")
        );
    }
}
