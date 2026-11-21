//! The PostgreSQL history store (`storage.backend: postgres`) against a throwaway local
//! PostgreSQL cluster (initdb in a temp dir, trust auth, a free port). Skipped when no
//! PostgreSQL binaries are installed; set `CPA_TEST_PG_BIN` to their directory to choose
//! one, and `CPA_TEST_NO_SKIP=1` to fail instead of skipping.
// initdb and pg_ctl: Unix only.

use std::path::PathBuf;
use std::process::Command;
use std::time::SystemTime;

use cliproxy_internal::persistence::history::{Cursor, HistoryStore, RequestEntry, RequestFilter, UnixMillis};
use cliproxy_internal::storage::PostgresHistory;

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

/// The history store contract on PostgreSQL, in a quoted schema, and the schema version
/// guard: a database from a newer server is refused, an existing one is reused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_history_meets_the_contract() {
    use cliproxy_internal::persistence::history::conformance;
    let Some(cluster) = Cluster::start() else { return };
    let schema = "history \"tenant\"";
    let store = PostgresHistory::connect(&cluster.dsn(), schema).await.unwrap();
    conformance::run(&store).await;
    // Reconnecting keeps the rows and does not run the migration again.
    let again = PostgresHistory::connect(&cluster.dsn(), schema).await.unwrap();
    let rows = again
        .requests(RequestFilter::default(), Cursor::Newest, 10)
        .await
        .unwrap()
        .rows;
    assert_eq!(rows.len(), 4, "the conformance run leaves four requests");
    let sql = cluster.client().await;
    sql.batch_execute("INSERT INTO \"history \"\"tenant\"\"\".cliproxy_history_schema (version) VALUES (99)")
        .await
        .unwrap();
    let error = PostgresHistory::connect(&cluster.dsn(), schema)
        .await
        .err()
        .expect("refused");
    assert!(
        format!("{error:#}").contains("newer than this server supports"),
        "{error:#}"
    );
    // Another schema in the same database is independent.
    let other = PostgresHistory::connect(&cluster.dsn(), "").await.unwrap();
    assert!(
        other
            .requests(RequestFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap()
            .rows
            .is_empty()
    );
    let missing = PostgresHistory::connect("", "").await.err().expect("a DSN is required");
    assert!(format!("{missing:#}").contains("postgres-dsn"), "{missing:#}");
}

/// `search_path` in the DSN places the tables when `storage.postgres-schema` is empty,
/// including a schema whose name needs quoting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_path_in_the_dsn_selects_the_schema() {
    let Some(cluster) = Cluster::start() else { return };
    let sql = cluster.client().await;
    sql.batch_execute("CREATE SCHEMA tenant; CREATE SCHEMA \"my tenant\";")
        .await
        .unwrap();
    for (param, schema) in [("tenant", "tenant"), ("%22my%20tenant%22", "my tenant")] {
        let dsn = format!("{}&search_path={param}&application_name=cpa%20history", cluster.dsn());
        let store = PostgresHistory::connect(&dsn, "").await.unwrap();
        store
            .append_requests(vec![RequestEntry {
                at: UnixMillis(1),
                request_id: "r".into(),
                ..RequestEntry::default()
            }])
            .await
            .unwrap();
        let tables: i64 = sql
            .query_one(
                "SELECT count(*) FROM information_schema.tables WHERE table_schema = $1",
                &[&schema],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(tables, 3, "{schema}");
        let rows = store
            .requests(RequestFilter::default(), Cursor::Newest, 10)
            .await
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1, "{schema}");
    }
}
