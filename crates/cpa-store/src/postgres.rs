//! `PGSTORE_*`: config, auth files and cooldown state in PostgreSQL (Go
//! internal/store/postgresstore.go and postgres_cooldown_store.go). The config and auth
//! files are mirrored into `<root>/config` and `<root>/auths`; the tables and SQL are
//! Go's, so both implementations can share one database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Utc};
use cpa_server::cooldown_store::Record;
use futures_util::future::BoxFuture;
use tokio::sync::Mutex;

use crate::clean;
use crate::object::{normalize_line_endings, seed_config, write_file};
use crate::pgconn::{Dsn, Pg};

const DEFAULT_CONFIG_TABLE: &str = "config_store";
const DEFAULT_AUTH_TABLE: &str = "auth_store";
const DEFAULT_COOLDOWN_TABLE: &str = "cooldown_store";
const DEFAULT_CONFIG_KEY: &str = "config";
const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// Go `PostgresStoreConfig` (the table names keep Go's defaults).
#[derive(Debug, Clone, Default)]
pub struct PostgresConfig {
    pub dsn: String,
    pub schema: String,
    pub spool_dir: PathBuf,
}

/// Go `PostgresStore`.
pub struct PostgresStore {
    pg: Pg,
    config_table: String,
    auth_table: String,
    cooldown_table: String,
    schema: String,
    spool_root: PathBuf,
    config_path: PathBuf,
    auth_dir: PathBuf,
    lock: Mutex<()>,
}

/// Go `quoteIdentifier`.
fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn mkdir_0700(path: &Path) -> std::io::Result<()> {
    crate::private_fs::create_dir_all(path)
}

fn db_error(context: &str, error: tokio_postgres::Error) -> anyhow::Error {
    anyhow!("{context}: {}", crate::pgconn::error_text(&error))
}

impl PostgresStore {
    /// Go `NewPostgresStore`: spool directories, then a connection that answers.
    pub async fn connect(cfg: PostgresConfig) -> Result<Self> {
        Self::connect_with_env(cfg, &|key| std::env::var(key).ok()).await
    }

