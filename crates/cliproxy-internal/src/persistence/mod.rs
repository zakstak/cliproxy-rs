//! Persistence contracts: the history store ([`history`]) and cooldown state
//! ([`cooldown`]). Config and credentials live in `config.yaml` and the auth directory.

pub mod cooldown;
pub mod history;
