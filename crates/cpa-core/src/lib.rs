//! Contracts shared by every cliproxy-rs crate.
//!
//! - [`config`]: `config.yaml`, compatible with CLIProxyAPI.
//! - [`credential`]: runtime credentials loaded from `auth-dir` or config.
//! - [`exec`]: the execution envelope passed between the runtime and executors.
//! - [`format`]: request/response wire formats.
//! - [`gostr`]: Go's table-driven case and print rules (`strings.ToLower`, ...).
//! - [`registry`]: the pinned static model catalog.

// Every workspace crate depends on this one, so this is the single place the supported
// build targets are enforced: a non-Linux build stops here, before any crate reaches
// Linux-only APIs (inotify, `std::os::unix`). See docs/DIFFERENCES-FROM-GO.md.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "cliproxy-rs (zakstak fork) builds for Linux only; native Windows and macOS builds are not supported. \
     Build and run the Linux container from the repository's Dockerfile instead \
     (docs/INSTALL.md, \"Windows and macOS: run the Linux container\"), or use a Linux host. \
     Windows and macOS clients can still connect to a proxy running there."
);

pub mod config;
pub mod credential;
pub mod exec;
pub mod format;
pub mod gostr;
mod gostr_tables;
pub mod registry;
