//! Codex Sign in with ChatGPT (SIWC): the authorization URL, the code exchange and the
//! bound credential file (Go internal/auth/codex/siwc_auth.go, openai.go
//! `CodexSIWCTokenData` and sdk/cliproxy/auth/codex_siwc_bind.go). SIWC is a second
//! token partition (`siwc`) of an existing Codex OAuth credential. Refresh and request
//! use (`use_siwc`) belong to the Codex executor.

use chrono::{Datelike, Local, TimeDelta};
use cpa_core::config::Config;
use cpa_core::exec::{ExecError, FailureScope};
use cpa_core::gostr::GoStr;
use serde_json::{Map, Value};
use url::Url;

use crate::codex_oauth::CodexOAuth;

pub const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
pub const DEFAULT_IDENTIFIER: &str = "CLIProxyAPI";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/api/accounts/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/api/accounts/oauth/token";
pub const RESOURCE: &str = "https://api.openai.com/v1";
/// The fixed loopback redirect. Nothing listens there for a management binding: the
/// browser's final URL is pasted into the v8 callback.
pub const REDIRECT_URI: &str = "http://127.0.0.1:1455/auth/callback";
pub const DIRECT_TOKEN_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const EXPIRY_MARGIN: TimeDelta = TimeDelta::minutes(3);

/// `oauth.providers.codex.signin-with-chatgpt-identifier`, trimmed, else `CLIProxyAPI`
/// (Go `EffectiveSignInWithChatGPTIdentifier`): the agent name the consent screen shows
/// (`agent_name_hint`). Go also builds the Sign in with ChatGPT request `User-Agent`
/// from it; here it never reaches a request header (#26, `crate::codex_identity`).
pub fn identifier(cfg: &Config) -> String {
    ["oauth", "providers", "codex", "signin-with-chatgpt-identifier"]
        .iter()
        .try_fold(&cfg.document, |node, key| node.get(*key))
        // A Go string field: yaml.v3 takes a number or bool as its text.
        .map(cpa_core::config::go_string)
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_IDENTIFIER.to_owned())
}

/// Go `codexUseSIWC`: the `use_siwc` attribute (`strconv.ParseBool`), else the metadata
/// bool or string.
pub fn enabled(credential: &cpa_core::credential::Credential) -> bool {
    let attribute = credential
        .attributes
        .get("use_siwc")
        .and_then(|raw| parse_bool(raw.trim()))
        .unwrap_or(false);
    attribute
        || match credential.metadata.get("use_siwc") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) => parse_bool(s.trim()).unwrap_or(false),
            _ => false,
        }
}

/// Go `codexSIWCAccessToken`: `siwc.access_token`, else `siwc.access`, trimmed.
pub fn access_token(credential: &cpa_core::credential::Credential) -> &str {
    let Some(Value::Object(siwc)) = credential.metadata.get("siwc") else {
        return "";
    };
    ["access_token", "access"]
        .iter()
        .filter_map(|k| siwc.get(*k).and_then(Value::as_str).map(str::trim))
        .find(|t| !t.is_empty())
        .unwrap_or_default()
}

/// Go `ensureCodexSIWCCredentials`: an SIWC credential without its token fails before
/// any request is built. So does one whose grant does not allow ChatGPT plan usage
/// ([`plan_use_denied`]): a 403 that cools the credential, never a 401, so it starts no
/// refresh after an upstream 401 and no sign-in loop (#92).
pub fn require_token(credential: &cpa_core::credential::Credential) -> Result<(), ExecError> {
    if enabled(credential) && access_token(credential).is_empty() {
        return Err(siwc_error(
            401,
            "codex executor: use_siwc is enabled but siwc access_token is missing",
        ));
    }
    if enabled(credential) && plan_use_denied(credential) {
        return Err(siwc_error(
            403,
            "codex executor: the Sign in with ChatGPT grant does not include chatgpt.tokens.use.direct, so ChatGPT plan usage is not enabled for this credential; bind it again and allow ChatGPT plan usage",
        ));
    }
    Ok(())
}

/// Whether the SIWC partition's stored grant explicitly leaves out
/// `chatgpt.tokens.use.direct`: its `scope` (else its `scopes` list) names scopes and
/// that one is not among them. OpenAI's "errors and recovery": such a sign-in is kept,
/// but ChatGPT plan usage is disabled. A partition that stores no scope is not denied:
/// an unknown grant is the one that was bound, and binding requires direct use
/// ([`token_response`]). A refresh that reports no readable scope keeps the stored one.
pub fn plan_use_denied(credential: &cpa_core::credential::Credential) -> bool {
    let Some(siwc) = partition(credential) else {
        return false;
    };
    if let Some(Value::String(scope)) = siwc.get("scope")
        && !scope.trim().is_empty()
    {
        return !scope.split_whitespace().any(|s| s == DIRECT_TOKEN_SCOPE);
    }
    match siwc.get("scopes") {
        Some(Value::Array(scopes)) if !scopes.is_empty() && scopes.iter().all(Value::is_string) => {
            !scopes.iter().any(|s| s.as_str() == Some(DIRECT_TOKEN_SCOPE))
        }
        _ => false,
    }
}

