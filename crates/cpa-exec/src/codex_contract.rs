//! Codex wire-contract fixtures (`tests/fixtures/codex_contract`) for contract tests.
//!
//! Each fixture is a small hand-authored wire payload with recorded provenance: the
//! pinned first-party Codex source or test it follows, the provider documentation it
//! quotes, or the contract rule it probes. `docs/codex/CONTRACT.md` defines the
//! contract; `manifest.json` in the fixture directory lists every fixture.
//!
//! The reader returns file bytes as they are on disk. It never parses and re-serializes
//! a payload, so a test can compare a captured upstream body or client response against
//! the fixture byte for byte. Behavioural assertions belong to the PR that implements
//! the behaviour; this module only loads fixtures.

use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde::Deserialize;

/// The pinned `openai/codex` commit every first-party fixture cites.
pub const FIRST_PARTY_COMMIT: &str = "44ccf394573cabb95e1f751a59f9d76f6f79faf1";

/// What a fixture's content rests on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Evidence {
    /// Follows a first-party type or request builder at [`FIRST_PARTY_COMMIT`].
    FirstPartySource,
    /// Copies or follows a payload in a first-party test at [`FIRST_PARTY_COMMIT`].
    FirstPartyTest,
    /// Follows OpenAI's published API documentation.
    ProviderDocs,
    /// Hand-authored to probe a contract rule; not evidence of what a client sends.
    ContractProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// One JSON document: an HTTP body or one WebSocket text frame.
    Json,
    /// One WebSocket text frame per line.
    Jsonl,
    /// A `text/event-stream` body.
    Sse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Http,
    Websocket,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Sent by a client and expected at the upstream.
    ToUpstream,
    /// Sent by the upstream and expected at the client.
    ToClient,
}

/// The endpoint profiles of `docs/codex/CONTRACT.md`. A fixture lists only the
/// profiles its evidence covers; an absent profile is unknown, not unsupported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Endpoint {
    /// ChatGPT Codex backend (`https://chatgpt.com/backend-api/codex`).
    Chatgpt,
    /// OpenAI API with an API key (`https://api.openai.com/v1`).
    ApiKey,
    /// The OpenAI API with a credential's Sign in with ChatGPT token.
    Siwc,
    /// An operator-configured compatible `base_url`.
    Compatible,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub name: String,
    /// Path relative to the fixture directory.
    pub file: String,
    pub format: Format,
    pub transport: Transport,
    pub direction: Direction,
    pub endpoints: Vec<Endpoint>,
    pub evidence: Evidence,
    /// First-party paths with line ranges, or documentation URLs.
    pub sources: Vec<String>,
    /// The contract the payload demonstrates.
    pub contract: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub repository: String,
    pub commit: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub pin: Pin,
    pub fixtures: Vec<Fixture>,
}

/// The fixture directory in this crate's source tree.
pub fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_contract")
}

/// Reads and parses `manifest.json`. Panics with the path on failure: this is test
/// support, and a broken fixture set should stop the test that uses it.
pub fn manifest() -> Manifest {
    let path = dir().join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The fixture called `name`.
pub fn fixture(name: &str) -> Fixture {
    manifest()
        .fixtures
        .into_iter()
        .find(|f| f.name == name)
        .unwrap_or_else(|| panic!("no Codex contract fixture named {name:?}"))
}

impl Fixture {
    pub fn path(&self) -> PathBuf {
        dir().join(&self.file)
    }

    /// The file's bytes, exactly as stored.
    pub fn bytes(&self) -> Bytes {
        let path = self.path();
        Bytes::from(std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
    }

    /// The payloads a transport carries one at a time, as slices of [`Self::bytes`]:
    /// the whole document for JSON, each non-empty line for JSONL, and each event's
    /// `data` field for SSE. An event with several `data:` lines is not split here; such
    /// fixtures should be compared through [`Self::bytes`].
    pub fn frames(&self) -> Vec<Bytes> {
        let bytes = self.bytes();
        match self.format {
            Format::Json => vec![bytes],
            Format::Jsonl => lines(&bytes)
                .filter(|(_, line)| !line.is_empty())
                .map(|(start, line)| bytes.slice(start..start + line.len()))
                .collect(),
            Format::Sse => lines(&bytes)
                .filter_map(|(start, line)| {
                    let data = line.strip_prefix(b"data:")?;
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    let offset = start + (line.len() - data.len());
                    Some(bytes.slice(offset..offset + data.len()))
                })
                .collect(),
        }
    }
}

/// Lines of `bytes` without their `\n` or `\r\n`, each with its start offset.
fn lines(bytes: &[u8]) -> impl Iterator<Item = (usize, &[u8])> {
    let mut start = 0;
    bytes.split(|&b| b == b'\n').map(move |line| {
        let at = start;
        start += line.len() + 1;
        (at, line.strip_suffix(b"\r").unwrap_or(line))
    })
}

#[cfg(test)]
#[path = "codex_contract_tests.rs"]
mod tests;
