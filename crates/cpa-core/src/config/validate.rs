//! Go's custom checks in `ParseConfigBytes` beyond YAML shape: credential in-flight
//! bounds (internal/config/credential_in_flight.go), the Codex Live media relay
//! (internal/config/codex_live.go), and the model catalog sources Go checks while
//! decoding (internal/registry/catalog_config.go). Messages are Go's, except for the
//! Codex identity check, which Go does not have (#26, docs/DIFFERENCES-FROM-GO.md), and
//! the Claude native entrypoint names (#70).

use anyhow::{Result, bail};
use serde_yaml_ng::Value;

use super::go_url;

/// Runs the checks on the canonical v8 document.
pub(super) fn go_custom(doc: &Value) -> Result<()> {
    let at = |path: &[&str]| path.iter().try_fold(doc, |node, part| node.get(*part));
    in_flight(at(&["credentials", "in-flight"]))?;
    live_media_relay(at(&["oauth", "providers", "codex", "live-media-relay"]))?;
    codex_identity(at(&["oauth", "providers", "codex", "header-defaults"]))?;
    claude_native_entrypoints(at(&["upstream", "claude", "header-defaults", "native-entrypoints"]))
}

/// `upstream.claude.header-defaults.native-entrypoints` names Claude Code entrypoints
/// (the label after `external,` in its User-Agent) whose requests pass through as
/// Claude Code's own. A malformed entry could never match a User-Agent, so the list is
/// refused instead of silently ignored: a sequence of names of ASCII letters, digits,
/// `.`, `_` and `-`.
fn claude_native_entrypoints(list: Option<&Value>) -> Result<()> {
    const KEY: &str = "upstream.claude.header-defaults.native-entrypoints";
    let Some(list) = list.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(items) = list.as_sequence() else {
        bail!("{KEY} must be a list of Claude Code entrypoint names");
    };
    for (i, item) in items.iter().enumerate() {
        let name = item.as_str().map(str::trim).unwrap_or_default();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            bail!("{KEY}[{i}] must be a Claude Code entrypoint name: ASCII letters, digits, '.', '_' or '-'");
        }
    }
    Ok(())
}

/// The Codex `header-defaults.user-agent` (legacy `codex-header-defaults`) set the
/// identity of every OAuth Codex request. Codex requests now send the caller's own
/// identity or cliproxy-rs's (#26), so a configured value is refused rather than
/// silently ignored: at startup, on a reload (the running config stays) and on a
/// Management API write. `beta-features` is protocol negotiation and stays.
fn codex_identity(section: Option<&Value>) -> Result<()> {
    if text(section, "user-agent", "").trim().is_empty() {
        return Ok(());
    }
    bail!(
        "oauth.providers.codex.header-defaults.user-agent is no longer supported: Codex requests send \
         the caller's own User-Agent or cliproxy-rs's; remove the setting (see docs/DIFFERENCES-FROM-GO.md)"
    )
}

/// A `codex-api-key` model marked `is-compat` chose Go's compatibility rewrites for a
/// Codex request: the compat Claude translator and dropped output replay. Codex requests
/// are no longer rewritten (#28): the `compatible` profile forwards the native body and
/// the translated route has one strict conversion, so the flag would silently do nothing.
/// It is refused at startup, on a reload (the running config stays) and on a Management
/// API write. Other providers keep `is-compat`.
pub(super) fn codex_compat(cfg: &super::Config) -> Result<()> {
    for entry in super::credentials::api_key_entries(cfg, "codex") {
        for (i, model) in entry.models.iter().enumerate() {
            if model.get("is-compat") == Some(&serde_json::Value::Bool(true)) {
                bail!(
                    "codex-api-key[{}].models[{i}].is-compat is no longer supported: Codex requests are not \
                     rewritten for compatible upstreams; remove the flag (see docs/DIFFERENCES-FROM-GO.md)",
                    entry.index
                );
            }
        }
    }
    Ok(())
}

/// v8.0.35 `CatalogSources.Validate` (internal/registry/catalog_config.go): each model
/// catalog source is empty, an absolute local path (`filepath.IsAbs`) or an http(s) URL
/// with a host. Go walks a map, so with several invalid sources it names any one of
/// them; this names the first in field order.
pub fn catalog_sources(section: Option<&Value>) -> Result<()> {
    for name in ["catalog", "codex-catalog"] {
        let source = text(section, name, "");
        if source.is_empty() || go_is_abs(&source) {
            continue;
        }
        let url = go_url::parse(&source);
        if !url.is_some_and(|u| matches!(u.scheme.as_str(), "http" | "https") && !u.hostname().is_empty()) {
            bail!("models.{name} must be an http(s) URL or an absolute local path");
        }
    }
    Ok(())
}

