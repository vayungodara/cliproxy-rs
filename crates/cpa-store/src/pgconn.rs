//! The Postgres connection behind `PGSTORE_DSN`: pgx's DSN rules on top of
//! tokio-postgres, TLS through BoringSSL, and one worker thread that owns the
//! connection. Async callers and the runtime's synchronous cooldown hook both queue
//! jobs to it, so neither depends on the caller's runtime flavour.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use btls::ssl::{SslConnector, SslFiletype, SslMethod, SslVerifyMode};
use futures_util::future::BoxFuture;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::config::SslMode;
use tokio_postgres::tls::{ChannelBinding, MakeTlsConnect, TlsConnect};
use tokio_postgres::{Client, Socket};

/// A job may not hold the worker longer than this; the connection is then replaced.
const JOB_TIMEOUT: Duration = Duration::from_secs(60);

/// Keys pgx consumes that tokio-postgres takes as they are. `dbname` stands for pgx's
/// `database`.
const NATIVE_KEYS: &[&str] = &[
    "user",
    "password",
    "dbname",
    "options",
    "application_name",
    "sslnegotiation",
    "host",
    "port",
    "connect_timeout",
    "target_session_attrs",
    "channel_binding",
];

/// pgx `notRuntimeParams` (and pgx.ParseConfig's own keys) tokio-postgres cannot
/// express; dropped with a warning.
// ponytail: no .pgpass, service files, Kerberos, encrypted keys or protocol bounds.
const UNSUPPORTED_KEYS: &[&str] = &[
    "passfile",
    "sslpassword",
    "sslsni",
    "krbspn",
    "krbsrvname",
    "service",
    "servicefile",
    "min_protocol_version",
    "max_protocol_version",
    "statement_cache_capacity",
    "description_cache_capacity",
    "default_query_exec_mode",
];

/// Keys handled here because tokio-postgres knows only disable/prefer/require.
const TLS_KEYS: &[&str] = &["sslmode", "sslrootcert", "sslcert", "sslkey"];

/// pgx `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TlsMode {
    Disable,
    Allow,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Debug, Default, Clone)]
struct TlsFiles {
    mode: Option<String>,
    root_cert: Option<String>,
    cert: Option<String>,
    key: Option<String>,
}

impl TlsFiles {
    fn set(&mut self, key: &str, value: String) {
        match key {
            "sslmode" => self.mode = Some(value),
            "sslrootcert" => self.root_cert = Some(value),
            "sslcert" => self.cert = Some(value),
            _ => self.key = Some(value),
        }
    }
}

/// A parsed `PGSTORE_DSN`.
#[derive(Clone)]
pub(crate) struct Dsn {
    config: tokio_postgres::Config,
    tls: Tls,
}

impl std::fmt::Debug for Dsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the password.
        f.debug_struct("Dsn")
            .field("hosts", &self.config.get_hosts())
            .field("ports", &self.config.get_ports())
            .field("user", &self.config.get_user())
            .field("dbname", &self.config.get_dbname())
            .field("ssl_mode", &self.config.get_ssl_mode())
            .field("verify", &self.tls.verify)
            .field("verify_hostname", &self.tls.verify_hostname)
            .finish()
    }
}