    pub(crate) async fn connect_with_env(cfg: PostgresConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let dsn = cfg.dsn.trim();
        if dsn.is_empty() {
            bail!("postgres store: DSN is required");
        }
        let spool = if cfg.spool_dir.as_os_str().is_empty() {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join("pgstore"),
                Err(_) => std::env::temp_dir().join("pgstore"),
            }
        } else {
            cfg.spool_dir.clone()
        };
        let spool_root = crate::go_abs(&spool).map_err(|e| anyhow!("postgres store: resolve spool directory: {e}"))?;
        let config_dir = spool_root.join("config");
        let auth_dir = spool_root.join("auths");
        mkdir_0700(&config_dir).map_err(|e| anyhow!("postgres store: create config directory: {e}"))?;
        mkdir_0700(&auth_dir).map_err(|e| anyhow!("postgres store: create auth directory: {e}"))?;
        let dsn = Dsn::parse(dsn, env).map_err(|e| anyhow!("postgres store: open database connection: {e}"))?;
        let pg = Pg::start(dsn).map_err(|e| anyhow!("postgres store: open database connection: {e}"))?;
        let ping = pg.run(|client| async move {
            client.simple_query("").await.map_err(|e| db_error("", e))?;
            Ok(())
        });
        match tokio::time::timeout(PING_TIMEOUT, ping).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                bail!(
                    "postgres store: ping database: {}",
                    error.to_string().trim_start_matches(": ")
                )
            }
            Err(_) => bail!("postgres store: ping database: context deadline exceeded"),
        }
        let schema = cfg.schema.clone();
        let table = |name: &str| {
            if schema.trim().is_empty() {
                quote_identifier(name)
            } else {
                format!("{}.{}", quote_identifier(&schema), quote_identifier(name))
            }
        };
        Ok(Self {
            config_table: table(DEFAULT_CONFIG_TABLE),
            auth_table: table(DEFAULT_AUTH_TABLE),
            cooldown_table: table(DEFAULT_COOLDOWN_TABLE),
            schema: cfg.schema,
            pg,
            spool_root,
            config_path: config_dir.join("config.yaml"),
            auth_dir,
            lock: Mutex::new(()),
        })
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_path.clone()
    }

    pub fn auth_dir(&self) -> PathBuf {
        self.auth_dir.clone()
    }

    /// Go `WorkDir`.
    pub fn work_dir(&self) -> PathBuf {
        self.spool_root.clone()
    }

    /// Go `EnsureSchema`.
    pub async fn ensure_schema(&self) -> Result<()> {
        let schema = self.schema.trim().to_owned();
        let schema_sql =
            (!schema.is_empty()).then(|| format!("CREATE SCHEMA IF NOT EXISTS {}", quote_identifier(&self.schema)));
        let config_sql = format!(
            "
		CREATE TABLE IF NOT EXISTS {} (
			id TEXT PRIMARY KEY,
			content TEXT NOT NULL,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		)
	",
            self.config_table
        );
        let auth_sql = format!(
            "
		CREATE TABLE IF NOT EXISTS {} (
			id TEXT PRIMARY KEY,
			content JSONB NOT NULL,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
		)
	",
            self.auth_table
        );
        let cooldown_sql = format!(
            "
		CREATE TABLE IF NOT EXISTS {} (
			auth_id TEXT NOT NULL,
			model TEXT NOT NULL DEFAULT '',
			content JSONB NOT NULL,
			deleted BOOLEAN NOT NULL DEFAULT FALSE,
			created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
			PRIMARY KEY (auth_id, model)
		)
	",
            self.cooldown_table
        );
        self.pg
            .run(move |client| async move {
                if let Some(sql) = schema_sql {
                    client
                        .batch_execute(&sql)
                        .await
                        .map_err(|e| db_error("postgres store: create schema", e))?;
                }
                for (sql, what) in [(config_sql, "config"), (auth_sql, "auth"), (cooldown_sql, "cooldown")] {
                    client
                        .batch_execute(&sql)
                        .await
                        .map_err(|e| db_error(&format!("postgres store: create {what} table"), e))?;
                }
                Ok(())
            })
            .await
    }

    /// Go `Bootstrap`.
    pub async fn bootstrap(&self, example: &Path) -> Result<()> {
        self.ensure_schema().await?;
        self.sync_config(example).await?;
        self.sync_auth().await
    }

    /// Go `syncConfigFromDatabase`.
    async fn sync_config(&self, example: &Path) -> Result<()> {
        let sql = format!("SELECT content FROM {} WHERE id = $1", self.config_table);
        let content: Option<String> = self
            .pg
            .run(move |client| async move {
                let row = client
                    .query_opt(&sql, &[&DEFAULT_CONFIG_KEY])
                    .await
                    .map_err(|e| db_error("postgres store: load config from database", e))?;
                Ok(row.map(|row| row.get(0)))
            })
            .await?;
        match content {
            None => {
                if !self.config_path.exists() {
                    if example.as_os_str().is_empty() {
                        mkdir_0700(self.config_path.parent().unwrap_or(&self.spool_root))
                            .map_err(|e| anyhow!("postgres store: prepare config directory: {e}"))?;
                        write_file(&self.config_path, b"")
                            .map_err(|e| anyhow!("postgres store: create empty config: {e}"))?;
                    } else {
                        seed_config(example, &self.config_path)
                            .map_err(|e| anyhow!("postgres store: copy example config: {e:#}"))?;
                    }
                }
                let data =
                    std::fs::read(&self.config_path).map_err(|e| anyhow!("postgres store: read local config: {e}"))?;
                self.upsert_config(&data).await
            }
            Some(content) => {
                mkdir_0700(self.config_path.parent().unwrap_or(&self.spool_root))
                    .map_err(|e| anyhow!("postgres store: prepare config directory: {e}"))?;
                write_file(&self.config_path, &normalize_line_endings(content.as_bytes()))
                    .map_err(|e| anyhow!("postgres store: write config to spool: {e}"))
            }
        }
    }

    /// Go `syncAuthFromDatabase`: the mirror is replaced by the table.
    async fn sync_auth(&self) -> Result<()> {
        let sql = format!("SELECT id, content::text FROM {}", self.auth_table);
        let rows: Vec<(String, String)> = self
            .pg
            .run(move |client| async move {
                let rows = client
                    .query(&sql, &[])
                    .await
                    .map_err(|e| db_error("postgres store: load auth from database", e))?;
                Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
            })
            .await?;
        match std::fs::remove_dir_all(&self.auth_dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("postgres store: reset auth directory: {e}"),
        }
        mkdir_0700(&self.auth_dir).map_err(|e| anyhow!("postgres store: recreate auth directory: {e}"))?;
        for (id, payload) in rows {
            let path = match self.absolute_auth_path(&id) {
                Ok(path) => path,
                Err(error) => {
                    tracing::warn!("postgres store: skipping auth {id} outside spool: {error}");
                    continue;
                }
            };
            if let Some(parent) = path.parent() {
                mkdir_0700(parent).map_err(|e| anyhow!("postgres store: create auth subdir: {e}"))?;
            }
            write_file(&path, payload.as_bytes()).map_err(|e| anyhow!("postgres store: write auth file: {e}"))?;
        }
        Ok(())
    }

    /// Go `PersistAuthFiles`.
    pub async fn persist_auth_files(&self, paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let _guard = self.lock.lock().await;
        for path in paths {
            if path.as_os_str().is_empty() {
                continue;
            }
            let rel = match self.relative_auth_id(path) {
                Ok(rel) => rel,
                Err(error) => {
                    tracing::warn!("postgres store: ignoring auth path {}: {error}", path.display());
                    continue;
                }
            };
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                self.auth_dir.join(path)
            };
            self.sync_auth_file(rel, &abs).await?;
        }
        Ok(())
    }

    /// Go `syncAuthFile`: a missing or empty file deletes the row.
    async fn sync_auth_file(&self, rel: String, path: &Path) -> Result<()> {
        match std::fs::read(path) {
            Ok(data) if !data.is_empty() => self.upsert_auth(rel, data).await,
            Ok(_) => self.delete_auth_record(rel).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => self.delete_auth_record(rel).await,
            Err(e) => bail!("postgres store: read auth file: {e}"),
        }
    }

    /// Go `persistAuth`.
    async fn upsert_auth(&self, rel: String, data: Vec<u8>) -> Result<()> {
        let sql = format!(
            "
		INSERT INTO {} (id, content, created_at, updated_at)
		VALUES ($1, $2::text::jsonb, NOW(), NOW())
		ON CONFLICT (id)
		DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()
	",
            self.auth_table
        );
        // Not UTF-8 is not JSON either; Postgres rejects it as Go's pgx call is rejected.
        let text = String::from_utf8(data).map_err(|_| anyhow!("postgres store: upsert auth record: invalid UTF-8"))?;
        self.pg
            .run(move |client| async move {
                client
                    .execute(&sql, &[&rel, &text])
                    .await
                    .map_err(|e| db_error("postgres store: upsert auth record", e))?;
                Ok(())
            })
            .await
    }

    async fn delete_auth_record(&self, rel: String) -> Result<()> {
        let sql = format!("DELETE FROM {} WHERE id = $1", self.auth_table);
        self.pg
            .run(move |client| async move {
                client
                    .execute(&sql, &[&rel])
                    .await
                    .map_err(|e| db_error("postgres store: delete auth record", e))?;
                Ok(())
            })
            .await
    }

    /// Go `PersistConfig`.
    pub async fn persist_config(&self) -> Result<()> {
        let _guard = self.lock.lock().await;
        match std::fs::read(&self.config_path) {
            Ok(data) => self.upsert_config(&data).await,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let sql = format!("DELETE FROM {} WHERE id = $1", self.config_table);
                self.pg
                    .run(move |client| async move {
                        client
                            .execute(&sql, &[&DEFAULT_CONFIG_KEY])
                            .await
                            .map_err(|e| db_error("postgres store: delete config", e))?;
                        Ok(())
                    })
                    .await
            }
            Err(e) => bail!("postgres store: read config file: {e}"),
        }
    }

    /// Go `persistConfig`: line endings normalized.
    async fn upsert_config(&self, data: &[u8]) -> Result<()> {
        let sql = format!(
            "
		INSERT INTO {} (id, content, created_at, updated_at)
		VALUES ($1, $2, NOW(), NOW())
		ON CONFLICT (id)
		DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()
	",
            self.config_table
        );
        let text = String::from_utf8_lossy(&normalize_line_endings(data)).into_owned();
        self.pg
            .run(move |client| async move {
                client
                    .execute(&sql, &[&DEFAULT_CONFIG_KEY, &text])
                    .await
                    .map_err(|e| db_error("postgres store: upsert config", e))?;
                Ok(())
            })
            .await
    }

    /// Go `Delete` with the file path the server removed.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        if path.as_os_str().is_empty() {
            bail!("postgres store: id is empty");
        }
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.auth_dir.join(path)
        };
        let _guard = self.lock.lock().await;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("postgres store: delete auth file: {e}"),
        }
        let rel = self.relative_auth_id(&path)?;
        self.delete_auth_record(rel).await
    }

    /// Go `relativeAuthID`.
    fn relative_auth_id(&self, path: &Path) -> Result<String> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.auth_dir.join(path)
        };
        let cleaned = clean(&joined);
        let rel = match cleaned.strip_prefix(&self.auth_dir) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => "..".to_owned(),
        };
        if rel.starts_with("..") {
            bail!("postgres store: path {} outside managed directory", joined.display());
        }
        Ok(rel)
    }

    /// Go `absoluteAuthPath`.
    fn absolute_auth_path(&self, id: &str) -> Result<PathBuf> {
        let cleaned = clean(Path::new(id));
        let text = cleaned.to_string_lossy();
        if text.starts_with("..") {
            bail!("postgres store: invalid auth identifier {id}");
        }
        // Go `filepath.Join` appends even an absolute identifier.
        let path = clean(&self.auth_dir.join(text.trim_start_matches('/')));
        if !path.starts_with(&self.auth_dir) || path == self.auth_dir {
            bail!("postgres store: resolved auth path escapes auth directory");
        }
        Ok(path)
    }

    /// Go `CooldownStateStore`.
    pub fn cooldown_backend(&self) -> PgCooldown {
        PgCooldown {
            pg: self.pg.clone(),
            table: self.cooldown_table.clone(),
            previous: Arc::default(),
        }
    }
}

