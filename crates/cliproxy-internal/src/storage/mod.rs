//! Storage backends for the history store ([`crate::persistence::history::HistoryStore`]):
//! embedded SQLite (the default) and PostgreSQL, chosen by the `storage` config section.
//!
//! Go CLIProxyAPI's remote config and credential stores (`PGSTORE_*`, `OBJECTSTORE_*`,
//! `GITSTORE_*`) are not supported (#222): [`refuse_go_store_env`] stops the server when
//! one is configured, instead of silently running on local files.

mod pgconn;
mod postgres_history;
mod sqlite;

use anyhow::{Result as AnyResult, anyhow};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use postgres_history::PostgresHistory;
pub use sqlite::SqliteHistory;

/// Runs blocking work (file I/O, SQLite) on Tokio's blocking pool.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> AnyResult<T> + Send + 'static) -> AnyResult<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| anyhow!("store task failed: {e}"))?
}

/// The variables that selected one of Go's remote config and credential stores.
const GO_STORE_ENV: [(&str, &str); 3] = [
    ("PGSTORE_DSN", "PostgreSQL"),
    ("OBJECTSTORE_ENDPOINT", "object storage"),
    ("GITSTORE_GIT_URL", "Git"),
];

/// Refuses a Go remote store setting (`PGSTORE_DSN`, `OBJECTSTORE_ENDPOINT`,
/// `GITSTORE_GIT_URL`, upper or lower case, non-blank): the error names the variable
/// and the way forward. Starting on local files instead would silently lose the
/// config and logins the operator keeps in that store.
pub fn refuse_go_store_env(env: &dyn Fn(&str) -> Option<String>) -> Result<(), String> {
    for (key, name) in GO_STORE_ENV {
        let set = [key.to_owned(), key.to_ascii_lowercase()]
            .iter()
            .any(|k| env(k).is_some_and(|v| !v.trim().is_empty()));
        if set {
            return Err(format!(
                "{key} is set, but cliproxy-rs no longer keeps config.yaml and credential files in a remote {name} store (issue #222). \
                 Copy config.yaml and the auth directory out of the store, point -config and auth-dir at them (or mount them), \
                 unset {key}, and start again. See docs/DIFFERENCES-FROM-GO.md."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod go_store_env_tests {
    use super::*;

    #[test]
    fn a_go_remote_store_setting_stops_the_server_with_the_way_forward() {
        let none = |_: &str| None;
        assert!(refuse_go_store_env(&none).is_ok());
        let blank = |_: &str| Some("  ".to_owned());
        assert!(refuse_go_store_env(&blank).is_ok(), "blank values select nothing");
        for key in [
            "PGSTORE_DSN",
            "pgstore_dsn",
            "OBJECTSTORE_ENDPOINT",
            "GITSTORE_GIT_URL",
            "gitstore_git_url",
        ] {
            let env = |k: &str| (k == key).then(|| "postgres://user:secret@db/x".to_owned());
            let error = refuse_go_store_env(&env).expect_err(key);
            assert!(error.contains(&key.to_ascii_uppercase()), "{error}");
            assert!(error.contains("auth directory"), "{error}");
            assert!(!error.contains("secret"), "the value is never echoed: {error}");
        }
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
                None => default_sqlite_path(cfg),
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

fn default_sqlite_path(cfg: &crate::config::Config) -> PathBuf {
    for key in ["WRITABLE_PATH", "writable_path"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return Path::new(v.trim()).join(HISTORY_FILE);
        }
    }
    cfg.auth_dir.join(HISTORY_FILE)
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
        assert!(settings("storage: {retention-days: -1}\n").is_err());
        assert!(
            crate::config::Config::parse("storage: {retention-days: soon}\n").is_err(),
            "the schema types it"
        );
    }
}
