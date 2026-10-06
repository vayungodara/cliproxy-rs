//! `PGSTORE_*` against a throwaway local PostgreSQL cluster (initdb in a temp dir,
//! trust auth, a free port). Skipped when no PostgreSQL binaries are installed; set
//! `CPA_TEST_PG_BIN` to their directory to choose one.
// Mode bits, initdb and local git remotes: Unix only.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cpa_server::cooldown_store::{Backend, Record};
use cpa_server::persist::StorePersister;
use cpa_store::{PostgresConfig, PostgresPersister, PostgresStore};

fn scratch(name: &str) -> PathBuf {
    // Parallel tests can read the same clock value: the counter keeps their clusters apart.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("cpa-pg-{name}-{}-{nanos}-{n}", std::process::id()));
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
            // CI sets CPA_TEST_NO_SKIP: there a missing PostgreSQL fails the test instead
            // of passing silently (the harness hides a passing test's output).
            assert!(
                std::env::var_os("CPA_TEST_NO_SKIP").is_none(),
                "no PostgreSQL binaries (install postgresql or set CPA_TEST_PG_BIN), and CPA_TEST_NO_SKIP is set"
            );
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
        assert!(
            output.status.success(),
            "initdb: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // ponytail: the server binds the port itself, so another test's listener or client
        // socket can take it between this probe and that bind; a start that fails there
        // retries with a new port, three times. A cluster on its Unix socket alone would
        // need no port, but these tests would then no longer cover TCP connections.
        let log = root.join("log");
        let mut retries = 3;
        let port = loop {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let options = format!(
                "-p {port} -c listen_addresses=127.0.0.1 -k {} -c fsync=off -c shared_buffers=16MB -c max_connections=20",
                root.display()
            );
            // pg_ctl appends to the log; each attempt starts a new one.
            let _ = std::fs::remove_file(&log);
            let status = Command::new(bin.join("pg_ctl"))
                .args(["-s", "-w", "-l"])
                .arg(&log)
                .args(["-D"])
                .arg(&data)
                .args(["-o", &options, "start"])
                .status()
                .unwrap();
            if status.success() {
                break port;
            }
            let server_log = std::fs::read_to_string(&log).unwrap_or_default();
            assert!(
                retries > 0 && server_log.contains("Address already in use"),
                "pg_ctl start failed: {server_log}"
            );
            retries -= 1;
        };
        Some(Cluster { bin, data, port })
    }

    fn dsn(&self) -> String {
        format!("postgres://postgres@127.0.0.1:{}/postgres?sslmode=disable", self.port)
    }

    async fn client(&self) -> tokio_postgres::Client {
        let (client, connection) = tokio_postgres::connect(&self.dsn(), tokio_postgres::NoTls)
            .await
            .unwrap();
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
        .query_one(
            "SELECT content FROM \"cpa \"\"tenant\"\"\".\"config_store\" WHERE id = 'config'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(config, "port: 8317\nauth-dir: x\n");
    assert_eq!(
        std::fs::read(first.config_path()).unwrap(),
        b"port: 8317\r\nauth-dir: x\r\n"
    );

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
    persister
        .persist_auth_files("Sync auth c.json".into(), vec![auth.join("c.json")])
        .await
        .unwrap();
    std::fs::write(auth.join("bad.json"), "{not json").unwrap();
    let error = format!(
        "{:#}",
        persister
            .persist_auth_files("x".into(), vec![auth.join("bad.json")])
            .await
            .unwrap_err()
    );
    assert!(
        error.contains("postgres store: upsert auth record: ERROR: invalid input syntax for type json"),
        "{error}"
    );
    let ids: Vec<String> = sql
        .query("SELECT id FROM \"cpa \"\"tenant\"\"\".\"auth_store\" ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(ids, vec!["a.json", "team/b.json"]);

    // A second node mirrors the table and replaces whatever its auth dir held.
    // The spool path has a `..` segment (PGSTORE_LOCAL_PATH=../state): Go's
    // filepath.Abs cleans it, so rows still land inside the mirror.
    std::fs::create_dir_all(dir.join("node2/tmp")).unwrap();
    let second_spool = dir.join("node2/tmp/../pgstore");
    std::fs::create_dir_all(second_spool.join("auths")).unwrap();
    std::fs::write(second_spool.join("auths/stale.json"), "{}").unwrap();
    let second = store(&cluster, schema, &second_spool).await;
    assert_eq!(second.auth_dir(), dir.join("node2/pgstore/auths"));
    second.bootstrap(&example).await.unwrap();
    let mirrored = second.auth_dir();
    // jsonb's canonical text: keys sorted by length then bytes, one space after colons.
    assert_eq!(
        std::fs::read_to_string(mirrored.join("a.json")).unwrap(),
        r#"{"a": "x", "b": [1, 2], "type": "claude"}"#
    );
    assert_eq!(
        std::fs::read_to_string(mirrored.join("team/b.json")).unwrap(),
        r#"{"type": "codex"}"#
    );
    assert!(!mirrored.join("stale.json").exists());
    assert_eq!(mode(&mirrored.join("a.json")), 0o600);
    assert_eq!(
        std::fs::read(second.config_path()).unwrap(),
        b"port: 8317\nauth-dir: x\n"
    );

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
        cooldown
            .save(
                vec![record("a.json", "m1", t0), untimed],
                now + Duration::from_nanos(999),
            )
            .unwrap();
        let b = cooldown
            .load()
            .unwrap()
            .into_iter()
            .find(|r| r.auth_id == "b.json")
            .unwrap();
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
        assert!(
            loaded.iter().any(|r| r.model == "m1" && r.reason == "other node"),
            "{loaded:?}"
        );
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

/// pgx sends `search_path` to the server, so without PGSTORE_SCHEMA the unqualified
/// tables resolve in that schema rather than in `public`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_path_in_the_dsn_selects_the_tables() {
    let Some(cluster) = Cluster::start() else { return };
    let sql = cluster.client().await;
    sql.batch_execute(
        "CREATE SCHEMA tenant;
         CREATE TABLE public.config_store (id TEXT PRIMARY KEY, content TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
         INSERT INTO public.config_store (id, content) VALUES ('config', 'port: 1\n');
         CREATE TABLE tenant.config_store (id TEXT PRIMARY KEY, content TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
         INSERT INTO tenant.config_store (id, content) VALUES ('config', 'port: 2\n');",
    )
    .await
    .unwrap();
    let dir = scratch("search-path");
    let store = PostgresStore::connect(PostgresConfig {
        dsn: format!("{}&search_path=tenant&application_name=cpa%20store", cluster.dsn()),
        schema: String::new(),
        spool_dir: dir.join("pgstore"),
    })
    .await
    .unwrap();
    store.bootstrap(Path::new("")).await.unwrap();
    assert_eq!(std::fs::read(store.config_path()).unwrap(), b"port: 2\n");
    // The auth and cooldown tables were created next to the tenant's config.
    let tables: i64 = sql
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'tenant'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(tables, 3);
    // A schema whose name has a space: the server must receive the escaped value as
    // one setting (keyword form, quoted identifier inside the value).
    sql.batch_execute(
        "CREATE SCHEMA \"my tenant\";
         CREATE TABLE \"my tenant\".config_store (id TEXT PRIMARY KEY, content TEXT NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(), updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW());
         INSERT INTO \"my tenant\".config_store (id, content) VALUES ('config', 'port: 3\n');",
    )
    .await
    .unwrap();
    let spaced = PostgresStore::connect(PostgresConfig {
        dsn: format!(
            "host=127.0.0.1 port={} user=postgres dbname=postgres sslmode=disable search_path='\"my tenant\"'",
            cluster.port
        ),
        schema: String::new(),
        spool_dir: dir.join("spaced"),
    })
    .await
    .unwrap();
    spaced.bootstrap(Path::new("")).await.unwrap();
    assert_eq!(std::fs::read(spaced.config_path()).unwrap(), b"port: 3\n");
    // An unknown runtime parameter fails like pgx's startup message does.
    let error = PostgresStore::connect(PostgresConfig {
        dsn: format!("{}&pool_max_conns=4", cluster.dsn()),
        schema: String::new(),
        spool_dir: dir.join("other"),
    })
    .await
    .err()
    .unwrap()
    .to_string();
    assert!(
        error.contains("FATAL: unrecognized configuration parameter \"pool_max_conns\" (SQLSTATE 42704)"),
        "{error}"
    );
}

/// A write whose caller gave up while it waited for the connection never runs later
/// (Go's expired context), so it cannot overwrite a newer row from another node.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abandoned_queued_writes_never_run() {
    let Some(cluster) = Cluster::start() else { return };
    let dir = scratch("abandoned");
    let store = store(&cluster, "", &dir.join("pgstore")).await;
    store.bootstrap(Path::new("")).await.unwrap();
    let auth = store.auth_dir();
    for name in ["a.json", "b.json", "c.json"] {
        std::fs::write(auth.join(name), format!(r#"{{"name":"{name}"}}"#)).unwrap();
    }
    // Another session holds the table, so the worker blocks on the first write.
    let locker = cluster.client().await;
    locker
        .batch_execute("BEGIN; LOCK TABLE \"auth_store\" IN ACCESS EXCLUSIVE MODE")
        .await
        .unwrap();
    let short = Duration::from_millis(300);
    let first = tokio::time::timeout(short, store.persist_auth_files(&[auth.join("a.json")])).await;
    assert!(first.is_err(), "the first write must be blocked by the lock");
    let queued = tokio::time::timeout(short, store.persist_auth_files(&[auth.join("b.json")])).await;
    assert!(queued.is_err(), "the second write waits behind the first");
    locker.batch_execute("COMMIT").await.unwrap();
    // The worker drains its queue; a later write proves it got past the abandoned one.
    store.persist_auth_files(&[auth.join("c.json")]).await.unwrap();
    let sql = cluster.client().await;
    let ids: Vec<String> = sql
        .query("SELECT id FROM \"auth_store\" ORDER BY id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    // The statement already sent before the timeout completes, as a cancelled Go
    // ExecContext may too; the queued one never starts.
    assert_eq!(ids, vec!["a.json", "c.json"]);
}

/// Go `TestPostgresCooldownStateStore_MergesConcurrentInstances` and the `Save(nil)`
/// step of `_SaveLoad`, against a real server: instances only clear rows they saw,
/// and a stale instance cannot resurrect or remove a newer row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cooldown_instances_merge_like_go() {
    let Some(cluster) = Cluster::start() else { return };
    let dir = scratch("merge");
    let store = store(&cluster, "", &dir.join("pgstore")).await;
    store.ensure_schema().await.unwrap();
    let (a, b, stale) = (
        store.cooldown_backend(),
        store.cooldown_backend(),
        store.cooldown_backend(),
    );
    let resurrect = store.cooldown_backend();
    let reader = store.cooldown_backend();
    tokio::task::spawn_blocking(move || {
        let updated = SystemTime::now() - Duration::from_secs(60);
        let rec = |auth: &str, model: &str, at: SystemTime| Record {
            auth_id: auth.into(),
            model: model.into(),
            updated_at: Some(at),
            ..Default::default()
        };
        a.load().unwrap();
        b.load().unwrap();
        let now = SystemTime::now();
        a.save(vec![rec("account-a", "model-a", updated)], now).unwrap();
        b.save(vec![rec("account-b", "model-b", updated)], now).unwrap();
        assert_eq!(stale.load().unwrap().len(), 2, "both instances' rows survive");

        a.save(
            vec![rec("account-a", "model-a", updated + Duration::from_secs(3600))],
            now,
        )
        .unwrap();
        // The stale instance saw account-a's older row; dropping it must not delete the
        // newer one.
        stale.save(vec![rec("account-b", "model-b", updated)], now).unwrap();
        let active = resurrect.load().unwrap();
        assert_eq!(active.len(), 2, "{active:?}");

        a.save(vec![], SystemTime::now()).unwrap();
        resurrect.save(active, SystemTime::now()).unwrap();
        let loaded = reader.load().unwrap();
        assert_eq!(loaded.len(), 1, "{loaded:?}");
        assert_eq!(loaded[0].auth_id, "account-b");

        // Save(nil) from an instance that saw everything clears it.
        reader.save(vec![], SystemTime::now()).unwrap();
        assert_eq!(reader.load().unwrap(), Vec::<Record>::new());
    })
    .await
    .unwrap();
}
