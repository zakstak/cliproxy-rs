//! Remote homes for `config.yaml` and the auth files, chosen by `PGSTORE_*`,
//! `OBJECTSTORE_*` and `GITSTORE_*` (Go cmd/server/main.go and internal/store). Each
//! store mirrors its content into a local workspace; the server runs on those files
//! and reports changes back through [`crate::persistence::StorePersister`].

// Used only by the object-store tests, which assert Unix modes.
#[cfg(test)]
mod fake_s3;
mod git;
mod mirror;
mod object;
mod pgconn;
mod postgres;
mod postgres_history;
mod sigv4;
mod sqlite;

use crate::text::GoStr;
use anyhow::{Result as AnyResult, anyhow};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

pub use git::{GitPersister, GitStore};
pub use object::{ObjectConfig, ObjectStore};
pub use postgres::{PgCooldown, PostgresConfig, PostgresStore};
pub use postgres_history::PostgresHistory;
pub use sqlite::SqliteHistory;

use crate::persistence::StorePersister;
use crate::persistence::cooldown::Backend as CooldownBackend;

/// Go main's 30-second contexts around store setup.
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// The store the environment asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Postgres {
        dsn: String,
        schema: String,
        /// `<local path>/pgstore`.
        spool: PathBuf,
    },
    Object {
        endpoint: String,
        access_key: String,
        secret_key: String,
        bucket: String,
        /// `<local path>/objectstore`.
        root: PathBuf,
    },
    Git {
        url: String,
        username: String,
        token: String,
        branch: String,
        /// `<local path>/gitstore`.
        root: PathBuf,
    },
}

/// Go `filepath.Clean`, lexically.
pub(crate) fn clean(path: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.last() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir) => {}
                _ => out.push(component),
            },
            other => out.push(other),
        }
    }
    if out.is_empty() {
        return PathBuf::from(".");
    }
    out.iter().collect()
}

/// Go `filepath.Abs`: joined with the working directory, then cleaned.
pub(crate) fn go_abs(path: &Path) -> std::io::Result<PathBuf> {
    std::path::absolute(path).map(|p| clean(&p))
}

/// Go main's `lookupEnv`: the first key holding a non-blank value, trimmed.
fn lookup(env: &dyn Fn(&str) -> Option<String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| env(key))
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// Go `util.WritablePath`.
fn writable_path(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    lookup(env, &["WRITABLE_PATH", "writable_path"]).map(PathBuf::from)
}

/// Go main's store choice: a Home JWT disables every store; otherwise Postgres, then
/// the object store, then git. `wd` is the working directory.
pub fn select(env: &dyn Fn(&str) -> Option<String>, wd: &Path, home: bool) -> Option<Selection> {
    if home {
        return None;
    }
    let get = |upper: &str| lookup(env, &[upper, &upper.go_lower()]);
    let local = |key: &str| {
        get(key)
            .map(PathBuf::from)
            .or_else(|| writable_path(env))
            .unwrap_or_else(|| wd.to_path_buf())
    };
    if let Some(dsn) = get("PGSTORE_DSN") {
        return Some(Selection::Postgres {
            dsn,
            schema: get("PGSTORE_SCHEMA").unwrap_or_default(),
            spool: local("PGSTORE_LOCAL_PATH").join("pgstore"),
        });
    }
    if let Some(endpoint) = get("OBJECTSTORE_ENDPOINT") {
        return Some(Selection::Object {
            endpoint,
            access_key: get("OBJECTSTORE_ACCESS_KEY").unwrap_or_default(),
            secret_key: get("OBJECTSTORE_SECRET_KEY").unwrap_or_default(),
            bucket: get("OBJECTSTORE_BUCKET").unwrap_or_default(),
            root: local("OBJECTSTORE_LOCAL_PATH").join("objectstore"),
        });
    }
    get("GITSTORE_GIT_URL").map(|url| Selection::Git {
        url,
        username: get("GITSTORE_GIT_USERNAME").unwrap_or_default(),
        token: get("GITSTORE_GIT_TOKEN").unwrap_or_default(),
        branch: get("GITSTORE_GIT_BRANCH").unwrap_or_default(),
        root: local("GITSTORE_LOCAL_PATH").join("gitstore"),
    })
}

/// A store ready to serve: its config file, its auth mirror and the hooks the server
/// needs.
pub struct Store {
    pub config_path: PathBuf,
    pub auth_dir: PathBuf,
    pub persister: Arc<dyn StorePersister>,
    /// Postgres keeps cooldown state in its own table.
    pub cooldown: Option<Arc<dyn CooldownBackend>>,
    /// Go's info line once the config loaded.
    pub enabled: String,
}

pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> AnyResult<T> + Send + 'static) -> AnyResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!("store task failed: {e}"))?
}

async fn bounded<T>(work: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(SETUP_TIMEOUT, work)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("context deadline exceeded")))
}

/// Go main's store setup. The error is the line Go logs before exiting.
pub async fn bootstrap(selection: Selection, wd: &Path) -> Result<Store, String> {
    let example = wd.join("config.example.yaml");
    match selection {
        Selection::Postgres { dsn, schema, spool } => {
            let cfg = PostgresConfig {
                dsn,
                schema,
                spool_dir: spool,
            };
            let store = PostgresStore::connect(cfg)
                .await
                .map_err(|e| format!("failed to initialize postgres token store: {e:#}"))?;
            bounded(store.bootstrap(&example))
                .await
                .map_err(|e| format!("failed to bootstrap postgres-backed config: {e:#}"))?;
            let store = Arc::new(store);
            Ok(Store {
                config_path: store.config_path(),
                auth_dir: store.auth_dir(),
                cooldown: Some(Arc::new(store.cooldown_backend())),
                enabled: format!(
                    "postgres-backed token store enabled, workspace path: {}",
                    store.work_dir().display()
                ),
                persister: store,
            })
        }
        Selection::Object {
            endpoint,
            access_key,
            secret_key,
            bucket,
            root,
        } => {
            let (resolved, use_ssl) = ObjectConfig::endpoint_from(&endpoint).map_err(|e| e.to_string())?;
            let cfg = ObjectConfig {
                endpoint: resolved,
                bucket: bucket.clone(),
                access_key,
                secret_key,
                local_root: root,
                use_ssl,
            };
            let store = blocking(move || ObjectStore::new(cfg))
                .await
                .map_err(|e| format!("failed to initialize object token store: {e:#}"))?;
            bounded(store.bootstrap(&example))
                .await
                .map_err(|e| format!("failed to bootstrap object-backed config: {e:#}"))?;
            let store = Arc::new(store);
            Ok(Store {
                config_path: store.config_path(),
                auth_dir: store.auth_dir(),
                cooldown: None,
                enabled: format!("object-backed token store enabled, bucket: {bucket}"),
                persister: store,
            })
        }
        Selection::Git {
            url,
            username,
            token,
            branch,
            root,
        } => {
            let store = Arc::new(GitStore::new(&url, &username, &token, &branch, &root));
            let config_path = store.config_path();
            let prepared = store.clone();
            blocking(move || prepared.ensure_repository())
                .await
                .map_err(|e| format!("failed to prepare git token store: {e:#}"))?;
            match std::fs::metadata(&config_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::metadata(&example)
                        .map_err(|e| format!("failed to find template config file: stat {}: {e}", example.display()))?;
                    object::seed_config(&example, &config_path)
                        .map_err(|e| format!("failed to bootstrap git-backed config: {e:#}"))?;
                    // Go `persistSeededConfig`: a copy that cannot be prepared is removed,
                    // so every start refuses the template instead of running unpersisted.
                    if let Err(e) = crate::config::prepare_for_upload(&config_path) {
                        if let Err(remove) = std::fs::remove_file(&config_path) {
                            tracing::warn!("failed to remove the config copied from the template: {remove}");
                        }
                        return Err(format!(
                            "failed to commit initial git-backed config: prepare config for upload: {e:#}"
                        ));
                    }
                    let committing = store.clone();
                    blocking(move || committing.persist_config())
                        .await
                        .map_err(|e| format!("failed to commit initial git-backed config: {e:#}"))?;
                    tracing::info!("git-backed config initialized from template: {}", config_path.display());
                }
                Err(e) => return Err(format!("failed to inspect git-backed config: {e}")),
                Ok(_) => {}
            }
            Ok(Store {
                config_path,
                auth_dir: store.auth_dir(),
                cooldown: None,
                enabled: format!("git-backed token store enabled, repository path: {}", root.display()),
                persister: Arc::new(GitPersister(store)),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key| map.get(key).cloned()
    }

    /// Go `TestPersistSeededConfig*` (f9437700): the template copy is prepared before
    /// its first commit; a refused copy is removed and nothing is pushed.
    #[tokio::test]
    async fn the_git_seed_commits_the_prepared_config() {
        const PLAIN: &str = "seed-plaintext-secret";
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        for (template, accepted) in [
            (format!("port: 8317\nremote-management:\n  secret-key: {PLAIN}\n"), true),
            (
                format!("remote-management:\n  secret-key: {PLAIN}\nport: [8317\n"),
                false,
            ),
            (
                format!("defaults: &management\n  secret-key: {PLAIN}\nmanagement: *management\n"),
                false,
            ),
        ] {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let wd = std::env::temp_dir().join(format!("cpa-store-git-seed-{}-{nanos}", std::process::id()));
            let remote = wd.join("remote.git");
            std::fs::create_dir_all(&remote).unwrap();
            git(&remote, &["init", "-q", "--bare"]).unwrap();
            git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).unwrap();
            std::fs::write(wd.join("config.example.yaml"), &template).unwrap();
            let root = wd.join("gitstore");
            let selection = Selection::Git {
                url: remote.to_string_lossy().into_owned(),
                username: String::new(),
                token: String::new(),
                branch: "main".into(),
                root: root.clone(),
            };
            let result = bootstrap(selection, &wd).await;
            let committed = git(&remote, &["show", "main:config/config.yaml"]);
            if accepted {
                result.unwrap();
                let committed = committed.unwrap();
                assert!(!committed.contains(PLAIN), "{committed}");
                let hash = crate::config::Config::parse(&committed).unwrap().management.secret_key;
                assert!(bcrypt::verify(PLAIN, &hash).unwrap());
            } else {
                let error = result.err().unwrap();
                assert!(error.contains("prepare config for upload"), "{error}");
                assert_eq!(committed, None, "{template}");
                assert!(!root.join("config/config.yaml").exists(), "the refused copy is removed");
            }
            let _ = std::fs::remove_dir_all(&wd);
        }
    }

    #[test]
    fn store_selection_respects_explicit_operator_precedence() {
        let wd = Path::new("/srv/app");
        // Postgres wins over the others; blank values do not count; lower-case keys work.
        let env = env_of(&[
            ("PGSTORE_DSN", "   "),
            ("pgstore_dsn", " postgres://db/x "),
            ("OBJECTSTORE_ENDPOINT", "http://s3"),
            ("GITSTORE_GIT_URL", "https://git/x.git"),
            ("WRITABLE_PATH", "/data"),
        ]);
        assert_eq!(
            select(&env, wd, false),
            Some(Selection::Postgres {
                dsn: "postgres://db/x".into(),
                schema: String::new(),
                spool: "/data/pgstore".into(),
            })
        );
        // A Home JWT disables every store.
        assert_eq!(select(&env, wd, true), None);
        let env = env_of(&[
            ("OBJECTSTORE_ENDPOINT", "http://s3"),
            ("OBJECTSTORE_BUCKET", "b"),
            ("OBJECTSTORE_LOCAL_PATH", "/obj"),
            ("GITSTORE_GIT_URL", "https://git/x.git"),
        ]);
        assert!(
            matches!(select(&env, wd, false), Some(Selection::Object { root, bucket, .. }) if root == Path::new("/obj/objectstore") && bucket == "b")
        );
        let env = env_of(&[
            ("GITSTORE_GIT_URL", "https://git/x.git"),
            ("gitstore_git_branch", "main"),
        ]);
        assert_eq!(
            select(&env, wd, false),
            Some(Selection::Git {
                url: "https://git/x.git".into(),
                username: String::new(),
                token: String::new(),
                branch: "main".into(),
                root: "/srv/app/gitstore".into(),
            })
        );
        assert_eq!(select(&env_of(&[]), wd, false), None);
    }
}

/// The history store the `storage` section asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryBackend {
    /// `storage.backend: none`: nothing is recorded.
    None,
    /// `storage.backend: sqlite` (the default) at `storage.sqlite-path`.
    Sqlite(PathBuf),
    /// `storage.backend: postgres` at `storage.postgres-dsn`, in `storage.postgres-schema`.
    Postgres { dsn: String, schema: String },
}

