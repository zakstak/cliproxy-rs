//! `config.yaml`, read the way CLIProxyAPI v8 reads it (internal/config/config_v8.go,
//! config_load.go).
//!
//! Both the v8 layout (`server.port`, `access.api-keys`, `oauth.auth-dir`) and the
//! legacy top-level spellings are accepted. Presence decides: a v8 key that is present
//! wins even when it is `null`, `0`, `false` or empty. A v8 parent that is present but
//! not a mapping is an error, so `access: null` is rejected instead of silently opening
//! the proxy.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};

mod binary;
pub mod credentials;
mod document;
pub mod go_url;
mod sanitize;
mod schema;
mod text;
mod timestamp;
mod trusted;
mod validate;
pub use document::{ConfigDocument, archive_comments, credential_error, is_v8_layout, v8_group_error, v8_layout_error};
pub use schema::validate as validate_config_fields;
pub use schema::{coerce_typed_scalars, go_bool, go_int};
pub use timestamp::{AsJson, TAG as TIMESTAMP_TAG};

/// `text` parsed as serde_yaml_ng does, except for plain scalars it reads otherwise
/// than yaml.v3: an integer beyond int64 and uint64, which serde_yaml_ng refuses when
/// it fits 128 bits (yaml.v3 reads a decimal one as a float and any other, `0x…` or
/// `0o…`, as text), and a number with underscores (`1_000`) or a leading zero (`007`,
/// `08`), which serde_yaml_ng reads as text and yaml.v3 as a number. Those scalars are
/// read as yaml.v3 reads them, and a timestamp (`2001-12-14`), which yaml.v3 reads into
/// an untyped value as a `time.Time`, is its RFC 3339 text tagged [`TIMESTAMP_TAG`]. A
/// string setting still gets the scalar's text from the literal walk, which reads `text`
/// itself; such a mapping key stays text, as yaml.v3 reads it into a string-keyed map.
pub fn parse_yaml(text: &str) -> Result<Value, serde_yaml_ng::Error> {
    // serde_yaml_ng, like libyaml with its encoding set, reads a leading BOM as part of
    // the first line, which then fails a second top-level key; yaml.v3 skips it.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let parse = |text: &str| serde_yaml_ng::from_str::<Value>(text);
    // Without an underscore or a leading zero, only a failed parse needs a second look;
    // it keeps its error when no scalar is rewritten.
    let err = if may_resolve(text) {
        None
    } else {
        match parse(text) {
            Ok(value) if !needs_scan(&value) => return Ok(value),
            Ok(_) => None,
            Err(err) => Some(err),
        }
    };
    let as_is = |err: Option<serde_yaml_ng::Error>| err.map_or_else(|| parse(text), Err);
    let Ok(scalars) = binary::plain_scalars(text) else {
        return as_is(err);
    };
    // One pass over the scalars, then one parse.
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for plain in scalars.into_iter().filter(|p| p.timestamp_tag.is_none()) {
        let read = std::str::from_utf8(&plain.value)
            .ok()
            .and_then(|s| go_read(s, plain.key));
        let (Some(read), Some(before)) = (read, text.get(last..plain.span.start)) else {
            continue;
        };
        out.push_str(before);
        out.push_str(&read);
        last = plain.span.end;
    }
    if last == 0 {
        return as_is(err);
    }
    out.push_str(&text[last.min(text.len())..]);
    parse(&out)
}

/// `value` as serde_yaml_ng writes it, except that a string [`parse_yaml`] would read
/// back as a number or timestamp when written plain (`1_000`, `0_8`, `0b-1`,
/// `2001-12-14`) is double-quoted, as yaml.v3 quotes it, and a [`TIMESTAMP_TAG`] value
/// is its text, plain, as yaml.v3 writes a `time.Time`. serde_yaml_ng quotes only the
/// strings it reads as something else itself.
pub fn yaml_string(value: &Value) -> Result<String, serde_yaml_ng::Error> {
    let text = serde_yaml_ng::to_string(value)?;
    // A number serde_yaml_ng writes has no underscore or leading zero, so each scalar
    // flagged here is a string. It quotes a string beyond 64 bits (`'18446744073709551616'`)
    // itself.
    if !may_resolve(&text) && !needs_scan(value) {
        return Ok(text);
    }
    let Ok(scalars) = binary::plain_scalars(&text) else {
        return Ok(text);
    };
    let mut out = String::with_capacity(text.len() + 2);
    let mut last = 0;
    for plain in scalars {
        if let Some(tag) = plain.timestamp_tag {
            out.push_str(&text[last..tag]);
            out.push_str(&text[plain.span.clone()]);
            last = plain.span.end;
            continue;
        }
        // A key is checked as a value: yaml.v3 reads a plain `1_000:` key as a number,
        // which in an untyped map is no longer the string key it was.
        let flagged = std::str::from_utf8(&plain.value).ok().and_then(|s| go_read(s, false));
        if flagged.is_none() {
            continue;
        }
        // The flagged spellings hold only ASCII letters, digits, spaces and `_.,:+-`.
        out.push_str(&text[last..plain.span.start]);
        out.push('"');
        out.push_str(&text[plain.span.clone()]);
        out.push('"');
        last = plain.span.end;
    }
    out.push_str(&text[last..]);
    Ok(out)
}

/// Whether `text` may hold a plain scalar with an underscore or a leading zero, which
/// [`go_read`] rewrites without a parse error to point at it.
fn may_resolve(text: &str) -> bool {
    let zero = |w: &[u8]| w[0] == b'0' && (w[1].is_ascii_digit() || matches!(w[1], b'b' | b'o' | b'X' | b'O' | b'B'));
    text.contains('_')
        || text.as_bytes().starts_with(b"0")
        || text
            .as_bytes()
            .windows(3)
            .any(|w| zero(&w[1..]) && !(w[0].is_ascii_alphanumeric() || w[0] == b'.'))
}

/// Whether `value` holds a string, key or value, that may be a plain timestamp, a
/// [`TIMESTAMP_TAG`] value, or an integer past int64, which `+` makes a float in Go.
fn needs_scan(value: &Value) -> bool {
    match value {
        Value::String(s) => timestamp::go_timestamp(s).is_some(),
        Value::Number(n) => n.as_i64().is_none() && n.as_u64().is_some(),
        Value::Sequence(items) => items.iter().any(needs_scan),
        Value::Mapping(m) => m.iter().any(|(k, v)| needs_scan(k) || needs_scan(v)),
        Value::Tagged(t) => t.tag == timestamp::TAG || needs_scan(&t.value),
        _ => false,
    }
}

/// What [`parse_yaml`] writes for a plain scalar so that serde_yaml_ng reads it as
/// yaml.v3 does; `None` where they agree. A mapping key keeps its text unless
/// serde_yaml_ng would refuse it.
fn go_read(scalar: &str, key: bool) -> Option<String> {
    if !key && let Some(time) = timestamp::go_timestamp(scalar) {
        // Verbatim, so a `%TAG !` directive cannot redefine it.
        return Some(format!("!<{}> {time}", timestamp::TAG));
    }
    let unsigned = scalar.strip_prefix(['+', '-']).unwrap_or(scalar).as_bytes();
    let leading_zero = unsigned.first() == Some(&b'0')
        && unsigned
            .get(1)
            .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'b' | b'o' | b'X' | b'O' | b'B'));
    if scalar.contains('_') || leading_zero {
        // A `0o` or `0b` integer beyond 64 bits that Go reads as text.
        (!key)
            .then(|| go_resolved(scalar))
            .flatten()
            .or_else(|| go_big_integer(scalar, key))
    } else {
        go_big_integer(scalar, key)
    }
}

/// yaml.v3's reading of a plain scalar with underscores or a leading zero (its
/// `resolve`), written so that serde_yaml_ng reads the same; `None` where yaml.v3 reads
/// text or a timestamp (`0001-01-01`), which serde_yaml_ng reads as text. A scalar that
/// starts with a digit or sign drops its underscores and reads as an integer (`0x`,
/// `0o`, `0b` or a leading `0` for octal), else as a float in YAML's float syntax; one
/// that starts with `.` reads as a float when each underscore sits between digits.
fn go_resolved(text: &str) -> Option<String> {
    let plain = text.replace('_', "");
    let float = |s: &str| {
        let f = go_yaml_float(s)?;
        Some(format!("{f:e}"))
    };
    match text.as_bytes().first()? {
        b'.' => {
            let bytes = text.as_bytes();
            let between_digits = bytes.iter().enumerate().all(|(i, &b)| {
                b != b'_'
                    || (i > 0 && bytes[i - 1].is_ascii_digit() && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
            });
            if between_digits { float(&plain) } else { None }
        }
        b'0'..=b'9' | b'+' | b'-' => {
            let negative = plain.starts_with('-');
            let signed = plain.starts_with(['+', '-']);
            let unsigned = plain.strip_prefix(['+', '-']).unwrap_or(&plain);
            let (radix, digits) = match unsigned.get(..2) {
                Some("0x" | "0X") => (16, &unsigned[2..]),
                Some("0o" | "0O") => (8, &unsigned[2..]),
                Some("0b" | "0B") => (2, &unsigned[2..]),
                _ if unsigned.len() > 1 && unsigned.starts_with('0') => (8, &unsigned[1..]),
                _ => (10, unsigned),
            };
            if !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)) {
                // Go's ParseInt, then ParseUint (which takes no sign).
                if let Ok(n) = u64::from_str_radix(digits, radix) {
                    if negative && n <= 1 << 63 {
                        return Some(format!("-{n}"));
                    }
                    if !negative && (n <= i64::MAX as u64 || !signed) {
                        return Some(n.to_string());
                    }
                    // `+0x…`, `+0o…` or `+0b…` past int64: ParseInt overflows and
                    // ParseUint takes no sign, so Go reads text; serde_yaml_ng a u64.
                    if !negative && unsigned.as_bytes().get(1).is_some_and(u8::is_ascii_alphabetic) {
                        return Some(format!("'{text}'"));
                    }
                }
            }
            if let Some(f) = float(&plain) {
                return Some(f);
            }
            // Go's last resort: `0b` or `0o` then ParseInt of the rest, which takes a
            // sign (`0b-1` is -1), then ParseUint.
            let (radix, rest) = match plain.get(..2)? {
                "0b" => (2, &plain[2..]),
                "0o" => (8, &plain[2..]),
                _ => return None,
            };
            let n = i64::from_str_radix(rest, radix).map(|n| n.to_string());
            let unsigned = || {
                (!rest.starts_with('+'))
                    .then(|| u64::from_str_radix(rest, radix).ok())
                    .flatten()
            };
            n.ok().or_else(|| unsigned().map(|n| n.to_string()))
        }
        _ => None,
    }
}

