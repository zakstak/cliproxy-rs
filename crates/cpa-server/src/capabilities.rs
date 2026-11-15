//! Per-attempt model capability binding (Go sdk/cliproxy/auth/api_key_model_capabilities.go).
//!
//! Go's conductor binds the capabilities of the model an attempt actually sends upstream
//! to the execution request (`attachResolvedExecutionModelInfo`), so executors read
//! `ResolvedModelInfo(req)` instead of re-resolving config entries. In order:
//! - a configured API-key model (`lookupAPIKeyModelCapability`), routed by the client's
//!   model (alias or name, with and without a thinking suffix) and the selected upstream;
//! - any Codex API-key model the credential's entry does not list
//!   (`lookupUnlistedCodexAPIKeyModelCapability`);
//! - a Codex OAuth credential's plan catalog model (`lookupCodexOAuthModelCapability`).
//!
//! The result is [`cpa_core::exec::ExecRequest::resolved_model`].

use cpa_common::gostr::GoStr;
use cpa_common::thinking::parse_suffix;
use cpa_core::config::Config;
use cpa_core::config::credentials;
use cpa_core::credential::Credential;
use cpa_core::exec::{ResolvedModel, ResolvedSource};
use cpa_core::registry::dynamic::{self, ConfigModel};
use cpa_core::registry::{ModelInfo, ThinkingSupport, pinned};
use serde_json::{Map, Value};

/// Go `attachResolvedExecutionModelInfo`. `execution_model` is set when the attempt
/// executes the request's own model rather than the selection's (Go
/// `restoreExecutionModel`): then only Codex binds, routing by that model.
// ponytail: compiled per attempt from the config snapshot (Go precompiles a routing
// table on every config or credential change); entries are small. Cache it on
// `Config::derived` if profiling ever shows it.
pub fn resolve_attempt(
    cfg: &Config,
    c: &Credential,
    route_model: &str,
    upstream_model: &str,
    execution_model: Option<&str>,
) -> Option<ResolvedModel> {
    match execution_model {
        Some(_) if !c.provider.trim().go_eq_fold("codex") => None,
        Some(model) => resolve(cfg, c, model, model),
        None => resolve(cfg, c, route_model, upstream_model),
    }
}

/// Go `attachResolvedAPIKeyModelInfo` for a fresh attempt.
pub fn resolve(cfg: &Config, c: &Credential, route_model: &str, upstream_model: &str) -> Option<ResolvedModel> {
    let api_key = configured(cfg, c, route_model, upstream_model).or_else(|| unlisted_codex(cfg, c, upstream_model));
    if let Some(info) = api_key {
        return Some(ResolvedModel {
            info,
            source: ResolvedSource::ApiKey,
        });
    }
    codex_oauth(c, upstream_model).map(|info| ResolvedModel {
        info,
        source: ResolvedSource::CodexOAuth,
    })
}

/// Go `attachResolvedHomeModelInfo`, after the local binding of an attempt on a Home
/// credential that executes the selection's model: Home's definition of the dispatched
/// model ([`crate::remote::MODEL_INFO`]) is the attempt's model (Go's
/// `ResolvedModelInfo` reads it first). Its configuration-update support is Home's
/// explicit flag, else `local`'s when that names the same model; Go's stream path binds
/// Home's model before the local one, so callers pass no `local` there. Its is-compat
/// is the selected `credential_options` entry's, never `local`'s. `None` without a Home
/// definition: the local binding stays.
pub fn bind_home(c: &Credential, local: Option<&ResolvedModel>) -> Option<ResolvedModel> {
    let wire: Map<String, Value> = serde_json::from_str(c.attributes.get(crate::remote::MODEL_INFO)?).ok()?;
    let mut info = home_model_info(&wire)?;
    let base = |id: &str| parse_suffix(id.trim()).model_name.trim().to_owned();
    let flag = |info: &ModelInfo, key: &str| info.raw.get(key) == Some(&Value::Bool(true));
    let support = match wire.get("support_configuration_update").and_then(Value::as_bool) {
        Some(explicit) => explicit,
        None => local.is_some_and(|local| {
            base(&local.info.id).go_eq_fold(&base(&info.id)) && flag(&local.info, "support_configuration_update")
        }),
    };
    set_flag(&mut info, "support_configuration_update", support);
    if let Some(options) = c.attributes.get(credentials::HOME_MODEL_OPTIONS) {
        let options: Map<String, Value> = serde_json::from_str(options).unwrap_or_default();
        set_flag(
            &mut info,
            "is_compat",
            options.get("is-compat") == Some(&Value::Bool(true)),
        );
    }
    Some(ResolvedModel {
        info,
        source: ResolvedSource::Home,
    })
}