fn partition(credential: &cpa_core::credential::Credential) -> Option<&Map<String, Value>> {
    match credential.metadata.get("siwc") {
        Some(Value::Object(siwc)) if !siwc.is_empty() => Some(siwc),
        _ => None,
    }
}

fn text<'a>(siwc: &'a Map<String, Value>, key: &str) -> &'a str {
    siwc.get(key).and_then(Value::as_str).map(str::trim).unwrap_or_default()
}

type Time = chrono::DateTime<chrono::Utc>;

/// A Go `time.Time` instant as Unix nanoseconds. chrono cannot hold every instant Go
/// reads here (a millisecond Unix value up to `i64::MAX` is some 292 million years out),
/// and a range failure must not read as Go's zero time.
type GoTime = i128;
const NANOS: i128 = 1_000_000_000;
/// Go's zero time, 0001-01-01T00:00:00Z: `IsZero` treats it as unset.
const ZERO: GoTime = -62_135_596_800 * NANOS;

fn go_now(now: Time) -> GoTime {
    i128::from(now.timestamp()) * NANOS + i128::from(now.timestamp_subsec_nanos())
}

/// Go's `int64(f)` for a JSON number (decoded as float64). Go leaves an out-of-range
/// result to the platform: amd64 gives `math.MinInt64`, arm64 saturates as Rust does.
fn go_float_to_int(f: f64) -> i64 {
    if cfg!(target_arch = "x86_64") && !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f) {
        return i64::MIN;
    }
    f as i64
}

/// Go `normaliseUnix`: a non-positive value is the zero time; above 1e12, milliseconds.
fn go_unix(raw: i64) -> GoTime {
    match raw {
        ..=0 => ZERO,
        1_000_000_000_001.. => i128::from(raw) * 1_000_000,
        _ => i128::from(raw) * NANOS,
    }
}

/// Go `time.Parse` for the layouts `parseTimeValue` tries: `2006-01-02T15:04:05Z07:00`
/// (RFC3339; RFC3339Nano accepts the same strings) with `sep` `T` and `zone`, and
/// `2006-01-02 15:04:05` / `2006-01-02 15:04` with `sep` space. Go's grammar, not
/// RFC 3339's: a one- or two-digit hour, a `.` or `,` fraction after the seconds even
/// where the layout has none, no leap second, only an upper-case `Z`, and a space in the
/// layout matching any run of spaces.
fn go_parse(s: &str, sep: u8, seconds: bool, zone: bool) -> Option<GoTime> {
    // Go `getnum`: one or two digits, two when `fixed`.
    fn num(v: &[u8], fixed: bool) -> Option<(i64, &[u8])> {
        let digit = |i: usize| v.get(i).filter(|b| b.is_ascii_digit()).map(|b| i64::from(b - b'0'));
        let first = digit(0)?;
        match digit(1) {
            Some(second) => Some((first * 10 + second, &v[2..])),
            None if fixed => None,
            None => Some((first, &v[1..])),
        }
    }
    // Go `skip` for a one-byte layout literal.
    fn lit(v: &[u8], b: u8) -> Option<&[u8]> {
        if b == b' ' {
            if v.first().is_some_and(|c| *c != b' ') {
                return None;
            }
            return Some(&v[v.iter().take_while(|c| **c == b' ').count()..]);
        }
        (v.first() == Some(&b)).then(|| &v[1..])
    }
    let v = s.as_bytes();
    // stdLongYear: four bytes through `atoi`, so four digits.
    if v.len() < 4 || !v[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let year: i32 = s[..4].parse().ok()?;
    let (month, v) = num(lit(&v[4..], b'-')?, true)?;
    let (day, v) = num(lit(v, b'-')?, true)?;
    let (hour, v) = num(lit(v, sep)?, false)?;
    let (min, mut v) = num(lit(v, b':')?, true)?;
    if !(1..=12).contains(&month) || hour >= 24 || min >= 60 {
        return None;
    }
    let (mut sec, mut nsec) = (0, 0);
    if seconds {
        (sec, v) = num(lit(v, b':')?, true)?;
        if sec >= 60 {
            return None;
        }
        // Go `parseNanoseconds`: at most nine digits count; the rest are read and dropped.
        if v.len() >= 2 && matches!(v[0], b'.' | b',') && v[1].is_ascii_digit() {
            let n = 1 + v[1..].iter().take_while(|b| b.is_ascii_digit()).count();
            let digits = &v[1..n.min(10)];
            nsec =
                digits.iter().fold(0i128, |n, d| n * 10 + i128::from(d - b'0')) * 10i128.pow(9 - digits.len() as u32);
            v = &v[n..];
        }
    }
    let mut offset = 0;
    if zone {
        if v.first() == Some(&b'Z') {
            v = &v[1..];
        } else {
            // stdISO8601ColonTZ: `±hh:mm`; Go allows up to 24 hours and 60 minutes.
            if v.len() < 6 || v[3] != b':' {
                return None;
            }
            let (hr, _) = num(&v[1..3], true)?;
            let (mm, _) = num(&v[4..6], true)?;
            if hr > 24 || mm > 60 {
                return None;
            }
            offset = match v[0] {
                b'+' => (hr * 60 + mm) * 60,
                b'-' => -(hr * 60 + mm) * 60,
                _ => return None,
            };
            v = &v[6..];
        }
    }
    if !v.is_empty() {
        return None;
    }
    let midnight = chrono::NaiveDate::from_ymd_opt(year, month as u32, day as u32)?
        .and_hms_opt(0, 0, 0)?
        .and_utc()
        .timestamp();
    Some(i128::from(midnight + hour * 3600 + min * 60 + sec - offset) * NANOS + nsec)
}

/// Go `parseTimeValue` on a decoded JSON value; `None` when it does not parse. The zero
/// time is a value: it still ends a key search.
fn go_time(value: &Value) -> Option<GoTime> {
    match value {
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            go_parse(s, b'T', true, true)
                .or_else(|| go_parse(s, b' ', true, false))
                .or_else(|| go_parse(s, b' ', false, false))
                .or_else(|| s.parse().ok().map(go_unix))
        }
        Value::Number(n) => Some(go_unix(go_float_to_int(n.as_f64()?))),
        _ => None,
    }
}