/// `storage.*`: where the history lives and how long it is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorySettings {
    pub backend: HistoryBackend,
    /// `storage.retention-days`; 0 keeps everything.
    pub retention_days: i64,
}

pub const DEFAULT_RETENTION_DAYS: i64 = 30;
const HISTORY_FILE: &str = "history.db";

impl HistorySettings {
    /// Reads `storage.*` from `cfg`. The SQLite file defaults to `$WRITABLE_PATH/history.db`,
    /// else `history.db` in the auth directory, which the server can always write.
    pub fn from_config(cfg: &crate::config::Config) -> AnyResult<Self> {
        Self::read(cfg, None)
    }

    /// [`Self::from_config`] at startup. `data_dir` replaces the auth directory as the
    /// default SQLite location: with a remote store, the auth directory is the store's
    /// mirror, which belongs to the store.
    pub fn at_startup(cfg: &crate::config::Config, data_dir: Option<&Path>) -> AnyResult<Self> {
        Self::read(cfg, data_dir)
    }

    fn read(cfg: &crate::config::Config, data_dir: Option<&Path>) -> AnyResult<Self> {
        let section = cfg.document.get("storage");
        let text = |key: &str| {
            section
                .and_then(|s| s.get(key))
                .and_then(serde_yaml_ng::Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let retention_days = match section.and_then(|s| s.get("retention-days")) {
            None | Some(serde_yaml_ng::Value::Null) => DEFAULT_RETENTION_DAYS,
            Some(v) => match v.as_i64() {
                Some(days) if days >= 0 => days,
                _ => {
                    return Err(anyhow!(
                        "storage.retention-days must be a whole number of days, 0 or more"
                    ));
                }
            },
        };
        let backend = match text("backend").map(str::to_ascii_lowercase).as_deref() {
            None | Some("sqlite") => HistoryBackend::Sqlite(match text("sqlite-path") {
                Some(path) => PathBuf::from(path),
                None => default_sqlite_path(cfg, data_dir),
            }),
            Some("none") => HistoryBackend::None,
            Some("postgres" | "postgresql") => HistoryBackend::Postgres {
                dsn: text("postgres-dsn")
                    .ok_or_else(|| anyhow!("storage.backend: postgres needs storage.postgres-dsn"))?
                    .to_owned(),
                schema: text("postgres-schema").unwrap_or_default().to_owned(),
            },
            Some(other) => {
                return Err(anyhow!(
                    "storage.backend {other:?} is not one of sqlite, postgres, none"
                ));
            }
        };
        Ok(Self {
            backend,
            retention_days,
        })
    }

    /// Opens the configured store; `None` for `storage.backend: none`.
    pub async fn open(&self) -> AnyResult<Option<Arc<dyn crate::persistence::history::HistoryStore>>> {
        Ok(match &self.backend {
            HistoryBackend::None => None,
            HistoryBackend::Sqlite(path) => {
                let path = path.clone();
                Some(Arc::new(blocking(move || SqliteHistory::open(&path)).await?))
            }
            HistoryBackend::Postgres { dsn, schema } => Some(Arc::new(PostgresHistory::connect(dsn, schema).await?)),
        })
    }
}

fn default_sqlite_path(cfg: &crate::config::Config, data_dir: Option<&Path>) -> PathBuf {
    for key in ["WRITABLE_PATH", "writable_path"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return Path::new(v.trim()).join(HISTORY_FILE);
        }
    }
    data_dir.unwrap_or(&cfg.auth_dir).join(HISTORY_FILE)
}

#[cfg(test)]
mod history_settings_tests {
    use super::*;