/// One `options` argument: PostgreSQL's `pg_split_opts` splits on `isspace()` and
/// lets `\` escape the next character.
fn escape_option(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Percent-encodes all but unreserved characters (tokio-postgres decodes only `%XX`).
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// libpq keyword/value pairs: `key = value`, values optionally single-quoted, `\`
/// escaping the next character.
fn keyword_pairs(dsn: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    let mut chars = dsn.chars().peekable();
    loop {
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.peek().is_none() {
            return Ok(pairs);
        }
        let mut key = String::new();
        while let Some(c) = chars.next_if(|c| *c != '=' && !c.is_whitespace()) {
            key.push(c);
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        if chars.next() != Some('=') {
            bail!("invalid dsn: missing \"=\" after {key:?}");
        }
        while chars.next_if(|c| c.is_whitespace()).is_some() {}
        let mut value = String::new();
        if chars.next_if_eq(&'\'').is_some() {
            loop {
                match chars.next() {
                    Some('\\') => value.extend(chars.next()),
                    Some('\'') => break,
                    Some(c) => value.push(c),
                    None => bail!("invalid dsn: unterminated quoted string in connection info string"),
                }
            }
        } else {
            while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
                if c == '\\' {
                    value.extend(chars.next());
                } else {
                    value.push(c);
                }
            }
        }
        pairs.push((key, value));
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// pgx `defaultHost`: the first existing socket directory, else localhost.
fn default_host() -> String {
    ["/var/run/postgresql", "/private/tmp", "/tmp"]
        .into_iter()
        .find(|dir| std::path::Path::new(dir).exists())
        .unwrap_or("localhost")
        .to_owned()
}

impl Dsn {
    /// pgx `ParseConfig`: URL or keyword/value form, `PG*` environment defaults, and
    /// `sslmode` defaulting to `prefer`.
    // ponytail: no .pgpass, PGSERVICE or multi-host fallback configs; keys pgx turns
    // into runtime parameters are dropped with a warning.
    pub(crate) fn parse(raw: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Dsn> {
        let raw = raw.trim();
        let mut tls = TlsFiles::default();
        // pgx sends every other key to the server as a startup parameter (`search_path`
        // picks the tables when PGSTORE_SCHEMA is unset); here they travel as `-c`.
        let mut runtime: Vec<(String, String)> = Vec::new();
        let mut keep = |key: &str, value: String| -> bool {
            if TLS_KEYS.contains(&key) {
                tls.set(key, value);
                false
            } else if NATIVE_KEYS.contains(&key) {
                true
            } else if UNSUPPORTED_KEYS.contains(&key) {
                tracing::warn!(key, "postgres store: ignoring unsupported connection parameter");
                false
            } else {
                runtime.push((key.to_owned(), value));
                false
            }
        };
        let canonical = |key: String| if key == "database" { "dbname".to_owned() } else { key };
        let stripped = if raw.starts_with("postgres://") || raw.starts_with("postgresql://") {
            let (base, query) = raw.split_once('?').unwrap_or((raw, ""));
            // pgx: Go's `url.Query()` decoding (`+` is a space), first value per key.
            let mut seen: Vec<String> = Vec::new();
            let mut kept: Vec<String> = Vec::new();
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                let key = canonical(key.into_owned());
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key.clone());
                if keep(&key, value.clone().into_owned()) {
                    kept.push(format!("{key}={}", percent_encode(&value)));
                }
            }
            if kept.is_empty() {
                base.to_owned()
            } else {
                format!("{base}?{}", kept.join("&"))
            }
        } else {
            // pgx: the last value per key.
            let mut pairs: Vec<(String, String)> = Vec::new();
            for (key, value) in keyword_pairs(raw)? {
                let key = canonical(key);
                match pairs.iter_mut().find(|(k, _)| *k == key) {
                    Some(pair) => pair.1 = value,
                    None => pairs.push((key, value)),
                }
            }
            pairs
                .into_iter()
                .filter_map(|(key, value)| keep(&key, value.clone()).then(|| format!("{key}={}", quote(&value))))
                .collect::<Vec<_>>()
                .join(" ")
        };
        let mut config: tokio_postgres::Config = stripped.parse().map_err(|e| anyhow!("cannot parse dsn: {e}"))?;
        let env = |key: &str| env(key).filter(|v| !v.is_empty());
        if config.get_options().is_none()
            && let Some(options) = env("PGOPTIONS")
        {
            config.options(options);
        }
        if config.get_application_name().is_none()
            && let Some(name) = env("PGAPPNAME")
        {
            config.application_name(name);
        }
        if !runtime.is_empty() {
            let mut options = config.get_options().unwrap_or_default().to_owned();
            // The server drops a final unescaped `\`; keep it from escaping our separator.
            let trailing = options.len() - options.trim_end_matches('\\').len();
            if trailing % 2 == 1 {
                options.pop();
            }
            for (key, value) in &runtime {
                if !options.is_empty() {
                    options.push(' ');
                }
                options.push_str(&format!("-c {}={}", escape_option(key), escape_option(value)));
            }
            config.options(options);
        }
        if config.get_hosts().is_empty() {
            let hosts = env("PGHOST").unwrap_or_else(default_host);
            for host in hosts.split(',') {
                config.host(host);
            }
        }
        if config.get_ports().is_empty()
            && let Some(port) = env("PGPORT")
        {
            config.port(port.parse().map_err(|_| anyhow!("cannot parse dsn: invalid port"))?);
        }
        if config.get_user().is_none()
            && let Some(user) = env("PGUSER").or_else(|| env("USER")).or_else(|| env("LOGNAME"))
        {
            config.user(user);
        }
        if config.get_password().is_none()
            && let Some(password) = env("PGPASSWORD")
        {
            config.password(password);
        }
        if config.get_dbname().is_none()
            && let Some(dbname) = env("PGDATABASE")
        {
            config.dbname(dbname);
        }
        if config.get_connect_timeout().is_none()
            && let Some(secs) = env("PGCONNECT_TIMEOUT")
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|s| *s > 0)
        {
            config.connect_timeout(Duration::from_secs(secs));
        }
        for (key, var) in [
            ("sslmode", "PGSSLMODE"),
            ("sslrootcert", "PGSSLROOTCERT"),
            ("sslcert", "PGSSLCERT"),
            ("sslkey", "PGSSLKEY"),
        ] {
            let unset = match key {
                "sslmode" => tls.mode.is_none(),
                "sslrootcert" => tls.root_cert.is_none(),
                "sslcert" => tls.cert.is_none(),
                _ => tls.key.is_none(),
            };
            if unset && let Some(value) = env(var) {
                tls.set(key, value);
            }
        }
        let mode = match tls.mode.as_deref().unwrap_or("prefer") {
            "disable" => TlsMode::Disable,
            "allow" => TlsMode::Allow,
            "prefer" => TlsMode::Prefer,
            "require" => TlsMode::Require,
            "verify-ca" => TlsMode::VerifyCa,
            "verify-full" => TlsMode::VerifyFull,
            _ => bail!("cannot parse dsn: sslmode is invalid"),
        };
        // pgx: no TLS over Unix sockets.
        #[cfg(unix)]
        let unix_only = config
            .get_hosts()
            .iter()
            .all(|h| matches!(h, tokio_postgres::config::Host::Unix(_)));
        // tokio-postgres has Unix-socket hosts on Unix only.
        #[cfg(not(unix))]
        let unix_only = false;
        let root_cert = tls.root_cert.filter(|p| !p.is_empty()).map(PathBuf::from);
        // pgx: `require` with a root certificate verifies like `verify-ca`.
        let verify = matches!(mode, TlsMode::VerifyCa | TlsMode::VerifyFull)
            || (mode == TlsMode::Require && root_cert.is_some());
        config.ssl_mode(match mode {
            _ if unix_only => SslMode::Disable,
            TlsMode::Disable => SslMode::Disable,
            // ponytail: `allow` tries TLS first, as `prefer` does.
            TlsMode::Allow | TlsMode::Prefer => SslMode::Prefer,
            TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull => SslMode::Require,
        });
        let tls = Tls::new(
            verify,
            mode == TlsMode::VerifyFull,
            root_cert,
            tls.cert.filter(|p| !p.is_empty()).map(PathBuf::from),
            tls.key.filter(|p| !p.is_empty()).map(PathBuf::from),
        )?;
        Ok(Dsn { config, tls })
    }

    async fn connect(&self) -> Result<Client> {
        let (client, connection) = self
            .config
            .connect(self.tls.clone())
            .await
            .map_err(|e| anyhow!("{}", error_text(&e)))?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!("postgres store: connection closed: {error}");
            }
        });
        Ok(client)
    }
}

