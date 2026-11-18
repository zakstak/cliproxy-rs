//! v8 config `api-keys` with each key's live `auth_index` (Go v8.0.35
//! `injectV8APIKeyAuthIndexesLocked`, `0fb50a18`), and the matching strip of those
//! read-only fields from writes.

use std::collections::{BTreeMap, HashMap};

use crate::config::{Config, credentials};
use crate::credential::Credential;
use serde_json::{Map, Value};

/// Adds `auth_index` to every v8 `api-keys` entry that a credential synthesized from
/// `cfg` backs, preferring the index the live credential with that ID carries.
pub(super) fn inject(root: &mut Value, cfg: &Config, live: &HashMap<String, String>) {
    let Some(api_keys) = root.get_mut("api-keys").and_then(Value::as_object_mut) else {
        return;
    };
    let auths = credentials::from_config(cfg);
    let index = |c: &Credential| match live.get(&c.id).filter(|i| !i.trim().is_empty()) {
        Some(i) => i.trim().to_owned(),
        None => credentials::auth_index(c),
    };
    let config_index = |c: &Credential| c.attributes.get("config_index").cloned().unwrap_or_default();
    let of = |provider: &'static str| auths.iter().filter(move |c| c.provider == provider);

    // gemini and interactions match by their dedup key, vertex by key and base URL.
    let mut by_key: HashMap<&str, HashMap<String, String>> = HashMap::new();
    for (family, provider) in [("gemini", "gemini"), ("interactions", "gemini-interactions")] {
        let entries = credentials::api_key_entries(cfg, family);
        let map = by_key.entry(family).or_default();
        for c in of(provider) {
            if let Some(e) = config_index(c).parse::<usize>().ok().and_then(|i| entries.get(i)) {
                map.insert(
                    dedup(&e.api_key, &e.base_url, &e.proxy_url, &e.prefix, &e.headers),
                    index(c),
                );
            }
        }
    }
    let vertex = credentials::api_key_entries(cfg, "vertex");
    let map = by_key.entry("vertex").or_default();
    for c in of("vertex") {
        if let Some(e) = config_index(c).parse::<usize>().ok().and_then(|i| vertex.get(i)) {
            map.insert(format!("{}|{}", e.api_key.trim(), e.base_url.trim()), index(c));
        }
    }
    let auths = &auths;
    let by_position = |provider: &str| -> HashMap<String, &Credential> {
        auths
            .iter()
            .filter(|c| c.provider == provider)
            .map(|c| (config_index(c), c))
            .collect()
    };

    for (provider, groups) in api_keys.iter_mut() {
        let Some(groups) = groups.as_array_mut() else { continue };
        let groups = groups.iter_mut().filter_map(Value::as_object_mut);
        match provider.trim() {
            "openai-compatibility" => {
                let mut position = 0;
                for group in groups {
                    if scalar(group, "base-url").is_empty() {
                        continue;
                    }
                    let target = position.to_string();
                    position += 1;
                    let mine: Vec<&Credential> = auths
                        .iter()
                        .filter(|c| {
                            config_index(c) == target
                                && (c.provider == "openai-compatibility"
                                    || c.provider.starts_with("openai-compatible-"))
                        })
                        .collect();
                    let keys = group.get_mut("keys").and_then(Value::as_array_mut);
                    match keys {
                        Some(keys) if !keys.is_empty() => {
                            for (key, c) in keys.iter_mut().zip(&mine) {
                                if let Some(key) = key.as_object_mut() {
                                    set(key, index(c));
                                }
                            }
                        }
                        _ => {
                            if let Some(c) = mine.iter().find(|c| !c.attributes.contains_key("api_key")) {
                                set(group, index(c));
                            }
                        }
                    }
                }
            }
            family @ ("claude" | "codex" | "xai" | "meta") => {
                let positions = by_position(family);
                let mut position = 0;
                for group in groups {
                    let base = scalar(group, "base-url");
                    if matches!(family, "codex" | "xai") && base.is_empty() {
                        continue;
                    }
                    let Some(keys) = group.get_mut("keys").and_then(Value::as_array_mut) else {
                        continue;
                    };
                    for key in keys.iter_mut().filter_map(Value::as_object_mut) {
                        let api_key = scalar(key, "api-key");
                        if family == "meta" && (api_key.is_empty() || api_key.starts_with("dca:")) {
                            continue;
                        }
                        let target = position.to_string();
                        position += 1;
                        if family == "claude" && api_key.is_empty() && base.is_empty() {
                            continue;
                        }
                        if let Some(c) = positions.get(&target) {
                            set(key, index(c));
                        }
                    }
                }
            }
            family => {
                let Some(map) = by_key.get(family) else { continue };
                for group in groups {
                    let base = scalar(group, "base-url");
                    let inherited: Map<String, Value> = group.clone();
                    let Some(keys) = group.get_mut("keys").and_then(Value::as_array_mut) else {
                        continue;
                    };
                    for key in keys.iter_mut().filter_map(Value::as_object_mut) {
                        let api_key = scalar(key, "api-key");
                        let found = if family == "vertex" {
                            map.get(&format!("{api_key}|{base}"))
                        } else {
                            let pick = |field: &str| present(key, field).or_else(|| present(&inherited, field));
                            let text = |v: Option<&Value>| v.map(text).unwrap_or_default();
                            let headers = pick("headers").map(headers).unwrap_or_default();
                            map.get(&dedup(
                                &api_key,
                                &base,
                                &text(pick("proxy-url")),
                                &text(pick("prefix")),
                                &headers,
                            ))
                        };
                        if let Some(i) = found.cloned() {
                            set(key, i);
                        }
                    }
                }
            }
        }
    }
}