/// Go `homeDispatchModelInfo.registryModelInfo`: nil without an ID; the context limits,
/// thinking, native capabilities and user-defined flag carried over; never is-compat.
fn home_model_info(wire: &Map<String, Value>) -> Option<ModelInfo> {
    let text = |key: &str| {
        wire.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let id = text("id");
    if id.is_empty() {
        return None;
    }
    let mut raw = Map::new();
    raw.insert("id".into(), id.into());
    let kind = text("type");
    if !kind.is_empty() {
        raw.insert("type".into(), kind.into());
    }
    for key in [
        "inputTokenLimit",
        "outputTokenLimit",
        "context_length",
        "max_completion_tokens",
    ] {
        if let Some(n) = wire.get(key).and_then(Value::as_i64).filter(|n| *n != 0) {
            raw.insert(key.into(), n.into());
        }
    }
    for key in ["thinking", "native_capabilities"] {
        if let Some(value) = wire.get(key).filter(|v| !v.is_null()) {
            raw.insert(key.into(), go_zero_nulls(value));
        }
    }
    if wire.get("user_defined") == Some(&Value::Bool(true)) {
        raw.insert("user_defined".into(), true.into());
    }
    ModelInfo::from_raw(raw).ok()
}

/// Go decodes a JSON null as the zero value: a null field of `ThinkingSupport` or
/// `NativeCapabilities` stays unset, and a null `levels` element (their only list, of
/// strings) is "". Rust's decoder rejects nulls, so drop or zero them first.
fn go_zero_nulls(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k.clone(), go_zero_nulls(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| match v {
                    Value::Null => Value::String(String::new()),
                    v => go_zero_nulls(v),
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn attr<'a>(c: &'a Credential, key: &str) -> &'a str {
    c.attributes.get(key).map_or("", |v| v.trim())
}

/// `isConfiguredModelRoutingAuth`: API keys (Go also counts config-sourced
/// OpenAI-compatible entries, removed with that provider, issue #49).
fn configured_routing(c: &Credential) -> bool {
    dynamic::is_api_key(c)
}

/// One configured model as Go compiles it (`addConfiguredModelCapability` inputs).
struct Configured {
    name: String,
    alias: String,
    thinking: Option<ThinkingSupport>,
    is_compat: bool,
    /// Codex only: `SupportConfigurationUpdate` overrides the static definition.
    support_configuration_update: Option<bool>,
}

/// `compileAPIKeyModelCapabilitiesForAuth`: the credential's config entry models, with
/// Go's model type per provider.
fn entry_models(cfg: &Config, c: &Credential) -> Option<(Vec<Configured>, &'static str)> {
    let provider = c.provider.trim().go_lower();
    let (family, kind) = match provider.as_str() {
        "claude" => ("claude", "claude"),
        "codex" => ("codex", "codex"),
        "xai" => ("xai", "xai"),
        "meta" => ("meta", "meta"),
        _ => return None,
    };
    let entry = credentials::resolve_api_key_entry(cfg, family, c)?;
    let models = entry
        .models
        .iter()
        .filter_map(|m| serde_json::from_value::<ConfigModel>(m.clone()).ok())
        .map(|m| Configured {
            name: m.name,
            alias: m.alias,
            thinking: m.thinking,
            is_compat: m.is_compat,
            support_configuration_update: (family == "codex").then_some(m.support_configuration_update),
        })
        .collect();
    Some((models, kind))
}

/// `modelAliasLookupCandidates`: the model, then its suffix-free base when different.
fn candidates(model: &str) -> Vec<String> {
    let model = model.trim();
    if model.is_empty() {
        return Vec::new();
    }
    let base = parse_suffix(model).model_name;
    let base = if base.is_empty() { model.to_owned() } else { base };
    if base == model {
        vec![model.to_owned()]
    } else {
        vec![model.to_owned(), base]
    }
}

/// `configuredUpstreamFallbackMatches`: a suffix-free configured name matches the
/// selection's base.
fn fallback_matches(configured: &str, selected: &str) -> bool {
    let configured = parse_suffix(configured.trim());
    !configured.has_suffix
        && configured
            .model_name
            .trim()
            .go_eq_fold(parse_suffix(selected.trim()).model_name.trim())
}

/// `lookupAPIKeyModelCapability` over the routes `addConfiguredModelCapability` builds:
/// each model is keyed by its alias and name candidates (lowercased); a key keeps one
/// route per upstream name, first model first.
fn configured(cfg: &Config, c: &Credential, route_model: &str, upstream_model: &str) -> Option<ModelInfo> {
    if !configured_routing(c) {
        return None;
    }
    let (models, kind) = entry_models(cfg, c)?;
    // (key, upstream name, model index) in insertion order.
    let mut routes: Vec<(String, String, usize)> = Vec::new();
    for (index, m) in models.iter().enumerate() {
        let (mut name, mut alias) = (m.name.trim(), m.alias.trim());
        if name.is_empty() {
            name = alias;
        }
        if alias.is_empty() {
            alias = name;
        }
        if name.is_empty() {
            continue;
        }
        let mut seen: Vec<String> = Vec::new();
        for candidate in [alias, name].into_iter().flat_map(candidates) {
            let key = candidate.trim().go_lower();
            if key.is_empty() || seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            if !routes.iter().any(|(k, up, _)| *k == key && up.go_eq_fold(name)) {
                routes.push((key, name.to_owned(), index));
            }
        }
    }
    let requested = dynamic::strip_prefix(route_model.trim(), c);
    let matched: Vec<(&str, usize)> = candidates(requested)
        .iter()
        .flat_map(|candidate| {
            let key = candidate.trim().go_lower();
            routes
                .iter()
                .filter(move |(k, ..)| *k == key)
                .map(|(_, up, i)| (up.as_str(), *i))
        })
        .collect();
    let selected = upstream_model.trim();
    let (name, index) = matched
        .iter()
        .find(|(up, _)| up.trim().go_eq_fold(selected))
        .or_else(|| matched.iter().find(|(up, _)| fallback_matches(up, selected)))?;
    let m = &models[*index];
    let mut info = resolve_model_info(name, kind, m.thinking.clone());
    set_flag(&mut info, "is_compat", m.is_compat);
    if let Some(support) = m.support_configuration_update {
        set_flag(&mut info, "support_configuration_update", support);
    }
    Some(info)
}

/// `lookupUnlistedCodexAPIKeyModelCapability`: every upstream model of a Codex API key
/// whose entry still matches its key and base URL binds; a configured name (exact or
/// suffix-free base) lends its thinking, configuration-update support and is-compat.
fn unlisted_codex(cfg: &Config, c: &Credential, upstream_model: &str) -> Option<ModelInfo> {
    let upstream = upstream_model.trim();
    if !dynamic::is_api_key(c) || !c.provider.trim().go_eq_fold("codex") || upstream.is_empty() {
        return None;
    }
    let entry = credentials::resolve_api_key_entry(cfg, "codex", c)?;
    let (key, base) = (attr(c, "api_key"), attr(c, "base_url"));
    let entry_base = entry.base_url.trim();
    if (key.is_empty() && base.is_empty())
        || !key.go_eq_fold(entry.api_key.trim())
        || (!entry_base.is_empty() && !base.go_eq_fold(entry_base))
    {
        return None;
    }
    let configured = entry
        .models
        .iter()
        .filter_map(|m| serde_json::from_value::<ConfigModel>(m.clone()).ok())
        .find(|m| m.name.trim().go_eq_fold(upstream) || fallback_matches(&m.name, upstream));
    let mut info = resolve_model_info(
        upstream_model,
        "codex",
        configured.as_ref().and_then(|m| m.thinking.clone()),
    );
    let (support, compat) = configured.map_or((false, false), |m| (m.support_configuration_update, m.is_compat));
    set_flag(&mut info, "support_configuration_update", support);
    set_flag(&mut info, "is_compat", compat);
    Some(info)
}

/// `lookupCodexOAuthModelCapability`: the plan catalog entry for the selection's
/// suffix-free model (`plan_type` plus, team/business/go, free; otherwise pro).
fn codex_oauth(c: &Credential, upstream_model: &str) -> Option<ModelInfo> {
    if dynamic::auth_kind(c) != Some("oauth") || !c.provider.trim().go_eq_fold("codex") {
        return None;
    }
    let channel = match attr(c, "plan_type").go_lower().as_str() {
        "plus" => "codex-plus",
        "team" | "business" | "go" => "codex-team",
        "free" => "codex-free",
        _ => "codex-pro",
    };
    let selected = parse_suffix(upstream_model.trim()).model_name;
    let selected = selected.trim();
    pinned()
        .channel(channel)
        .iter()
        .find(|m| m.id.go_eq_fold(selected))
        .cloned()
}

/// `modelconfig.ResolveModelInfo`: the static definition of the suffix-free name (Go
/// `LookupStaticModelInfo`), renamed to the configured name, typed for the provider,
/// configured thinking normalized, never user-defined.
fn resolve_model_info(name: &str, kind: &str, thinking: Option<ThinkingSupport>) -> ModelInfo {
    let name = name.trim();
    let base = parse_suffix(name).model_name;
    let mut info = pinned().lookup(base.trim()).cloned().unwrap_or_else(|| ModelInfo {
        id: String::new(),
        kind: String::new(),
        thinking: None,
        raw: Map::new(),
    });
    info.id = name.to_owned();
    info.raw.insert("id".into(), name.into());
    info.kind = kind.trim().to_owned();
    info.raw.insert("type".into(), info.kind.clone().into());
    if let Some(support) = thinking {
        let mut support = dynamic::normalize_thinking(support);
        // Configured levels replace the catalog's, but how a known model turns thinking
        // off is an upstream fact (Go ad9e456d).
        if support.disable_type.is_empty()
            && let Some(catalog) = &info.thinking
        {
            support.disable_type.clone_from(&catalog.disable_type);
        }
        info.raw.insert("thinking".into(), dynamic::thinking_value(&support));
        info.thinking = Some(support);
    }
    info.raw.remove("user_defined");
    info
}

/// Internal boolean metadata (`json:"-"` in Go) kept in `raw` only when true.
fn set_flag(info: &mut ModelInfo, key: &str, value: bool) {
    if value {
        info.raw.insert(key.into(), Value::Bool(true));
    } else {
        info.raw.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn configured_capabilities_bind_to_the_selected_entry_route_and_upstream() {
        let cfg = Config::parse(
            r#"config-version: 8
api-keys:
  claude:
    - base-url: https://claude.example.invalid
      models:
        - name: own-model
          alias: shared
          is-compat: true
          thinking:
            levels: [HIGH, none, high, ' auto ']
        - name: own-second
          alias: shared
        - alias: alias-only
      keys: [{api-key: fake-claude}]
    - base-url: https://claude.example.invalid
      headers: {X-Entry: second}
      models: [{name: second-entry, alias: entry-alias}]
      keys: [{api-key: fake-claude}]
  codex:
    - base-url: https://codex.example.invalid
      prefix: team
      models:
        - name: own-codex
          alias: assistant
          support-configuration-update: true
          thinking: {min: 1, max: 9}
      keys: [{api-key: fake-codex}]
  xai:
    - base-url: https://xai.example.invalid
      models: [{name: own-xai, alias: research}]
      keys: [{api-key: fake-xai}]
  meta:
    - models: [{name: own-meta, alias: social}]
      keys: [{api-key: fake-meta}]
"#,
        )
        .unwrap();
        let all = credentials::from_config(&cfg);
        let first = |provider: &str| {
            all.iter()
                .find(|c| c.provider == provider)
                .unwrap_or_else(|| panic!("missing configured {provider} credential"))
        };
        for (provider, route, upstream) in [
            ("claude", "shared", "own-model"),
            ("claude", "shared(high)", "own-model(high)"),
            ("claude", "OWN-MODEL", "own-model"),
            ("claude", "alias-only", "alias-only"),
            ("codex", "team/assistant", "own-codex"),
            ("xai", "research", "own-xai"),
            ("meta", "social", "own-meta"),
        ] {
            let got = resolve_attempt(&cfg, first(provider), route, upstream, None).unwrap();
            assert_eq!(got.source, ResolvedSource::ApiKey);
            assert_eq!(got.info.kind, provider);
            let expected = if provider == "claude" && route == "alias-only" {
                "alias-only"
            } else if provider == "claude" {
                "own-model"
            } else {
                upstream
            };
            assert_eq!(got.info.id, expected);
            assert!(!cpa_common::thinking::ModelCaps::from(&got.info).user_defined);
            assert_eq!(got.is_compat(), provider == "claude" && route != "alias-only");
        }
        let configured = resolve(&cfg, first("claude"), "shared", "own-model").unwrap();
        let thinking = configured.info.thinking.unwrap();
        assert_eq!(thinking.levels, ["high", "none", "auto"]);
        assert!(thinking.zero_allowed && thinking.dynamic_allowed);
        let second = resolve(&cfg, first("claude"), "shared", "own-second").unwrap();
        assert_eq!(second.info.id, "own-second");
        assert!(!second.is_compat());
        assert!(resolve(&cfg, first("claude"), "shared", "unrelated").is_none());
        assert!(resolve(&cfg, first("claude"), "missing", "own-model").is_none());
        let second_entry = all
            .iter()
            .find(|c| c.provider == "claude" && c.attributes["config_index"] == "1")
            .unwrap();
        assert_eq!(
            resolve(&cfg, second_entry, "entry-alias", "second-entry")
                .unwrap()
                .info
                .id,
            "second-entry"
        );
        assert!(
            resolve(&cfg, second_entry, "shared", "own-model").is_none(),
            "a matching key does not borrow another configured entry's capabilities"
        );
        let mut stale = second_entry.clone();
        stale.attributes.insert("config_index".into(), "999".into());
        assert_eq!(
            resolve(&cfg, &stale, "shared", "own-model").unwrap().info.id,
            "own-model",
            "a stale entry index resolves by key/base URL"
        );
        // Restoring an execution model binds Codex only; it must not borrow a Claude route.
        assert!(resolve_attempt(&cfg, first("claude"), "shared", "own-model", Some("own-model")).is_none());
        let codex = resolve_attempt(&cfg, first("codex"), "unrelated", "ignored", Some("own-codex")).unwrap();
        assert_eq!(codex.info.id, "own-codex");
        assert!(cpa_common::thinking::ModelCaps::from(&codex.info).support_configuration_update);
        let unknown = resolve(&cfg, first("codex"), "unlisted", "future-codex").unwrap();
        assert_eq!(unknown.info.id, "future-codex");
        assert_eq!(unknown.source, ResolvedSource::ApiKey);
        assert!(!unknown.is_compat());
        let mut wrong = first("codex").clone();
        wrong.attributes.insert("api_key".into(), "unmatched".into());
        assert!(resolve(&cfg, &wrong, "unlisted", "future-codex").is_none());
        let mut no_match = first("claude").clone();
        no_match.attributes.insert("api_key".into(), "unmatched".into());
        assert!(resolve(&cfg, &no_match, "shared", "own-model").is_none());
    }

    #[test]
    fn codex_oauth_capabilities_follow_plan_catalog_without_borrowing_api_key_configuration() {
        let cfg = Config::default();
        for plan in ["pro", "plus", "team", "business", "go", "free", "unknown"] {
            let mut credential = Credential::from_file(
                Path::new("/fake"),
                Path::new("/fake/codex.json"),
                serde_json::json!({"type":"codex","access_token":"fake-token"})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap();
            credential.attributes.insert("plan_type".into(), plan.into());
            let got = resolve(&cfg, &credential, "client-alias", "gpt-5.5(high)").unwrap();
            assert_eq!(got.source, ResolvedSource::CodexOAuth);
            assert_eq!(got.info.id, "gpt-5.5");
            assert!(!got.is_compat());
            assert!(got.info.thinking.is_some());
            assert!(resolve(&cfg, &credential, "client-alias", "nonexistent-codex-model").is_none());
        }
        let other = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/claude.json"),
            serde_json::json!({"type":"claude","access_token":"fake-token"})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert!(resolve(&cfg, &other, "gpt-5.5", "gpt-5.5").is_none());
    }
    /// Go `TestResolveModelInfoConfiguredThinkingKeepsCatalogDisableType`: configured levels
    /// replace the catalog's, but an empty `disable-type` keeps the catalog's way of turning
    /// thinking off; an explicit one wins.
    #[test]
    fn configured_thinking_keeps_the_catalog_disable_type() {
        let support = |yaml: &str| serde_yaml_ng::from_str::<ThinkingSupport>(yaml).unwrap();
        let configured = support("levels: [low, medium, high, none]");
        let info = resolve_model_info("claude-sonnet-5-5", "claude", Some(configured.clone()));
        let thinking = info.thinking.as_ref().unwrap();
        assert_eq!(thinking.disable_type, "between_tools");
        assert_eq!(thinking.levels, ["low", "medium", "high", "none"]);
        let body = br#"{"model":"claude-sonnet-5-5","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#;
        let caps = cpa_common::thinking::ModelCaps::from(&info);
        let out = cpa_common::thinking::apply_thinking_with_model_info(
            body,
            &[],
            "claude-sonnet-5-5",
            "claude",
            "claude",
            "claude",
            Some(&caps),
        )
        .unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["thinking"], serde_json::json!({"type": "between_tools"}), "{out}");

        // Configured capabilities are authoritative: a model the proxy knows refuses
        // `disabled` still gets it once its configuration allows turning thinking off.
        let opus = resolve_model_info("claude-opus-5-5", "claude", Some(support("levels: [low, none]")));
        let body = br#"{"model":"claude-opus-5-5","messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#;
        let caps = cpa_common::thinking::ModelCaps::from(&opus);
        let out = cpa_common::thinking::apply_thinking_with_model_info(
            body,
            &[],
            "claude-opus-5-5",
            "claude",
            "claude",
            "claude",
            Some(&caps),
        )
        .unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(out["thinking"], serde_json::json!({"type": "disabled"}), "{out}");

        let explicit = support("levels: [low]\ndisable-type: disabled");
        let explicit = resolve_model_info("claude-sonnet-5-5", "claude", Some(explicit));
        assert_eq!(explicit.thinking.unwrap().disable_type, "disabled");
        let other = resolve_model_info("claude-opus-5", "claude", Some(configured.clone()));
        assert_eq!(other.thinking.unwrap().disable_type, "");
        let variant = resolve_model_info("claude-sonnet-5-5[1m]", "claude", Some(configured));
        assert_eq!(variant.thinking.unwrap().disable_type, "between_tools");
    }
}