type CooldownKey = (String, String);

/// Go `postgresCooldownStateStore`.
pub struct PgCooldown {
    pg: Pg,
    table: String,
    /// Rows this process last loaded or saved, with their `updated_at`.
    previous: Arc<StdMutex<HashMap<CooldownKey, DateTime<Utc>>>>,
}

fn cooldown_key(record: &Record) -> CooldownKey {
    (record.auth_id.trim().to_owned(), record.model.trim().to_owned())
}

/// Go `normalizePostgresCooldownTime`: UTC, microseconds.
fn micros(t: SystemTime) -> DateTime<Utc> {
    let t = DateTime::<Utc>::from(t);
    DateTime::from_timestamp_micros(t.timestamp_micros()).unwrap_or(t)
}

impl cpa_server::cooldown_store::Backend for PgCooldown {
    fn load(&self) -> Result<Vec<Record>, String> {
        let sql = format!(
            "SELECT content::text, updated_at FROM {} WHERE deleted = FALSE",
            self.table
        );
        let previous = self.previous.clone();
        self.pg
            .run_blocking(move |client| async move {
                let rows = client
                    .query(&sql, &[])
                    .await
                    .map_err(|e| db_error("postgres cooldown store: load state", e))?;
                let mut records = Vec::with_capacity(rows.len());
                let mut seen = HashMap::new();
                for row in rows {
                    let content: String = row.get(0);
                    let updated_at: DateTime<Utc> = row.get(1);
                    let record: Record = serde_json::from_str(&content)
                        .map_err(|e| anyhow!("postgres cooldown store: decode state: {e}"))?;
                    let key = cooldown_key(&record);
                    if key.0.is_empty() {
                        bail!("postgres cooldown store: decoded state has empty auth ID");
                    }
                    records.push(record);
                    seen.insert(key, updated_at);
                }
                *previous.lock().unwrap_or_else(PoisonError::into_inner) = seen;
                Ok(records)
            })
            .map_err(|e| e.to_string())
    }

