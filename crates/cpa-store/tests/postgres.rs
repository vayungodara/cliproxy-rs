//! `PGSTORE_*` against a throwaway local PostgreSQL cluster (initdb in a temp dir,
//! trust auth, a free port). Skipped when no PostgreSQL binaries are installed; set
//! `CPA_TEST_PG_BIN` to their directory to choose one.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cpa_server::cooldown_store::{Backend, Record};
use cpa_server::persist::StorePersister;
use cpa_store::{PostgresConfig, PostgresPersister, PostgresStore};

fn scratch(name: &str) -> PathBuf {
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("cpa-pg-{name}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn pg_bin() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CPA_TEST_PG_BIN") {
        return Some(PathBuf::from(dir));
    }
    let mut versions: Vec<PathBuf> = std::fs::read_dir("/usr/lib/postgresql")
        .ok()?
        .flatten()
        .map(|e| e.path().join("bin"))
        .filter(|bin| bin.join("initdb").exists())
        .collect();
    versions.sort();
    versions.pop()
}

/// A private cluster, stopped on drop.
struct Cluster {
    bin: PathBuf,
    data: PathBuf,
    port: u16,
}

impl Cluster {
    fn start() -> Option<Cluster> {
        let Some(bin) = pg_bin() else {
            eprintln!("skipping: no PostgreSQL binaries (install postgresql or set CPA_TEST_PG_BIN)");
            return None;
        };
        let root = scratch("cluster");
        let data = root.join("data");
        let output = Command::new(bin.join("initdb"))
            .args(["-A", "trust", "-U", "postgres", "-E", "UTF8", "--no-sync", "-D"])
            .arg(&data)
            .output()
            .unwrap();
        assert!(output.status.success(), "initdb: {}", String::from_utf8_lossy(&output.stderr));
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let options = format!(
            "-p {port} -c listen_addresses=127.0.0.1 -k {} -c fsync=off -c shared_buffers=16MB -c max_connections=20",
            root.display()
        );
        let status = Command::new(bin.join("pg_ctl"))
            .args(["-s", "-w", "-l"])
            .arg(root.join("log"))
            .args(["-D"])
            .arg(&data)
            .args(["-o", &options, "start"])
            .status()
            .unwrap();
        assert!(status.success(), "pg_ctl start failed");
        Some(Cluster { bin, data, port })
    }

    fn dsn(&self) -> String {
        format!("postgres://postgres@127.0.0.1:{}/postgres?sslmode=disable", self.port)
    }