/// Go `lookupMetadataTime`: the first key whose value parses, zero time included.
fn lookup_time(map: &Map<String, Value>, keys: &[&str]) -> Option<GoTime> {
    keys.iter().find_map(|k| map.get(*k).and_then(go_time))
}

/// Go `parseIntAny` for `expires_in`: whole seconds from a number or a decimal string.
fn go_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => Some(go_float_to_int(n.as_f64()?)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Go `expirationFromMap`: the expiry keys, then `expires_in` after its issue time, then
/// a nested `token` or `Token` map.
fn expiration_from_map(map: &Map<String, Value>) -> Option<GoTime> {
    if let Some(t) = lookup_time(
        map,
        &["expired", "expire", "expires_at", "expiresAt", "expiry", "expires"],
    ) {
        return Some(t);
    }
    let seconds = ["expires_in", "expiresIn"]
        .iter()
        .find_map(|k| map.get(*k).and_then(go_int).filter(|s| *s > 0));
    if let Some(seconds) = seconds {
        let issued = ["timestamp", "issued_at", "issuedAt"]
            .iter()
            .find_map(|k| map.get(*k).and_then(go_time).filter(|t| *t != ZERO));
        if let Some(issued) = issued {
            // `time.Duration(expiresIn) * time.Second` wraps like Go's int64 product.
            return Some(issued + i128::from(seconds.wrapping_mul(1_000_000_000)));
        }
    }
    ["token", "Token"].iter().find_map(|k| match map.get(*k) {
        Some(Value::Object(nested)) => expiration_from_map(nested),
        _ => None,
    })
}

/// Go `parseJWTExp`'s payload decoding: the middle of exactly three `.`-separated
/// segments, through `base64.URLEncoding` with the padding its length implies and then
/// `RawURLEncoding`. Both skip CR and LF and accept non-zero trailing bits.
fn jwt_payload(token: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    let parts: Vec<&str> = token.trim().split('.').collect();
    let [_, segment, _] = parts[..] else {
        return None;
    };
    let engine = |mode| {
        let config = GeneralPurposeConfig::new()
            .with_decode_padding_mode(mode)
            .with_decode_allow_trailing_bits(true);
        GeneralPurpose::new(&base64::alphabet::URL_SAFE, config)
    };
    let strip = |s: &str| s.bytes().filter(|b| !matches!(b, b'\r' | b'\n')).collect::<Vec<u8>>();
    // The padding comes from the length before CR and LF are skipped, as in Go.
    let pad = ["", "", "==", "="][segment.len() % 4];
    engine(DecodePaddingMode::RequireCanonical)
        .decode(strip(&format!("{segment}{pad}")))
        .or_else(|_| engine(DecodePaddingMode::RequireNone).decode(strip(segment)))
        .ok()
}

/// Go `json.Unmarshal` of a JWT payload into `struct{ Exp any }`: every member whose
/// name folds to `exp` (ASCII case; nothing else folds into those letters) replaces the
/// field in input order, null clearing it; other members are skipped unconverted. `None`
/// when the payload is not one JSON object or null, or a value fails to decode.
// ponytail: an `exp` value holding invalid UTF-8 or a lone surrogate fails here, where Go
// substitutes U+FFFD; such a value is never a number, so only a later `exp` in the same
// payload could read differently.
fn jwt_exp_claim(payload: &[u8]) -> Option<Value> {
    use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, Visitor};
    struct IsExp(bool);
    impl<'de> serde::Deserialize<'de> for IsExp {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Key;
            impl Visitor<'_> for Key {
                type Value = IsExp;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a member name")
                }
                fn visit_bytes<E>(self, v: &[u8]) -> Result<IsExp, E> {
                    Ok(IsExp(v.eq_ignore_ascii_case(b"exp")))
                }
                fn visit_str<E>(self, v: &str) -> Result<IsExp, E> {
                    Ok(IsExp(v.eq_ignore_ascii_case("exp")))
                }
            }
            // Raw bytes: a name that is not UTF-8 is skipped, not an error.
            d.deserialize_bytes(Key)
        }
    }
    struct Claims;
    impl<'de> DeserializeSeed<'de> for Claims {
        type Value = Value;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Claims {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("an object or null")
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut exp = Value::Null;
            while let Some(IsExp(is_exp)) = map.next_key()? {
                if is_exp {
                    exp = map.next_value()?;
                } else {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            Ok(exp)
        }
    }
    let mut de = serde_json::Deserializer::from_slice(payload);
    let exp = Claims.deserialize(&mut de).ok()?;
    de.end().ok()?;
    Some(exp)
}

/// Go `parseJWTExp`: a positive number or decimal string, through `normaliseUnix`.
fn jwt_exp(token: &str) -> Option<GoTime> {
    match jwt_exp_claim(&jwt_payload(token)?)? {
        Value::Number(n) => n.as_f64().filter(|f| *f > 0.0).map(|f| go_unix(go_float_to_int(f))),
        Value::String(s) => s.trim().parse::<i64>().ok().filter(|s| *s > 0).map(go_unix),
        _ => None,
    }
}

/// Go `SIWCExpirationTime`: the partition's expiration, else its access token's JWT
/// `exp`; a zero time is no expiration.
fn expiry(siwc: &Map<String, Value>) -> Option<GoTime> {
    expiration_from_map(siwc)
        .filter(|t| *t != ZERO)
        .or_else(|| jwt_exp(text(siwc, "access_token")).filter(|t| *t != ZERO))
}

/// Go v8.0.67 `SIWCRealExpirationTime` (06fd5b8b): the token's real expiry, never the
/// margin-deducted `expired`: `expires_at`, else `expires_in` after the last refresh
/// (or issue time), else the access token's JWT `exp`.
fn real_expiry(siwc: &Map<String, Value>) -> Option<GoTime> {
    if let Some(t) = lookup_time(siwc, &["expires_at", "expiresAt"]).filter(|t| *t != ZERO) {
        return Some(t);
    }
    let seconds = ["expires_in", "expiresIn"]
        .iter()
        .find_map(|k| siwc.get(*k).and_then(go_int).filter(|s| *s > 0));
    if let Some(seconds) = seconds
        && let Some(from) = lookup_time(
            siwc,
            &["last_refresh", "lastRefresh", "timestamp", "issued_at", "issuedAt"],
        )
        .filter(|t| *t != ZERO)
    {
        return Some(from + i128::from(seconds.wrapping_mul(1_000_000_000)));
    }
    jwt_exp(text(siwc, "access_token")).filter(|t| *t != ZERO)
}

/// Go v8.0.67 `CodexSIWCStatus` (06fd5b8b): a Codex credential's local SIWC status at
/// `now`, read from the credential alone, with no refresh or upstream request. `off`
/// unless SIWC is in use; `invalid` without an access token, with an unknown or past
/// real expiry, when `rejected` (an upstream refused the token), or when its grant does
/// not allow ChatGPT plan usage ([`plan_use_denied`], #92); else `valid`.
pub fn status(credential: &cpa_core::credential::Credential, rejected: bool, now: Time) -> &'static str {
    if !enabled(credential) {
        return "off";
    }
    if access_token(credential).is_empty() || rejected || plan_use_denied(credential) {
        return "invalid";
    }
    match partition(credential).and_then(real_expiry) {
        Some(expiry) if expiry > go_now(now) => "valid",
        _ => "invalid",
    }
}

