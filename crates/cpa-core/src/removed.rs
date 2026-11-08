//! The upstream providers this fork removed (issue #49) and the migration errors for
//! their settings, credential files and operations.
//!
//! The fork keeps four built-in upstreams: Claude, Codex, Grok (`xai`) and Muse (`meta`).
//! A removed provider's setting is refused when the config loads, reloads or is written
//! through the Management API, and its saved logins stay on disk untouched but are not
//! loaded. Nothing falls back to another provider.

use serde_yaml_ng::Value;

use crate::gostr::GoStr;

/// The built-in upstream providers, by credential `type`.
pub const SUPPORTED: [&str; 4] = ["claude", "codex", "xai", "meta"];

/// One removed integration: its display name and the provider names (credential `type`,
/// config channel) that select it.
pub struct Removed {
    pub name: &'static str,
    pub providers: &'static [&'static str],
}

/// Every removed integration.
pub const REMOVED: [Removed; 7] = [
    Removed {
        name: "Kimi",
        providers: &["kimi", "kimi-ai", "kimi.ai", "kimi.com"],
    },
    Removed {
        name: "Devin",
        providers: &["devin"],
    },
    Removed {
        name: "Gemini",
        providers: &["gemini", "gemini-cli", "gemini-interactions", "interactions"],
    },
    Removed {
        name: "Vertex AI",
        providers: &["vertex"],
    },
    Removed {
        name: "Google AI Studio",
        providers: &["aistudio"],
    },
    Removed {
        name: "Antigravity",
        providers: &["antigravity"],
    },
    Removed {
        name: "OpenAI-compatible upstreams",
        providers: &["openai-compatibility"],
    },
];

/// The removed integration a provider name selects (Go's `strings.ToLower` fold), if
/// any. `openai-compatible-<name>` keys select the OpenAI-compatible integration.
pub fn provider(name: &str) -> Option<&'static Removed> {
    let name = name.trim().go_lower();
    if name.starts_with("openai-compatible-") {
        return REMOVED.iter().find(|r| r.providers.contains(&"openai-compatibility"));
    }
    REMOVED.iter().find(|r| r.providers.contains(&name.as_str()))
}

/// The guidance every removal error ends with.
pub const GUIDANCE: &str = "this build supports only the Claude, Codex, Grok (xai) and Muse (meta) upstreams \
    (see docs/DIFFERENCES-FROM-GO.md, \"Removed providers\")";

/// The error for a request to log in to, refresh or otherwise operate a removed provider.
pub fn operation_error(provider: &str, operation: &str) -> String {
    let name = self::provider(provider).map_or(provider, |r| r.name);
    format!("{operation} for {name} is no longer supported: {GUIDANCE}")
}

/// The warning for a saved login of a removed provider, which stays on disk unread.
pub fn credential_error(provider: &str) -> String {
    let name = self::provider(provider).map_or(provider, |r| r.name);
    format!(
        "{name} credentials are no longer loaded; the file is kept unchanged. Remove it or move it out of \
         auth-dir to silence this warning; {GUIDANCE}"
    )
}

/// Settings of the removed providers in the canonical v8 document, with the legacy
/// spelling that moves there.
const SETTINGS: &[(&[&str], &str)] = &[
    (&["api-keys", "gemini"], "gemini-api-key"),
    (&["api-keys", "interactions"], "interactions-api-key"),
    (&["api-keys", "vertex"], "vertex-api-key"),
    (&["api-keys", "openai-compatibility"], "openai-compatibility"),
    (
        &["oauth", "providers", "antigravity"],
        "antigravity, antigravity-signature-*, quota-exceeded.antigravity-credits",
    ),
    (&["oauth", "providers", "devin"], "devin"),
    (&["oauth", "providers", "aistudio"], "ws-auth"),
    (&["oauth", "providers", "kimi"], ""),
    (&["oauth", "providers", "gemini"], ""),
    (&["oauth", "providers", "gemini-cli"], ""),
    (&["oauth", "providers", "vertex"], ""),
    (&["models", "devin-catalog"], ""),
];

