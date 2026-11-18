//! Passive quota evidence used by credential selection.

use std::collections::BTreeMap;
use std::time::SystemTime;

/// The latest passive quota snapshot per credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub observed_at: SystemTime,
    pub signals: BTreeMap<String, String>,
}

/// A provider's latest observed quota for one credential. Reading never sends I/O.
pub(crate) trait QuotaSource: Send + Sync {
    fn quota_snapshot(&self, credential_id: &str) -> Option<Snapshot>;
}