    async fn client(&self) -> tokio_postgres::Client {
        let (client, connection) = tokio_postgres::connect(&self.dsn(), tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        client
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .args(["-s", "-m", "immediate", "-D"])
            .arg(&self.data)
            .arg("stop")
            .status();
    }
}

async fn store(cluster: &Cluster, schema: &str, spool: &Path) -> PostgresStore {
    PostgresStore::connect(PostgresConfig {
        dsn: cluster.dsn(),
        schema: schema.into(),
        spool_dir: spool.to_path_buf(),
    })
    .await
    .unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn record(auth: &str, model: &str, updated: SystemTime) -> Record {
    Record {
        provider: "claude".into(),
        auth_id: auth.into(),
        model: model.into(),
        status: "error".into(),
        next_retry_after: Some(updated + Duration::from_secs(60)),
        reason: "quota".into(),
        updated_at: Some(updated),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_store_round_trips_config_auth_and_cooldowns() {
    let Some(cluster) = Cluster::start() else { return };
    let sql = cluster.client().await;
    let dir = scratch("store");
    let example = dir.join("config.example.yaml");
    std::fs::write(&example, "port: 8317\r\nauth-dir: x\r\n").unwrap();

    // A fresh database: Go's tables appear in the quoted schema and the template
    // seeds the config row with normalized line endings.
    let schema = "cpa \"tenant\"";
    let first = store(&cluster, schema, &dir.join("node1/pgstore")).await;
    first.bootstrap(&example).await.unwrap();
    let tables: Vec<String> = sql
        .query(
            "SELECT table_name::text FROM information_schema.tables WHERE table_schema = $1 ORDER BY 1",
            &[&schema],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(tables, vec!["auth_store", "config_store", "cooldown_store"]);
    let config: String = sql
        .query_one("SELECT content FROM \"cpa \"\"tenant\"\"\".\"config_store\" WHERE id = 'config'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(config, "port: 8317\nauth-dir: x\n");
    assert_eq!(std::fs::read(first.config_path()).unwrap(), b"port: 8317\r\nauth-dir: x\r\n");

    // Auth files: upsert, subdirectories, deletes for empty and missing files.
    let auth = first.auth_dir();
    std::fs::write(auth.join("a.json"), r#"{"type":"claude","b":[1,2],  "a":"x"}"#).unwrap();
    std::fs::create_dir_all(auth.join("team")).unwrap();
    std::fs::write(auth.join("team/b.json"), r#"{"type":"codex"}"#).unwrap();
    std::fs::write(auth.join("c.json"), r#"{"c":true}"#).unwrap();
    let persister = PostgresPersister(Arc::new(first));
    persister
        .persist_auth_files(
            "Sync auth".into(),
            vec![auth.join("a.json"), PathBuf::from("team/b.json"), auth.join("c.json")],
        )
        .await
        .unwrap();
    std::fs::write(auth.join("c.json"), "").unwrap();
    persister.persist_auth_files("Sync auth c.json".into(), vec![auth.join("c.json")]).await.unwrap();
    std::fs::write(auth.join("bad.json"), "{not json").unwrap();
    let error = format!(
        "{:#}",
        persister.persist_auth_files("x".into(), vec![auth.join("bad.json")]).await.unwrap_err()
    );
    assert!(error.contains("postgres store: upsert auth record: ERROR: invalid input syntax for type json"), "{error}");
    let ids: Vec<String> = sql
        .query("SELECT id FROM \"cpa \"\"tenant\"\"\".\"auth_store\" ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(ids, vec!["a.json", "team/b.json"]);

    // A second node mirrors the table and replaces whatever its auth dir held.
    let second_spool = dir.join("node2/pgstore");
    std::fs::create_dir_all(second_spool.join("auths")).unwrap();
    std::fs::write(second_spool.join("auths/stale.json"), "{}").unwrap();
    let second = store(&cluster, schema, &second_spool).await;
    second.bootstrap(&example).await.unwrap();
    let mirrored = second.auth_dir();
    // jsonb's canonical text: keys sorted by length then bytes, one space after colons.
    assert_eq!(
        std::fs::read_to_string(mirrored.join("a.json")).unwrap(),
        r#"{"a": "x", "b": [1, 2], "type": "claude"}"#
    );
    assert_eq!(std::fs::read_to_string(mirrored.join("team/b.json")).unwrap(), r#"{"type": "codex"}"#);
    assert!(!mirrored.join("stale.json").exists());
    assert_eq!(mode(&mirrored.join("a.json")), 0o600);
    assert_eq!(std::fs::read(second.config_path()).unwrap(), b"port: 8317\nauth-dir: x\n");

    // Explicit delete and a removed config.
    second.delete(&mirrored.join("team/b.json")).await.unwrap();
    assert!(!mirrored.join("team/b.json").exists());
    std::fs::remove_file(second.config_path()).unwrap();
    second.persist_config().await.unwrap();
    let counts: (i64, i64) = {
        let row = sql
            .query_one(
                "SELECT (SELECT count(*) FROM \"cpa \"\"tenant\"\"\".\"auth_store\"), (SELECT count(*) FROM \"cpa \"\"tenant\"\"\".\"config_store\")",
                &[],
            )
            .await
            .unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!(counts, (1, 0));

    // Cooldowns: a full set, then a smaller one soft-deletes the missing row.
    let cooldown = second.cooldown_backend();
    let t0 = SystemTime::UNIX_EPOCH + Duration::from_micros(1_790_000_000_123_456);
    let now = t0 + Duration::from_secs(5);
    tokio::task::spawn_blocking(move || {
        assert_eq!(cooldown.load().unwrap(), Vec::<Record>::new());
        cooldown
            .save(vec![record("a.json", "m1", t0), record("a.json", "", t0)], now)
            .unwrap();
        let loaded = cooldown.load().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&record("a.json", "m1", t0)));
        cooldown.save(vec![record("a.json", "m1", t0)], now).unwrap();
        assert_eq!(cooldown.load().unwrap(), vec![record("a.json", "m1", t0)]);
        // A record without a time takes the save time, truncated to microseconds.
        let mut untimed = record("b.json", "", t0);
        untimed.updated_at = None;
        cooldown.save(vec![record("a.json", "m1", t0), untimed], now + Duration::from_nanos(999)).unwrap();
        let b = cooldown.load().unwrap().into_iter().find(|r| r.auth_id == "b.json").unwrap();
        assert_eq!(b.updated_at, Some(now));
        assert!(cooldown.save(vec![record(" ", "", t0)], now).is_err());
    })
    .await
    .unwrap();
    let rows: Vec<(String, String, bool)> = sql
        .query(
            "SELECT auth_id, model, deleted FROM \"cpa \"\"tenant\"\"\".\"cooldown_store\" ORDER BY 1, 2",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("a.json".into(), "".into(), true),
            ("a.json".into(), "m1".into(), false),
            ("b.json".into(), "".into(), false),
        ]
    );
    // A newer row written elsewhere is not overwritten by an older snapshot.
    sql.execute(
        "UPDATE \"cpa \"\"tenant\"\"\".\"cooldown_store\" SET updated_at = updated_at + interval '1 hour', content = '{\"auth_id\":\"a.json\",\"model\":\"m1\",\"reason\":\"other node\"}' WHERE model = 'm1'",
        &[],
    )
    .await
    .unwrap();
    let cooldown = second.cooldown_backend();
    tokio::task::spawn_blocking(move || {
        cooldown.save(vec![record("a.json", "m1", t0)], now).unwrap();
        let loaded = cooldown.load().unwrap();
        assert!(loaded.iter().any(|r| r.model == "m1" && r.reason == "other node"), "{loaded:?}");
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn postgres_store_reports_go_errors() {
    let dir = scratch("errors");
    let error = PostgresStore::connect(PostgresConfig {
        dsn: "  ".into(),
        ..Default::default()
    })
    .await
    .err()
    .unwrap();
    assert_eq!(error.to_string(), "postgres store: DSN is required");
    // Nothing listens on port 1.
    let error = PostgresStore::connect(PostgresConfig {
        dsn: "postgres://u:pw-s3cret@127.0.0.1:1/db?sslmode=disable&connect_timeout=2".into(),
        spool_dir: dir.join("pgstore"),
        ..Default::default()
    })
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(error.starts_with("postgres store: ping database: "), "{error}");
    assert!(!error.contains("pw-s3cret"), "{error}");
    assert!(dir.join("pgstore/config").is_dir() && dir.join("pgstore/auths").is_dir());
}