/// Whether a setting is present with a value that would do something: not null, not
/// an empty list, map or string, and not `false`.
fn set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.trim().is_empty(),
        Value::Sequence(s) => !s.is_empty(),
        Value::Mapping(m) => m.values().any(set),
        Value::Tagged(t) => set(&t.value),
        Value::Number(_) => true,
    }
}

/// Refuses a canonical v8 config document that configures a removed provider. Called
/// on load, reload and Management API writes, so the running config stays as it was.
pub fn config(doc: &Value) -> anyhow::Result<()> {
    for (path, legacy) in SETTINGS {
        let Some(value) = path.iter().try_fold(doc, |node, part| node.get(*part)) else {
            continue;
        };
        if !set(value) {
            continue;
        }
        let name = provider(path[path.len() - 1].trim_end_matches("-catalog")).map_or("a removed provider", |r| r.name);
        let legacy = if legacy.is_empty() {
            String::new()
        } else {
            format!(" (legacy `{legacy}`)")
        };
        anyhow::bail!(
            "{}{legacy} configures {name}, which is no longer supported: remove the setting; {GUIDANCE}",
            path.join(".")
        );
    }
    // Every provider-keyed OAuth map must enforce the same supported channels.
    for path in [
        ["oauth", "model-alias"],
        ["oauth", "excluded-models"],
        ["oauth", "request-scoped-errors"],
        ["oauth", "settings"],
    ] {
        let Some(Value::Mapping(map)) = path.iter().try_fold(doc, |node, part| node.get(*part)) else {
            continue;
        };
        for (key, value) in map {
            let Some(key) = key.as_str() else { continue };
            if let Some(removed) = provider(key)
                && set(value)
            {
                anyhow::bail!(
                    "{}.{key} configures {}, which is no longer supported: remove the entry; {GUIDANCE}",
                    path.join("."),
                    removed.name
                );
            }
        }
    }
    Ok(())
}

/// The legacy root spellings the loader moves to a [`SETTINGS`] path
/// (`config::document` `PATHS` and `FAMILIES`).
const LEGACY_SETTINGS: &[&[&str]] = &[
    &["gemini-api-key"],
    &["interactions-api-key"],
    &["vertex-api-key"],
    &["openai-compatibility"],
    &["antigravity"],
    &["antigravity-signature-cache-enabled"],
    &["antigravity-signature-bypass-strict"],
    &["quota-exceeded", "antigravity-credits"],
    &["devin"],
    &["ws-auth"],
];

/// The provider-keyed OAuth maps, canonical and legacy; an entry whose key selects a
/// removed provider (folded as Go folds channel names) is that provider's setting.
const KEYED_MAPS: &[&[&str]] = &[
    &["oauth", "model-alias"],
    &["oauth", "excluded-models"],
    &["oauth", "request-scoped-errors"],
    &["oauth", "settings"],
    &["oauth-model-alias"],
    &["oauth-excluded-models"],
    &["oauth-request-scoped-errors"],
    &["oauth-settings"],
];

/// Whether `path`, mapping keys from the document root, names a removed provider's
/// setting: a canonical v8 path, a legacy spelling the loader moves there, or a removed
/// channel's entry in a provider-keyed OAuth map. The match is exact: the same key
/// anywhere else (`requests.payload.override[0].params.metadata.gemini`, a `metadata`
/// map) is user data.
pub fn is_setting_path<S: AsRef<str>>(path: &[S]) -> bool {
    let parts: Vec<&str> = path.iter().map(AsRef::as_ref).collect();
    SETTINGS.iter().any(|(setting, _)| *setting == parts.as_slice())
        || LEGACY_SETTINGS.contains(&parts.as_slice())
        || parts
            .split_last()
            .is_some_and(|(key, map)| KEYED_MAPS.contains(&map) && provider(key).is_some())
}

