//! `OBJECTSTORE_*`: config and auth files in an S3-compatible bucket (Go
//! internal/store/objectstore.go over minio-go): `config/config.yaml` and `auths/<file>`,
//! mirrored into `<root>/config` and `<root>/auths`. Path-style requests signed with
//! SigV4; the region comes from the bucket location like minio-go.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::BoxFuture;
use tokio::sync::{Mutex, OnceCell};

use crate::sigv4;

const CONFIG_KEY: &str = "config/config.yaml";
const AUTH_PREFIX: &str = "auths";
const DEFAULT_REGION: &str = "us-east-1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Go `ObjectStoreConfig` after main's endpoint parsing.
#[derive(Debug, Clone)]
pub struct ObjectConfig {
    /// `host[:port]`.
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub local_root: PathBuf,
    pub use_ssl: bool,
}

impl ObjectConfig {
    /// Go main's `OBJECTSTORE_ENDPOINT` handling: an `http://` or `https://` URL picks the
    /// scheme, anything else is `host[:port]` over HTTPS.
    pub fn endpoint_from(raw: &str) -> Result<(String, bool)> {
        let raw = raw.trim();
        let mut endpoint = raw.to_owned();
        let mut use_ssl = true;
        if raw.contains("://") {
            let parsed =
                url::Url::parse(raw).map_err(|e| anyhow!("failed to parse object store endpoint {raw:?}: {e}"))?;
            use_ssl = match parsed.scheme().to_lowercase().as_str() {
                "http" => false,
                "https" => true,
                other => bail!("unsupported object store scheme {other:?} (only http and https are allowed)"),
            };
            let Some(host) = parsed.host_str().filter(|h| !h.is_empty()) else {
                bail!("object store endpoint {raw:?} is missing host information");
            };
            let host = match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            };
            endpoint = if parsed.path().is_empty() || parsed.path() == "/" {
                host
            } else {
                format!("{host}{}", parsed.path().trim_end_matches('/'))
            };
        }
        Ok((endpoint.trim_end_matches('/').to_owned(), use_ssl))
    }
}

/// Go `ObjectTokenStore`.
pub struct ObjectStore {
    cfg: ObjectConfig,
    http: wreq::Client,
    config_path: PathBuf,
    auth_dir: PathBuf,
    region: OnceCell<String>,
    lock: Mutex<()>,
}

/// An S3 error reply.
#[derive(Debug)]
struct S3Error {
    status: u16,
    code: String,
    message: String,
    /// `<Region>` or `x-amz-bucket-region`.
    region: String,
    /// The `Server` header.
    server: String,
}

/// An HTTP answer with the headers the store reads.
struct Reply {
    status: u16,
    body: Vec<u8>,
    server: String,
    bucket_region: String,
}

impl std::fmt::Display for S3Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "{} (HTTP {})", self.code, self.status)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for S3Error {}

/// Go `isObjectNotFound`.
fn not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<S3Error>()
        .is_some_and(|e| e.status == 404 || matches!(e.code.as_str(), "NoSuchKey" | "NotFound" | "NoSuchBucket"))
}

/// The text of the first `<tag>` element.
fn xml_text(body: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    loop {
        let start = from + body[from..].find(&open)?;
        let rest = &body[start + open.len()..];
        // `<Key` must not match `<KeyCount>`.
        if !rest.starts_with(|c: char| c == '>' || c == '/' || c.is_whitespace()) {
            from = start + open.len();
            continue;
        }
        let close = rest.find('>')?;
        if rest[..close].ends_with('/') {
            return Some(String::new());
        }
        let content = &rest[close + 1..];
        let end = content.find(&format!("</{tag}>"))?;
        return Some(xml_unescape(&content[..end]));
    }
}

fn xml_all(body: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        out.push(xml_unescape(&after[..end]));
        rest = &after[end + close.len()..];
    }
    out
}