/// pgx `PgError.Error()` for server errors (tokio-postgres hides the message behind
/// `source()`), the client error text otherwise.
pub(crate) fn error_text(error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("{}: {} (SQLSTATE {})", db.severity(), db.message(), db.code().code()),
        None => error.to_string(),
    }
}

/// BoringSSL for tokio-postgres (pgx's `tls.Config` per `sslmode`).
#[derive(Clone)]
struct Tls {
    connector: SslConnector,
    verify: bool,
    verify_hostname: bool,
}

impl Tls {
    fn new(
        verify: bool,
        verify_hostname: bool,
        root_cert: Option<PathBuf>,
        cert: Option<PathBuf>,
        key: Option<PathBuf>,
    ) -> Result<Self> {
        let mut builder = SslConnector::builder(SslMethod::tls()).map_err(|e| anyhow!("{e}"))?;
        if let Some(root) = &root_cert {
            builder
                .set_ca_file(root)
                .map_err(|e| anyhow!("unable to read CA file: {e}"))?;
        }
        if let (Some(cert), Some(key)) = (&cert, &key) {
            builder
                .set_certificate_chain_file(cert)
                .map_err(|e| anyhow!("unable to read cert: {e}"))?;
            builder
                .set_private_key_file(key, SslFiletype::PEM)
                .map_err(|e| anyhow!("unable to read key: {e}"))?;
        }
        if !verify {
            builder.set_verify(SslVerifyMode::NONE);
        }
        Ok(Self {
            connector: builder.build(),
            verify,
            verify_hostname: verify && verify_hostname,
        })
    }
}

