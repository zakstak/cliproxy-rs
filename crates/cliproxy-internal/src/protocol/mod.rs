//! Shared request and response helpers for the supported client and provider formats.
//! Transformations preserve untouched wire bytes. Tests use explicit behavior contracts
//! and independent JSON models rather than another implementation's recorded output.

pub mod codex_body;
pub mod codex_catalog;
pub mod codex_client;
pub mod headers;
pub mod idle;
pub mod json;
#[cfg(test)]
mod json_edit_tests;
pub mod payload;
pub mod rawjson;
pub mod schema;
pub mod schema_refs;
pub mod session;
pub mod signature;
pub mod thinking;
