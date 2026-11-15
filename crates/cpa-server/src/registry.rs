//! The dynamic model registry lives in `cpa_core::registry::dynamic` so translators and
//! shared helpers can read capabilities; this module publishes the runtime's copy.

use cpa_core::registry::ModelInfo;
pub use cpa_core::registry::dynamic::*;

/// Exposes the runtime's registry through `cpa_core::registry::lookup_model`.
pub struct Overlay(pub std::sync::Weak<crate::Runtime>);

impl cpa_core::registry::Overlay for Overlay {
    fn lookup(&self, id: &str, provider: Option<&str>) -> Option<ModelInfo> {
        self.0.upgrade()?.registry().info(id, provider).map(|s| s.to_info())
    }

    fn for_credential(&self, credential_id: &str, model: &str) -> Option<ModelInfo> {
        self.0
            .upgrade()?
            .registry()
            .client_model(credential_id, model)
            .map(|s| s.to_info())
    }

    fn available_by_provider(&self, provider: &str) -> Vec<ModelInfo> {
        let Some(rt) = self.0.upgrade() else {
            return Vec::new();
        };
        rt.registry()
            .available_by_provider(provider, |client, model| rt.suspension(client, model))
            .iter()
            .map(|s| s.to_info())
            .collect()
    }

    fn model_providers(&self, model: &str) -> Vec<String> {
        self.0
            .upgrade()
            .map(|rt| rt.registry().model_providers(model))
            .unwrap_or_default()
    }

