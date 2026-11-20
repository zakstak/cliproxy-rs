//! Pinned static model catalog: `internal/registry/models/models.json` from CLIProxyAPI
//! v8.0.35, with removed provider sections cut out (issue #49) and sourced additions.
//! GPT-6.2 defaults and reasoning levels follow openai/codex@6c0cee502d21,
//! `codex-rs/models-manager/models.json`. Unknown ChatGPT output limits stay unset.
//! See `docs/DIFFERENCES-FROM-GO.md` for catalog sources and endpoint distinctions.
//!
//! This is the static layer only. Go overlays a dynamic registry (per-credential
//! registrations, config model definitions, remote catalog refresh) and consults it before
//! these definitions (internal/registry/model_registry.go); that overlay belongs to the
//! server runtime.

use std::sync::{Arc, LazyLock, Mutex, RwLock};

pub mod dynamic;

use serde::Deserialize;
use serde_json::{Map, Value};

/// The models.json built into the binary.
pub static MODELS_JSON: &str = include_str!("catalog/models.json");

/// Channels searched by [`Catalog::lookup`], in Go's `LookupStaticModelInfo` order
/// (internal/registry/model_definitions.go) without the removed providers' sections
/// (issue #49). `codex-free`, `codex-team` and `codex-plus` are not searched; Go reaches
/// them only through per-channel getters.
const LOOKUP_ORDER: [&str; 4] = ["claude", "codex-pro", "xai", "meta"];

/// Thinking budget and level support (`ThinkingSupport` in model_registry.go).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ThinkingSupport {
    pub min: i64,
    pub max: i64,
    /// `zero_allowed` in models.json, `zero-allowed` in config YAML (Go's yaml tag).
    #[serde(alias = "zero-allowed")]
    pub zero_allowed: bool,
    #[serde(alias = "dynamic-allowed")]
    pub dynamic_allowed: bool,
    /// The Claude `thinking.type` that turns thinking off (`disable-type` in config
    /// YAML). Empty means `disabled`; claude-sonnet-5-5 needs `between_tools`.
    #[serde(alias = "disable-type")]
    pub disable_type: String,
    /// Discrete effort levels; empty means budget-based thinking.
    pub levels: Vec<String>,
}

/// One model definition. Typed fields cover what routing and translation read; `raw` keeps
/// the full published object in file key order for handlers that render Go's shapes.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Go's `type` field: the provider family that serves the model.
    pub kind: String,
    pub thinking: Option<ThinkingSupport>,
    pub raw: Map<String, Value>,
}

impl ModelInfo {
    /// Whether the resolved model is a config entry marked `is-compat` (Go
    /// `ModelInfo.IsCompat`). Only registry-resolved models carry it.
    pub fn is_compat(&self) -> bool {
        self.raw.get("is_compat").and_then(Value::as_bool).unwrap_or(false)
    }

    pub fn from_raw(raw: Map<String, Value>) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct Typed {
            id: String,
            #[serde(default, rename = "type")]
            kind: String,
            #[serde(default)]
            thinking: Option<ThinkingSupport>,
        }
        let typed: Typed = serde_json::from_value(Value::Object(go_integers(&raw)))?;
        Ok(Self {
            id: typed.id,
            kind: typed.kind,
            thinking: typed.thinking,
            raw,
        })
    }
}

pub struct Catalog {
    /// Channels in file order.
    channels: Vec<(String, Vec<ModelInfo>)>,
}