    fn settings(yaml: &str) -> AnyResult<HistorySettings> {
        HistorySettings::from_config(&crate::config::Config::parse(&format!("auth-dir: /srv/auths\n{yaml}")).unwrap())
    }

    #[test]
    fn storage_settings_have_safe_defaults_and_refuse_bad_values() {
        let defaults = settings("").unwrap();
        assert_eq!(defaults.retention_days, DEFAULT_RETENTION_DAYS);
        if std::env::var_os("WRITABLE_PATH").is_none() && std::env::var_os("writable_path").is_none() {
            assert_eq!(
                defaults.backend,
                HistoryBackend::Sqlite(PathBuf::from("/srv/auths/history.db"))
            );
        }
        let custom = settings("storage:\n  backend: SQLite\n  sqlite-path: /data/h.db\n  retention-days: 0\n").unwrap();
        assert_eq!(custom.backend, HistoryBackend::Sqlite(PathBuf::from("/data/h.db")));
        assert_eq!(custom.retention_days, 0);
        assert_eq!(
            settings("storage: {backend: none}\n").unwrap().backend,
            HistoryBackend::None
        );
        assert_eq!(
            settings("storage: {backend: postgres, postgres-dsn: 'postgres://h/db', postgres-schema: proxy}\n")
                .unwrap()
                .backend,
            HistoryBackend::Postgres {
                dsn: "postgres://h/db".into(),
                schema: "proxy".into()
            }
        );
        assert!(settings("storage: {backend: postgres}\n").is_err(), "a DSN is required");
        assert!(settings("storage: {backend: mongo}\n").is_err());
        if std::env::var_os("WRITABLE_PATH").is_none() && std::env::var_os("writable_path").is_none() {
            let cfg = crate::config::Config::parse("auth-dir: /srv/pgstore/auths\n").unwrap();
            let mirrored = HistorySettings::at_startup(&cfg, Some(Path::new("/srv"))).unwrap();
            assert_eq!(
                mirrored.backend,
                HistoryBackend::Sqlite(PathBuf::from("/srv/history.db")),
                "never inside a remote store's auth mirror"
            );
        }
        assert!(settings("storage: {retention-days: -1}\n").is_err());
        assert!(
            crate::config::Config::parse("storage: {retention-days: soon}\n").is_err(),
            "the schema types it"
        );
    }
}