/// `s` as a float when it has yaml.v3's float syntax and Go parses it in range.
fn go_yaml_float(s: &str) -> Option<f64> {
    let digits = |s: &str| s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    let rest = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (mantissa, exponent) = match rest.find(['e', 'E']) {
        Some(at) => (&rest[..at], Some(&rest[at + 1..])),
        None => (rest, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some(("", frac)) => !frac.is_empty() && digits(frac) == frac.len(),
        Some((int, frac)) => !int.is_empty() && digits(int) == int.len() && digits(frac) == frac.len(),
        None => !mantissa.is_empty() && digits(mantissa) == mantissa.len(),
    };
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e) == e.len()
    });
    if !(mantissa_ok && exponent_ok) {
        return None;
    }
    s.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// yaml.v3's reading of a plain integer beyond int64 and uint64 that serde_yaml_ng
/// refuses (one that fits u128, or i128 when negative, and is not a decimal with a
/// leading zero), written so that serde_yaml_ng reads the same: a decimal one as its
/// float, any other quoted. A value with a `+` past int64 reads so too: Go's ParseUint
/// takes no sign, where serde_yaml_ng reads a u64. `None` for any other scalar, which
/// serde_yaml_ng reads.
fn go_big_integer(text: &str, key: bool) -> Option<String> {
    let negative = text.starts_with('-');
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (radix, digits) = match unsigned.get(..2) {
        Some("0x" | "0X") => (16, &unsigned[2..]),
        Some("0o" | "0O") => (8, &unsigned[2..]),
        Some("0b" | "0B") => (2, &unsigned[2..]),
        _ => (10, unsigned),
    };
    if digits.is_empty() || (radix == 10 && digits.len() > 1 && digits.starts_with('0')) {
        return None;
    }
    let magnitude = u128::from_str_radix(digits, radix).ok()?;
    if negative && magnitude > 1 << 127 {
        return None;
    }
    let fits = if negative {
        i64::from_str_radix(&format!("-{digits}"), radix).is_ok()
    } else if text.starts_with('+') && !key {
        i64::from_str_radix(digits, radix).is_ok()
    } else {
        u64::from_str_radix(digits, radix).is_ok()
    };
    if fits {
        return None;
    }
    Some(if radix == 10 {
        format!("{:e}", text.parse::<f64>().ok()?)
    } else {
        format!("'{text}'")
    })
}

/// A config text with its tagged scalars as yaml.v3 decodes them (`!!binary` values
/// base64-decoded, aliases of them included), merges not yet expanded. The management
/// document reads config text through here and the loader through [`config_value`], so
/// a view or a save candidate sees the tagged values the loader sees.
pub fn yaml_value(text: &str) -> anyhow::Result<Value> {
    let mut value = parse_yaml(text)?;
    timestamp::refuse_wide_offsets(&value)?;
    binary::decode(text, &mut value, None, binary::Mode::File)?;
    Ok(value)
}

/// [`yaml_value`] as Go's typed decode reads it into the config: a plain scalar in a Go
/// string setting is its text (`1.50`, `0x1F`, `true`), where serde_yaml_ng reads a
/// number or boolean. The management views keep [`yaml_value`]'s YAML types, as Go's
/// read the file's nodes.
pub fn config_value(text: &str) -> anyhow::Result<Value> {
    let mut value = parse_yaml(text)?;
    timestamp::refuse_wide_offsets(&value)?;
    binary::decode(text, &mut value, Some(&[]), binary::Mode::File)?;
    Ok(value)
}

/// The load error for a config that still has a `plugins` section. Native plugins are
/// removed (docs/DIFFERENCES-FROM-GO.md "Native plugins"): the section would configure
/// nothing, so the config is refused instead of running without what it asks for.
pub const PLUGINS_REMOVED: &str = "the plugins section is not supported: cliproxy-rs removed native plugin support \
     (loader, plugin store, plugin hooks and Home plugin sync); delete the plugins section from the config and \
     see docs/MIGRATING-FROM-GO.md \"Native plugins\" for replacements";

/// A management write's body, the value at `keys` of the config: Go parses it as YAML
/// nodes and saves their text, so a number or boolean in a string setting whose text the
/// save would not write back (`1.50`, `1e3`) is that text. One the save writes back as
/// it is (`5`, `true`) keeps its YAML type, so the saved file and its views stay Go's.
pub fn body_value(text: &str, keys: &[&str]) -> anyhow::Result<Value> {
    fn keep_written(literal: &mut Value, parsed: Value) {
        match (literal, parsed) {
            (Value::Mapping(l), Value::Mapping(p)) => {
                for ((_, l), (_, p)) in l.iter_mut().zip(p) {
                    keep_written(l, p);
                }
            }
            (Value::Sequence(l), Value::Sequence(p)) => {
                for (l, p) in l.iter_mut().zip(p) {
                    keep_written(l, p);
                }
            }
            (l @ Value::String(_), p @ (Value::Number(_) | Value::Bool(_)))
                if l.as_str() == Some(go_string(&p).as_str()) =>
            {
                *l = p;
            }
            _ => {}
        }
    }
    let parsed = parse_yaml(text)?;
    let mut value = parsed.clone();
    binary::decode(text, &mut value, Some(keys), binary::Mode::Body)?;
    keep_written(&mut value, parsed);
    Ok(value)
}

/// yaml.v3's error for a merge value that is not a mapping or a list of mappings.
const MERGE_NOT_MAPS: &str = "yaml: map merge requires map or sequence of maps as the value";

/// Go `expandConfigAliases` plus yaml.v3's merge rules: `<<` keys expand bottom-up
/// (nested merges first); explicit keys win, then earlier merged mappings. A merge
/// value that is not a mapping or a list of mappings is yaml.v3's decode error, even in
/// a merge source an explicit key or an earlier source shadows, which yaml.v3 never
/// decodes.
/// (Aliases are already expanded by the parser.) The parsed value no longer says whether
/// a key was quoted or a merge operand an alias, so [`yaml_value`] refuses a quoted
/// `<<` key (yaml.v3 reads it as a plain key) and fails an alias of a sequence as a
/// merge operand. A JSON management body, which is not read through it, keeps `<<`
/// literal until the completed candidate is expanded, as Go's save ends up writing it plain.
pub fn expand_merges(value: &mut Value) -> anyhow::Result<()> {
    match value {
        Value::Mapping(map) => {
            for (_, child) in map.iter_mut() {
                expand_merges(child)?;
            }
            if let Some(merge) = map.shift_remove("<<") {
                let sources: Vec<Mapping> = match untagged(merge) {
                    Value::Mapping(m) => vec![m],
                    Value::Sequence(items) => items
                        .into_iter()
                        .map(|item| match untagged(item) {
                            Value::Mapping(m) => Ok(m),
                            _ => Err(anyhow::anyhow!(MERGE_NOT_MAPS)),
                        })
                        .collect::<anyhow::Result<_>>()?,
                    _ => bail!(MERGE_NOT_MAPS),
                };
                for source in sources {
                    for (key, item) in source {
                        map.entry(key).or_insert(item);
                    }
                }
            }
        }
        Value::Sequence(items) => {
            for item in items {
                expand_merges(item)?;
            }
        }
        Value::Tagged(tagged) => expand_merges(&mut tagged.value)?,
        _ => {}
    }
    Ok(())
}

/// A merge operand without its local tags: yaml.v3 merges a tagged mapping by its kind.
fn untagged(value: Value) -> Value {
    match value {
        Value::Tagged(tagged) => untagged(tagged.value),
        other => other,
    }
}
pub use trusted::{TrustedProxies, go_trim_space};
pub use validate::{catalog_sources as validate_catalog_sources, go_is_abs, parse_duration};

/// Default port used only where CLIProxyAPI falls back to it (management base URL).
/// The loader itself leaves an omitted port at 0, as Go does.
pub const DEFAULT_PORT: u16 = 8317;
pub const DEFAULT_AUTH_DIR: &str = "~/.cli-proxy-api";