/// Go `CodexSIWCNeedsRefresh`: `None` when the partition cannot refresh (no refresh
/// token or client ID), else whether it is due at `now`: no access token, five minutes
/// before its expiry, five minutes after its last refresh, or now when neither is known.
pub fn refresh_due(credential: &cpa_core::credential::Credential, now: Time) -> Option<bool> {
    let siwc = partition(credential)?;
    if text(siwc, "refresh_token").is_empty() || text(siwc, "client_id").is_empty() {
        return None;
    }
    if text(siwc, "access_token").is_empty() {
        return Some(true);
    }
    let (now, lead) = (go_now(now), 5 * 60 * NANOS);
    if let Some(expiry) = expiry(siwc) {
        return Some(expiry - lead <= now);
    }
    let last = lookup_time(siwc, &["last_refresh", "lastRefresh"]).filter(|t| *t != ZERO);
    Some(last.is_none_or(|last| last + lead <= now))
}

/// The partition's request token is present and not expired at `now`.
pub(crate) fn access_usable(credential: &cpa_core::credential::Credential, now: Time) -> bool {
    !access_token(credential).is_empty()
        && partition(credential)
            .and_then(expiry)
            .is_none_or(|exp| exp > go_now(now))
}

/// The `siwc` patch of a refresh: the stored partition with the refreshed fields over it.
pub(crate) fn refresh_patch(
    credential: &cpa_core::credential::Credential,
    tokens: &Tokens,
) -> cpa_core::credential::MetadataPatch {
    let mut merged = partition(credential).cloned().unwrap_or_default();
    merged.extend(tokens.to_map());
    let mut patch = cpa_core::credential::MetadataPatch::default();
    patch.set.insert("siwc".into(), Value::Object(merged));
    patch
}