/// Go `stripAPIKeysAuthIndexesFromUpdate`: `auth_index` and `auth-index` never reach
/// the file, whether a client echoes a read back at the root, at `api-keys` or at one
/// provider's list.
pub(super) fn strip_update(parts: &[&str], value: &mut serde_yaml_ng::Value) {
    match parts {
        [] => strip_root(value),
        ["api-keys"] => strip_providers(value),
        ["api-keys", ..] => strip_groups(value),
        _ => {}
    }
}

/// Go `stripAPIKeysAuthIndexesFromRoot`.
pub(super) fn strip_root(root: &mut serde_yaml_ng::Value) {
    if let Some(api_keys) = root.get_mut("api-keys") {
        strip_providers(api_keys);
    }
}

fn strip_providers(api_keys: &mut serde_yaml_ng::Value) {
    if let Some(map) = api_keys.as_mapping_mut() {
        for (_, groups) in map.iter_mut() {
            strip_groups(groups);
        }
    }
}

fn strip_groups(groups: &mut serde_yaml_ng::Value) {
    let strip = |group: &mut serde_yaml_ng::Value| {
        let Some(group) = group.as_mapping_mut() else { return };
        group.remove("auth_index");
        group.remove("auth-index");
        if let Some(keys) = group.get_mut("keys").and_then(serde_yaml_ng::Value::as_sequence_mut) {
            for key in keys.iter_mut().filter_map(serde_yaml_ng::Value::as_mapping_mut) {
                key.remove("auth_index");
                key.remove("auth-index");
            }
        }
    };
    match groups {
        serde_yaml_ng::Value::Sequence(list) => list.iter_mut().for_each(strip),
        group @ serde_yaml_ng::Value::Mapping(_) => strip(group),
        _ => {}
    }
}

fn set(entry: &mut Map<String, Value>, index: String) {
    if !index.is_empty() {
        entry.insert("auth_index".into(), index.into());
    }
}

/// Go `yamlMapScalar`: a scalar's text, trimmed; empty for anything else.
fn scalar(entry: &Map<String, Value>, key: &str) -> String {
    entry.get(key).map(text).unwrap_or_default()
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

/// Go `yamlMapScalarPresent`: present and not null.
fn present<'a>(entry: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    entry.get(key).filter(|v| !v.is_null())
}

/// Go `NormalizeHeaders` of a decoded `map[string]string`; a map holding a list or a
/// map does not decode and counts as none.
fn headers(v: &Value) -> BTreeMap<String, String> {
    let Some(map) = v.as_object() else {
        return BTreeMap::new();
    };
    if map.values().any(|v| v.is_array() || v.is_object()) {
        return BTreeMap::new();
    }
    map.iter()
        .map(|(k, v)| (k.trim().to_owned(), text(v)))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// Go `formatCredentialDedupKey`.
fn dedup(key: &str, base: &str, proxy: &str, prefix: &str, headers: &BTreeMap<String, String>) -> String {
    let headers: String = headers.iter().map(|(k, v)| format!("{k}\0{v}\0")).collect();
    [
        key.trim(),
        base.trim(),
        proxy.trim(),
        &credentials::normalize_prefix(prefix),
        &headers,
    ]
    .join("\0")
}