#[derive(Clone, PartialEq, Eq)]
pub struct Config {
    /// Bind address. Empty binds all interfaces.
    pub host: String,
    /// 0 when omitted, like Go: the listener then gets an ephemeral port.
    pub port: u16,
    /// Client keys for the proxy API, trimmed and de-duplicated. Empty disables client auth.
    pub api_keys: Vec<String>,
    /// `server.trusted-proxies`, validated. Go applies it at startup only.
    pub trusted_proxies: Vec<String>,
    pub auth_dir: PathBuf,
    pub routing: RoutingConfig,
    pub management: ManagementConfig,
    /// Presence-preserving v8 view, including fields not yet wired into executors.
    pub document: Value,
    /// v8 `oauth.providers.*` leaf paths present in the source text (Go
    /// `Config.OAuthOnlyFields`, keyed by v8 path rather than legacy name). They
    /// apply to OAuth credentials only; see [`Config::for_api_key`]. Settings written
    /// in the legacy layout stay global, as in Go.
    pub oauth_only: std::collections::BTreeSet<String>,
    /// Values derived from this snapshot, built on first use ([`Config::derived`]).
    pub derived: Derived,
}

/// A per-snapshot cache of values computed from a [`Config`] (for example parsed payload
/// rules), one per type. Everyone holding the same snapshot shares them. A clone starts
/// empty and the cache never affects equality. Derive from configs that will not be
/// mutated afterwards; the runtime's published snapshots are immutable.
#[derive(Default)]
pub struct Derived(std::sync::Mutex<Vec<std::sync::Arc<dyn std::any::Any + Send + Sync>>>);

impl Clone for Derived {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for Derived {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Derived {}

impl Config {
    /// `models.catalog` and `models.codex-catalog` (Go `CatalogSources` without the
    /// removed Devin catalog); empty when unset.
    pub fn catalog_sources(&self) -> [String; 2] {
        ["catalog", "codex-catalog"].map(|name| {
            self.document
                .get("models")
                .and_then(|models| models.get(name))
                .filter(|v| !v.is_null())
                .map(go_string)
                .unwrap_or_default()
        })
    }

    /// The value of type `T` derived from this snapshot, computing it with `init` on first
    /// use. Concurrent first uses may both compute; the first stored value wins.
    pub fn derived<T: std::any::Any + Send + Sync>(&self, init: impl FnOnce(&Config) -> T) -> std::sync::Arc<T> {
        let find = |slots: &[std::sync::Arc<dyn std::any::Any + Send + Sync>]| {
            slots.iter().find_map(|v| v.clone().downcast::<T>().ok())
        };
        let lock = || self.derived.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(found) = find(&lock()) {
            return found;
        }
        let value = std::sync::Arc::new(init(self));
        let mut slots = lock();
        if let Some(found) = find(&slots) {
            return found;
        }
        slots.push(value.clone());
        value
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("routing", &self.routing)
            .finish_non_exhaustive()
    }
}

/// yaml.v3 decodes any scalar into a Go `string` field; serde would reject numbers.
fn de_go_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(go_string(&Value::deserialize(d)?))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RoutingConfig {
    #[serde(deserialize_with = "de_go_string")]
    pub strategy: String,
    pub force_model_prefix: bool,
    pub session_affinity: bool,
    #[serde(deserialize_with = "de_go_string")]
    pub session_affinity_ttl: String,
    pub session_affinity_subagents: Option<bool>,
    /// `never` keeps a bound session on its account and fails its requests while that
    /// account is unavailable; anything else (Go's default `any`) moves it.
    #[serde(deserialize_with = "de_go_string", skip_serializing_if = "String::is_empty")]
    pub session_affinity_rebind_on: String,
    pub retry: RetryConfig,
    pub cooldown: CooldownConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct RetryConfig {
    pub request_retry: i64,
    pub max_retry_credentials: i64,
    pub max_retry_interval: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CooldownConfig {
    pub disable_cooling: bool,
    pub save_cooldown_status: bool,
    pub transient_error_cooldown_seconds: i64,
    /// cliproxy-rs only: the longest quota reset trusted at first, as a duration
    /// (`"1h"`) or seconds; empty means one hour, `0` trusts every reset (Go).
    #[serde(deserialize_with = "de_go_string", skip_serializing_if = "String::is_empty")]
    pub max_trusted_cooldown: String,
}

// Do not derive Debug: the key must never appear in diagnostic output.
#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ManagementConfig {
    pub allow_remote: bool,
    #[serde(deserialize_with = "de_go_string")]
    pub secret_key: String,
    pub disable_control_panel: bool,
    pub disable_auto_update_panel: bool,
    #[serde(deserialize_with = "de_go_string")]
    pub panel_github_repository: String,
}

impl std::fmt::Debug for ManagementConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementConfig")
            .field("allow_remote", &self.allow_remote)
            .field("disable_control_panel", &self.disable_control_panel)
            .finish_non_exhaustive()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::parse("{}").expect("empty mapping is valid")
    }
}

/// Writes `hash` over the plaintext `secret-key` (see [`with_secret_hash`]).
fn persist_secret_hash(path: &Path, text: &str, hash: &str) -> anyhow::Result<()> {
    ConfigDocument::write(path, &with_secret_hash(text, hash)?)
}

/// `text` with `hash` over the effective `secret-key`: in place when the key is written
/// literally and nothing else shares it, else the merge- and alias-expanded tree, as
/// Go's `updateNestedScalarYAML` writes it. ponytail: that fallback re-emits without
/// comments; Go keeps the node comments.
fn with_secret_hash(text: &str, hash: &str) -> anyhow::Result<String> {
    // Go's save writes its parsed nodes, which drops a leading BOM.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let expanded = |t: &str| -> anyhow::Result<Value> {
        let mut root: Value = parse_yaml(t)?;
        expand_merges(&mut root)?;
        Ok(root)
    };
    let mut want = expanded(text)?;
    let mapping = want.as_mapping().context("config must be a mapping")?;
    let v8 = lookup(mapping, "management.secret-key").is_some();
    let parent = if v8 { "management" } else { "remote-management" };
    // In place only over a literal key: an inherited one would stay in the merge.
    let literal = parse_yaml(text)
        .ok()
        .and_then(|raw| raw.get(parent)?.as_mapping()?.get("secret-key").cloned())
        .is_some();
    let in_place = literal.then(|| {
        let file: yaml_edit::YamlFile = text.parse().ok()?;
        file.document()?.get_mapping(parent)?.set("secret-key", hash);
        Some(file.to_string())
    });
    want.get_mut(parent)
        .and_then(Value::as_mapping_mut)
        .context("management must be a mapping")?
        .insert(Value::from("secret-key"), Value::from(hash));
    // An anchored key edited in place would also change every alias of it.
    let updated = match in_place.flatten().filter(|t| expanded(t).is_ok_and(|v| v == want)) {
        Some(t) => t,
        None => {
            let t = yaml_string(&want)?;
            // The re-emitted tree writes parsed values (`0x1F` becomes `31`) and drops
            // tags; Go keeps each node's text and tag. Refuse rather than change
            // another setting: on any explicit tag, any key that is not a string, or
            // any other scalar whose text would change.
            fn plain_keys(v: &Value) -> bool {
                match v {
                    Value::Mapping(m) => m.iter().all(|(k, v)| matches!(k, Value::String(_)) && plain_keys(v)),
                    Value::Sequence(items) => items.iter().all(plain_keys),
                    Value::Tagged(_) => false,
                    _ => true,
                }
            }
            let tagged = yaml_edit::lex(text)
                .iter()
                .any(|(kind, _)| *kind == yaml_edit::SyntaxKind::TAG);
            anyhow::ensure!(
                !tagged && plain_keys(&want),
                "rewriting the config file would change the text of another setting"
            );
            let others = |t: &str| -> anyhow::Result<Value> {
                let mut tree = scalar_texts(t)?;
                expand_merges(&mut tree)?;
                if let Some(m) = tree.get_mut(parent).and_then(Value::as_mapping_mut) {
                    m.remove("secret-key");
                }
                Ok(tree)
            };
            anyhow::ensure!(
                others(text)? == others(&t)?,
                "rewriting the config file would change the text of another setting"
            );
            t
        }
    };
    let stored = scalar_texts(&updated).ok().map(|tree| effective_secret(&tree));
    anyhow::ensure!(stored.as_deref() == Some(hash), "secret key not updated");
    Ok(updated)
}