/// The partition's refresh grant inputs: refresh token, client ID and device ID.
pub(crate) fn refresh_inputs(credential: &cpa_core::credential::Credential) -> (&str, &str, &str) {
    partition(credential).map_or(("", "", ""), |siwc| {
        (
            text(siwc, "refresh_token"),
            text(siwc, "client_id"),
            text(siwc, "device_id"),
        )
    })
}

/// Go `strconv.ParseBool`.
fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "1" | "t" | "T" | "true" | "TRUE" | "True" => Some(true),
        "0" | "f" | "F" | "false" | "FALSE" | "False" => Some(false),
        _ => None,
    }
}

/// Go `IsValidUUID`: a canonical 36-character UUID, any case, surrounding space ignored.
pub fn is_valid_uuid(raw: &str) -> bool {
    let raw = raw.trim().as_bytes();
    raw.len() == 36
        && raw.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Go `GenerateRandomBase64URL`: 32 random bytes, unpadded base64url.
pub fn random_base64url() -> Result<String, ExecError> {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| siwc_error(500, "failed to generate random bytes"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Go `GenerateSIWCAuthURL`; `url.Values.Encode` sorts the parameters.
pub fn authorize_url(
    state: &str,
    nonce: &str,
    device_id: &str,
    redirect: &str,
    challenge: &str,
    identifier: &str,
) -> Result<String, ExecError> {
    let device_id = device_id.trim();
    if !is_valid_uuid(device_id) {
        return Err(siwc_error(400, "Sign in with ChatGPT requires a valid device UUID"));
    }
    if state.trim().is_empty() {
        return Err(siwc_error(400, "state is required"));
    }
    if nonce.trim().is_empty() {
        return Err(siwc_error(400, "nonce is required"));
    }
    let redirect = match redirect.trim() {
        "" => REDIRECT_URI,
        r => r,
    };
    let host = format!("urn:uuid:{}", device_id.go_lower());
    let mut url = Url::parse(AUTHORIZE_URL).expect("constant URL");
    url.query_pairs_mut().extend_pairs([
        ("agent_name_hint", identifier),
        ("client_id", DYNAMIC_CLIENT_ID),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("ext_agent_host_id", &host),
        ("nonce", nonce.trim()),
        ("redirect_uri", redirect),
        ("resource", RESOURCE),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("state", state.trim()),
    ]);
    Ok(url.into())
}

/// Go v8.0.67 `GenerateSIWCReauthURL` (3891ebaa): [`authorize_url`] for an account
/// that already holds an issued client, which replaces `dynamic_agent_client`, without
/// `agent_name_hint`. OpenAI's "Accounts and sessions" guide: reauthorize through the
/// same endpoint with the selected account's issued `client_id`, keep the host's
/// `ext_agent_host_id`, and omit `agent_name_hint`.
pub fn reauth_url(
    client_id: &str,
    state: &str,
    nonce: &str,
    device_id: &str,
    redirect: &str,
    challenge: &str,
) -> Result<String, ExecError> {
    let client_id = client_id.trim();
    if client_id.is_empty() || client_id == DYNAMIC_CLIENT_ID {
        return Err(siwc_error(
            400,
            "SIWC reauthorization requires the saved issued client ID",
        ));
    }
    let url = authorize_url(state, nonce, device_id, redirect, challenge, "")?;
    let mut url = Url::parse(&url).expect("built above");
    // `url.Values.Encode` sorts the keys.
    let pairs: std::collections::BTreeMap<String, String> = url
        .query_pairs()
        .into_owned()
        .filter(|(k, _)| k != "agent_name_hint" && k != "client_id")
        .chain([("client_id".to_owned(), client_id.to_owned())])
        .collect();
    url.query_pairs_mut().clear().extend_pairs(pairs);
    Ok(url.into())
}

/// Go `CodexSIWCTokenData`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub client_id: String,
    pub scope: String,
    pub scopes: Vec<String>,
    pub expired: String,
    /// The real expiry, `expires_in` after the grant with no margin (Go v8.0.67
    /// `ExpiresAt`, 06fd5b8b); empty when unknown.
    pub expires_at: String,
    pub last_refresh: String,
    pub device_id: String,
}

impl Tokens {
    /// Go `ToMap`: the `siwc` partition as persisted; optional fields only when set.
    /// An unknown expiry (`expired` empty, a refresh answer whose `expires_in` could not
    /// be read, see [`token_response`]) writes `expired` and `expires_at` as `null`, so a
    /// refresh merged over the stored partition clears its old expiries instead of
    /// keeping a past one that would make the new token due at once. A `null` expiry
    /// reads as unset.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut out = Map::new();
        let mut set = |k: &str, v: &str| {
            out.insert(k.into(), Value::String(v.to_owned()));
        };
        set("access_token", &self.access_token);
        set("refresh_token", &self.refresh_token);
        set("client_id", &self.client_id);
        set("expired", &self.expired);
        set("last_refresh", &self.last_refresh);
        if !self.expires_at.is_empty() {
            set("expires_at", &self.expires_at);
        }
        if !self.id_token.is_empty() {
            set("id_token", &self.id_token);
        }
        if !self.scope.is_empty() {
            set("scope", &self.scope);
        }
        if !self.device_id.is_empty() {
            set("device_id", &self.device_id);
        }
        if !self.scopes.is_empty() {
            out.insert("scopes".into(), self.scopes.clone().into());
        }
        if self.expired.is_empty() {
            out.insert("expired".into(), Value::Null);
            out.insert("expires_at".into(), Value::Null);
        }
        out
    }
}

/// `expires_in` seconds after `now`: the margin-deducted scheduling expiry (`expired`)
/// and the real one (`expires_at`), both as stored. `None` when the real expiry is out of
/// chrono's range or its year, written in `now`'s offset, has more than four digits: an
/// RFC 3339 `date-fullyear` is four digits (§5.6), the partition stores its expiries in
/// that form, and this module reads them back with a four-digit year ([`go_parse`]).
/// That is a storage-format limit, not a provider rule. Checked arithmetic, never a
/// panic, and never a fallback to `now` that would make the new token due at once.
fn expiry_after(now: chrono::DateTime<Local>, expires_in: u64) -> Option<(String, String)> {
    let lifetime = TimeDelta::try_seconds(i64::try_from(expires_in).ok()?)?;
    let ttl = match lifetime.checked_sub(&EXPIRY_MARGIN)? {
        ttl if ttl > TimeDelta::zero() => ttl,
        _ => lifetime,
    };
    let real = now
        .checked_add_signed(lifetime)
        .filter(|t| (0..=9999).contains(&t.year()))?;
    let scheduled = now.checked_add_signed(ttl)?;
    let rfc3339 = |t: chrono::DateTime<Local>| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    Some((rfc3339(scheduled), rfc3339(real)))
}

/// A 200 answer of the SIWC token endpoint, read at `now`. OpenAI's token reference lists
/// `access_token`, `refresh_token`, `id_token`, `token_type`, `expires_in` (`3600` in its
/// example), `scope` and `earliest_refresh_at`, and says a successful refresh returns a
/// new access token and a replacement refresh token.
///
/// The token set is the access token and the refresh token. Each must be a non-blank
/// string, or nothing of the answer is used ("invalid access_token" or "invalid
/// refresh_token"): without a replacement refresh token there is nothing to keep.
///
/// A refresh (`exchange` false) has spent the stored refresh token, so its other fields
/// never discard the token set (#92):
/// - `expires_in` is read as a JSON integer. That is the parse this module inherited
///   from its typed reader (an `i64` field). The provider's example is `3600`, and no
///   source forbids another JSON spelling of the same number, so none is claimed; the
///   parse is not widened either. A value that is not a positive integer, or whose
///   expiry cannot be stored ([`expiry_after`]), leaves the expiry unknown:
///   [`Tokens::expired`] is empty and [`Tokens::to_map`] clears the stored expiries,
///   so the new access token's JWT `exp` claim (integer Unix seconds, token reference)
///   or `last_refresh` schedules the next refresh.
/// - `scope`: when absent (the refresh asks with none, which keeps the grant; RFC 6749
///   §5.1 lets the answer omit an unchanged scope) or not a non-blank string, the
///   stored grant stays. A reported scope is stored as reported, also without
///   `chatgpt.tokens.use.direct`: OpenAI's "errors and recovery" says to retain such a
///   sign-in and mark ChatGPT plan usage disabled, which [`plan_use_denied`] does, so
///   requests are refused before they are built ([`require_token`]).
/// - `id_token`: a string is kept; any other value is left out.
///
/// The code exchange (`exchange` true) is the SIWC binding (`--siwc-bind` and the
/// Management API bind), which exists only to add direct-inference credentials to a
/// Codex credential; the proxy has no general SIWC sign-in to retain. It spends nothing
/// stored, so it keeps its inherited strict reading: an ID token, a granted scope
/// holding `chatgpt.tokens.use.direct` (without it the bind fails and nothing is
/// written; the user binds again and allows ChatGPT plan usage), and a positive integer
/// `expires_in`. An expiry that cannot be stored is unknown there too, not a refusal.
/// `token_type` has no documented constraint, and `earliest_refresh_at` has no
/// documented format or meaning, so neither is read or persisted.
pub(crate) fn token_response(
    body: &[u8],
    client_id: &str,
    device_id: &str,
    exchange: bool,
    now: chrono::DateTime<Local>,
) -> Result<Tokens, ExecError> {
    let Ok(Value::Object(answer)) = serde_json::from_slice::<Value>(body) else {
        return Err(siwc_error(502, "failed to parse SIWC token response"));
    };
    let invalid = |field: &str| siwc_error(502, &format!("OpenAI OAuth token response has invalid {field}"));
    let token = |field: &str| match answer.get(field) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(invalid(field)),
    };
    let access_token = token("access_token")?;
    let refresh_token = token("refresh_token")?;
    let id_token = match answer.get("id_token") {
        Some(Value::String(s)) => s.clone(),
        None => String::new(),
        Some(_) if !exchange => String::new(),
        Some(_) => return Err(invalid("id_token")),
    };
    if exchange && id_token.trim().is_empty() {
        return Err(siwc_error(
            502,
            "OpenAI OAuth token response did not contain an ID token",
        ));
    }
    let scope = match answer.get("scope") {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_owned()),
        _ if exchange => return Err(invalid("scope")),
        _ => None,
    };
    let scopes: Vec<String> = scope
        .iter()
        .flat_map(|s| s.split_whitespace())
        .map(str::to_owned)
        .collect();
    if exchange && !scopes.iter().any(|s| s == DIRECT_TOKEN_SCOPE) {
        return Err(siwc_error(
            502,
            &format!("OpenAI OAuth grant did not include {DIRECT_TOKEN_SCOPE}"),
        ));
    }
    let seconds = answer.get("expires_in").and_then(Value::as_u64).filter(|s| *s > 0);
    if exchange && seconds.is_none() {
        return Err(invalid("expires_in"));
    }
    let (expired, expires_at) = seconds.and_then(|s| expiry_after(now, s)).unwrap_or_default();
    Ok(Tokens {
        access_token,
        refresh_token,
        id_token,
        client_id: client_id.trim().to_owned(),
        scope: scope.unwrap_or_default(),
        scopes,
        expired,
        expires_at,
        last_refresh: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        device_id: device_id.trim().to_owned(),
    })
}