    fn available(&self) -> Vec<ModelInfo> {
        let Some(rt) = self.0.upgrade() else {
            return Vec::new();
        };
        let registry = rt.registry();
        registry
            .available_with(|c, m| rt.suspension(c, m))
            .map(|s| s.to_info())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{Registry, Spec, Suspension};
    use cpa_core::config::Config;
    use cpa_core::credential::Credential;
    use serde_json::{Value, json};
    use std::{path::Path, sync::Arc};

    fn credential(attrs: &[(&str, &str)], metadata: Value) -> Credential {
        let mut c = Credential::from_file(
            Path::new("/fake"),
            Path::new("/fake/credential.json"),
            json!({"type":"claude"}).as_object().unwrap().clone(),
        )
        .unwrap();
        c.metadata = metadata.as_object().unwrap().clone();
        c.attributes = attrs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        c
    }

    #[test]
    fn credential_kind_uses_explicit_source_precedence_and_token_evidence() {
        for spelling in ["apikey", " API_KEY ", "api-key"] {
            let c = credential(
                &[("auth_kind", spelling)],
                json!({"auth_kind":"oauth","access_token":"fake"}),
            );
            assert_eq!(super::auth_kind(&c), Some("apikey"));
            assert!(super::is_api_key(&c));
        }
        for spelling in ["oauth", " OAuth2 "] {
            let c = credential(
                &[("auth_kind", spelling), ("api_key", "fake")],
                json!({"auth_kind":"apikey"}),
            );
            assert_eq!(super::auth_kind(&c), Some("oauth"));
            assert!(!super::is_api_key(&c));
        }
        for (attrs, meta, expected) in [
            (vec![], json!({}), None),
            (
                vec![("auth_kind", "unknown")],
                json!({"auth_kind":"api_key"}),
                Some("apikey"),
            ),
            (
                vec![("auth_kind", "unknown"), ("api_key", "fake")],
                json!({}),
                Some("apikey"),
            ),
            (vec![("api_key", " ")], json!({}), None),
            (
                vec![("api_key", "fake")],
                json!({"access_token":"fake-token"}),
                Some("apikey"),
            ),
            (vec![], json!({"auth_kind":5}), None),
            (
                vec![("auth_kind", "apikey"), ("use_siwc", "true")],
                json!({}),
                Some("oauth"),
            ),
            (vec![("auth_kind", "apikey")], json!({"use_siwc":true}), Some("oauth")),
            (
                vec![("auth_kind", "apikey"), ("use_siwc", "false")],
                json!({}),
                Some("apikey"),
            ),
        ] {
            let c = credential(&attrs, meta);
            assert_eq!(super::auth_kind(&c), expected);
            assert_eq!(super::is_api_key(&c), expected == Some("apikey"));
        }
        for field in [
            "access_token",
            "refresh_token",
            "id_token",
            "email",
            "token_type",
            "expires_at",
            "expired",
        ] {
            for (evidence, expected) in [
                (json!("fake"), Some("oauth")),
                (json!(" "), None),
                (Value::Null, None),
                (json!(false), None),
            ] {
                let c = credential(&[], json!({field:evidence}));
                assert_eq!(super::auth_kind(&c), expected, "{field}");
            }
        }
        for field in ["token", "siwc"] {
            assert_eq!(
                super::auth_kind(&credential(&[], json!({field:{"access_token":"fake"}}))),
                Some("oauth")
            );
            assert_eq!(super::auth_kind(&credential(&[], json!({field:{}}))), None);
        }
    }

    fn states() -> [Suspension; 4] {
        [
            Suspension::None,
            Suspension::Quota,
            Suspension::Other { quota_exceeded: false },
            Suspension::Other { quota_exceeded: true },
        ]
    }

    #[test]
    fn provider_listing_is_scoped_while_lookup_aggregates_search_capability() {
        for first in states() {
            for second in states() {
                for search_a in [false, true] {
                    for search_b in [false, true] {
                        let mut r = Registry::default();
                        for (id, provider, search) in [
                            ("a", "claude", search_a),
                            ("b", "claude", search_b),
                            ("other", "codex", true),
                        ] {
                            r.register(
                                id,
                                provider,
                                vec![Spec {
                                    id: "shared-model".into(),
                                    supports_web_search: search,
                                    ..Spec::default()
                                }],
                            );
                        }
                        let status = |client: &str, _: &str| match client {
                            "a" => first,
                            "b" => second,
                            _ => Suspension::None,
                        };
                        let list = r.available_by_provider(" CLAUDE ", status);
                        // At least one ready credential, or a set blocked only by quota, is listable.
                        let ready = matches!(first, Suspension::None) || matches!(second, Suspension::None);
                        let quota_only = matches!(first, Suspension::Quota) && matches!(second, Suspension::Quota);
                        assert_eq!(list.len(), usize::from(ready || quota_only), "{first:?} {second:?}");
                        if let Some(info) = list.first() {
                            assert_eq!(
                                info.supports_web_search, search_a,
                                "listing uses this provider's first registration"
                            );
                        }
                        assert!(
                            r.info("shared-model", None).unwrap().supports_web_search,
                            "other provider participates in global lookup"
                        );
                        assert_eq!(
                            r.info("shared-model", Some("claude")).unwrap().supports_web_search,
                            search_a || search_b
                        );
                        assert_eq!(r.available_by_provider("codex", status).len(), 1);
                        assert!(r.available_by_provider("", status).is_empty());
                        assert!(r.available_by_provider("missing", status).is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn model_listing_handles_every_ready_quota_and_other_suspension_combination() {
        let credentials: Vec<Arc<Credential>> = (0..3)
            .map(|i| {
                Arc::new(
                    Credential::from_file(
                        Path::new("/fake"),
                        &Path::new("/fake").join(format!("{i}.json")),
                        json!({"type":"claude"}).as_object().unwrap().clone(),
                    )
                    .unwrap(),
                )
            })
            .collect();
        let model = "claude-sonnet-4-6";
        for count in 0..=3 {
            let r = Registry::build(&Config::default(), &credentials[..count]);
            for encoded in 0..4usize.pow(count as u32) {
                let statuses: Vec<_> = (0..count)
                    .map(|i| states()[(encoded / 4usize.pow(i as u32)) % 4])
                    .collect();
                let expected = statuses.iter().any(|s| matches!(s, Suspension::None))
                    || (!statuses.is_empty() && statuses.iter().all(|s| matches!(s, Suspension::Quota)));
                let actual = r
                    .available_with(|id, m| {
                        if m == model {
                            credentials[..count]
                                .iter()
                                .position(|c| c.id == id)
                                .map(|i| statuses[i])
                                .unwrap_or(Suspension::None)
                        } else {
                            Suspension::None
                        }
                    })
                    .any(|m| m.id == model);
                assert_eq!(actual, expected, "{statuses:?}");
            }
        }
    }
}