/// The removed providers' settings in `doc`, as exact key paths in document order. With
/// `active_only`, only those holding a value that would do something (what [`config`]
/// refuses); otherwise empty and `false` ones too. Only root mappings down to the
/// deepest setting path are walked, so the cost does not grow with nested user data.
#[doc(hidden)]
pub fn setting_paths(doc: &Value, active_only: bool) -> Vec<Vec<String>> {
    const DEPTH: usize = 3;
    fn walk(node: &Value, at: &mut Vec<String>, active_only: bool, out: &mut Vec<Vec<String>>) {
        let Value::Mapping(map) = node else { return };
        for (key, value) in map {
            let Some(key) = key.as_str() else { continue };
            at.push(key.to_owned());
            if is_setting_path(at) {
                if !active_only || set(value) {
                    out.push(at.clone());
                }
            } else if at.len() < DEPTH {
                walk(value, at, active_only, out);
            }
            at.pop();
        }
    }
    let mut out = Vec::new();
    walk(doc, &mut Vec::new(), active_only, &mut out);
    out
}

/// Test support for Go goldens recorded with settings of the removed providers: `yaml`
/// without each removed setting that holds a value ([`setting_paths`] with
/// `active_only`), so the rest of the recorded document still loads. Removed settings
/// left empty or `false` stay, as the loader accepts them.
///
/// Each setting goes at its exact path through the syntax tree, block or flow, so every
/// other byte stays: comments, quoting, the same key names under other parents, and
/// string values that only look like settings. A mapping left empty reads as `{}`, not
/// null, as its Go source would have it. Panics when the edited text does not read as
/// the original document without exactly those settings.
#[doc(hidden)]
pub fn strip_settings(yaml: &str) -> String {
    strip(yaml, true)
}

/// [`strip_settings`] for every removed setting, empty and `false` ones too: for a
/// comparison of what the retained settings do, where an accepted inert one still
/// shows (a reload summary's `ws-auth: true -> false`).
#[doc(hidden)]
pub fn strip_every_setting(yaml: &str) -> String {
    strip(yaml, false)
}