fn siwc_error(status: u16, message: &str) -> ExecError {
    ExecError::local(status, FailureScope::Credential, message)
}

impl CodexOAuth {
    /// Go `requestSIWCToken`. Upstream bodies are never echoed: the management session
    /// reports a fixed message.
    pub(crate) async fn siwc_token_request(
        &self,
        form: &[(&str, &str)],
        client_id: &str,
        device_id: &str,
        require_id_token: bool,
    ) -> Result<Tokens, crate::codex_oauth::GrantError> {
        // url.Values.Encode sorts keys; callers pass them sorted.
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let (status, body) = match self
            .post_status(
                self.siwc_token_url(),
                "application/x-www-form-urlencoded",
                body.into_bytes(),
            )
            .await?
        {
            (status, Some(body)) => (status, body),
            // A refresh answered 200 whose body could not be read: the endpoint accepted
            // the grant and may have spent the refresh token, so this refresh does not
            // send it again. Transient and ambiguous, never a refusal (#92). The code
            // exchange keeps its transport failure.
            (200, None) if !require_id_token => {
                return Err(crate::codex_oauth::GrantError {
                    error: siwc_error(502, "OpenAI OAuth token response could not be read"),
                    terminal: false,
                    answered: true,
                });
            }
            (_, None) => {
                return Err(ExecError::local(502, FailureScope::Transport, "codex oauth request failed").into());
            }
        };
        if status != 200 && !require_id_token {
            // A refresh keeps the OAuth error code (Go echoes the body), and a documented
            // unusable-token code stops the retries; bodies may echo request material.
            let code = crate::codex_oauth::oauth_code(&body);
            return Err(crate::codex_oauth::GrantError {
                error: crate::codex_oauth::refresh_error(
                    status,
                    &format!("OpenAI OAuth token request failed ({status}): {code}"),
                ),
                // Read as the first-party client reads the same issuer's codes.
                terminal: crate::codex_oauth::siwc_terminal(&crate::codex_oauth::grant_code(&body)),
                answered: false,
            });
        }
        if status != 200 {
            return Err(siwc_error(status, &format!("OpenAI OAuth token request failed ({status})")).into());
        }
        token_response(&body, client_id, device_id, require_id_token, Local::now()).map_err(|error| {
            crate::codex_oauth::GrantError {
                error,
                terminal: false,
                answered: true,
            }
        })
    }

