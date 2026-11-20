//! Watcher events and management hooks share a store lock. A management hook can
//! occupy the server's last blocking thread while a watcher owns that lock. Give
//! locked file reads and S3 DNS lookups a pool independent of the server.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use cpa_server::persist::SaveFn;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::OwnedMutexGuard;
use tokio_util::context::RuntimeExt;

pub(super) fn runtime() -> Result<&'static Runtime> {
    static RUNTIME: LazyLock<io::Result<Runtime>> = LazyLock::new(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("store-io")
            .enable_all()
            .build()
    });
    RUNTIME.as_ref().map_err(|e| anyhow!("start store I/O runtime: {e}"))
}

#[derive(Clone, Copy)]
pub(super) enum FileIo {
    Async(&'static Runtime),
    /// Management already runs on a blocking thread.
    Inline,
}

impl FileIo {
    pub(super) async fn read(self, path: &Path) -> io::Result<Vec<u8>> {
        match self {
            Self::Async(rt) => rt.wrap(tokio::fs::read(path)).await,
            Self::Inline => std::fs::read(path),
        }
    }

    pub(super) async fn remove(self, path: PathBuf, guard: OwnedMutexGuard<()>) -> io::Result<OwnedMutexGuard<()>> {
        let unlink = move || {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            Ok(guard)
        };
        match self {
            // Keep the lock until unlink finishes even if the caller is cancelled.
            Self::Async(rt) => rt.spawn_blocking(unlink).await?,
            Self::Inline => unlink(),
        }
    }
}

pub(super) fn save_auth(write: SaveFn<'_>, persist: impl FnOnce(&[PathBuf]) -> Result<()>) -> Result<Option<PathBuf>> {
    let Some(path) = write()? else { return Ok(None) };
    let path = std::path::absolute(&path).unwrap_or(path);
    persist(std::slice::from_ref(&path))?;
    Ok(Some(path))
}