impl Catalog {
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        Self::from_root(serde_json::from_str(json)?)
    }

    fn from_root(root: Map<String, Value>) -> Result<Self, serde_json::Error> {
        let mut channels = Vec::with_capacity(root.len());
        for (name, models) in root {
            if models.is_null() {
                continue;
            }
            let models: Vec<Map<String, Value>> = serde_json::from_value(models)?;
            let models = models.into_iter().map(ModelInfo::from_raw).collect::<Result<_, _>>()?;
            channels.push((name, models));
        }
        Ok(Self { channels })
    }

    /// Definitions for one channel (`claude`, `codex-plus`, ...), empty when unknown.
    pub fn channel(&self, name: &str) -> &[ModelInfo] {
        self.channels
            .iter()
            .find(|(n, _)| n == name)
            .map_or(&[], |(_, models)| models.as_slice())
    }

    /// First definition with this exact ID across channels, in Go's search order. The same
    /// ID can mean different things per channel, so callers that know the provider should use [`Self::channel`].
    pub fn lookup(&self, id: &str) -> Option<&ModelInfo> {
        if id.is_empty() {
            return None;
        }
        LOOKUP_ORDER
            .iter()
            .flat_map(|channel| self.channel(channel).iter())
            .find(|m| m.id == id)
            .or_else(|| {
                let claude = self.channel("claude");
                claude_family_ids(id)
                    .into_iter()
                    .find_map(|family| claude.iter().find(|m| m.id == family))
            })
    }
}

/// How Go's `ModelInfo` and `ThinkingSupport` fields decode.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Int,
    Flag,
    List,
    Thinking,
    Config,
    Native,
}
use Kind::*;
/// `ModelInfo`'s JSON fields: name, kind, `omitempty`.
const MODEL: [(&str, Kind, bool); 22] = [
    ("id", Text, false),
    ("object", Text, false),
    ("created", Int, false),
    ("owned_by", Text, false),
    ("type", Text, false),
    ("display_name", Text, true),
    ("name", Text, true),
    ("version", Text, true),
    ("description", Text, true),
    ("inputTokenLimit", Int, true),
    ("outputTokenLimit", Int, true),
    ("supportedGenerationMethods", List, true),
    ("context_length", Int, true),
    ("max_completion_tokens", Int, true),
    ("supported_parameters", List, true),
    ("supportedInputModalities", List, true),
    ("supportedOutputModalities", List, true),
    ("supports_web_search", Flag, true),
    ("thinking", Thinking, true),
    ("config", Config, true),
    // Decoded (and type-checked) only.
    ("native_capabilities", Native, true),
    ("support_configuration_update", Flag, true),
];
/// `ThinkingSupport`'s JSON fields.
const THINKING: [(&str, Kind, bool); 6] = [
    ("min", Int, true),
    ("max", Int, true),
    ("zero_allowed", Flag, true),
    ("dynamic_allowed", Flag, true),
    ("disable_type", Text, true),
    ("levels", List, true),
];