    /// Go `ExchangeSIWCCodeForTokens`: the issued client ID from the callback, never the
    /// dynamic one in the authorization URL.
    pub async fn exchange_siwc(
        &self,
        code: &str,
        client_id: &str,
        redirect: &str,
        device_id: &str,
        verifier: &str,
    ) -> Result<Tokens, ExecError> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(siwc_error(
                400,
                "OpenAI OAuth registration callback did not contain an issued client ID",
            ));
        }
        let code = code.trim();
        if code.is_empty() {
            return Err(siwc_error(400, "authorization code is required"));
        }
        let redirect = match redirect.trim() {
            "" => REDIRECT_URI,
            r => r,
        };
        self.siwc_token_request(
            &[
                ("client_id", client_id),
                ("code", code),
                ("code_verifier", verifier),
                ("grant_type", "authorization_code"),
                ("redirect_uri", redirect),
                ("resource", RESOURCE),
            ],
            client_id,
            device_id,
            true,
        )
        .await
        .map_err(|e| e.error)
    }
}

/// Go `BindCodexSIWC`'s file: every top-level member of `disk` kept as written (large
/// numbers included), `siwc` replaced by `tokens` and `use_siwc` set, encoded like Go's
/// `json.Marshal` of the merged map (sorted keys, compact, HTML-escaped, no newline).
/// `None` when `disk` is not a JSON object.
pub fn bind_file(disk: &[u8], tokens: &Tokens) -> Option<Vec<u8>> {
    let mut members: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>> =
        serde_json::from_slice(disk).ok()?;
    let siwc = crate::codex_oauth::encode_go_json(&Value::Object(tokens.to_map()));
    members.insert(
        "siwc".into(),
        serde_json::value::RawValue::from_string(siwc.trim_end().to_owned()).ok()?,
    );
    members.insert(
        "use_siwc".into(),
        serde_json::value::RawValue::from_string("true".into()).ok()?,
    );
    let mut out = String::with_capacity(disk.len() + 512);
    out.push('{');
    for (i, (key, value)) in members.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&crate::codex_json::go_quote(key, true));
        out.push(':');
        compact_html(value.get(), &mut out);
    }
    out.push('}');
    Some(out.into_bytes())
}

/// Go `compact(dst, src, escapeHTML=true)` over valid JSON: whitespace outside strings
/// dropped, `<`, `>`, `&`, U+2028 and U+2029 escaped, everything else verbatim.
fn compact_html(raw: &str, out: &mut String) {
    let (mut in_string, mut escaped) = (false, false);
    for c in raw.chars() {
        if in_string {
            match c {
                '<' => out.push_str("\\u003c"),
                '>' => out.push_str("\\u003e"),
                '&' => out.push_str("\\u0026"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => out.push(c),
            }
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if !matches!(c, ' ' | '\t' | '\n' | '\r') {
            in_string = c == '"';
            out.push(c);
        }
    }
}

#[cfg(test)]
#[path = "codex_siwc_tests.rs"]
mod tests;