fn xml_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(end) = after.find(';') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let entity = &after[..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&rest[i..i + 2 + end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Go `url.QueryUnescape` (S3 `encoding-type=url` keys).
fn query_unescape(value: &str) -> String {
    url::form_urlencoded::parse(format!("k={value}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

fn mkdir_0700(path: &Path) -> Result<()> {
    crate::private_fs::create_dir_all(path)?;
    Ok(())
}

/// Go `os.WriteFile(path, data, 0o600)`.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = crate::private_fs::create_truncate(path)?;
    file.write_all(bytes)?;
    Ok(())
}

pub(crate) fn normalize_line_endings(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' if bytes.get(i + 1) == Some(&b'\n') => {
                out.push(b'\n');
                i += 2;
                continue;
            }
            b'\r' => out.push(b'\n'),
            b => out.push(b),
        }
        i += 1;
    }
    out
}

/// Go `misc.CopyConfigTemplate`, or an empty file without a template.
pub(crate) fn seed_config(example: &Path, target: &Path) -> Result<()> {
    if let Some(parent) = target.parent() {
        mkdir_0700(parent)?;
    }
    if example.as_os_str().is_empty() {
        return write_file(target, b"");
    }
    let template = std::fs::read(example).with_context(|| format!("open {}", example.display()))?;
    write_file(target, &template)?;
    std::fs::File::open(target)?.sync_all()?;
    Ok(())
}

impl ObjectStore {
    /// Go `NewObjectTokenStore`.
    pub fn new(cfg: ObjectConfig) -> Result<Self> {
        let mut cfg = cfg;
        cfg.endpoint = cfg.endpoint.trim().to_owned();
        cfg.bucket = cfg.bucket.trim().to_owned();
        cfg.access_key = cfg.access_key.trim().to_owned();
        cfg.secret_key = cfg.secret_key.trim().to_owned();
        if cfg.endpoint.is_empty() {
            bail!("object store: endpoint is required");
        }
        if cfg.bucket.is_empty() {
            bail!("object store: bucket is required");
        }
        if cfg.access_key.is_empty() {
            bail!("object store: access key is required");
        }
        if cfg.secret_key.is_empty() {
            bail!("object store: secret key is required");
        }
        // minio.New rejects endpoints with a path.
        if cfg.endpoint.contains('/') {
            bail!("object store: create client: Endpoint url cannot have fully qualified paths.");
        }
        let root = crate::go_abs(&cfg.local_root).context("object store: resolve spool directory")?;
        let config_dir = root.join("config");
        let auth_dir = root.join("auths");
        mkdir_0700(&config_dir).context("object store: create config directory")?;
        mkdir_0700(&auth_dir).context("object store: create auth directory")?;
        let http = wreq::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("object store: create client")?;
        Ok(Self {
            cfg,
            http,
            config_path: config_dir.join("config.yaml"),
            auth_dir,
            region: OnceCell::new(),
            lock: Mutex::new(()),
        })
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone()
    }

    pub fn auth_dir(&self) -> PathBuf {
        self.auth_dir.clone()
    }

    pub fn bucket(&self) -> &str {
        &self.cfg.bucket
    }

    async fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        body: Vec<u8>,
        content_type: Option<&str>,
        region: &str,
    ) -> Result<Reply> {
        let mut path = format!("/{}", self.cfg.bucket);
        if let Some(key) = key {
            path.push('/');
            path.push_str(key);
        }
        let scheme = if self.cfg.use_ssl { "https" } else { "http" };
        let mut url = format!("{scheme}://{}{}", self.cfg.endpoint, sigv4::encode_path(&path));
        if !query.is_empty() {
            url.push('?');
            url.push_str(&sigv4::canonical_query(query));
        }
        let payload = if body.is_empty() {
            sigv4::EMPTY_SHA256.to_owned()
        } else {
            sigv4::sha256_hex(&body)
        };
        let mut headers = vec![("x-amz-content-sha256".to_owned(), payload.clone())];
        if let Some(content_type) = content_type {
            headers.push(("content-type".to_owned(), content_type.to_owned()));
        }
        let request = sigv4::Request {
            method,
            path: &path,
            query,
            headers: &headers,
            host: &self.cfg.endpoint,
            payload_sha256: &payload,
        };
        let (date, authorization) = sigv4::sign(
            &request,
            &self.cfg.access_key,
            &self.cfg.secret_key,
            region,
            chrono::Utc::now(),
        );
        let method = wreq::Method::from_bytes(method.as_bytes()).map_err(|e| anyhow!("{e}"))?;
        let mut builder = self
            .http
            .request(method, &url)
            .header("x-amz-date", date)
            .header("authorization", authorization);
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let response = builder.body(body).send().await.map_err(|e| anyhow!("{e}"))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let (server, bucket_region) = (header("server"), header("x-amz-bucket-region"));
        let body = response.bytes().await.map_err(|e| anyhow!("{e}"))?.to_vec();
        Ok(Reply {
            status,
            body,
            server,
            bucket_region,
        })
    }

    /// minio-go `httpRespToErrorResponse`: the XML error, else a code from the status;
    /// the region from `<Region>`, else `x-amz-bucket-region`.
    fn error(reply: &Reply) -> anyhow::Error {
        let text = String::from_utf8_lossy(&reply.body);
        let status = reply.status;
        let code = xml_text(&text, "Code").unwrap_or_else(|| match status {
            404 => "NoSuchKey".into(),
            403 => "AccessDenied".into(),
            _ => format!("HTTP{status}"),
        });
        let message = xml_text(&text, "Message").unwrap_or_default();
        let region = xml_text(&text, "Region")
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| reply.bucket_region.clone());
        anyhow::Error::new(S3Error {
            status,
            code,
            message,
            region,
            server: reply.server.clone(),
        })
    }

    /// minio-go `getBucketLocation` / `processBucketLocationResponse`, cached once found.
    async fn region(&self) -> Result<String> {
        self.region
            .get_or_try_init(|| async {
                let query = [("location".to_owned(), String::new())];
                let reply = self.send("GET", None, &query, Vec::new(), None, DEFAULT_REGION).await?;
                if reply.status == 200 {
                    let location =
                        xml_text(&String::from_utf8_lossy(&reply.body), "LocationConstraint").unwrap_or_default();
                    return Ok(match location.as_str() {
                        "" => DEFAULT_REGION.to_owned(),
                        // Legacy buckets answer `EU`.
                        "EU" => "eu-west-1".to_owned(),
                        _ => location,
                    });
                }
                let error = Self::error(&reply);
                let Some(e) = error.downcast_ref::<S3Error>() else {
                    return Err(error);
                };
                match (e.code.as_str(), e.server.as_str()) {
                    ("NotImplemented", "AmazonSnowball") => Ok("snowball".to_owned()),
                    ("NotImplemented", "cloudflare") => Ok(DEFAULT_REGION.to_owned()),
                    ("AuthorizationHeaderMalformed" | "InvalidRegion" | "AccessDenied", _) if e.region.is_empty() => {
                        Ok(DEFAULT_REGION.to_owned())
                    }
                    ("AuthorizationHeaderMalformed" | "InvalidRegion" | "AccessDenied", _) => Ok(e.region.clone()),
                    _ => Err(error),
                }
            })
            .await
            .cloned()
    }

    async fn call(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(String, String)],
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>> {
        let region = self.region().await?;
        let reply = self.send(method, key, query, body, content_type, &region).await?;
        if (200..300).contains(&reply.status) {
            return Ok(reply.body);
        }
        Err(Self::error(&reply))
    }

    /// Go `ensureBucket`.
    async fn ensure_bucket(&self) -> Result<()> {
        let exists = match self.call("HEAD", None, &[], Vec::new(), None).await {
            Ok(_) => true,
            Err(error) if not_found(&error) => false,
            Err(error) => return Err(error.context("object store: check bucket")),
        };
        if !exists {
            // MakeBucket in us-east-1 sends no location constraint.
            let reply = self.send("PUT", None, &[], Vec::new(), None, DEFAULT_REGION).await?;
            if !(200..300).contains(&reply.status) {
                return Err(Self::error(&reply).context("object store: create bucket"));
            }
        }
        Ok(())
    }

    fn prefixed(key: &str) -> String {
        key.trim_start_matches('/').to_owned()
    }

    async fn put(&self, key: &str, data: &[u8], content_type: &str) -> Result<()> {
        if data.is_empty() {
            return self.remove(key).await;
        }
        let key = Self::prefixed(key);
        self.call("PUT", Some(&key), &[], data.to_vec(), Some(content_type))
            .await
            .map(|_| ())
            .with_context(|| format!("object store: put object {key}"))
    }

    async fn remove(&self, key: &str) -> Result<()> {
        let key = Self::prefixed(key);
        match self.call("DELETE", Some(&key), &[], Vec::new(), None).await {
            Ok(_) => Ok(()),
            Err(error) if not_found(&error) => Ok(()),
            Err(error) => Err(error.context(format!("object store: delete object {key}"))),
        }
    }

    /// Go `Bootstrap`.
    pub async fn bootstrap(&self, example: &Path) -> Result<()> {
        self.ensure_bucket().await?;
        self.sync_config(example).await?;
        self.sync_auth().await
    }

    async fn sync_config(&self, example: &Path) -> Result<()> {
        let key = Self::prefixed(CONFIG_KEY);
        match self.call("HEAD", Some(&key), &[], Vec::new(), None).await {
            Ok(_) => {
                let data = self
                    .call("GET", Some(&key), &[], Vec::new(), None)
                    .await
                    .context("object store: fetch config")?;
                write_file(&self.config_path, &normalize_line_endings(&data)).context("object store: write config")?;
            }
            Err(error) if not_found(&error) => {
                if !self.config_path.exists() {
                    seed_config(example, &self.config_path).context("object store: copy example config")?;
                }
                let data = std::fs::read(&self.config_path).context("object store: read local config")?;
                if !data.is_empty() {
                    self.put(CONFIG_KEY, &data, "application/x-yaml").await?;
                }
            }
            Err(error) => return Err(error.context("object store: stat config")),
        }
        Ok(())
    }

    /// Go `syncAuthFromBucket`: incremental, never wipes the mirror (a wipe would turn
    /// into remote deletions through the watcher).
    async fn sync_auth(&self) -> Result<()> {
        mkdir_0700(&self.auth_dir).context("object store: create auth directory")?;
        let prefix = Self::prefixed(&format!("{AUTH_PREFIX}/"));
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.clone()),
                ("encoding-type".to_owned(), "url".to_owned()),
            ];
            if let Some(token) = &token {
                query.push(("continuation-token".to_owned(), token.clone()));
            }
            let body = self
                .call("GET", None, &query, Vec::new(), None)
                .await
                .context("object store: list auth objects")?;
            let text = String::from_utf8_lossy(&body).into_owned();
            for key in xml_all(&text, "Key").iter().map(|k| query_unescape(k)) {
                let Some(rel) = key.strip_prefix(&prefix) else { continue };
                if rel.is_empty() || rel.ends_with('/') {
                    continue;
                }
                let rel_path = Path::new(rel);
                // ponytail: Go cleans `sub/../x.json` to `x.json` and fetches it; URL
                // parsing here would rewrite such a key, so any dot segment is skipped.
                if rel_path.is_absolute() || rel.split('/').any(|s| s == "." || s == "..") {
                    tracing::warn!(key, "object store: skip auth outside mirror");
                    continue;
                }
                let local = self.auth_dir.join(crate::clean(rel_path));
                if let Some(parent) = local.parent() {
                    mkdir_0700(parent).context("object store: prepare auth subdir")?;
                }
                let data = self
                    .call("GET", Some(&key), &[], Vec::new(), None)
                    .await
                    .with_context(|| format!("object store: download auth {key}"))?;
                write_file(&local, &data).with_context(|| format!("object store: write auth {}", local.display()))?;
            }
            let truncated = xml_text(&text, "IsTruncated").is_some_and(|t| t == "true");
            token = xml_text(&text, "NextContinuationToken").filter(|t| !t.is_empty());
            if !truncated || token.is_none() {
                return Ok(());
            }
        }
    }

    /// Go `filepath.Rel(authDir, path)` on cleaned paths.
    // ponytail: Go would upload a path outside the mirror as `auths/../<name>`; it is
    // refused here instead.
    fn auth_key(&self, path: &Path) -> Result<String> {
        let rel = path.strip_prefix(&self.auth_dir).map_err(|_| {
            anyhow!(
                "object store: resolve auth relative path: {} is outside the mirror",
                path.display()
            )
        })?;
        Ok(format!("{AUTH_PREFIX}/{}", rel.to_string_lossy().replace('\\', "/")))
    }

    /// Go `uploadAuth`: a missing or empty file deletes the object.
    async fn upload_auth(&self, path: &Path) -> Result<()> {
        let key = self.auth_key(path)?;
        match std::fs::read(path) {
            Ok(data) if !data.is_empty() => self.put(&key, &data, "application/json").await,
            Ok(_) => self.remove(&key).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.remove(&key).await,
            Err(e) => Err(anyhow!("object store: read auth file: {e}")),
        }
    }

    /// Go `PersistAuthFiles`.
    pub async fn persist_auth_files(&self, paths: &[PathBuf]) -> Result<()> {
        let _guard = self.lock.lock().await;
        for path in paths {
            self.upload_auth(&self.resolve(path)).await?;
        }
        Ok(())
    }

    /// A mirror path as Go's `filepath.Join` gives it: relative to the mirror, cleaned
    /// once, so the file read and the object key name the same thing.
    fn resolve(&self, path: &Path) -> PathBuf {
        crate::clean(&self.auth_dir.join(path))
    }

    /// Go `PersistConfig`.
    pub async fn persist_config(&self) -> Result<()> {
        let _guard = self.lock.lock().await;
        match std::fs::read(&self.config_path) {
            Ok(data) if !data.is_empty() => self.put(CONFIG_KEY, &data, "application/x-yaml").await,
            Ok(_) => self.remove(CONFIG_KEY).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.remove(CONFIG_KEY).await,
            Err(e) => Err(anyhow!("object store: read config file: {e}")),
        }
    }

    /// Go `Delete`.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        let path = &self.resolve(path);
        let _guard = self.lock.lock().await;
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("object store: delete auth file: {e}"),
        }
        let key = self.auth_key(path)?;
        self.remove(&key).await
    }
}

