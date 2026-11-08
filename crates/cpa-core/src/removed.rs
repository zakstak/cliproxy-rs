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

/// Test support for Go goldens recorded with settings of the removed providers: `yaml`
/// without the block (or flow line) of each removed setting, so the rest of the recorded
/// document still loads. Indentation decides a block's extent; a key matches quoted, padded
/// or in any case (`" Gemini-CLI ":`), as Go folds channel names.
#[doc(hidden)]
pub fn strip_settings(yaml: &str) -> String {
    const KEYS: [&str; 16] = [
        "vertex",
        "openai-compatibility",
        "gemini",
        "gemini-cli",
        "interactions",
        "vertex-api-key",
        "gemini-api-key",
        "interactions-api-key",
        "antigravity",
        "antigravity-signature-cache-enabled",
        "antigravity-signature-bypass-strict",
        "aistudio",
        "ws-auth",
        "devin",
        "devin-catalog",
        "kimi",
    ];
    // The mapping key a block line starts with, unquoted, trimmed and folded.
    let key = |text: &str| -> Option<String> {
        let key = match text.as_bytes().first() {
            Some(&q @ (b'"' | b'\'')) => {
                let end = text[1..].find(q as char)? + 1;
                text[end + 1..].trim_start().strip_prefix(':')?;
                &text[1..end]
            }
            _ => text.split_once(':')?.0,
        };
        Some(key.trim().go_lower())
    };
    const FLOW: [&str; 5] = ["{antigravity:", "{aistudio:", "{devin:", "{gemini:", "{vertex:"];
    let mut kept: Vec<String> = Vec::new();
    // Output lines that lost a child block to the strip.
    let mut emptied: Vec<usize> = Vec::new();
    let mut skip: Option<usize> = None;
    for line in yaml.split_inclusive('\n') {
        let indent = line.len() - line.trim_start().len();
        let text = line.trim_start().trim_start_matches("- ");
        if let Some(level) = skip {
            if line.trim().is_empty() || indent > level {
                continue;
            }
            skip = None;
        }
        if key(text).is_some_and(|k| KEYS.contains(&k.as_str())) {
            skip = Some(indent);
            if let Some(last) = kept.len().checked_sub(1) {
                emptied.push(last);
            }
            continue;
        }
        if FLOW.iter().any(|k| line.contains(k)) {
            continue;
        }
        kept.push(line.to_owned());
    }
    // A mapping key whose only children were stripped reads as an empty mapping, not
    // null, as its Go source would have it.
    for &i in &emptied {
        let indent = kept[i].len() - kept[i].trim_start().len();
        let body = kept[i].trim_end().to_owned();
        let has_child = kept[i + 1..]
            .iter()
            .find(|l| !l.trim().is_empty())
            .is_some_and(|l| l.len() - l.trim_start().len() > indent);
        if body.ends_with(':') && !has_child {
            kept[i] = format!("{body} {{}}\n");
        }
    }
    kept.concat()
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