/// Go `filepath.IsAbs` on Linux: a leading `/`.
pub fn go_is_abs(path: &str) -> bool {
    path.starts_with('/')
}

/// Present non-null value, else Go's pre-decode default.
fn int(section: Option<&Value>, key: &str, default: i64) -> i64 {
    section
        .and_then(|s| s.get(key))
        .filter(|v| !v.is_null())
        .and_then(Value::as_i64)
        .unwrap_or(default)
}

fn text(section: Option<&Value>, key: &str, default: &str) -> String {
    match section.and_then(|s| s.get(key)).filter(|v| !v.is_null()) {
        Some(v) => super::go_string(v),
        None => default.to_owned(),
    }
}

/// `CredentialInFlightConfig.Validate`.
fn in_flight(section: Option<&Value>) -> Result<()> {
    const MAX_PART_COUNT: i64 = 64;
    const MAX_REVISION_BYTES: i64 = 16 * 1024 * 1024;
    const MAX_AGGREGATE_GROUPS: i64 = 100_000;
    const MAX_DETAILS: i64 = 10_000;
    const MAX_STRING_BYTES: i64 = 256;
    let duration = |key: &str, default: &str| parse_duration(&text(section, key, default));
    let snapshot = duration("snapshot-interval", "2s").filter(|d| *d > 0);
    let Some(snapshot) = snapshot else {
        bail!("credential-in-flight.snapshot-interval must be positive");
    };
    if !duration("stale-after", "10s").is_some_and(|stale| stale > 0 && snapshot <= stale / 3) {
        bail!("credential-in-flight.stale-after must be at least three snapshot intervals");
    }
    if !duration("staging-retention", "1m").is_some_and(|d| d > 0) {
        bail!("credential-in-flight.staging-retention must be positive");
    }
    let part_bytes = int(section, "max-part-bytes", 256 * 1024);
    let part_count = int(section, "max-part-count", MAX_PART_COUNT);
    if part_bytes < 1024 || part_count <= 0 || part_count > MAX_PART_COUNT {
        bail!("credential-in-flight part bounds are invalid");
    }
    let revision = int(section, "max-revision-bytes", MAX_REVISION_BYTES);
    if revision < part_bytes || revision > MAX_REVISION_BYTES {
        bail!("credential-in-flight.max-revision-bytes is outside hard bounds");
    }
    if (revision + part_bytes - 1) / part_bytes > part_count {
        bail!("credential-in-flight.max-revision-bytes exceeds part capacity");
    }
    let groups = int(section, "max-aggregate-groups", MAX_AGGREGATE_GROUPS);
    if groups <= 0 || groups > MAX_AGGREGATE_GROUPS {
        bail!("credential-in-flight.max-aggregate-groups is invalid");
    }
    let details = int(section, "max-details", MAX_DETAILS);
    if !(0..=MAX_DETAILS).contains(&details) {
        bail!("credential-in-flight.max-details is invalid");
    }
    let strings = int(section, "max-string-bytes", MAX_STRING_BYTES);
    if strings <= 0 || strings > MAX_STRING_BYTES {
        bail!("credential-in-flight.max-string-bytes is invalid");
    }
    Ok(())
}

/// Checks Go makes while decoding: the Codex Live media relay's UDP ports are Go
/// `uint16` fields, so decoding rejects out-of-range values whether or not the relay is
/// enabled, and `Config.UnmarshalYAML` validates the model catalog sources.
pub(super) fn decoded(doc: &Value) -> Result<()> {
    catalog_sources(doc.get("models"))?;
    let section = ["oauth", "providers", "codex", "live-media-relay"]
        .iter()
        .try_fold(doc, |node, part| node.get(*part));
    for key in ["udp-port-min", "udp-port-max"] {
        if !(0..=65_535).contains(&int(section, key, 0)) {
            bail!("codex.live-media-relay.{key} must be between 0 and 65535");
        }
    }
    Ok(())
}