pub struct ObjectPersister(pub Arc<ObjectStore>);

impl cpa_server::persist::StorePersister for ObjectPersister {
    fn persist_config(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.0.persist_config())
    }

    fn persist_auth_files(&self, _message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.0.persist_auth_files(&paths).await })
    }

    fn delete_auth(&self, path: PathBuf) -> Result<()> {
        // The request runs on the runtime's reactor; this thread only waits for it.
        tokio::runtime::Handle::current().block_on(self.0.delete(&path))
    }

    fn auth_dir(&self) -> PathBuf {
        self.0.auth_dir()
    }
}

// Mode-bit assertions: Unix only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn endpoints_parse_like_go_main() {
        assert_eq!(
            ObjectConfig::endpoint_from("http://127.0.0.1:9000/").unwrap(),
            ("127.0.0.1:9000".into(), false)
        );
        assert_eq!(
            ObjectConfig::endpoint_from("https://s3.example.com").unwrap(),
            ("s3.example.com".into(), true)
        );
        assert_eq!(
            ObjectConfig::endpoint_from("minio:9000").unwrap(),
            ("minio:9000".into(), true)
        );
        assert_eq!(
            ObjectConfig::endpoint_from("https://h/base/").unwrap(),
            ("h/base".into(), true)
        );
        assert_eq!(
            ObjectConfig::endpoint_from("ftp://h").unwrap_err().to_string(),
            "unsupported object store scheme \"ftp\" (only http and https are allowed)"
        );
    }

    #[test]
    fn xml_and_key_decoding() {
        let body = "<ListBucketResult><IsTruncated>true</IsTruncated><Contents><Key>auths/a%2Bb+c.json</Key></Contents><Contents><Key>auths/x&amp;y.json</Key></Contents><NextContinuationToken>t&lt;1</NextContinuationToken></ListBucketResult>";
        assert_eq!(xml_all(body, "Key"), vec!["auths/a%2Bb+c.json", "auths/x&y.json"]);
        assert_eq!(query_unescape("auths/a%2Bb+c.json"), "auths/a+b c.json");
        assert_eq!(xml_text(body, "NextContinuationToken").unwrap(), "t<1");
        assert_eq!(
            xml_text("<LocationConstraint xmlns=\"x\"/>", "LocationConstraint").unwrap(),
            ""
        );
        assert_eq!(xml_text("<KeyCount>2</KeyCount><Key>a</Key>", "Key").unwrap(), "a");
        assert_eq!(normalize_line_endings(b"a\r\nb\rc\n"), b"a\nb\nc\n");
    }

    use crate::fake_s3::{ACCESS, FakeS3, Location, SECRET};
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn scratch(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("cpa-store-{name}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store(s3: &FakeS3, root: &Path) -> ObjectStore {
        ObjectStore::new(ObjectConfig {
            endpoint: s3.addr.to_string(),
            bucket: " tokens ".into(),
            access_key: format!(" {ACCESS} "),
            secret_key: SECRET.into(),
            local_root: root.to_path_buf(),
            use_ssl: false,
        })
        .unwrap()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn bootstrap_creates_the_bucket_and_uploads_the_template() {
        let s3 = FakeS3::start("us-east-1", Location::Answer, 1000).await;
        let dir = scratch("obj-seed");
        let example = dir.join("config.example.yaml");
        std::fs::write(&example, "port: 8317\r\n").unwrap();
        let store = store(&s3, &dir.join("objectstore"));
        store.bootstrap(&example).await.unwrap();
        assert!(
            s3.state.log.lock().unwrap().iter().any(|l| l == "PUT /tokens"),
            "bucket created"
        );
        // Go uploads the local bytes as they are; only downloads normalize.
        assert_eq!(s3.object("tokens", "config/config.yaml").unwrap(), b"port: 8317\r\n");
        assert_eq!(std::fs::read(store.config_path()).unwrap(), b"port: 8317\r\n");
        assert_eq!(mode(&store.config_path()), 0o600);
        assert_eq!(
            store.auth_dir(),
            std::path::absolute(dir.join("objectstore/auths")).unwrap()
        );
    }

    #[tokio::test]
    async fn bootstrap_mirrors_the_bucket_without_wiping_local_files() {
        // A regional bucket and two keys per page: the store must find the region and
        // follow continuation tokens.
        let s3 = FakeS3::start("eu-central-1", Location::Answer, 2).await;
        s3.put("tokens", "config/config.yaml", b"a: 1\r\nb: 2\r");
        s3.put("tokens", "auths/x.json", b"{\"x\":1}");
        s3.put("tokens", "auths/sub/y.json", b"{\"y\":1}");
        s3.put("tokens", "auths/a b+c.json", b"{\"s\":1}");
        s3.put("tokens", "auths/sub/../z.json", b"{\"z\":1}");
        s3.put("tokens", "auths/../evil.json", b"{}");
        s3.put("tokens", "auths/folder/", b"");
        let dir = scratch("obj-mirror");
        let store = store(&s3, &dir);
        std::fs::write(store.auth_dir().join("local.json"), b"{}").unwrap();
        store.bootstrap(Path::new("")).await.unwrap();
        assert_eq!(std::fs::read(store.config_path()).unwrap(), b"a: 1\nb: 2\n");
        let auth = store.auth_dir();
        assert_eq!(std::fs::read(auth.join("x.json")).unwrap(), b"{\"x\":1}");
        assert_eq!(std::fs::read(auth.join("sub/y.json")).unwrap(), b"{\"y\":1}");
        assert_eq!(std::fs::read(auth.join("a b+c.json")).unwrap(), b"{\"s\":1}");
        // Dot segments never reach the disk; folder markers are not files.
        assert!(!auth.join("z.json").exists());
        assert!(!dir.join("evil.json").exists() && !auth.join("folder").exists());
        assert!(auth.join("local.json").exists(), "the mirror is never wiped");
        assert_eq!(mode(&auth.join("x.json")), 0o600);
        let log = s3.state.log.lock().unwrap();
        let lists: Vec<&String> = log.iter().filter(|l| l.contains("list-type=2")).collect();
        assert!(lists.len() >= 3, "{lists:?}");
        assert!(lists[1].contains("continuation-token="), "{lists:?}");
        assert!(lists[0].contains("encoding-type=url"), "{lists:?}");
    }

    #[tokio::test]
    async fn persist_and_delete_follow_the_mirror() {
        let s3 = FakeS3::start("us-east-1", Location::DeniedBare, 1000).await;
        let dir = scratch("obj-persist");
        let store = store(&s3, &dir);
        store.bootstrap(Path::new("")).await.unwrap();
        // An empty seeded config is not uploaded.
        assert!(s3.object("tokens", "config/config.yaml").is_none());
        let auth = store.auth_dir();
        std::fs::write(auth.join("a.json"), b"{\"a\":1}").unwrap();
        std::fs::create_dir_all(auth.join("team")).unwrap();
        std::fs::write(auth.join("team/b.json"), b"{\"b\":1}").unwrap();
        store
            .persist_auth_files(&[auth.join("a.json"), PathBuf::from("team/b.json")])
            .await
            .unwrap();
        assert_eq!(s3.object("tokens", "auths/a.json").unwrap(), b"{\"a\":1}");
        assert_eq!(s3.object("tokens", "auths/team/b.json").unwrap(), b"{\"b\":1}");
        // Empty and missing files remove the object.
        std::fs::write(auth.join("a.json"), b"").unwrap();
        std::fs::remove_file(auth.join("team/b.json")).unwrap();
        store
            .persist_auth_files(&[auth.join("a.json"), auth.join("team/b.json")])
            .await
            .unwrap();
        assert!(s3.object("tokens", "auths/a.json").is_none());
        assert!(s3.object("tokens", "auths/team/b.json").is_none());
        // Explicit delete removes both copies.
        std::fs::write(auth.join("c.json"), b"{}").unwrap();
        store.persist_auth_files(&[auth.join("c.json")]).await.unwrap();
        store.delete(&auth.join("c.json")).await.unwrap();
        assert!(!auth.join("c.json").exists());
        assert!(s3.object("tokens", "auths/c.json").is_none());
        // Config follows the file, including its removal.
        std::fs::write(store.config_path(), b"port: 1\n").unwrap();
        store.persist_config().await.unwrap();
        assert_eq!(s3.object("tokens", "config/config.yaml").unwrap(), b"port: 1\n");
        std::fs::remove_file(store.config_path()).unwrap();
        store.persist_config().await.unwrap();
        assert!(s3.object("tokens", "config/config.yaml").is_none());
        // Paths outside the mirror are refused.
        let outside = dir.join("outside.json");
        std::fs::write(&outside, b"{}").unwrap();
        assert!(store.persist_auth_files(&[outside]).await.is_err());
    }

    /// minio-go maps the legacy `EU` location to eu-west-1 for signing.
    #[tokio::test]
    async fn the_legacy_eu_location_signs_for_eu_west_1() {
        let s3 = FakeS3::start("eu-west-1", Location::Answer, 1000).await;
        s3.put("tokens", "auths/x.json", b"{}");
        let store = store(&s3, &scratch("obj-eu"));
        store.bootstrap(Path::new("")).await.unwrap();
        assert!(store.auth_dir().join("x.json").exists());
    }

    /// A path through a missing directory resolves before both the read and the key:
    /// `missing/../a.json` uploads `a.json` rather than deleting `auths/a.json`.
    #[tokio::test]
    async fn dot_segments_resolve_before_the_read_and_the_key() {
        let s3 = FakeS3::start("us-east-1", Location::Answer, 1000).await;
        let store = store(&s3, &scratch("obj-dots"));
        store.bootstrap(Path::new("")).await.unwrap();
        std::fs::write(store.auth_dir().join("a.json"), b"{\"v\":1}").unwrap();
        store.persist_auth_files(&[PathBuf::from("a.json")]).await.unwrap();
        std::fs::write(store.auth_dir().join("a.json"), b"{\"v\":2}").unwrap();
        store
            .persist_auth_files(&[PathBuf::from("missing/../a.json")])
            .await
            .unwrap();
        assert_eq!(s3.object("tokens", "auths/a.json").unwrap(), b"{\"v\":2}");
    }

    /// minio-go's discovery recovery: a regional bucket whose location cannot be read
    /// still works when the error names the region (header or XML).
    #[tokio::test]
    async fn regions_come_from_location_errors_like_minio() {
        for location in [Location::Denied, Location::Malformed] {
            let s3 = FakeS3::start("eu-central-1", location, 1000).await;
            s3.put("tokens", "config/config.yaml", b"port: 2\n");
            s3.put("tokens", "auths/x.json", b"{}");
            let dir = scratch("obj-region");
            let store = store(&s3, &dir);
            store.bootstrap(Path::new("")).await.unwrap();
            assert_eq!(std::fs::read(store.config_path()).unwrap(), b"port: 2\n");
            assert!(store.auth_dir().join("x.json").exists());
        }
        // Without a region in the denial, minio falls back to us-east-1, which the
        // regional bucket rejects (a HEAD reply carries no error body).
        let s3 = FakeS3::start("eu-central-1", Location::DeniedBare, 1000).await;
        s3.put("tokens", "config/config.yaml", b"port: 2\n");
        let error = format!(
            "{:#}",
            store(&s3, &scratch("obj-noregion"))
                .bootstrap(Path::new(""))
                .await
                .unwrap_err()
        );
        assert!(error.starts_with("object store: check bucket: "), "{error}");
    }

    #[tokio::test]
    async fn wrong_credentials_surface_the_s3_error() {
        let s3 = FakeS3::start("us-east-1", Location::Answer, 1000).await;
        let dir = scratch("obj-denied");
        let store = ObjectStore::new(ObjectConfig {
            endpoint: s3.addr.to_string(),
            bucket: "tokens".into(),
            access_key: ACCESS.into(),
            secret_key: "wrong".into(),
            local_root: dir,
            use_ssl: false,
        })
        .unwrap();
        let error = format!("{:#}", store.bootstrap(Path::new("")).await.unwrap_err());
        assert!(error.contains("SignatureDoesNotMatch"), "{error}");
        assert!(!error.contains("wrong"), "{error}");
    }
}