impl MakeTlsConnect<Socket> for Tls {
    type Stream = TlsStream;
    type TlsConnect = TlsConnector;
    type Error = btls::error::ErrorStack;

    fn make_tls_connect(&mut self, domain: &str) -> Result<TlsConnector, Self::Error> {
        let mut config = self.connector.configure()?;
        config.set_verify_hostname(self.verify_hostname);
        Ok(TlsConnector {
            ssl: config.into_ssl(domain)?,
        })
    }
}

struct TlsConnector {
    ssl: btls::ssl::Ssl,
}

impl TlsConnect<Socket> for TlsConnector {
    type Stream = TlsStream;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = BoxFuture<'static, Result<TlsStream, Self::Error>>;

    fn connect(self, socket: Socket) -> Self::Future {
        Box::pin(async move {
            let mut stream = tokio_btls::SslStream::new(self.ssl, socket)?;
            Pin::new(&mut stream).connect().await?;
            Ok(TlsStream(stream))
        })
    }
}

struct TlsStream(tokio_btls::SslStream<Socket>);

impl AsyncRead for TlsStream {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsStream {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl tokio_postgres::tls::TlsStream for TlsStream {
    fn channel_binding(&self) -> ChannelBinding {
        // ponytail: no tls-server-end-point binding, so SCRAM-SHA-256-PLUS is not offered.
        ChannelBinding::none()
    }
}

type Job = Box<dyn FnOnce(Result<Arc<Client>>) -> BoxFuture<'static, ()> + Send>;

/// The worker owning the connection; jobs run one at a time, in order.
#[derive(Clone)]
pub(crate) struct Pg {
    jobs: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl Pg {
    /// Starts the worker; the connection opens with the first job and reopens after it
    /// closes (Go's `database/sql` pool).
    pub(crate) fn start(dsn: Dsn) -> Result<Pg> {
        let (jobs, mut queue) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        std::thread::Builder::new().name("pgstore".into()).spawn(move || {
            runtime.block_on(async move {
                let mut client: Option<Arc<Client>> = None;
                while let Some(job) = queue.recv().await {
                    let current = match client.as_ref().filter(|c| !c.is_closed()) {
                        Some(c) => Ok(c.clone()),
                        None => match dsn.connect().await {
                            Ok(c) => {
                                let c = Arc::new(c);
                                client = Some(c.clone());
                                Ok(c)
                            }
                            Err(e) => {
                                client = None;
                                Err(e)
                            }
                        },
                    };
                    if tokio::time::timeout(JOB_TIMEOUT, job(current)).await.is_err() {
                        client = None;
                    }
                }
            });
        })?;
        Ok(Pg { jobs })
    }

    /// Queues `f` without waiting for it.
    pub(crate) fn submit<F, Fut>(&self, f: F)
    where
        F: FnOnce(Result<Arc<Client>>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let _ = self.jobs.send(Box::new(move |client| Box::pin(f(client))));
    }

    pub(crate) async fn run<T, F, Fut>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Client>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.submit(move |client| async move {
            // Go's expired context never starts its statement: a caller that gave up
            // while this waited in the queue must not write stale data later.
            if tx.is_closed() {
                return;
            }
            let result = match client {
                Ok(client) => f(client).await,
                Err(error) => Err(error),
            };
            let _ = tx.send(result);
        });
        rx.await
            .unwrap_or_else(|_| Err(anyhow!("postgres store: request abandoned (timeout or shutdown)")))
    }