/// The management secret as Go's `Config` decodes it from a [`scalar_texts`] tree
/// with merges expanded: `management.secret-key` when present (presence wins, even
/// when null), else `remote-management.secret-key`.
fn effective_secret(tree: &Value) -> String {
    let root = tree.as_mapping();
    let at = |path| root.and_then(|m| lookup(m, path));
    match at("management.secret-key").or_else(|| at("remote-management.secret-key")) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// `text` as a tree whose scalars are their decoded text, as yaml.v3's `Node.Value`
/// holds it (`0x1F` stays `0x1F`, `'31'` is `31`, `!!binary c2VjcmV0` is `c2VjcmV0`),
/// and whose nulls stay null. Custom tags are dropped. Keys are kept as parsed;
/// aliases are expanded, merges are not.
fn scalar_texts(text: &str) -> anyhow::Result<Value> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
    struct Shaped<'a>(&'a Value);
    struct Entries<'a>(&'a Mapping);
    struct Items<'a>(&'a [Value]);
    impl<'de> DeserializeSeed<'de> for Shaped<'_> {
        type Value = Value;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            match self.0 {
                Value::Mapping(map) => d.deserialize_map(Entries(map)),
                Value::Sequence(items) => d.deserialize_seq(Items(items)),
                Value::Null => IgnoredAny::deserialize(d).map(|_| Value::Null),
                // A plain timestamp, which parse_yaml tags; the text holds no tag.
                Value::Tagged(tagged) if tagged.tag == timestamp::TAG => String::deserialize(d).map(Value::String),
                // A custom tag reaches serde as an enum variant; Go reads the node.
                Value::Tagged(tagged) => d.deserialize_any(Untagged(&tagged.value)),
                _ => String::deserialize(d).map(Value::String),
            }
        }
    }
    struct Untagged<'a>(&'a Value);
    impl<'de> Visitor<'de> for Untagged<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a tagged node")
        }
        fn visit_enum<A: serde::de::EnumAccess<'de>>(self, access: A) -> Result<Value, A::Error> {
            use serde::de::VariantAccess as _;
            let (IgnoredAny, variant) = access.variant()?;
            match self.0 {
                Value::Mapping(_) | Value::Sequence(_) => variant.newtype_variant_seed(Shaped(self.0)),
                // Text even when spelled `null` or `~`: only a `!!null` node is null.
                _ => variant.newtype_variant::<String>().map(Value::String),
            }
        }
    }
    impl<'de> Visitor<'de> for Entries<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a mapping")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
            let mut out = Mapping::new();
            for (key, value) in self.0 {
                access.next_key::<IgnoredAny>()?;
                out.insert(key.clone(), access.next_value_seed(Shaped(value))?);
            }
            Ok(Value::Mapping(out))
        }
    }
    impl<'de> Visitor<'de> for Items<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Value, A::Error> {
            let mut out = Vec::with_capacity(self.0.len());
            for item in self.0 {
                out.push(access.next_element_seed(Shaped(item))?.unwrap_or(Value::Null));
            }
            Ok(Value::Sequence(out))
        }
    }
    use serde::Deserialize as _;
    // The shape and the text must hold the same nodes; parse_yaml skips a BOM.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let shape: Value = parse_yaml(text)?;
    Ok(Shaped(&shape).deserialize(serde_yaml_ng::Deserializer::from_str(text))?)
}

/// Which Go entry point [`decode`] follows.
#[derive(Clone, Copy, PartialEq)]
enum Decode {
    /// `LoadConfig`: the credential preflight (`validateCredentialWeightYAML`), then the
    /// typed decode.
    Load,
    /// `ParseConfigBytes`: as `Load`, with typed-decode errors wrapped as
    /// `parse config payload: ...` (Go wraps its `yaml.Unmarshal`).
    Payload,
}

/// The decoding part of [`Config::parse`]: the YAML, its layout and the setting types,
/// before the validators. Its errors name settings, not their values. The layout checks
/// come first, unwrapped, as in Go.
fn decode(text: &str, mode: Decode) -> anyhow::Result<(Mapping, ConfigDocument)> {
    let mut parsed = config_value(text)?;
    // Before any migration, as Go expands aliases first.
    expand_merges(&mut parsed)?;
    let root = match parsed {
        Value::Null if text.trim().is_empty() => Mapping::new(),
        Value::Mapping(m) => m,
        _ => bail!("config must be a mapping"),
    };
    // Any `plugins` key, even an empty or disabled one, before any other check: nothing
    // reads it any more, so the operator learns that first.
    anyhow::ensure!(!root.contains_key("plugins"), PLUGINS_REMOVED);
    validate_shape(&root)?;
    if let Some(e) = credential_error(&root) {
        bail!(e);
    }
    let mut document = ConfigDocument::from_mapping(root.clone())?;
    document.coerce_typed_scalars();
    // Before the field checks, so a removed provider's setting reads as a migration.
    crate::removed::config(document.value())?;
    schema::validate(document.value(), false)
        .and_then(|()| validate::decoded(document.value()))
        .map_err(|e| {
            if mode == Decode::Payload {
                anyhow::anyhow!("parse config payload: {e}")
            } else {
                e
            }
        })?;
    Ok((root, document))
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        Self::load_text(path, &std::fs::read_to_string(path)?)
    }

    /// [`Config::load`] of text already read from `path`, so a caller knows exactly which
    /// bytes it parsed.
    pub fn load_text(path: &Path, text: &str) -> anyhow::Result<Self> {
        let mut config = Self::parse(text)?;
        if !config.management.secret_key.is_empty() && !is_bcrypt(&config.management.secret_key) {
            let hash = bcrypt::hash(&config.management.secret_key, 10)?;
            // Go hashes in memory and persists best-effort (a read-only mount starts).
            let _ = persist_secret_hash(path, text, &hash);
            config.management.secret_key = hash.clone();
            if let Some(management) = config.document.get_mut("management").and_then(Value::as_mapping_mut) {
                management.insert(Value::from("secret-key"), Value::from(hash));
            }
        }
        Ok(config)
    }

    /// Go `ParseConfigBytes`, as the v8 config writer reports it: [`Config::parse`] with
    /// typed-decode errors wrapped as `parse config payload: ...` (see [`decode`]).
    pub fn parse_payload(text: &str) -> anyhow::Result<Self> {
        Self::parse_as(text, Decode::Payload)
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Self::parse_as(text, Decode::Load)
    }

    fn parse_as(text: &str, mode: Decode) -> anyhow::Result<Self> {
        let (root, document) = decode(text, mode)?;
        let canonical = document.value().as_mapping().expect("document is a mapping");
        let routing = decode_section::<RoutingConfig>(canonical, "routing")?;
        let mut routing = routing;
        routing.retry.max_retry_credentials = routing.retry.max_retry_credentials.max(0);
        let management = decode_section::<ManagementConfig>(canonical, "management")?;
        let pick = |v8: &str, legacy: &str| lookup(&root, v8).or_else(|| lookup(&root, legacy));
        let host = string_or_empty(pick("server.host", "host"), "server.host")?;
        let port = match pick("server.port", "port") {
            None | Some(Value::Null) => 0,
            // yaml.v3 truncates a float into Go's int port.
            Some(v) => go_int(v)
                .and_then(|p| u16::try_from(p).ok())
                .context("server.port must be an integer port number")?,
        };
        // A top-level `api-keys` mapping is the v8 upstream key map, not client keys.
        let legacy_keys = lookup(&root, "api-keys").filter(|v| !v.is_mapping());
        let api_keys = match lookup(&root, "access.api-keys").or(legacy_keys) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Sequence(seq)) => seq
                .iter()
                .map(|k| {
                    k.as_str()
                        .map(str::to_owned)
                        .context("access.api-keys must be a list of strings")
                })
                .collect::<anyhow::Result<_>>()?,
            Some(_) => bail!("access.api-keys must be a list of strings"),
        };
        let auth_dir = string_or_empty(pick("oauth.auth-dir", "auth-dir"), "oauth.auth-dir")?;
        let trusted_proxies: Vec<String> = match lookup(canonical, "server.trusted-proxies") {
            Some(Value::Sequence(seq)) => seq.iter().map(go_string).collect(),
            _ => Vec::new(),
        };
        trusted::validate(&trusted_proxies)?;
        validate::go_custom(document.value())?;
        let oauth_only = schema::oauth_only_paths(&Value::Mapping(root.clone()));
        let cfg = Config {
            host,
            port,
            api_keys: normalize_keys(api_keys),
            trusted_proxies,
            auth_dir: resolve_auth_dir(&auth_dir),
            routing,
            management,
            document: document.into_value(),
            oauth_only,
            derived: Derived::default(),
        };
        validate::codex_compat(&cfg)?;
        Ok(cfg)
    }

    /// `server.max-request-body-bytes` (legacy root `max-request-body-bytes`), Go
    /// `SDKConfig.MaxRequestBodyBytes`: the request body limit in bytes. `None` when
    /// omitted or not positive, which disables it (Go's default 0).
    pub fn max_request_body_bytes(&self) -> Option<u64> {
        let value = self.document.get("server")?.get("max-request-body-bytes")?;
        u64::try_from(go_int(value)?).ok().filter(|&n| n > 0)
    }

    /// Go `Config.ForAPIKey`: the view an API-key credential sees, with the v8
    /// OAuth-only provider settings set to their typed zero. Borrowed when there are
    /// none.
    pub fn for_api_key(&self) -> std::borrow::Cow<'_, Config> {
        if self.oauth_only.is_empty() {
            return std::borrow::Cow::Borrowed(self);
        }
        let mut view = self.clone();
        for path in &self.oauth_only {
            schema::zero_at(&mut view.document, path);
        }
        view.oauth_only.clear();
        std::borrow::Cow::Owned(view)
    }
}

/// Go `looksLikeBcrypt`.
pub fn is_bcrypt(key: &str) -> bool {
    looks_like_bcrypt(key.as_bytes())
}

fn looks_like_bcrypt(key: &[u8]) -> bool {
    key.len() > 4 && [b"$2a$", b"$2b$", b"$2y$"].iter().any(|p| key.starts_with(*p))
}

fn decode_section<T: serde::de::DeserializeOwned + Default>(root: &Mapping, name: &str) -> anyhow::Result<T> {
    match lookup(root, name) {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => Ok(serde_yaml_ng::from_value(null_scalars_to_zero(value.clone()))?),
    }
}

// Go YAML decoding null into a non-pointer field leaves its zero/default value.
// Omit nulls when decoding the typed runtime view; the document keeps them intact.
fn null_scalars_to_zero(mut value: Value) -> Value {
    if let Value::Mapping(ref mut map) = value {
        map.retain(|_, v| !v.is_null());
        for (_, v) in map {
            *v = null_scalars_to_zero(v.clone());
        }
    }
    value
}