    // ponytail: queued without waiting (Go blocks the caller on the transaction);
    // order is kept and a failure is logged by the worker.
    fn save(&self, records: Vec<Record>, now: SystemTime) -> Result<(), String> {
        let now = micros(now);
        let mut current = HashMap::with_capacity(records.len());
        let mut encoded = Vec::with_capacity(records.len());
        for mut record in records {
            let key = cooldown_key(&record);
            if key.0.is_empty() {
                return Err("postgres cooldown store: state has empty auth ID".into());
            }
            let updated = micros(record.updated_at.unwrap_or_else(|| now.into()));
            record.updated_at = Some(updated.into());
            let content = serde_json::to_string(&record)
                .map_err(|e| format!("postgres cooldown store: encode state for {:?}: {e}", key.0))?;
            current.insert(key.clone(), updated);
            encoded.push((key, content, updated));
        }
        let upsert = format!(
            "
		INSERT INTO {} AS target (auth_id, model, content, deleted, created_at, updated_at)
		VALUES ($1, $2, $3::text::jsonb, FALSE, NOW(), $4)
		ON CONFLICT (auth_id, model) DO UPDATE SET
			content = EXCLUDED.content,
			deleted = FALSE,
			updated_at = EXCLUDED.updated_at
		WHERE target.updated_at <= EXCLUDED.updated_at
	",
            self.table
        );
        let clear = format!(
            "
		INSERT INTO {} AS target (auth_id, model, content, deleted, created_at, updated_at)
		VALUES ($1, $2, $3::text::jsonb, TRUE, NOW(), $4)
		ON CONFLICT (auth_id, model) DO UPDATE SET
			content = EXCLUDED.content,
			deleted = TRUE,
			updated_at = EXCLUDED.updated_at
		WHERE NOT target.deleted AND target.updated_at <= $5
	",
            self.table
        );
        let previous = self.previous.clone();
        self.pg.submit(move |client| async move {
            let result = async {
                let client = client?;
                let stale: Vec<(CooldownKey, DateTime<Utc>)> = previous
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .iter()
                    .filter(|(key, _)| !current.contains_key(*key))
                    .map(|(key, at)| (key.clone(), *at))
                    .collect();
                client
                    .batch_execute("BEGIN")
                    .await
                    .map_err(|e| db_error("postgres cooldown store: begin save", e))?;
                let body = async {
                    for ((auth_id, model), content, updated) in &encoded {
                        client
                            .execute(&upsert, &[auth_id, model, content, updated])
                            .await
                            .map_err(|e| {
                                db_error(&format!("postgres cooldown store: save state for {auth_id:?}"), e)
                            })?;
                    }
                    for ((auth_id, model), was) in &stale {
                        let mut at = now;
                        if at <= *was {
                            at = *was + chrono::TimeDelta::microseconds(1);
                        }
                        client
                            .execute(&clear, &[auth_id, model, &"{}", &at, was])
                            .await
                            .map_err(|e| {
                                db_error(&format!("postgres cooldown store: clear state for {auth_id:?}"), e)
                            })?;
                    }
                    client
                        .batch_execute("COMMIT")
                        .await
                        .map_err(|e| db_error("postgres cooldown store: commit save", e))
                };
                if let Err(error) = body.await {
                    let _ = client.batch_execute("ROLLBACK").await;
                    return Err(error);
                }
                *previous.lock().unwrap_or_else(PoisonError::into_inner) = current;
                anyhow::Ok(())
            };
            if let Err(error) = result.await {
                tracing::warn!(%error, "failed to persist cooldown state");
            }
        });
        Ok(())
    }
}

pub struct PostgresPersister(pub Arc<PostgresStore>);

impl cpa_server::persist::StorePersister for PostgresPersister {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_paths_follow_go() {
        assert_eq!(quote_identifier(r#"my"schema"#), r#""my""schema""#);
        assert_eq!(clean(Path::new("a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(clean(Path::new("../x")), PathBuf::from("../x"));
        assert_eq!(clean(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(clean(Path::new("")), PathBuf::from("."));
    }
}
