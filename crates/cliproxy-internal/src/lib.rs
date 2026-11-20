//! Internal implementation of cliproxy: request handling, runtime policy and providers.
//!
//! Start with `gateway` for HTTP routes, `runtime` for credential selection and request
//! lifecycle, `providers` for upstream I/O, and `translation` for format conversion.
//! Shared data contracts live in `config`, `credential`, `execution` and `format`.
//! `protocol` owns byte-preserving JSON and request/response rules. `persistence`
//! defines storage contracts; `storage` and `home` implement the remote integrations.

// The internal library owns the Linux host requirement. See docs/DIFFERENCES-FROM-GO.md.
#[cfg(not(target_os = "linux"))]
compile_error!(
    "cliproxy-rs (zakstak fork) builds for Linux only; native Windows and macOS builds are not supported. \
     Build and run the Linux container from the repository's Dockerfile instead \
     (docs/INSTALL.md, \"Windows and macOS: run the Linux container\"), or use a Linux host. \
     Windows and macOS clients can still connect to a proxy running there."
);

pub mod catalog;
pub mod config;
pub mod credential;
pub mod execution;
pub mod format;
pub mod removed;
pub mod sent;
pub mod text;
mod text_tables;

pub mod gateway;
pub mod home;
pub mod persistence;
pub mod protocol;
pub mod providers;
pub mod runtime;
pub mod storage;
pub mod translation;

mod private_fs;
