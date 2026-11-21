//! Remote persistence for config and auth files (Go's `storePersister` and the token
//! store's `Delete`). The Postgres, git and object stores mirror their content into a
//! local workspace; the server keeps working on those files and reports each change
//! here so the store can push it.

pub mod cooldown;
pub mod history;

use std::path::PathBuf;

use futures_util::future::BoxFuture;

/// Writes a credential for [`StorePersister::save_auth`] and returns its path.
pub type SaveFn<'a> = Box<dyn FnOnce() -> anyhow::Result<Option<PathBuf>> + 'a>;

pub trait StorePersister: Send + Sync + 'static {
    /// Go `PersistConfig`: the config file changed (or was removed).
    fn persist_config(&self) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `PersistAuthFiles`. `message` is `Sync auth <name>` or `Remove auth <name>`.
    fn persist_auth_files(&self, message: String, paths: Vec<PathBuf>) -> BoxFuture<'_, anyhow::Result<()>>;
    /// Go `Store.Delete`: an explicit removal (management), unlike a file event.
    /// Blocking: called on a Tokio blocking thread (inside the runtime) that holds the
    /// management disk lock, so it must not wait for another blocking-pool task.
    fn delete_auth(&self, path: PathBuf) -> anyhow::Result<()>;
    /// Go `Store.Save`: writes one credential with `write` (`None`: nothing written) and
    /// returns once the store has it. Blocking, like `delete_auth`. This default writes
    /// and then pushes the file; the git store pulls first and holds its lock across
    /// the write and the commit.
    fn save_auth(&self, write: SaveFn<'_>) -> anyhow::Result<Option<PathBuf>> {
        let Some(path) = write()? else {
            return Ok(None);
        };
        let path = std::path::absolute(&path).unwrap_or(path);
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let message = format!("Update auth {name}");
        tokio::runtime::Handle::current().block_on(self.persist_auth_files(message, vec![path.clone()]))?;
        Ok(Some(path))
    }
    /// The mirrored auth directory; every config load uses it instead of `auth-dir`.
    fn auth_dir(&self) -> PathBuf;
}