fn validate_shape(root: &Mapping) -> anyhow::Result<()> {
    if let Some(e) = document::shape_error(root) {
        bail!(e);
    }
    if let Some(version) = lookup(root, "config-version")
        && version.as_u64() != Some(8)
    {
        bail!("unsupported config-version (expected 8)");
    }
    Ok(())
}

/// Value at a dotted path, or `None` when any segment is absent or a parent is not a
/// mapping. Present-but-null returns `Some(Value::Null)`.
fn lookup<'a>(root: &'a Mapping, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut cur = root.get(parts.next()?)?;
    for part in parts {
        cur = cur.as_mapping()?.get(part)?;
    }
    Some(cur)
}

/// A YAML scalar decoded into a Go `string` field (yaml.v3 accepts numbers/bools).
pub fn go_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => String::new(),
    }
}

fn string_or_empty(value: Option<&Value>, name: &str) -> anyhow::Result<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        // yaml.v3 decodes any scalar into a Go string field as its text.
        Some(v @ (Value::Number(_) | Value::Bool(_))) => Ok(go_string(v)),
        Some(_) => bail!("{name} must be a string"),
    }
}

fn normalize_keys(keys: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(keys.len());
    for key in keys {
        let key = key.trim();
        if !key.is_empty() && !out.iter().any(|k| k == key) {
            out.push(key.to_owned());
        }
    }
    out
}