/// `raw` for typed deserialisation, with `-0` as 0 in its integer fields and `thinking`'s:
/// Go reads that integer literal as 0, serde as the float `-0.0`. `raw` itself keeps it.
/// A refresh refuses `-0.0` itself by its text (`check_catalog_members`).
pub(crate) fn go_integers(raw: &Map<String, Value>) -> Map<String, Value> {
    fn zero(obj: &mut Map<String, Value>, fields: &[(&'static str, Kind, bool)]) {
        for (name, kind, _) in fields {
            let negative_zero = |v: &Value| v.as_f64().is_some_and(|f| f == 0.0 && f.is_sign_negative());
            if *kind == Int && obj.get(*name).is_some_and(negative_zero) {
                obj.insert((*name).into(), Value::from(0));
            }
        }
    }
    let mut out = raw.clone();
    zero(&mut out, &MODEL);
    if let Some(Value::Object(thinking)) = out.get_mut("thinking") {
        zero(thinking, &THINKING);
    }
    out
}

/// A catalog model as Go's typed decode reads it into `registry.ModelInfo` and
/// `json.Marshal` writes it back (model_registry.go): members match fields exactly, else
/// case-insensitively; null leaves a field at its zero value (and is `""` inside string
/// lists and header maps); integer fields take integer literals only; unknown members
/// are skipped; `json:"-"` fields are left out of the result. `Err` is the decode error
/// that makes Go refuse the whole catalog. `raw` itself is left as it is.
pub fn go_model_json(raw: &Map<String, Value>) -> Result<Value, String> {
    use crate::text::GoStr;
    fn field(fields: &[(&'static str, Kind, bool)], key: &str) -> Option<usize> {
        fields
            .iter()
            .position(|f| f.0 == key)
            .or_else(|| fields.iter().position(|f| f.0.go_eq_fold(key)))
    }
    fn err(key: &str, v: &Value) -> String {
        format!("cannot unmarshal {v} into field {key}")
    }
    fn string(key: &str, v: &Value) -> Result<String, String> {
        match v {
            Value::Null => Ok(String::new()),
            Value::String(s) => Ok(s.clone()),
            _ => Err(err(key, v)),
        }
    }
    /// Decodes `obj` into `out` (Go's struct, held as the fields set so far).
    fn decode(
        fields: &[(&'static str, Kind, bool)],
        obj: &Map<String, Value>,
        out: &mut [Option<Value>],
    ) -> Result<(), String> {
        for (key, v) in obj {
            let Some(i) = field(fields, key) else { continue };
            let (name, kind, _) = fields[i];
            if v.is_null() {
                // Null clears a pointer, slice or map; other fields keep their value.
                if matches!(kind, List | Thinking | Config | Native) {
                    out[i] = None;
                }
                continue;
            }
            out[i] = Some(match kind {
                Text => Value::String(string(name, v)?),
                Int => match v {
                    Value::Number(n) if n.is_i64() => Value::from(n.as_i64().unwrap_or_default()),
                    // `-0`, an integer literal, which serde reads as a float. A refresh
                    // refuses `-0.0` and other fractions by their text before this runs
                    // (`check_catalog_members`).
                    Value::Number(n) if n.as_f64().is_some_and(|f| f == 0.0 && f.is_sign_negative()) => Value::from(0),
                    _ => return Err(err(name, v)),
                },
                Flag => match v {
                    Value::Bool(b) => Value::Bool(*b),
                    _ => return Err(err(name, v)),
                },
                List => match v {
                    Value::Array(items) => Value::Array(
                        items
                            .iter()
                            .map(|item| string(name, item).map(Value::String))
                            .collect::<Result<_, _>>()?,
                    ),
                    _ => return Err(err(name, v)),
                },
                Thinking => {
                    let Value::Object(t) = v else { return Err(err(name, v)) };
                    // Decoding into a set pointer updates the struct it points to.
                    let mut inner = match out[i].take() {
                        Some(Value::Array(prev)) => prev.into_iter().map(|v| (!v.is_null()).then_some(v)).collect(),
                        _ => vec![None; THINKING.len()],
                    };
                    decode(&THINKING, t, &mut inner)?;
                    Value::Array(inner.into_iter().map(|v| v.unwrap_or(Value::Null)).collect())
                }
                Config => {
                    let Value::Object(c) = v else { return Err(err(name, v)) };
                    // The header map; nil and empty both marshal as absent (`omitempty`).
                    let mut headers = match out[i].take() {
                        Some(Value::Object(prev)) => prev,
                        _ => Map::new(),
                    };
                    for (k, h) in c {
                        if !k.as_str().go_eq_fold("override_header") {
                            continue;
                        }
                        match h {
                            Value::Null => headers.clear(),
                            // A map decode adds to the map already there.
                            Value::Object(m) => {
                                for (hk, hv) in m {
                                    headers.insert(hk.clone(), Value::String(string("override_header", hv)?));
                                }
                            }
                            _ => return Err(err("override_header", h)),
                        }
                    }
                    Value::Object(headers)
                }
                Native => {
                    let Value::Object(n) = v else { return Err(err(name, v)) };
                    for (k, b) in n {
                        let known = ["web_search", "tool_search"].iter().any(|f| *f == k || f.go_eq_fold(k));
                        if known && !matches!(b, Value::Null | Value::Bool(_)) {
                            return Err(err(k, b));
                        }
                    }
                    Value::Null
                }
            });
        }
        Ok(())
    }
    fn encode(fields: &[(&'static str, Kind, bool)], vals: Vec<Option<Value>>) -> Map<String, Value> {
        let mut out = Map::new();
        for ((name, kind, omit), v) in fields.iter().zip(vals) {
            let v = match (kind, v) {
                (Native, _) => continue,
                (_, None) if matches!(kind, List | Thinking | Config) => continue,
                (Text, None) => Value::from(""),
                (Int, None) => Value::from(0),
                (Flag, None) => Value::from(false),
                (Thinking, Some(Value::Array(t))) => {
                    let t = t.into_iter().map(|v| (!v.is_null()).then_some(v)).collect();
                    out.insert((*name).into(), Value::Object(encode(&THINKING, t)));
                    continue;
                }
                (Config, Some(Value::Object(h))) => {
                    let mut config = Map::new();
                    if !h.is_empty() {
                        // A Go map: keys sorted.
                        let sorted: std::collections::BTreeMap<String, Value> = h.into_iter().collect();
                        config.insert("override_header".into(), Value::Object(sorted.into_iter().collect()));
                    }
                    out.insert((*name).into(), Value::Object(config));
                    continue;
                }
                (_, Some(v)) => v,
                (_, None) => continue,
            };
            let empty = match &v {
                Value::String(s) => s.is_empty(),
                Value::Number(n) => n.as_i64() == Some(0),
                Value::Bool(b) => !b,
                Value::Array(a) => a.is_empty(),
                _ => false,
            };
            if !(*omit && empty) {
                out.insert((*name).into(), v);
            }
        }
        out
    }
    let mut vals = vec![None; MODEL.len()];
    decode(&MODEL, raw, &mut vals)?;
    // `support_configuration_update` is `json:"-"`.
    vals.truncate(MODEL.len() - 1);
    Ok(Value::Object(encode(&MODEL[..MODEL.len() - 1], vals)))
}

/// `staticModelsJSON`'s sections.
const SECTIONS: [&str; 7] = [
    "claude",
    "codex-free",
    "codex-team",
    "codex-plus",
    "codex-pro",
    "xai",
    "meta",
];

/// An object's members in order, repeats included, each value as its JSON text.
struct Members<'a>(Vec<(String, &'a serde_json::value::RawValue)>);

impl<'de> Deserialize<'de> for Members<'de> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = Members<'de>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("an object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut out = Vec::new();
                while let Some(member) = map.next_entry()? {
                    out.push(member);
                }
                Ok(Members(out))
            }
        }
        d.deserialize_map(Visit)
    }
}

/// Where Go's typed decode of a catalog would differ from the decoded `Map` the refresh
/// keeps (docs/DIFFERENCES-FROM-GO.md, remote model catalogs): a known section or field
/// named in another case (Go matches it case-insensitively) or repeated (Go decodes each
/// occurrence in turn into the same field), and an integer field holding anything but an
/// integer literal (serde reads `-0.0` and `1e-400` as zero).
fn check_catalog_members(json: &str) -> Result<(), String> {
    use crate::text::GoStr;
    use serde_json::value::RawValue;
    fn known<'a>(names: &[&'static str], obj: &Members<'a>, at: &str) -> Result<Vec<(usize, &'a RawValue)>, String> {
        let mut seen = vec![false; names.len()];
        let mut out = Vec::new();
        for (key, v) in &obj.0 {
            let Some(i) = names.iter().position(|n| n.go_eq_fold(key)) else {
                continue;
            };
            if key != names[i] {
                return Err(format!("{at}: member {key:?} is {:?} in another case", names[i]));
            }
            if std::mem::replace(&mut seen[i], true) {
                return Err(format!("{at}: member {key:?} is repeated"));
            }
            out.push((i, *v));
        }
        Ok(out)
    }
    fn object(v: &RawValue) -> Option<Result<Members<'_>, String>> {
        v.get()
            .starts_with('{')
            .then(|| serde_json::from_str(v.get()).map_err(|e| e.to_string()))
    }
    fn integer(v: &RawValue) -> bool {
        let text = v.get();
        let digits = text.strip_prefix('-').unwrap_or(text);
        text == "null"
            || (!digits.is_empty()
                && digits.bytes().all(|b| b.is_ascii_digit())
                && (digits == "0" || !digits.starts_with('0')))
    }
    fn fields(table: &[(&'static str, Kind, bool)], obj: &Members, at: &str) -> Result<(), String> {
        let names: Vec<&'static str> = table.iter().map(|f| f.0).collect();
        for (i, v) in known(&names, obj, at)? {
            let (name, kind, _) = table[i];
            let at = format!("{at}.{name}");
            match kind {
                Int if !integer(v) => return Err(format!("{at}: {} is not an integer", v.get())),
                Thinking => {
                    if let Some(t) = object(v) {
                        fields(&THINKING, &t?, &at)?;
                    }
                }
                Config => {
                    if let Some(c) = object(v) {
                        for (_, headers) in known(&["override_header"], &c?, &at)? {
                            // Go records a type error even when a repeated key's later value
                            // would replace it; header names are matched as written.
                            let Some(headers) = object(headers) else { continue };
                            for (name, value) in headers?.0 {
                                if !(value.get().starts_with('"') || value.get() == "null") {
                                    return Err(format!(
                                        "{at}.override_header.{name}: {} is not a string",
                                        value.get()
                                    ));
                                }
                            }
                        }
                    }
                }
                Native => {
                    if let Some(n) = object(v) {
                        known(&["web_search", "tool_search"], &n?, &at)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    let root: Members = serde_json::from_str(json).map_err(|e| e.to_string())?;
    for (i, models) in known(&SECTIONS, &root, "catalog")? {
        let section = SECTIONS[i];
        let Ok(models) = serde_json::from_str::<Vec<&RawValue>>(models.get()) else {
            continue;
        };
        for (n, model) in models.into_iter().enumerate() {
            if let Some(model) = object(model) {
                fields(&MODEL, &model?, &format!("{section}[{n}]"))?;
            }
        }
    }
    Ok(())
}

/// Go `json.Unmarshal` into `staticModelsJSON`: every model of its sections decodes.
fn decode_catalog(root: &Map<String, Value>) -> Result<(), String> {
    for section in SECTIONS {
        match root.get(section) {
            None | Some(Value::Null) => {}
            Some(Value::Array(models)) => {
                for model in models {
                    match model {
                        Value::Null => {}
                        Value::Object(m) => {
                            go_model_json(m).map_err(|e| format!("decode models catalog: {section}: {e}"))?;
                        }
                        other => return Err(format!("decode models catalog: {section}: cannot unmarshal {other}")),
                    }
                }
            }
            Some(other) => return Err(format!("decode models catalog: {section}: cannot unmarshal {other}")),
        }
    }
    Ok(())
}

/// Dated and bracketed Claude variants (`claude-sonnet-5-5-20261001`,
/// `claude-opus-5-5[1m]`) share their catalog entry's capabilities (Go
/// `claudeFamilyModelIDs`): the ID without a `[...]` suffix, then also without a
/// `-YYYYMMDD` date.
pub(crate) fn claude_family_ids(id: &str) -> Vec<&str> {
    let mut candidates = Vec::new();
    if !id.starts_with("claude-") {
        return candidates;
    }
    let mut base = id;
    if let Some(i) = base.find('[').filter(|&i| i > 0 && base.ends_with(']')) {
        base = &base[..i];
        candidates.push(base);
    }
    let n = base.len();
    if n > 9 && base.as_bytes()[n - 9] == b'-' && base.as_bytes()[n - 8..].iter().all(u8::is_ascii_digit) {
        candidates.push(&base[..n - 9]);
    }
    candidates
}

/// A snapshot of the current static catalog. Readers can keep it across a refresh.
pub fn pinned() -> Arc<Catalog> {
    CURRENT
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

static CURRENT: LazyLock<RwLock<Arc<Catalog>>> = LazyLock::new(|| {
    let embedded = Catalog::parse(MODELS_JSON).expect("embedded models.json is valid");
    RwLock::new(Arc::new(embedded))
});
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bumped whenever [`refresh_catalog`] swaps in a different catalog; caches derived from
/// [`pinned`] key on it.
pub fn catalog_generation() -> u64 {
    GENERATION.load(std::sync::atomic::Ordering::Acquire)
}

/// Go `validateModelsCatalog`: in the required sections every model is an object with a
/// non-empty, unique (trimmed) `id`. Empty or missing sections are allowed.
fn validate_catalog(root: &Map<String, Value>) -> Result<(), String> {
    const REQUIRED: [&str; 7] = [
        "claude",
        "codex-free",
        "codex-team",
        "codex-plus",
        "codex-pro",
        "xai",
        "meta",
    ];
    for section in REQUIRED {
        let models = match root.get(section) {
            None | Some(Value::Null) => continue,
            Some(Value::Array(models)) => models,
            Some(_) => return Err(format!("{section} is not a list")),
        };
        let mut seen = std::collections::HashSet::new();
        for (i, model) in models.iter().enumerate() {
            let Some(model) = model.as_object() else {
                return Err(format!("{section}[{i}] is null"));
            };
            // The ID Go's typed decode reads.
            let decoded = go_model_json(model).map_err(|e| format!("{section}[{i}]: {e}"))?;
            let id = decoded.get("id").and_then(Value::as_str).unwrap_or_default().trim();
            if id.is_empty() {
                return Err(format!("{section}[{i}] has empty id"));
            }
            if !seen.insert(id.to_owned()) {
                return Err(format!("{section} contains duplicate model id {id:?}"));
            }
        }
    }
    Ok(())
}

/// Go `detectChangedProviders` for the retained providers: those whose section differs.
/// The Codex tiers are one provider.
fn changed_providers(old: &Catalog, new: &Catalog) -> Vec<String> {
    const SECTIONS: [(&str, &str); 7] = [
        ("claude", "claude"),
        ("codex", "codex-free"),
        ("codex", "codex-team"),
        ("codex", "codex-plus"),
        ("codex", "codex-pro"),
        ("xai", "xai"),
        ("meta", "meta"),
    ];
    let mut changed: Vec<String> = Vec::new();
    for (provider, section) in SECTIONS {
        if changed.iter().any(|p| p == provider) {
            continue;
        }
        let raws = |c: &Catalog| c.channel(section).iter().map(|m| m.raw.clone()).collect::<Vec<_>>();
        if raws(old) != raws(new) {
            changed.push(provider.to_owned());
        }
    }
    changed
}

/// Go `validateCatalogBytes`: whether [`refresh_catalog`] would accept `json`.
pub fn validate_catalog_json(json: &str) -> Result<(), String> {
    checked_catalog(json).map(drop)
}

fn checked_catalog(json: &str) -> Result<Map<String, Value>, String> {
    let root: Map<String, Value> = serde_json::from_str(json).map_err(|e| e.to_string())?;
    check_catalog_members(json)?;
    decode_catalog(&root)?;
    validate_catalog(&root)?;
    Ok(root)
}

/// Go `tryRefreshModels` after a fetch: parses and validates a remote models.json,
/// keeps the current `meta` section when the remote one is empty, and swaps it in when
/// it differs. Returns the changed providers (Go's refresh callback argument).
pub fn refresh_catalog(json: &str) -> Result<Vec<String>, String> {
    // Serialize refreshes so the Meta fallback and comparison use the catalog we replace.
    // Readers can still pin that catalog while we prepare its successor.
    static REFRESH: Mutex<()> = Mutex::new(());
    let mut root = checked_catalog(json)?;
    let _refresh = REFRESH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let old = pinned();
    if root.get("meta").and_then(Value::as_array).is_none_or(Vec::is_empty) && !old.channel("meta").is_empty() {
        let meta = old
            .channel("meta")
            .iter()
            .map(|m| Value::Object(m.raw.clone()))
            .collect();
        root.insert("meta".into(), Value::Array(meta));
    }
    let new = Catalog::from_root(root).map_err(|e| e.to_string())?;
    let changed = changed_providers(&old, &new);
    let same =
        old.channels.len() == new.channels.len()
            && old.channels.iter().zip(&new.channels).all(|((a, am), (b, bm))| {
                a == b && am.len() == bm.len() && am.iter().zip(bm).all(|(x, y)| x.raw == y.raw)
            });
    if !same {
        let mut current = CURRENT.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        *current = Arc::new(new);
        GENERATION.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
    Ok(changed)
}

/// Look up a public model in the request's registrations, then in the pinned catalog.
pub fn lookup_model(registry: Option<&dynamic::Registry>, id: &str, provider: Option<&str>) -> Option<ModelInfo> {
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    registry
        .and_then(|registry| registry.info(id, provider))
        .map(|spec| spec.to_info())
        .or_else(|| pinned().lookup(id).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(json: &str) -> Catalog {
        Catalog::parse(json).unwrap()
    }

    /// Go TestLookupStaticModelInfoClaudeVariantsUseCatalogEntry (claude_family_lookup_test.go).
    #[test]
    fn claude_variants_use_their_catalog_entry() {
        let c = catalog(MODELS_JSON);
        for (id, family) in [
            ("claude-opus-5-5[1m]", "claude-opus-5-5"),
            ("claude-opus-5-5-20261001", "claude-opus-5-5"),
            ("claude-opus-5-5-20261001[1m]", "claude-opus-5-5"),
            ("claude-sonnet-5-5[1m]", "claude-sonnet-5-5"),
            ("claude-sonnet-5-5-20261001", "claude-sonnet-5-5"),
            ("claude-fable-5[1m]", "claude-fable-5"),
            ("claude-fable-5-1[1m]", "claude-fable-5-1"),
            ("claude-fable-5-2[1m]", "claude-fable-5-2"),
            ("claude-fable-5-2-20261109", "claude-fable-5-2"),
            ("claude-haiku-4-5-20251001[1m]", "claude-haiku-4-5-20251001"),
        ] {
            assert_eq!(c.lookup(id).map(|m| m.id.as_str()), Some(family), "{id}");
        }
        assert_eq!(
            c.lookup("claude-haiku-4-5-20251001").map(|m| m.id.as_str()),
            Some("claude-haiku-4-5-20251001")
        );
        for id in [
            "claude-unknown-20261001",
            "claude-opus-5-5-2026",
            "claude-opus-5-5[1m",
            "gpt-5[1m]",
        ] {
            assert!(c.lookup(id).is_none(), "{id}");
        }
        assert_eq!(
            c.lookup("claude-sonnet-5-5")
                .and_then(|m| m.thinking.as_ref())
                .map(|t| t.disable_type.as_str()),
            Some("between_tools")
        );
    }

    /// Go TestDetectChangedProviders_CodexConfigurationUpdate: an internal-field-only
    /// change in a Codex tier reports `codex`.
    #[test]
    fn detect_changed_providers_codex_configuration_update() {
        let old = catalog(r#"{"codex-free":[{"id":"gpt-6-luna"}]}"#);
        let new = catalog(r#"{"codex-free":[{"id":"gpt-6-luna","support_configuration_update":true}]}"#);
        assert_eq!(changed_providers(&old, &new), ["codex"]);
    }

    /// Go TestDetectChangedProviders for the retained sections: a change reports its
    /// provider once, unchanged sections report nothing, and the removed providers'
    /// sections (issue #49) report nothing.
    #[test]
    fn detect_changed_providers_reports_retained_providers() {
        let old = catalog(r#"{"xai":[{"id":"g"}],"claude":[{"id":"c"}],"kimi":[{"id":"k"}]}"#);
        let new = catalog(r#"{"xai":[{"id":"g","display_name":"G"}],"claude":[{"id":"c"}],"kimi":[]}"#);
        assert_eq!(changed_providers(&old, &new), ["xai"]);
    }

    /// Retained provider catalogs require nonempty unique IDs; decoder error prose is not a contract.
    #[test]
    fn retained_catalog_sections_reject_invalid_model_identities() {
        let check = |value: Value| validate_catalog(value.as_object().unwrap());
        for section in ["claude", "codex-pro", "xai", "meta"] {
            assert!(check(serde_json::json!({section: []})).is_ok());
            for models in [
                serde_json::json!([null]),
                serde_json::json!([{"id": " "}]),
                serde_json::json!([{"id":"same"},{"id":" same "}]),
            ] {
                assert!(check(serde_json::json!({section: models})).is_err(), "{section}");
            }
        }
        assert!(
            check(serde_json::json!({"gemini-cli": [{"id":"x"},{"id":"x"}]})).is_ok(),
            "removed sections are not routed (#49)"
        );
    }

    /// Model field types and capabilities are checked semantically without JSON ordering or runtime coercion goldens.
    #[test]
    fn model_schema_preserves_capabilities_and_rejects_invalid_field_types() {
        let model = serde_json::json!({
            "id": "claude-test", "type": "claude", "created": 123,
            "display_name": "Test <model>", "inputTokenLimit": 4096,
            "supportedInputModalities": ["text", "image"],
            "thinking": {"min": 1024, "max": 2048, "levels": ["low", "high"]},
            "config": {"override_header": {"X-Test": "<value>"}},
            "native_capabilities": {"web_search": true}
        });
        let decoded = go_model_json(model.as_object().unwrap()).unwrap();
        for key in [
            "id",
            "type",
            "created",
            "display_name",
            "inputTokenLimit",
            "supportedInputModalities",
            "thinking",
            "config",
        ] {
            assert_eq!(decoded[key], model[key], "{key}");
        }
        let fields = [
            ("inputTokenLimit", serde_json::json!(1.5)),
            ("inputTokenLimit", serde_json::json!("5")),
            ("created", serde_json::json!(9223372036854775808u64)),
            ("supportedInputModalities", serde_json::json!([1])),
            ("config", serde_json::json!({"override_header": {"a":1}})),
            ("native_capabilities", serde_json::json!({"web_search":"yes"})),
            ("support_configuration_update", serde_json::json!(1)),
            ("thinking", serde_json::json!([])),
        ];
        for (field, value) in fields {
            let mut malformed = model.clone();
            malformed[field] = value;
            assert!(
                go_model_json(malformed.as_object().unwrap()).is_err(),
                "{field}: {malformed}"
            );
        }
        // Null optional fields and unknown extensions must not invalidate a catalog.
        let optional = go_model_json(
            serde_json::json!({"id":"optional", "display_name": null, "thinking":null, "extension": {"x":true}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(optional["id"], "optional");
        decode_catalog(&serde_json::from_str(MODELS_JSON).unwrap()).unwrap();
        check_catalog_members(MODELS_JSON).unwrap();
    }

    /// The embedded catalog has complete retained channels and valid model identities, without historical population counts.
    #[test]
    fn embedded_catalog_has_valid_unique_models_for_retained_channels() {
        let catalog = catalog(MODELS_JSON);
        let channels: std::collections::BTreeSet<_> = catalog.channels.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            channels,
            [
                "claude",
                "codex-free",
                "codex-team",
                "codex-plus",
                "codex-pro",
                "xai",
                "meta"
            ]
            .into()
        );
        for (channel, models) in &catalog.channels {
            assert!(!models.is_empty(), "{channel}: no built-in models");
            let mut ids = std::collections::HashSet::new();
            for model in models {
                assert!(!model.id.trim().is_empty(), "{channel}");
                assert!(ids.insert(&model.id), "{channel}: duplicate {}", model.id);
                assert_eq!(model.raw["id"], model.id);
            }
        }
    }

    /// Provider-specific lookup remains available when a fallback ID occurs in several catalogs.
    #[test]
    fn lookup_has_explicit_provider_priority_for_duplicate_ids() {
        let catalog = catalog(
            r#"{"meta":[{"id":"shared","type":"meta"}],"xai":[{"id":"shared","type":"xai"}],"codex-pro":[{"id":"shared","type":"codex"}],"claude":[{"id":"shared","type":"claude"}]}"#,
        );
        assert_eq!(catalog.lookup("shared").unwrap().kind, "claude");
        assert_eq!(catalog.channel("xai")[0].kind, "xai");
        assert!(catalog.lookup("").is_none());
        assert!(catalog.lookup("missing").is_none());
        assert!(pinned().lookup("gemini-3-flash").is_none());
        assert!(pinned().lookup("kimi-k2.5").is_none());
    }

    #[test]
    fn raw_definition_preserves_model_fields_and_thinking_metadata() {
        let catalog = pinned();
        let haiku = &catalog.channel("claude")[0];
        for key in ["id", "object", "created", "owned_by"] {
            assert!(haiku.raw.contains_key(key), "missing {key}");
        }
        assert_eq!(haiku.raw["id"], haiku.id);
        assert_eq!(haiku.raw["thinking"]["min"], haiku.thinking.as_ref().unwrap().min);
        assert!(haiku.thinking.as_ref().unwrap().min > 0);
    }
}