/// `CodexLiveMediaRelayConfig.Validate`; only an enabled relay is checked.
fn live_media_relay(section: Option<&Value>) -> Result<()> {
    if !section
        .and_then(|s| s.get("enabled"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(());
    }
    let max_sessions = int(section, "max-sessions", 0);
    if max_sessions < 0 {
        bail!("codex.live-media-relay.max-sessions must not be negative");
    }
    let public_ip = text(section, "public-ip", "");
    let public_ip = public_ip.trim();
    if !public_ip.is_empty() && public_ip.parse::<std::net::IpAddr>().is_err() {
        bail!("codex.live-media-relay.public-ip is invalid: {public_ip:?}");
    }
    let (min, max) = (int(section, "udp-port-min", 0), int(section, "udp-port-max", 0));
    if (min == 0) != (max == 0) {
        bail!("codex.live-media-relay UDP port minimum and maximum must both be set");
    }
    if min > max {
        bail!("codex.live-media-relay.udp-port-min must not exceed udp-port-max");
    }
    if min != 0 {
        let sessions = if max_sessions > 0 { max_sessions } else { 32 };
        // Go int arithmetic wraps.
        let required = sessions.wrapping_mul(2);
        if max - min + 1 < required {
            bail!("codex.live-media-relay UDP range requires at least {required} ports for {sessions} sessions");
        }
    }
    let servers = section
        .and_then(|s| s.get("ice-servers"))
        .and_then(Value::as_sequence)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for (i, server) in servers.iter().enumerate() {
        let urls = server
            .get("urls")
            .and_then(Value::as_sequence)
            .map(Vec::as_slice)
            .unwrap_or_default();
        if urls.is_empty() {
            bail!("codex.live-media-relay.ice-servers[{i}].urls is required");
        }
        for raw in urls {
            let parsed = go_url::parse(super::go_string(raw).trim()).filter(|u| !u.scheme.is_empty());
            let Some(parsed) = parsed else {
                bail!("codex.live-media-relay.ice-servers[{i}] contains an invalid URL");
            };
            if !matches!(parsed.scheme.as_str(), "stun" | "stuns" | "turn" | "turns") {
                bail!(
                    "codex.live-media-relay.ice-servers[{i}] uses unsupported scheme {:?}",
                    parsed.scheme
                );
            }
        }
    }
    Ok(())
}

/// Go `time.ParseDuration` in nanoseconds; `None` where Go returns an error.
pub fn parse_duration(s: &str) -> Option<i64> {
    const LIMIT: u64 = 1 << 63;
    let (neg, mut rest) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if rest == "0" {
        return Some(0);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        if !rest.starts_with(|c: char| c == '.' || c.is_ascii_digit()) {
            return None;
        }
        // Go `leadingInt`: overflow past 1<<63 is an error.
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let mut whole: u64 = 0;
        for d in rest[..int_len].bytes() {
            if whole > LIMIT / 10 {
                return None;
            }
            whole = whole * 10 + u64::from(d - b'0');
            if whole > LIMIT {
                return None;
            }
        }
        rest = &rest[int_len..];
        // Go `leadingFraction`: digits past the overflow point are consumed, ignored.
        let (mut frac, mut scale, mut has_frac) = (0u64, 1f64, false);
        if let Some(after) = rest.strip_prefix('.') {
            let n = after.bytes().take_while(u8::is_ascii_digit).count();
            has_frac = n > 0;
            let mut overflow = false;
            for d in after[..n].bytes() {
                if overflow || frac > (LIMIT - 1) / 10 {
                    overflow = true;
                    continue;
                }
                let next = frac * 10 + u64::from(d - b'0');
                if next > LIMIT {
                    overflow = true;
                    continue;
                }
                frac = next;
                scale *= 10.0;
            }
            rest = &after[n..];
        }
        if int_len == 0 && !has_frac {
            return None;
        }
        let unit_len = rest.bytes().take_while(|b| *b != b'.' && !b.is_ascii_digit()).count();
        let (unit, after) = rest.split_at(unit_len);
        let unit: u64 = match unit {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        if whole > LIMIT / unit {
            return None;
        }
        let mut value = whole * unit;
        if frac > 0 {
            value += (frac as f64 * (unit as f64 / scale)) as u64;
            if value > LIMIT {
                return None;
            }
        }
        total = total.checked_add(value).filter(|t| *t <= LIMIT)?;
        rest = after;
    }
    if neg {
        return Some((total as i64).wrapping_neg());
    }
    i64::try_from(total).ok()
}

#[cfg(test)]
mod tests {
    use super::{Value, go_is_abs, in_flight, parse_duration};

    /// #26: a configured Codex `header-defaults.user-agent` is refused in the v8 layout
    /// and through each legacy spelling that migrates to it; an empty one and
    /// `beta-features` stay valid. The message names the key, never its value.
    #[test]
    fn codex_user_agent_default_is_refused() {
        let refused = [
            "oauth: {providers: {codex: {header-defaults: {user-agent: 'codex-tui/0.154.0 (Mac OS 26.5.2; arm64)'}}}}\n",
            "codex-header-defaults: {user-agent: legacy-agent}\n",
            "codex: {header-defaults: {user-agent: flat-agent}}\n",
        ];
        for yaml in refused {
            let error = crate::config::Config::parse(yaml).expect_err(yaml).to_string();
            assert!(
                error.contains("oauth.providers.codex.header-defaults.user-agent is no longer supported"),
                "{error}"
            );
            assert!(!error.contains("Mac OS") && !error.contains("legacy-agent"), "{error}");
            assert!(
                crate::config::Config::parse_payload(yaml).is_err(),
                "Management API writes too"
            );
        }
        for yaml in [
            "oauth: {providers: {codex: {header-defaults: {user-agent: '', beta-features: b1}}}}\n",
            "oauth: {providers: {codex: {header-defaults: {beta-features: b1}}}}\n",
            "upstream: {claude: {header-defaults: {user-agent: claude-cli/2.0}}}\n",
        ] {
            assert!(crate::config::Config::parse(yaml).is_ok(), "{yaml}");
        }
    }

    /// #70: Claude `native-entrypoints` is a list of entrypoint names in every spelling
    /// of the Claude header defaults; anything else is refused, at load and on a
    /// Management API write.
    #[test]
    fn claude_native_entrypoints_are_names() {
        for yaml in [
            "upstream: {claude: {header-defaults: {native-entrypoints: [t3-code, My_App.2]}}}\n",
            "claude-header-defaults: {native-entrypoints: []}\n",
            "upstream: {claude: {header-defaults: {native-entrypoints: null}}}\n",
            // A number decodes into the string field, as in Go's yaml.v3.
            "upstream: {claude: {header-defaults: {native-entrypoints: [7]}}}\n",
        ] {
            assert!(crate::config::Config::parse(yaml).is_ok(), "{yaml}");
        }
        for (yaml, want) in [
            (
                "upstream: {claude: {header-defaults: {native-entrypoints: t3-code}}}\n",
                "must be a list",
            ),
            (
                "upstream: {claude: {header-defaults: {native-entrypoints: ['']}}}\n",
                "native-entrypoints[0]",
            ),
            (
                "upstream: {claude: {header-defaults: {native-entrypoints: [ok, 'a, b']}}}\n",
                "native-entrypoints[1]",
            ),
            (
                "upstream: {claude: {header-defaults: {native-entrypoints: ['cli)']}}}\n",
                "native-entrypoints[0]",
            ),
            (
                "claude-header-defaults: {native-entrypoints: ['agent-sdk/1']}\n",
                "native-entrypoints[0]",
            ),
            (
                "oauth: {providers: {claude: {header-defaults: {native-entrypoints: [' ']}}}}\n",
                "native-entrypoints[0]",
            ),
        ] {
            let error = crate::config::Config::parse(yaml).expect_err(yaml).to_string();
            assert!(error.contains(want), "{yaml}: {error}");
            assert!(
                crate::config::Config::parse_payload(yaml).is_err(),
                "{yaml}: Management API writes too"
            );
        }
    }

    /// #28: a `codex-api-key` model marked `is-compat` is refused, in the legacy and the
    /// v8 layout, at load and on a Management API write (`parse_payload`); `false`, a
    /// Codex model without the flag and every other provider's `is-compat` stay valid.
    #[test]
    fn codex_is_compat_is_refused() {
        let refused = [
            "codex-api-key: [{api-key: k, base-url: https://x.test, models: [{name: a, alias: b, is-compat: true}]}]\n",
            "config-version: 8\napi-keys:\n  codex:\n    - base-url: https://x.test\n      models: [{name: m}, {name: a, alias: b, is-compat: true}]\n      keys: [{api-key: k}]\n",
        ];
        for yaml in refused {
            let error = crate::config::Config::parse(yaml).expect_err(yaml).to_string();
            assert!(error.contains(".is-compat is no longer supported"), "{error}");
            assert!(error.starts_with("codex-api-key[0].models["), "{error}");
            assert!(
                crate::config::Config::parse_payload(yaml).is_err(),
                "Management API writes too"
            );
        }
        for yaml in [
            "codex-api-key: [{api-key: k, base-url: https://x.test, models: [{name: a, is-compat: false}]}]\n",
            "codex-api-key: [{api-key: k, base-url: https://x.test, models: [{name: a}]}]\n",
            "claude-api-key: [{api-key: k, base-url: https://x.test, models: [{name: a, alias: b, is-compat: true}]}]\n",
            "xai-api-key: [{api-key: k, models: [{name: a, alias: b, is-compat: true}]}]\n",
        ] {
            assert!(crate::config::Config::parse(yaml).is_ok(), "{yaml}");
        }
    }

    /// Go 1.26.4 `TestIsAbs` (path/filepath/path_test.go): `isabstests`.
    #[test]
    fn absolute_paths_follow_go_filepath_is_abs() {
        let unix = [
            ("", false),
            ("/", true),
            ("/usr/bin/gcc", true),
            ("..", false),
            ("/a/../bb", true),
            (".", false),
            ("./", false),
            ("lala", false),
        ];
        for (path, want) in unix {
            assert_eq!(go_is_abs(path), want, "{path:?}");
        }
    }

    /// Go `TestCredentialInFlightConfigDurationBounds` and
    /// `TestCredentialInFlightConfigRejectsUnsafeBounds`.
    #[test]
    fn in_flight_bounds_follow_go() {
        let valid = |yaml: &str| in_flight(Some(&serde_yaml_ng::from_str::<Value>(yaml).unwrap())).is_ok();
        assert!(valid(""), "the defaults");
        assert!(
            valid("snapshot-interval: 1s\nstale-after: 3s\n"),
            "exactly three intervals"
        );
        assert!(!valid("snapshot-interval: 1s\nstale-after: 2999999999ns\n"));
        // time.Duration(math.MaxInt64 / 2).String() and time.Duration(math.MaxInt64).String().
        assert!(!valid(
            "snapshot-interval: 1281023h53m38.427387903s\nstale-after: 2562047h47m16.854775807s\n"
        ));
        assert!(!valid("stale-after: 5s\n"), "below three default intervals");
        assert!(!valid(&format!("max-revision-bytes: {}\n", 16 * 1024 * 1024 + 1)));
        assert!(!valid(&format!("max-part-bytes: {}\n", i64::MAX)));
    }

    /// Values that overflowed before: Go rejects out-of-range uint16 ports at decode
    /// and wraps `max-sessions * 2` (here to i64::MIN, so the range check passes).
    #[test]
    fn relay_arithmetic_never_overflows() {
        use crate::config::Config;
        let relay = |body: &str| {
            Config::parse(&format!(
                "oauth: {{providers: {{codex: {{live-media-relay: {body}}}}}}}\n"
            ))
        };
        assert!(relay("{enabled: true, udp-port-min: -1, udp-port-max: 9223372036854775807}").is_err());
        assert!(relay("{enabled: false, udp-port-min: -64, udp-port-max: -1}").is_err());
        assert!(relay("{enabled: true, udp-port-min: 1, udp-port-max: 64, max-sessions: 4611686018427387904}").is_ok());
        assert!(relay("{enabled: true, udp-port-min: 1, udp-port-max: 64, max-sessions: 33}").is_err());
    }

    #[test]
    fn durations_parse_like_go() {
        assert_eq!(parse_duration("1h1m0.5s"), Some(3_660_500_000_000));
        assert_eq!(parse_duration(".5us"), Some(500));
        assert_eq!(parse_duration("-1.5h"), Some(-5_400_000_000_000));
        assert_eq!(parse_duration("0"), Some(0));
        assert_eq!(parse_duration("-2562047h47m16.854775808s"), Some(i64::MIN));
        assert_eq!(parse_duration("9223372036.854775807s"), Some(i64::MAX));
        for bad in [
            "",
            "2",
            "1d",
            ".s",
            "1h-1m",
            "9999999999h",
            "9223372036.854775808s",
            "340282366920938463463374607431768211455.999999999999999999ns1s",
            "18446744073709551615ns",
        ] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
    }
}