/// Go's util.ResolveAuthDir: empty means the default, and a leading `~` is the home
/// directory with any following slashes dropped (so `~other/x` is `$HOME/other/x`).
fn resolve_auth_dir(dir: &str) -> PathBuf {
    let dir = if dir.is_empty() { DEFAULT_AUTH_DIR } else { dir };
    let (Some(rest), Some(home)) = (dir.strip_prefix('~'), std::env::var_os("HOME")) else {
        return PathBuf::from(dir);
    };
    let rest = rest.trim_start_matches(['/', '\\']).replace('\\', "/");
    if rest.is_empty() {
        PathBuf::from(home)
    } else {
        PathBuf::from(home).join(rest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from(std::env::var("HOME").unwrap())
    }

    #[test]
    fn v8_wins_by_presence_even_when_null_or_empty() {
        let cfg = Config::parse(
            "port: 9000\napi-keys: [legacy]\nauth-dir: /legacy\nhost: legacyhost\n\
             server: {port: 9100, host: null}\naccess: {api-keys: []}\noauth: {auth-dir: /v8}\n\
             remote-management: {allow-remote: true}\nmanagement: {allow-remote: false, disable-auto-update-panel: true}\n",
        )
        .unwrap();
        assert_eq!(cfg.port, 9100);
        assert_eq!(cfg.host, "", "present null beats legacy");
        assert!(cfg.api_keys.is_empty(), "present empty list beats legacy");
        assert_eq!(cfg.auth_dir, PathBuf::from("/v8"));
        assert!(!cfg.management.allow_remote, "present false beats legacy");
        assert!(cfg.management.disable_auto_update_panel);
        let cfg = Config::parse("api-keys: [legacy]\naccess: {api-keys: null}\n").unwrap();
        assert!(cfg.api_keys.is_empty(), "present null list beats legacy");
    }

    /// `worker-threads` is cliproxy-rs only: a whole-file upload through the Management
    /// API keeps it (Go's v8 upload refuses unknown sections), and its type is checked.
    #[test]
    fn worker_threads_is_a_known_int() {
        let doc = |text: &str| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text).unwrap();
        validate_config_fields(&doc("config-version: 8\nworker-threads: 3\n"), true).unwrap();
        assert!(validate_config_fields(&doc("worker-threads: many\n"), true).is_err());
        assert!(validate_config_fields(&doc("worker-thread: 3\n"), true).is_err());
    }

    #[test]
    fn legacy_spellings_still_work() {
        let cfg = Config::parse("host: 127.0.0.1\nport: 9000\napi-keys: [' a ', a, '', b]\nauth-dir: /x\n").unwrap();
        assert_eq!((cfg.host.as_str(), cfg.port), ("127.0.0.1", 9000));
        assert_eq!(cfg.api_keys, ["a", "b"]);
        assert_eq!(cfg.auth_dir, PathBuf::from("/x"));
    }

    #[test]
    fn null_or_scalar_parents_are_rejected() {
        for (yaml, want) in [
            ("access: null\n", "access must be a mapping"),
            ("access: [a]\n", "access must be a mapping"),
            ("server: 5\n", "server must be a mapping"),
            ("oauth:\n  providers: null\n", "oauth.providers must be a mapping"),
            ("config-version: 7\n", "unsupported config-version (expected 8)"),
            ("- a\n", "config must be a mapping"),
        ] {
            assert_eq!(Config::parse(yaml).unwrap_err().to_string(), want, "{yaml:?}");
        }
        assert!(
            Config::parse("routing: null\nconfig-version: 8\n").is_ok(),
            "routing: null means defaults"
        );
    }

    #[test]
    fn image_generation_mode_accepts_operator_spellings() {
        for (spelling, expected) in [
            ("' Off '", Value::Bool(false)),
            ("~", Value::Bool(false)),
            ("YES", Value::Bool(true)),
            ("' Chat'", Value::from("chat")),
            ("passthrough", Value::from("passthrough")),
        ] {
            let cfg = Config::parse(&format!("multimedia: {{disable-image-generation: {spelling}}}\n")).unwrap();
            assert_eq!(cfg.document["multimedia"]["disable-image-generation"], expected);
            assert_eq!(
                Config::parse(&yaml_string(&cfg.document).unwrap()).unwrap().document,
                cfg.document
            );
        }
        for invalid in ["unknown", "7", "[chat]", "\"\\u00dc\\u0001\""] {
            let error = Config::parse(&format!("multimedia: {{disable-image-generation: {invalid}}}\n")).unwrap_err();
            assert!(error.to_string().contains("disable-image-generation"));
        }
    }

    #[test]
    fn v8_upstream_key_map_is_not_client_keys() {
        let cfg = Config::parse("api-keys:\n  claude:\n    - keys: [{api-key: fake-key}]\n").unwrap();
        assert!(cfg.api_keys.is_empty());
        for yaml in [
            "api-keys: {claude: [{api-key: fake-key}]}",
            "api-keys: {claude: null}",
            "api-keys: {claude: [{keys: null}]}",
            "api-keys: {claude: [{weight: 2, keys: []}]}",
            "api-keys: {claude: [{keys: [{base-url: https://example.invalid}]}]}",
            "api-keys: {claude: [{keys: [{weight: 1000001}]}]}",
        ] {
            assert!(Config::parse(yaml).is_err(), "{yaml}");
        }
        assert!(Config::parse("api-keys: {claude: [{keys: [{weight: 0}]}]}").is_ok());
    }

    #[test]
    fn scheduler_legacy_keys_and_v8_presence_share_one_typed_view() {
        let cfg = Config::parse(
            "request-retry: 7\nmax-retry-credentials: -2\nmax-retry-interval: 41\n\
             disable-cooling: true\nsave-cooldown-status: true\n\
             transient-error-cooldown-seconds: -3\nforce-model-prefix: true\n\
             routing:\n  strategy: fill-first\n  session-affinity: true\n\
             \x20 session-affinity-ttl: 37m\n  session-affinity-subagents: false\n\
             \x20 session-affinity-rebind-on: never\n\
             \x20 retry: {request-retry: 0}\n  cooldown: {disable-cooling: false}\n",
        )
        .unwrap();
        assert_eq!(cfg.routing.strategy, "fill-first");
        assert!(cfg.routing.force_model_prefix && cfg.routing.session_affinity);
        assert_eq!(cfg.routing.session_affinity_ttl, "37m");
        assert_eq!(cfg.routing.session_affinity_subagents, Some(false));
        assert_eq!(cfg.routing.session_affinity_rebind_on, "never");
        assert_eq!(cfg.routing.retry.request_retry, 0);
        assert_eq!(cfg.routing.retry.max_retry_credentials, 0);
        assert_eq!(cfg.routing.retry.max_retry_interval, 41);
        assert!(!cfg.routing.cooldown.disable_cooling);
        assert!(cfg.routing.cooldown.save_cooldown_status);
        assert_eq!(cfg.routing.cooldown.transient_error_cooldown_seconds, -3);
        // cliproxy-rs's own cooldown key: an int or a duration, kept as written, and
        // part of the schema so management writes do not archive it.
        for (yaml, expect) in [("0", "0"), ("90m", "90m")] {
            let text = format!("routing:\n  cooldown:\n    max-trusted-cooldown: {yaml}\n");
            let cfg = Config::parse(&text).unwrap();
            assert_eq!(cfg.routing.cooldown.max_trusted_cooldown, expect);
            assert!(ConfigDocument::parse(&text).unwrap().archive_unknown().is_empty());
        }
        assert!(Config::parse("routing: {cooldown: {max-trusted-cooldown: [1]}}").is_err());
        let empty = Config::default();
        assert_eq!(empty.routing, RoutingConfig::default());
        assert_eq!(empty.routing.session_affinity_subagents, None);
        let null = Config::parse("request-retry: 7\nrouting: {retry: {request-retry: null}}\n").unwrap();
        assert_eq!(null.routing.retry.request_retry, 0);
    }

    /// v8.0.35 `buildV8Paths` moves the legacy `claude.session-date-pin-days` to
    /// `upstream.claude`.
    #[test]
    fn legacy_session_date_pin_days_migrates() {
        let doc = ConfigDocument::parse("claude: {tool-search: true, session-date-pin-days: 3}\n").unwrap();
        assert!(doc.value().get("claude").is_none(), "{:?}", doc.value());
        assert_eq!(doc.value()["upstream"]["claude"]["session-date-pin-days"], 3);
    }

    /// Go `shared_upstream_test.go` (52d5507d): the legacy, canonical `upstream` and
    /// historical `oauth.providers` layouts load the same shared settings for OAuth and
    /// API-key views alike, migrate without their aliases, and keep the Codex header
    /// defaults OAuth-only.
    #[test]
    fn shared_upstream_layouts_agree_and_migrate() {
        let keys = "api-keys: {codex: [{base-url: https://example.invalid, keys: [{api-key: test-key, disable-codex-cloaking: false}]}]}\n";
        let codex = "{disable-codex-cloaking: true, stream-bootstrap-buffering: true, stream-bootstrap-timeout: 10s, orphan-delegation-compatibility: true, model-level-cooling: true, response-steering: true";
        let headers = "{user-agent: client-agent, package-version: 1.2.3, runtime-version: v22.1.0, os: Linux, arch: arm64, timeout: '123', timezone: Asia/Shanghai, stabilize-device-profile: true}";
        let oauth = "oauth: {auth-dir: client-auth, auth-auto-refresh-workers: 3, providers: {codex: {header-defaults: {beta-features: oauth-beta}}}}\n";
        let legacy = format!(
            "auth-dir: client-auth\nauth-auto-refresh-workers: 3\ncodex: {codex}}}\nclaude: {{model-level-cooling: true}}\nclaude-code: {{disable-cloaking-model-list: true}}\ndisable-claude-cloak-mode: true\nclaude-header-defaults: {headers}\nxai: {{inject-x-search: true}}\n{oauth}{keys}"
        );
        let canonical = format!(
            "upstream:\n  codex: {codex}}}\n  claude:\n    model-level-cooling: true\n    disable-cloaking-model-list: true\n    disable-claude-cloak-mode: true\n    header-defaults: {headers}\n  xai: {{inject-x-search: true}}\n{oauth}{keys}"
        );
        let historical = format!(
            "oauth:\n  auth-dir: client-auth\n  auth-auto-refresh-workers: 3\n  providers:\n    codex: {codex}, header-defaults: {{beta-features: oauth-beta}}}}\n    claude:\n      model-level-cooling: true\n      claude-code: {{disable-cloaking-model-list: true}}\n      disable-claude-cloak-mode: true\n      header-defaults: {headers}\n    xai: {{inject-x-search: true}}\n{keys}"
        );
        let expected = Config::parse(&canonical).unwrap().document;
        assert_eq!(
            expected["upstream"]["claude"]["header-defaults"]["timezone"],
            "Asia/Shanghai"
        );
        for raw in [&legacy, &canonical, &historical] {
            let doc = ConfigDocument::parse(raw).unwrap();
            schema::validate(doc.value(), true).unwrap();
            for alias in ["codex", "claude", "claude-code", "xai", "claude-header-defaults"] {
                assert!(doc.value().get(alias).is_none(), "{alias} survived: {raw}");
            }
            let providers = &doc.value()["oauth"]["providers"];
            assert!(
                providers.get("claude").is_none() && providers.get("xai").is_none(),
                "{raw}"
            );
            let codex = providers["codex"].as_mapping().unwrap();
            assert_eq!(
                codex.keys().collect::<Vec<_>>(),
                [&Value::from("header-defaults")],
                "{raw}"
            );
            for text in [raw.to_string(), doc.yaml().unwrap()] {
                let cfg = Config::parse(&text).unwrap();
                for view in [&cfg, &*cfg.for_api_key()] {
                    assert_eq!(view.document["upstream"], expected["upstream"], "{text}");
                    assert_eq!(view.document["oauth"]["auth-auto-refresh-workers"], 3);
                    let key = &view.document["api-keys"]["codex"][0]["keys"][0];
                    assert_eq!(key["disable-codex-cloaking"], Value::Bool(false));
                }
                // The Codex user-agent default is refused since #26 (validate.rs); the
                // remaining header default shows the OAuth-only view.
                let beta =
                    |c: &Config| c.document["oauth"]["providers"]["codex"]["header-defaults"]["beta-features"].clone();
                assert_eq!(beta(&cfg), "oauth-beta");
                assert_eq!(beta(&cfg.for_api_key()), "");
            }
        }
    }

    /// Go `TestSharedUpstreamPresenceWinsOverHistoricalAliases` and
    /// `TestSharedUpstreamEmptyHistoricalContainers`.
    #[test]
    fn shared_upstream_presence_wins_and_empty_containers_move() {
        for value in ["false", "null"] {
            let raw = format!(
                "codex: {{response-steering: true}}\noauth: {{providers: {{codex: {{response-steering: true}}, claude: {{header-defaults: {{user-agent: historical-agent, stabilize-device-profile: true}}}}}}}}\nupstream: {{codex: {{response-steering: {value}}}, claude: {{header-defaults: {{user-agent: '', stabilize-device-profile: {value}}}}}}}\n"
            );
            let cfg = Config::parse(&raw).unwrap();
            let upstream = &cfg.document["upstream"];
            assert_ne!(upstream["codex"]["response-steering"], Value::Bool(true), "{raw}");
            assert_eq!(upstream["claude"]["header-defaults"]["user-agent"], "", "{raw}");
            assert_ne!(
                upstream["claude"]["header-defaults"]["stabilize-device-profile"],
                Value::Bool(true)
            );
            schema::validate(ConfigDocument::parse(&raw).unwrap().value(), true).unwrap();
        }
        for (historical, current) in [
            (
                "oauth.providers.claude.header-defaults",
                "upstream.claude.header-defaults",
            ),
            ("oauth.providers.claude.claude-code", "upstream.claude"),
            ("oauth.providers.claude", "upstream.claude"),
            ("oauth.providers.xai", "upstream.xai"),
        ] {
            for value in ["{}", "null"] {
                let parts: Vec<&str> = historical.split('.').collect();
                let raw = parts
                    .iter()
                    .rev()
                    .fold(value.to_owned(), |inner, key| format!("{{{key}: {inner}}}"));
                let doc = ConfigDocument::parse(&raw).unwrap();
                schema::validate(doc.value(), true).unwrap();
                assert!(lookup(doc.value().as_mapping().unwrap(), historical).is_none(), "{raw}");
                assert!(lookup(doc.value().as_mapping().unwrap(), current).is_some(), "{raw}");
            }
        }
        // The legacy Claude structs move by their declared fields only: Go has no
        // `claude.disable-cloaking-model-list` or `claude-code.model-level-cooling`.
        let cfg = Config::parse(
            "claude: {disable-cloaking-model-list: true, model-level-cooling: true}\nclaude-code: {disable-cloaking-model-list: false, model-level-cooling: true}\n",
        )
        .unwrap();
        let claude = &cfg.document["upstream"]["claude"];
        assert_eq!(claude["disable-cloaking-model-list"], Value::Bool(false));
        assert_eq!(claude["model-level-cooling"], Value::Bool(true));
        assert_eq!(cfg.document["claude"]["disable-cloaking-model-list"], Value::Bool(true));
        assert_eq!(cfg.document["claude-code"]["model-level-cooling"], Value::Bool(true));
        let cfg = Config::parse("claude-code: {model-level-cooling: true}\n").unwrap();
        assert!(cfg.document.get("upstream").is_none(), "{:?}", cfg.document);
        for legacy in ["claude: null\n", "claude-code: {}\n"] {
            let doc = ConfigDocument::parse(legacy).unwrap();
            assert_eq!(
                doc.value()["upstream"]["claude"],
                Value::Mapping(Mapping::new()),
                "{legacy}"
            );
            assert!(doc.value().get("claude").is_none() && doc.value().get("claude-code").is_none());
        }
        let err = Config::parse("oauth: {providers: {xai: true}}\n").unwrap_err();
        assert_eq!(err.to_string(), "oauth.providers.xai must be a mapping");
    }

    /// #71: `upstream.claude.server-side-fallback` loads from its canonical, historical
    /// `oauth.providers.claude` and flat `claude` spellings with the shared-setting
    /// precedence: the canonical path wins by presence, then the historical OAuth path,
    /// then the flat one, so an explicit `false` there beats a `true` further down. The
    /// alias migrates without being archived, and a saved document reloads the same.
    #[test]
    fn server_side_fallback_aliases_follow_shared_precedence() {
        let fallback = |raw: &str| {
            let cfg = Config::parse(raw).unwrap();
            cfg.document["upstream"]["claude"]["server-side-fallback"].clone()
        };
        let oauth = |v: &str| {
            format!(
                "oauth: {{providers: {{claude: {{server-side-fallback: {v}}}}}}}
"
            )
        };
        let flat = |v: &str| {
            format!(
                "claude: {{server-side-fallback: {v}}}
"
            )
        };
        let canonical = |v: &str| {
            format!(
                "upstream: {{claude: {{server-side-fallback: {v}}}}}
"
            )
        };
        for (raw, want) in [
            (String::new(), Value::Null),
            (oauth("true"), Value::Bool(true)),
            (flat("true"), Value::Bool(true)),
            (canonical("true"), Value::Bool(true)),
            (format!("{}{}", flat("true"), oauth("false")), Value::Bool(false)),
            (format!("{}{}", oauth("true"), flat("false")), Value::Bool(true)),
            (format!("{}{}", oauth("true"), canonical("false")), Value::Bool(false)),
            (
                format!("{}{}{}", flat("true"), oauth("true"), canonical("false")),
                Value::Bool(false),
            ),
            (format!("{}{}", oauth("false"), canonical("true")), Value::Bool(true)),
        ] {
            assert_eq!(fallback(&raw), want, "{raw}");
            let mut doc = ConfigDocument::parse(&raw).unwrap();
            assert!(doc.archive_unknown().is_empty(), "nothing archived: {raw}");
            schema::validate(doc.value(), true).unwrap();
            let providers = &doc.value()["oauth"]["providers"];
            assert!(providers.get("claude").is_none(), "alias migrated: {raw}");
            assert!(doc.value().get("claude").is_none(), "flat alias migrated: {raw}");
            // Saved and reloaded, the canonical form keeps the value.
            assert_eq!(fallback(&doc.yaml().unwrap()), want, "reload: {raw}");
        }
        // A Management API write that names the historical path lands on the canonical
        // one, which keeps precedence when both are sent.
        for (body, want) in [
            (
                "oauth: {providers: {claude: {server-side-fallback: true}}}",
                Value::Bool(true),
            ),
            (
                "oauth: {providers: {claude: {server-side-fallback: true}}}
upstream: {claude: {server-side-fallback: false}}",
                Value::Bool(false),
            ),
        ] {
            let mut value: Value = serde_yaml_ng::from_str(body).unwrap();
            ConfigDocument::normalize_aliases(&mut value);
            assert_eq!(value["upstream"]["claude"]["server-side-fallback"], want, "{body}");
            assert!(
                lookup(value.as_mapping().unwrap(), "oauth.providers.claude").is_none(),
                "{body}"
            );
        }
        // A read of the historical path sees the canonical value there.
        let mut doc = ConfigDocument::parse(&canonical("true")).unwrap();
        doc.project_aliases(&["oauth", "providers", "claude", "server-side-fallback"]);
        assert_eq!(
            doc.value()["oauth"]["providers"]["claude"]["server-side-fallback"],
            Value::Bool(true)
        );
    }

    /// Go `TestIsV8ConfigLayout` (3be5fa44).
    #[test]
    fn v8_layout_is_recognised_by_version_sections_and_paths() {
        for (raw, v8) in [
            ("request-retry: 3\ncodex: {response-steering: true}\n", false),
            (
                "routing: {strategy: fill-first}\nplugins: {configs: {example: {enabled: true}}}\nquota-exceeded: {switch-project: true}\nclient: {codex: {enable-apply-patch: true}}\napi-keys: [client-key]\n",
                false,
            ),
            ("codex: {optimize-multi-agent-v2: true}\n", false),
            ("config-version: 8\nrequest-retry: 3\n", true),
            ("oauth: {providers: {codex: {response-steering: true}}}\n", true),
            ("server: {}\n", true),
            ("upstream: {xai: {inject-x-search: false}}\n", true),
            ("api-keys: {}\n", true),
            ("request-retry: 3\nrouting: {retry: {max-retry-credentials: 2}}\n", true),
            ("routing: {retry: {}}\n", true),
            ("providers: {codex: {optimize-multi-agent-v2: true}}\n", true),
            ("client: {codex: {optimize-multi-agent-v2: false}}\n", true),
        ] {
            let value: Value = serde_yaml_ng::from_str(raw).unwrap();
            assert_eq!(is_v8_layout(&value), v8, "{raw}");
        }
    }

    #[test]
    fn empty_config_uses_documented_defaults() {
        let cfg = Config::parse("").unwrap();
        assert_eq!((cfg.port, cfg.host.as_str()), (0, ""));
        assert_eq!(cfg.auth_dir, home().join(".cli-proxy-api"));
        assert_eq!(
            Config::parse("auth-dir: ''\n").unwrap().auth_dir,
            home().join(".cli-proxy-api")
        );
        assert_eq!(resolve_auth_dir("~"), home());
        assert_eq!(resolve_auth_dir("~other/x"), home().join("other/x"));
        assert_eq!(resolve_auth_dir("/abs"), PathBuf::from("/abs"));
        let saved = yaml_string(&cfg.document).unwrap();
        let reloaded = Config::parse(&saved).unwrap();
        assert_eq!(
            (reloaded.host, reloaded.port, reloaded.auth_dir),
            (cfg.host, cfg.port, cfg.auth_dir)
        );
    }

    /// A model name that looks like a number stays the literal string, and plain numbers
    /// that are no model name, alias or exclusion keep their numeric values, on a small
    /// payload rule and on one with many params.
    #[test]
    fn yaml_literals_keep_names_and_numbers() {
        let check = |n: usize| {
            let params: Vec<String> = (0..n).map(|i| format!("k{i}: 1.50")).collect();
            let text = format!(
                "x: &a 1\nrequests: {{payload: {{override: [{{models: [{{name: 0x1F}}], params: {{{}}}}}]}}}}\n",
                params.join(", ")
            );
            let doc = config_value(&text).unwrap();
            let rule = &doc["requests"]["payload"]["override"][0];
            assert_eq!(rule["models"][0]["name"], Value::from("0x1F"));
            assert_eq!(rule["params"]["k0"], Value::from(1.5));
            assert_eq!(rule["params"][format!("k{}", n - 1).as_str()], Value::from(1.5));
        };
        check(1000);
        check(16000);
    }

    /// The aliases of an anchored list of numbers count toward the alias limit only where
    /// they land in a model name, alias or exclusion. (yaml.v3 refuses this many aliases
    /// as excessive aliasing; the limit here is the copies they make.)
    #[test]
    fn unused_number_aliases_do_not_expand_too_far() {
        let numbers: Vec<String> = (0..1024).map(|i| i.to_string()).collect();
        let text = format!(
            "a: &l [{}]\nb: [{}]\noauth-excluded-models: {{c: *l}}\n",
            numbers.join(", "),
            vec!["*l"; 1024].join(", ")
        );
        let doc = config_value(&text).unwrap();
        assert_eq!(doc["b"][1023][1023], Value::from(1023));
        assert_eq!(doc["oauth-excluded-models"]["c"][7], Value::from("7"));
    }

    /// Aliases whose copies no string setting keeps are bounded too: 2,048 aliases of a
    /// 2,049-number anchor outside any string setting visit just over the limit and are
    /// refused, where each would otherwise copy the whole anchor. Go refuses such a file
    /// as excessive aliasing.
    #[test]
    fn dropped_alias_copies_are_bounded() {
        let numbers = vec!["0"; 2049].join(", ");
        let text = format!("x: &x [{numbers}]\nzz-ignored: [{}]\n", vec!["*x"; 2048].join(", "));
        let err = config_value(&text).unwrap_err();
        assert_eq!(err.to_string(), "config YAML aliases expand too far");
    }

    /// Only alias copies count toward the alias limit: a list of more numbers than that,
    /// anchored (so kept for its aliases) or an exclusion list, loads as in Go.
    #[test]
    fn written_numbers_do_not_count_toward_the_alias_limit() {
        let numbers = vec!["0"; (1 << 20) + 1].join(", ");
        let doc = config_value(&format!(
            "x: &l [{numbers}]
oauth-excluded-models: {{c: [{numbers}]}}
"
        ))
        .unwrap();
        assert_eq!(doc["x"][1 << 20], Value::from(0));
        assert_eq!(doc["oauth-excluded-models"]["c"][1 << 20], Value::from("0"));
    }

    /// String settings preserve numeric text; bounded integer settings refuse overflow.
    #[test]
    fn oversized_numbers_keep_text_at_string_boundaries() {
        // Tokens and model IDs are opaque strings, even when they exceed integer limits.
        for token in [
            "123456789012345678901234567890",
            "18446744073709551616",
            "-9223372036854775809",
            "0x1FFFFFFFFFFFFFFFFF",
        ] {
            let cfg = Config::parse(&format!("server: {{github-token: {token}}}\n")).unwrap();
            assert_eq!(cfg.document["server"]["github-token"].as_str(), Some(token));
            assert_eq!(
                body_value(token, &["server", "github-token"]).unwrap(),
                Value::from(token)
            );
            let saved = yaml_string(&cfg.document).unwrap();
            assert_eq!(Config::parse(&saved).unwrap().document, cfg.document);
        }
        assert!(Config::parse("routing: {retry: {request-retry: 18446744073709551616}}\n").is_err());
    }

    /// Accepted numeric configuration spellings retain their type and string spelling after save and reload.
    #[test]
    fn numeric_config_values_have_explicit_types_and_round_trip() {
        // Preserve supported operator spellings, without pinning incidental fallback
        // results for malformed radix prefixes, trailing underscores, or double signs.
        for (text, value) in [("1_000", 1000), ("0x1F", 31), ("007", 7), ("0b10", 2), ("0o17", 15)] {
            let cfg = Config::parse(&format!(
                "server: {{github-token: {text}}}\nrouting: {{retry: {{request-retry: {text}}}}}\n"
            ))
            .unwrap();
            assert_eq!(cfg.document["server"]["github-token"].as_str(), Some(text));
            assert_eq!(cfg.routing.retry.request_retry, value);
            let reloaded = Config::parse(&yaml_string(&cfg.document).unwrap()).unwrap();
            assert_eq!(reloaded.document, cfg.document);
            assert_eq!(reloaded.routing.retry.request_retry, value);
        }
        let cfg = Config::parse("oauth-excluded-models: {1_0: [a], 007: [b]}\n").unwrap();
        assert!(cfg.document["oauth"]["excluded-models"].get("1_0").is_some());
        assert!(cfg.document["oauth"]["excluded-models"].get("007").is_some());
    }

    /// String settings retain date-like identifiers; timestamp values retain their represented instant.
    #[test]
    fn timestamps_preserve_string_settings_and_serialized_instants() {
        // Dates in typed string settings are identifiers, not dates to normalize.
        for text in [
            "2001-12-14",
            "2001-12-14T21:59:43.10-05:00",
            "2001-02-29",
            "2001-12-14T24:00:00Z",
        ] {
            let cfg = Config::parse(&format!("server: {{github-token: {text}}}\n")).unwrap();
            assert_eq!(cfg.document["server"]["github-token"].as_str(), Some(text));
            assert_eq!(
                Config::parse(&yaml_string(&cfg.document).unwrap()).unwrap().document,
                cfg.document
            );
        }
        // YAML timestamp values carry the instant when exposed through a JSON view.
        // These are YAML timestamp examples, not a language runtime's permissive parser table.
        for (text, expected) in [
            ("2001-12-14", "2001-12-14T00:00:00Z"),
            ("2001-12-14T21:59:43-05:00", "2001-12-14T21:59:43-05:00"),
        ] {
            let value = yaml_value(&format!("at: &at {text}\ncopy: *at\n")).unwrap();
            let json = serde_json::to_value(AsJson(&value)).unwrap();
            assert_eq!(json, serde_json::json!({"at": expected, "copy": expected}));
            assert_eq!(yaml_value(&yaml_string(&value).unwrap()).unwrap(), value);
        }
    }

    /// yaml.v3 skips a leading BOM; serde_yaml_ng and libyaml read it as part of the
    /// first line, so a second top-level key failed the load.
    #[test]
    fn a_leading_bom_is_skipped() {
        let cfg = Config::parse(
            "\u{feff}server: {github-token: !!binary dG9r}\nrouting: {retry: {request-retry: 3}}\n\
             x: [18446744073709551616, 007]\n",
        )
        .unwrap();
        assert_eq!(cfg.document["server"]["github-token"].as_str(), Some("tok"));
        assert_eq!(cfg.routing.retry.request_retry, 3);
        let x = yaml_value("\u{feff}a: b\nx: [18446744073709551616, 007]\n").unwrap();
        assert_eq!(x["x"], yaml_value("[1.8446744073709552e19, 7]").unwrap());
        // Go's save (SaveConfigPreserveComments, recorded at ae3ee3be) drops the BOM and
        // keeps the head comment, in the v8 layout and when it migrates a legacy one.
        for original in [
            "\u{feff}# head\nserver:\n  port: 1\n",
            "\u{feff}# head\nport: 1\nhost: x\n",
        ] {
            let mut doc = ConfigDocument::parse(original).unwrap();
            doc.update(&["server", "port"], Value::from(2), false).unwrap();
            let basis = doc.migrated_text(original).unwrap_or_else(|| original.to_owned());
            let saved = doc.render_preserving(&basis).unwrap();
            assert!(saved.starts_with("# head\n"), "{saved:?}");
            assert_eq!(Config::parse(&saved).unwrap().port, 2, "{saved:?}");
        }
        // The plaintext management key is hashed in place, and through a merge.
        let hash = "$2a$10$abcdefghijklmnopqrstuuvwxyzabcdefghijklmnopqrstuvwxy";
        for text in [
            "\u{feff}management:\n  secret-key: plain\n",
            "\u{feff}b: &b {secret-key: plain}\nmanagement: {<<: *b}\n",
        ] {
            let saved = with_secret_hash(text, hash).unwrap();
            assert!(!saved.starts_with('\u{feff}') && saved.contains(hash), "{saved:?}");
        }
    }

    /// A string that would read back as a number when written plain is double-quoted
    /// where a write re-emits it, as yaml.v3 quotes it, so it stays that string.
    #[test]
    fn re_emitted_strings_read_back_as_strings() {
        let strings = [
            "1_000",
            "0_7",
            "0_8",
            "1_0.5",
            "0x_1F",
            ".5_0",
            "007",
            "0b-1",
            "0o+17",
            "0X1F",
            "0O17",
            "+9223372036854775808",
            "18446744073709551616",
            "-18446744073709551616",
            "2001-12-14",
            "2001-12-14 21:59:43.10",
        ];
        let value = Value::Sequence(strings.iter().map(|s| Value::from(*s)).collect());
        let text = yaml_string(&value).unwrap();
        assert_eq!(parse_yaml(&text).unwrap(), value, "{text}");
        // String keys too, which Go would otherwise read as numbers.
        let keys = yaml_value("{'1_000': a, '007': b, 7: c}").unwrap();
        let text = yaml_string(&keys).unwrap();
        assert_eq!(text, "\"1_000\": a\n'007': b\n7: c\n");
        assert_eq!(parse_yaml(&text).unwrap(), keys, "{text}");
        assert_eq!(
            yaml_string(&yaml_value("[7, 1000, 0.5]").unwrap()).unwrap(),
            "- 7\n- 1000\n- 0.5\n"
        );
        // The secret-key fallback re-emits the whole file.
        let text = "base: &b {secret-key: plain}\nmanagement: {<<: *b}\n\
                    requests: {payload: {default: [{models: [{name: m}], params: {a: '1_000', b: '007'}}]}}\n";
        let saved = with_secret_hash(text, "$2a$10$abcdefghijklmnopqrstuuvwxyzabcdefghijklmnopqrstuvwxy").unwrap();
        let params = &parse_yaml(&saved).unwrap()["requests"]["payload"]["default"][0]["params"];
        assert_eq!(params["a"], Value::from("1_000"), "{saved}");
        assert_eq!(params["b"], Value::from("007"), "{saved}");
    }

    /// Opaque text survives typed config boundaries in both supported layouts and after serialization.
    #[test]
    fn scalar_settings_keep_their_yaml_text() {
        // Exercise the supported boundaries directly. The source text is the expected
        // value; there is no recorded decoder output to consult.
        for text in ["1.50", "0x1F", "007", "true", "2001-12-14", "1_000"] {
            for yaml in [
                format!(
                    "server: {{host: {text}, github-token: {text}, discovery: {{service-name: {text}, subtypes: [{text}]}}}}\naccess: {{api-keys: [{text}]}}\noauth: {{auth-dir: {text}}}\nmanagement: {{panel-github-repository: {text}}}\n"
                ),
                format!(
                    "host: {text}\ngithub-token: {text}\ndiscovery: {{service-name: {text}, subtypes: [{text}]}}\napi-keys: [{text}]\nauth-dir: {text}\nremote-management: {{panel-github-repository: {text}}}\n"
                ),
            ] {
                let cfg = Config::parse(&yaml).unwrap();
                assert_eq!(cfg.host, text);
                assert_eq!(cfg.api_keys, [text]);
                for path in [
                    &["server", "github-token"][..],
                    &["server", "discovery", "service-name"][..],
                    &["oauth", "auth-dir"][..],
                    &["management", "panel-github-repository"][..],
                ] {
                    let node = path.iter().fold(&cfg.document, |node, key| &node[*key]);
                    assert_eq!(node.as_str(), Some(text), "{path:?}: {yaml}");
                }
                assert_eq!(cfg.document["server"]["discovery"]["subtypes"][0].as_str(), Some(text));
                let saved = yaml_string(&cfg.document).unwrap();
                assert_eq!(Config::parse(&saved).unwrap().document, cfg.document);
            }
        }
        // Payload selector fields are strings, but arbitrary params retain their JSON types.
        let cfg = Config::parse("requests: {payload: {override: [{models: [{name: 7, protocol: 1.50}], params: {value: 1.50, enabled: true}}]}}\n").unwrap();
        let rule = &cfg.document["requests"]["payload"]["override"][0];
        assert_eq!(rule["models"][0]["name"], Value::from("7"));
        assert_eq!(rule["models"][0]["protocol"], Value::from("1.50"));
        assert_eq!(rule["params"]["value"], Value::from(1.5));
        assert_eq!(rule["params"]["enabled"], Value::from(true));
    }

    /// Native plugins are removed: any `plugins` section, enabled or not, empty, null or
    /// malformed, in either layout, fails the load with the migration error before any
    /// other check, on file loads and on management writes alike.
    #[test]
    fn a_plugins_section_is_refused_with_the_migration_error() {
        for text in [
            "plugins: {enabled: true, dir: plugins}\n",
            "plugins: {enabled: false}\n",
            "plugins: {}\n",
            "plugins:\n",
            "plugins: [x]\n",
            "config-version: 8\nplugins: {configs: {acme: {enabled: true, store: {version: v1}}}}\n",
            "request-retry: 3\nplugins: {store-sources: [https://example.invalid/index.json]}\n",
        ] {
            let err = Config::parse(text).unwrap_err();
            assert_eq!(err.to_string(), PLUGINS_REMOVED, "{text}");
            let err = Config::parse_payload(text).unwrap_err();
            assert!(format!("{err:#}").contains(PLUGINS_REMOVED), "{text}: {err:#}");
        }
        assert!(PLUGINS_REMOVED.contains("delete the plugins section"));
        // The same config without the section loads.
        Config::parse("config-version: 8\nrouting: {strategy: fill-first}\n").unwrap();
    }
}
