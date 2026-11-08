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
    (&["oauth", "providers", "antigravity"], "antigravity, antigravity-signature-*, quota-exceeded.antigravity-credits"),
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
        Value::Mapping(m) => !m.is_empty(),
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
        let name = provider(path[path.len() - 1].trim_end_matches("-catalog"))
            .map_or("a removed provider", |r| r.name);
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
    // Per-provider maps: OAuth model aliases and excluded models keyed by channel.
    for path in [["oauth", "model-alias"], ["oauth", "excluded-models"]] {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(yaml: &str) -> Value {
        serde_yaml_ng::from_str(yaml).unwrap()
    }

    #[test]
    fn providers_fold_like_go() {
        for name in ["kimi", " KIMI.com ", "Devin", "gemini-cli", "openai-compatible-deepseek", "AISTUDIO"] {
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
        assert!(err.starts_with("api-keys.gemini (legacy `gemini-api-key`) configures Gemini"), "{err}");
        assert!(err.contains("Claude, Codex, Grok (xai) and Muse (meta)"), "{err}");
        let err = config(&doc("oauth:\n  excluded-models:\n    antigravity: [a]\n"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("oauth.excluded-models.antigravity configures Antigravity"), "{err}");
        assert!(config(&doc("models:\n  devin-catalog: https://example.invalid/d.json\n")).is_err());
        assert!(config(&doc("oauth:\n  providers:\n    aistudio:\n      ws-auth: true\n")).is_err());
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