fn strip(yaml: &str, active_only: bool) -> String {
    let Ok(doc) = crate::config::yaml_value(yaml) else {
        return yaml.to_owned();
    };
    let paths = setting_paths(&doc, active_only);
    if paths.is_empty() {
        return yaml.to_owned();
    }
    let file: yaml_edit::YamlFile = yaml.parse().expect("the syntax tree reads what serde reads");
    let root = file
        .document()
        .and_then(|d| d.as_mapping())
        .expect("a document holding settings is a mapping");
    let mut want = doc;
    for path in &paths {
        let (last, parents) = path.split_last().expect("setting paths are not empty");
        let parent = parents
            .iter()
            .try_fold(root.clone(), |node, key| node.get_mapping(key.as_str()))
            .unwrap_or_else(|| panic!("{}: not a plain mapping path in the text", path.join(".")));
        assert!(
            parent.remove(last.as_str()).is_some(),
            "{}: not removed from the text",
            path.join(".")
        );
        let parent = parents
            .iter()
            .try_fold(&mut want, |node, key| node.get_mut(key.as_str()))
            .and_then(Value::as_mapping_mut)
            .expect("the path was found in the document");
        parent.remove(last.as_str());
    }
    let out = file.to_string();
    // A document of removed settings only is left empty, which reads as null.
    let got = crate::config::yaml_value(&out).ok().map(|v| {
        if v.is_null() {
            Value::Mapping(Default::default())
        } else {
            v
        }
    });
    assert!(
        got.as_ref() == Some(&want),
        "stripping {paths:?} changed more than those settings:\n{yaml}\n->\n{out}"
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> Value {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn providers_fold_like_go() {
        for name in [
            "kimi",
            " KIMI.com ",
            "Devin",
            "gemini-cli",
            "openai-compatible-deepseek",
            "AISTUDIO",
        ] {
            assert!(provider(name).is_some(), "{name}");
        }
        for name in SUPPORTED {
            assert!(provider(name).is_none(), "{name}");
        }
        assert!(provider("my-plugin").is_none());
    }

    #[test]
    fn removed_settings_fail_with_guidance() {
        let err = config(&doc("api-keys:\n  gemini:\n    - keys: [{api-key: k}]\n"))
            .unwrap_err()
            .to_string();
        assert!(
            err.starts_with("api-keys.gemini (legacy `gemini-api-key`) configures Gemini"),
            "{err}"
        );
        assert!(err.contains("Claude, Codex, Grok (xai) and Muse (meta)"), "{err}");
        assert!(strip_settings("a: 1\napi-keys:\n  vertex:\n    - k: v\n  claude: []\n").contains("claude"));
        assert_eq!(
            strip_settings("oauth:\n  model-alias:\n    \" Gemini-CLI \":\n      - {name: a}\n  x: 1\n"),
            "oauth:\n  model-alias: {}\n  x: 1\n"
        );
        let err = config(&doc("oauth:\n  excluded-models:\n    antigravity: [a]\n"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("oauth.excluded-models.antigravity configures Antigravity"),
            "{err}"
        );
        assert!(config(&doc("models:\n  devin-catalog: https://example.invalid/d.json\n")).is_err());
        assert!(config(&doc("oauth:\n  providers:\n    aistudio:\n      ws-auth: true\n")).is_err());
    }

    #[test]
    fn removed_channels_fail_in_settings_and_request_scoped_errors() {
        for (field, entry) in [
            ("settings", "[{name: model, max-context-length: 42}]"),
            (
                "request-scoped-errors",
                "[{status: 401, match: [denied], action: stop}]",
            ),
        ] {
            for channel in ["gemini", " Gemini-CLI ", "KIMI.com", "openai-compatible-legacy"] {
                for yaml in [
                    format!("config-version: 8\noauth:\n  {field}:\n    '{channel}': {entry}\n"),
                    format!("oauth-{field}:\n  '{channel}': {entry}\n"),
                ] {
                    let error = crate::config::Config::parse(&yaml).unwrap_err().to_string();
                    assert!(error.contains(&format!("oauth.{field}.{channel}")), "{yaml}: {error}");
                    assert!(error.contains(GUIDANCE), "{error}");
                }
            }
            config(&doc(&format!(
                "oauth:\n  {field}:\n    gemini: []\n    claude: {entry}\n"
            )))
            .unwrap();
        }
    }

    /// Every legacy spelling in the table is one the loader moves to a refused path, and
    /// every canonical and legacy path is stripped exactly, so the stripped text loads.
    #[test]
    fn stripping_follows_the_loader_paths() {
        let cases: &[(&str, &str)] = &[
            ("api-keys:\n  gemini:\n    - api-key: k\n", "api-keys.gemini"),
            (
                "api-keys:\n  interactions:\n    - api-key: k\n",
                "api-keys.interactions",
            ),
            ("api-keys:\n  vertex:\n    - api-key: k\n", "api-keys.vertex"),
            (
                "api-keys:\n  openai-compatibility:\n    - name: o\n      base-url: http://127.0.0.1:9\n",
                "api-keys.openai-compatibility",
            ),
            ("gemini-api-key:\n  - api-key: k\n", "api-keys.gemini"),
            ("interactions-api-key:\n  - api-key: k\n", "api-keys.interactions"),
            ("vertex-api-key:\n  - api-key: k\n", "api-keys.vertex"),
            (
                "openai-compatibility:\n  - name: o\n    base-url: http://127.0.0.1:9\n",
                "api-keys.openai-compatibility",
            ),
            ("antigravity:\n  sensitive-words: [x]\n", "oauth.providers.antigravity"),
            (
                "antigravity-signature-cache-enabled: true\n",
                "oauth.providers.antigravity",
            ),
            (
                "antigravity-signature-bypass-strict: true\n",
                "oauth.providers.antigravity",
            ),
            (
                "quota-exceeded:\n  antigravity-credits: true\n",
                "oauth.providers.antigravity",
            ),
            ("devin:\n  sensitive-words: [x]\n", "oauth.providers.devin"),
            ("ws-auth: true\n", "oauth.providers.aistudio"),
            (
                "oauth:\n  providers:\n    aistudio:\n      ws-auth: true\n",
                "oauth.providers.aistudio",
            ),
            (
                "models:\n  devin-catalog: https://d.example.invalid/m.json\n",
                "models.devin-catalog",
            ),
            (
                "oauth-excluded-models:\n  ' Gemini ': [x]\n",
                "oauth.excluded-models. Gemini ",
            ),
            (
                "oauth:\n  settings:\n    KIMI.com: [{name: m}]\n",
                "oauth.settings.KIMI.com",
            ),
        ];
        for (yaml, refused) in cases {
            let yaml = format!("config-version: 8\n# head\n{yaml}routing:\n  strategy: fill-first # kept\n");
            let error = crate::config::Config::parse(&yaml).unwrap_err().to_string();
            assert!(error.starts_with(refused), "{yaml}: {error}");
            let stripped = strip_settings(&yaml);
            crate::config::Config::parse(&stripped).unwrap_or_else(|e| panic!("{stripped}: {e}"));
            assert!(stripped.starts_with("config-version: 8\n# head\n"), "{stripped}");
            assert!(
                stripped.ends_with("routing:\n  strategy: fill-first # kept\n"),
                "{stripped}"
            );
        }
    }

    /// The same key names anywhere but a setting path, and text that only looks like a
    /// setting, are user data: they stay byte for byte, next to a stripped setting too.
    #[test]
    fn stripping_keeps_unrelated_user_data() {
        let user = "config-version: 8\n\
            note: 'keep {gemini: user-data}'\n\
            quoted: \"{antigravity: x}\\u00e9\"\n\
            metadata:\n  gemini: user-value\n  kimi: [a]\n\
            requests:\n  payload:\n    override:\n      - models: [{name: m}]\n        params:\n          metadata:\n            kimi: keep\n            gemini: {devin: 1}\n\
            upstream:\n  claude:\n    header-defaults: {}\n\
            oauth:\n  providers:\n    codex: {gemini: 1}\n  model-alias:\n    claude: [{name: gemini, alias: ws-auth}]\n\
            api-keys:\n  claude:\n    - api-key: k\n      models: [{name: devin-catalog, alias: vertex}]\n";
        assert_eq!(strip_settings(user), user);
        let with_removed = format!("{user}  gemini:\n    - api-key: k\nws-auth: true\n");
        assert_eq!(strip_settings(&with_removed), user);
        // Flow form: only the removed entry goes; empty removed settings stay.
        assert_eq!(
            strip_settings("oauth: {providers: {antigravity: {antigravity-credits: true}, xai: {}, devin: {}}}\n"),
            "oauth: {providers: {xai: {}, devin: {}}}\n"
        );
        for (kept, every) in [
            ("ws-auth: false\na: 1\n", "a: 1\n"),
            ("api-keys: {gemini: [], claude: []}\n", "api-keys: {claude: []}\n"),
            (
                "oauth:\n  providers:\n    antigravity: {}\n",
                "oauth:\n  providers: {}\n",
            ),
        ] {
            assert_eq!(strip_settings(kept), kept);
            assert_eq!(strip_every_setting(kept), every);
        }
        // A document of removed settings only is left empty.
        assert_eq!(
            strip_settings("gemini-api-key:\n  - api-key: k\nws-auth: true\n").trim(),
            ""
        );
        // Invalid YAML is not a document to edit.
        assert_eq!(strip_settings("a: [\n"), "a: [\n");
        for path in [
            &["metadata", "gemini"][..],
            &["requests", "payload", "override"],
            &["oauth", "providers", "codex"],
            &["oauth", "model-alias", "claude"],
            &["upstream", "gemini"],
            &["gemini"],
            &["api-keys", "claude"],
        ] {
            assert!(!is_setting_path(path), "{path:?}");
        }
        assert!(is_setting_path(&["oauth-request-scoped-errors", "openai-compatible-x"]));
    }

    #[test]
    fn empty_and_retained_settings_pass() {
        for yaml in [
            "api-keys:\n  gemini: []\n  claude:\n    - keys: [{api-key: k}]\n",
            "oauth:\n  providers:\n    antigravity: {}\n    aistudio:\n      ws-auth: false\n",
            "oauth:\n  model-alias:\n    claude: [{name: a, alias: b}]\n",
            "models:\n  devin-catalog: ''\n",
        ] {
            config(&doc(yaml)).unwrap_or_else(|e| panic!("{yaml}: {e}"));
        }
    }
}