    /// `run` for synchronous callers; must not be called from the worker itself.
    pub(crate) fn run_blocking<T, F, Fut>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Client>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.submit(move |client| async move {
            let result = match client {
                Ok(client) => f(client).await,
                Err(error) => Err(error),
            };
            let _ = tx.send(result);
        });
        rx.recv()
            .unwrap_or_else(|_| Err(anyhow!("postgres store: request abandoned (timeout or shutdown)")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_postgres::config::Host;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn keyword_values_follow_libpq_quoting() {
        let pairs = keyword_pairs(r"host=db  port = 5433 password='a b\'c\\d' dbname=x\ y").unwrap();
        assert_eq!(
            pairs,
            vec![
                ("host".into(), "db".into()),
                ("port".into(), "5433".into()),
                ("password".into(), r"a b'c\d".into()),
                ("dbname".into(), "x y".into()),
            ]
        );
        assert!(keyword_pairs("host").is_err());
        assert!(keyword_pairs("password='open").is_err());
    }

    #[test]
    fn sslmode_maps_to_pgx_verification() {
        let dsn = Dsn::parse("host=db user=u password=p sslmode=verify-full", &no_env).unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Require);
        assert!(dsn.tls.verify && dsn.tls.verify_hostname);
        let dsn = Dsn::parse(
            "postgres://u:p@db:5433/app?sslmode=verify-ca&application_name=x",
            &no_env,
        )
        .unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Require);
        assert!(dsn.tls.verify && !dsn.tls.verify_hostname);
        assert_eq!(dsn.config.get_ports(), &[5433]);
        assert_eq!(dsn.config.get_application_name(), Some("x"));
        let dsn = Dsn::parse("postgresql://u@db/app?sslmode=require", &no_env).unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Require);
        assert!(!dsn.tls.verify);
        // The default is pgx's `prefer`, without verification.
        let dsn = Dsn::parse("host=db user=u", &no_env).unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Prefer);
        assert!(!dsn.tls.verify);
        let dsn = Dsn::parse("host=db user=u sslmode=disable", &no_env).unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Disable);
        assert!(Dsn::parse("host=db sslmode=bogus", &no_env).is_err());
        // Unix sockets never use TLS.
        let dsn = Dsn::parse("host=/run/postgresql user=u sslmode=require", &no_env).unwrap();
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Disable);
    }

    #[test]
    fn other_keys_travel_as_runtime_parameters_like_pgx() {
        let dsn = Dsn::parse(
            r"host=db user=u options='-c work_mem=4MB' search_path='a b,c\\d' statement_cache_capacity=0",
            &no_env,
        )
        .unwrap();
        assert_eq!(
            dsn.config.get_options(),
            Some(r"-c work_mem=4MB -c search_path=a\ b,c\\d"),
            "pgx's own keys are consumed, the rest reach the server"
        );
        let dsn = Dsn::parse("postgres://u@db/app?search_path=tenant&database=other", &no_env).unwrap();
        assert_eq!(dsn.config.get_options(), Some("-c search_path=tenant"));
        assert_eq!(dsn.config.get_dbname(), Some("other"));
        // PGOPTIONS only when the DSN has no options.
        let env = |key: &str| (key == "PGOPTIONS").then(|| "-c statement_timeout=5s".to_owned());
        let dsn = Dsn::parse("host=db search_path=t", &env).unwrap();
        assert_eq!(
            dsn.config.get_options(),
            Some("-c statement_timeout=5s -c search_path=t")
        );
        // URL queries: Go's decoding (`+` is a space) and the first value per key.
        let dsn = Dsn::parse(
            "postgres://u@db/app?search_path=tenant+one&search_path=public&application_name=a%2Bb+c&application_name=x",
            &no_env,
        )
        .unwrap();
        assert_eq!(dsn.config.get_options(), Some(r"-c search_path=tenant\ one"));
        assert_eq!(dsn.config.get_application_name(), Some("a+b c"));
        // Keyword form: the last value per key, hosts included.
        let dsn = Dsn::parse("host=a host=b search_path=x search_path=y", &no_env).unwrap();
        assert_eq!(dsn.config.get_hosts(), &[Host::Tcp("b".into())]);
        assert_eq!(dsn.config.get_options(), Some("-c search_path=y"));
        // Every separator the server splits on is escaped.
        let dsn = Dsn::parse("host=db search_path='a\tb\nc\x0bd'", &no_env).unwrap();
        assert_eq!(dsn.config.get_options(), Some("-c search_path=a\\\tb\\\nc\\\x0bd"));
        // A dangling `\` in the given options (dropped by the server) cannot swallow the
        // separator before the added arguments. libpq quoting: `\\` is one backslash.
        let dsn = Dsn::parse(r"host=db options='-c x=1\\' search_path=t", &no_env).unwrap();
        assert_eq!(dsn.config.get_options(), Some("-c x=1 -c search_path=t"));
        let dsn = Dsn::parse(r"host=db options='-c x=1\\\\' search_path=t", &no_env).unwrap();
        assert_eq!(dsn.config.get_options(), Some(r"-c x=1\\ -c search_path=t"));
    }

    #[test]
    fn environment_fills_only_what_the_dsn_leaves_out() {
        let env = |key: &str| match key {
            "PGHOST" => Some("envhost".to_owned()),
            "PGPORT" => Some("6000".to_owned()),
            "PGUSER" => Some("envuser".to_owned()),
            "PGDATABASE" => Some("envdb".to_owned()),
            "PGSSLMODE" => Some("disable".to_owned()),
            _ => None,
        };
        let dsn = Dsn::parse("user=dsnuser", &env).unwrap();
        assert_eq!(dsn.config.get_hosts(), &[Host::Tcp("envhost".into())]);
        assert_eq!(dsn.config.get_ports(), &[6000]);
        assert_eq!(dsn.config.get_user(), Some("dsnuser"));
        assert_eq!(dsn.config.get_dbname(), Some("envdb"));
        assert_eq!(dsn.config.get_ssl_mode(), SslMode::Disable);
        // Unknown pgx keys do not fail the parse.
        assert!(Dsn::parse("host=db user=u pool_max_conns=4", &no_env).is_ok());
        // The debug form never shows the password.
        let dsn = Dsn::parse("postgres://u:s3cr3t@db/app", &no_env).unwrap();
        assert!(!format!("{dsn:?}").contains("s3cr3t"));
    }
}
