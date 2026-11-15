//! Reasoning behavior contracts using explicit model capabilities and typed decisions.
//! Synthetic capabilities test proxy policy, not live provider acceptance.
use super::*;
use cpa_core::registry::ThinkingSupport;
use serde_json::{Value, json};

fn budget_model() -> ModelCaps {
    ModelCaps {
        id: "budget-model".into(),
        kind: "claude".into(),
        thinking: Some(ThinkingSupport {
            min: 1024,
            max: 8192,
            zero_allowed: true,
            dynamic_allowed: true,
            ..Default::default()
        }),
        max_completion_tokens: 16384,
        ..Default::default()
    }
}
fn level_model() -> ModelCaps {
    ModelCaps {
        id: "level-model".into(),
        kind: "codex".into(),
        thinking: Some(ThinkingSupport {
            levels: vec!["none".into(), "low".into(), "medium".into(), "high".into()],
            zero_allowed: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
fn native_budget_validation_enforces_configured_minimum_and_maximum() {
    let model = budget_model();
    for (budget, valid) in [
        (-2, false),
        (1, false),
        (1023, false),
        (1024, true),
        (4096, true),
        (8192, true),
        (8193, false),
    ] {
        let result = validate_config(Config::budget(budget), Some(&model), "claude", "claude", false);
        if valid {
            assert_eq!(result.unwrap(), Config::budget(budget));
        } else {
            let err = result.unwrap_err();
            assert_eq!(err.code, Some(ErrorCode::BudgetOutOfRange));
            assert_eq!(err.status(), 400);
        }
    }
    assert_eq!(
        validate_config(Config::none(), Some(&model), "claude", "claude", false)
            .unwrap()
            .mode,
        Mode::None
    );
}

#[test]
fn translated_and_suffix_budgets_clamp_to_capability_boundaries() {
    let model = budget_model();
    for (requested, expected) in [
        (1, 1024),
        (1023, 1024),
        (1024, 1024),
        (4096, 4096),
        (8192, 8192),
        (8193, 8192),
    ] {
        for (source, suffix) in [("openai", false), ("claude", true)] {
            let result = validate_config(Config::budget(requested), Some(&model), source, "claude", suffix).unwrap();
            assert_eq!(result.budget, expected);
        }
    }
}

#[test]
fn unsupported_capabilities_return_typed_errors_and_disabled_remains_valid() {
    let model = ModelCaps {
        id: "no-thinking".into(),
        kind: "claude".into(),
        ..Default::default()
    };
    for config in [Config::budget(1024), Config::level("high"), Config::auto()] {
        let err = validate_config(config, Some(&model), "claude", "claude", false).unwrap_err();
        assert_eq!(err.code, Some(ErrorCode::ThinkingNotSupported));
        assert_eq!(err.model, "no-thinking");
        assert_eq!(err.status(), 400);
    }
    assert_eq!(
        validate_config(Config::none(), Some(&model), "claude", "claude", false).unwrap(),
        Config::none()
    );
    let err = validate_config(
        Config::level("imaginary"),
        Some(&level_model()),
        "codex",
        "codex",
        false,
    )
    .unwrap_err();
    assert_eq!(err.code, Some(ErrorCode::LevelNotSupported));
}

#[test]
fn kept_provider_appliers_write_their_own_reasoning_fields() {
    let body = br#"{ "messages":[{"role":"user","content":"hi"}], "untouched":{"n":9007199254740993,"s":"\u0061"} }"#;
    for (provider, path) in [
        ("openai", "reasoning_effort"),
        ("codex", "reasoning.effort"),
        ("xai", "reasoning.effort"),
    ] {
        for effort in ["low", "medium", "high"] {
            let output = apply_provider(provider, body, &Config::level(effort), Some(&level_model()))
                .unwrap()
                .unwrap();
            assert_eq!(json::get(&output, path).str(), effort);
            assert_eq!(
                json::get(&output, "untouched").raw(),
                json::get(body, "untouched").raw()
            );
            assert_eq!(json::get(&output, "messages").raw(), json::get(body, "messages").raw());
        }
    }
    for budget in [1024, 4096, 8192] {
        let output = apply_provider("claude", body, &Config::budget(budget), Some(&budget_model()))
            .unwrap()
            .unwrap();
        let parsed: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(parsed["thinking"], json!({"type":"enabled","budget_tokens":budget}));
        assert_eq!(
            json::get(&output, "untouched").raw(),
            json::get(body, "untouched").raw()
        );
    }
    assert!(apply_provider("unregistered", body, &Config::none(), None).is_none());
}

#[test]
fn resolved_pipeline_applies_suffix_precedence_and_preserves_unrelated_data() {
    let model = budget_model();
    let body = br#"{ "messages":[], "thinking":{"type":"enabled","budget_tokens":1024}, "n":9007199254740993 }"#;
    let output = apply_thinking_with_model_info(
        body,
        b"",
        "budget-model(4096)",
        "claude",
        "claude",
        "claude",
        Some(&model),
    )
    .unwrap();
    assert_eq!(json::get(&output, "thinking.budget_tokens").int(), 4096);
    assert_eq!(json::get(&output, "n").raw(), b"9007199254740993");
    let unchanged = br#"{ "messages": [], "n":1.2300e+04 }"#;
    let output = apply_thinking_with_model_info(
        unchanged,
        b"",
        "budget-model",
        "claude",
        "claude",
        "claude",
        Some(&model),
    )
    .unwrap();
    assert_eq!(output, unchanged);
}

#[test]
fn reasoning_extraction_and_strip_preserve_the_other_request_fields() {
    for (provider, body, expected) in [
        (
            "openai",
            r#"{"reasoning_effort":"high","messages":[],"other":1}"#,
            Config::level("high"),
        ),
        (
            "codex",
            r#"{"reasoning":{"effort":"low"},"input":[],"other":1}"#,
            Config::level("low"),
        ),
        (
            "claude",
            r#"{"thinking":{"type":"enabled","budget_tokens":2048},"messages":[],"other":1}"#,
            Config::budget(2048),
        ),
    ] {
        assert_eq!(extract_thinking_config(body.as_bytes(), provider), expected);
        let stripped = strip_thinking_config(body.as_bytes(), provider);
        assert_eq!(json::get(&stripped, "other").int(), 1);
        assert!(!extract_thinking_config(&stripped, provider).is_set());
    }
}

#[test]
fn explicit_summary_intent_remains_separate_from_effort() {
    let implicit = br#"{"reasoning_effort":"high"}"#;
    assert_eq!(extract_summary_config(implicit, "openai").mode, SummaryMode::Enabled);
    assert_eq!(
        extract_explicit_summary_config(implicit, "openai").mode,
        SummaryMode::Unspecified
    );
    assert_eq!(
        extract_translated_summary_config(implicit, "openai", "claude").mode,
        SummaryMode::Unspecified
    );
    for (raw, mode, detail) in [
        (
            r#"{"reasoning":{"summary":"concise"}}"#,
            SummaryMode::Enabled,
            "concise",
        ),
        (r#"{"reasoning":{"summary":"none"}}"#, SummaryMode::Disabled, ""),
        (r#"{"reasoning":{}}"#, SummaryMode::Unspecified, ""),
    ] {
        let config = extract_summary_config(raw.as_bytes(), "codex");
        assert_eq!(config.mode, mode);
        assert_eq!(config.detail, detail);
    }
    let source = br#"{"reasoning":{"summary":"concise"},"input":[],"other":true}"#;
    let output = apply_summary_config(
        source,
        "codex",
        SummaryConfig {
            mode: SummaryMode::Disabled,
            detail: String::new(),
        },
    );
    assert!(!json::get(&output, "reasoning.summary").exists());
    assert!(!json::get(&output, "reasoning.generate_summary").exists());
    assert!(json::get(&output, "other").bool());
    assert_eq!(apply_summary_config(source, "codex", SummaryConfig::default()), source);
}

#[test]
fn suffix_and_conversion_edges() {
    assert_eq!(parse_suffix("a(b)(c)").model_name, "a(b)");
    assert!(!parse_suffix("a(b").has_suffix);
    assert_eq!(parse_numeric_suffix("08192"), Some(8192));
    assert_eq!(parse_numeric_suffix("+5"), Some(5), "strconv.Atoi accepts a plus sign");
    assert_eq!(parse_numeric_suffix("-1"), None);
    assert_eq!(parse_numeric_suffix("9223372036854775808"), None);
    assert_eq!(convert_budget_to_level(-2), None);
    assert_eq!(convert_budget_to_level(512), Some(LEVEL_MINIMAL));
    assert_eq!(convert_budget_to_level(513), Some(LEVEL_LOW));
    assert_eq!(convert_budget_to_level(24577), Some(LEVEL_XHIGH));
    assert_eq!(map_to_claude_effort("xhigh", false), Some("high"));
    assert_eq!(map_to_claude_effort("", true), None);
}

/// Thinking off on Claude models that refuse `disabled` (Go ad9e456d,
/// internal/thinking/claude_issue_6698_test.go): Sonnet 5.5 takes its catalog's
/// `between_tools` with no other thinking fields, adaptive-only models fall back to
/// effort `low` with the summary omitted, other models keep `disabled`. Mythos, which
/// the catalog lacks, keeps the effort `low` fallback (docs/DIFFERENCES-FROM-GO.md).
#[test]
fn thinking_off_on_claude_models_that_refuse_disabled() {
    let apply = |model: &str, body: &str| -> Value {
        let out = apply_thinking(body.as_bytes(), model, "claude", "claude", "claude").unwrap();
        serde_json::from_slice(&out).unwrap()
    };
    let msgs = r#""messages":[{"role":"user","content":"hi"}]"#;
    let extras =
        r#""budget_tokens":8000,"display":"summarized","block_binding":{"prefix_mismatch_behavior":"drop_block"}"#;
    for (model, body) in [
        (
            "claude-sonnet-5-5",
            format!(r#"{{"model":"claude-sonnet-5-5",{msgs},"thinking":{{"type":"disabled"}}}}"#),
        ),
        (
            "claude-sonnet-5-5",
            format!(
                r#"{{"model":"claude-sonnet-5-5",{msgs},"thinking":{{"type":"disabled",{extras}}},"output_config":{{"effort":"xhigh"}}}}"#
            ),
        ),
        (
            "claude-sonnet-5-5(none)",
            format!(
                r#"{{"model":"claude-sonnet-5-5",{msgs},"thinking":{{"type":"adaptive"}},"output_config":{{"effort":"max"}}}}"#
            ),
        ),
        (
            "claude-sonnet-5-5(0)",
            format!(r#"{{"model":"claude-sonnet-5-5",{msgs},"thinking":{{"type":"adaptive"}}}}"#),
        ),
        (
            "claude-sonnet-5-5",
            format!(r#"{{"model":"claude-sonnet-5-5",{msgs},"thinking":{{"type":"enabled","budget_tokens":0}}}}"#),
        ),
        (
            "claude-sonnet-5-5[1m]",
            format!(r#"{{"model":"claude-sonnet-5-5[1m]",{msgs},"thinking":{{"type":"disabled"}}}}"#),
        ),
        (
            "claude-sonnet-5-5-20261001",
            format!(r#"{{"model":"claude-sonnet-5-5-20261001",{msgs},"thinking":{{"type":"disabled"}}}}"#),
        ),
    ] {
        let out = apply(model, &body);
        assert_eq!(
            out["thinking"],
            serde_json::json!({"type": "between_tools"}),
            "{model}: {out}"
        );
        assert!(out.get("output_config").is_none(), "{model}: {out}");
        let raw = serde_json::to_vec(&out).unwrap();
        assert_eq!(extract_translated_reasoning_effort(&raw, "claude"), "none", "{model}");
    }
    for (model, fallback) in [
        ("claude-opus-5-5", "low"),
        ("claude-opus-5-5[1m]", "low"),
        ("claude-opus-5-5-20261001", "low"),
        ("claude-fable-5", "low"),
        ("claude-fable-5-1[1m]", "low"),
        ("claude-opus-5", ""),
        ("claude-sonnet-5", ""),
        ("claude-opus-4-8", ""),
        ("claude-haiku-4-5-20251001", ""),
    ] {
        let out = apply(
            model,
            &format!(r#"{{"model":"{model}",{msgs},"thinking":{{"type":"disabled",{extras}}}}}"#),
        );
        if fallback.is_empty() {
            assert_eq!(
                out["thinking"],
                serde_json::json!({"type": "disabled"}),
                "{model}: {out}"
            );
            assert!(out.get("output_config").is_none(), "{model}: {out}");
        } else {
            assert_eq!(
                out["thinking"],
                serde_json::json!({"type": "adaptive", "display": "omitted"}),
                "{model}: {out}"
            );
            assert_eq!(
                out["output_config"],
                serde_json::json!({"effort": fallback}),
                "{model}: {out}"
            );
        }
    }
    // Configured levels in any order, or with `auto`, still fall back to `low`.
    for levels in [["high", "medium", "low"], ["auto", "high", "low"]] {
        let info = ModelCaps {
            id: "claude-opus-5-5".into(),
            kind: "claude".into(),
            thinking: Some(ThinkingSupport {
                dynamic_allowed: true,
                levels: levels.map(String::from).to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = format!(r#"{{"model":"claude-opus-5-5",{msgs},"thinking":{{"type":"disabled"}}}}"#);
        let out = apply_thinking_with_model_info(
            body.as_bytes(),
            b"",
            "claude-opus-5-5",
            "claude",
            "claude",
            "claude",
            Some(&info),
        )
        .unwrap();
        let out: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            out["output_config"],
            serde_json::json!({"effort": "low"}),
            "{levels:?}: {out}"
        );
    }
    // Adaptive-only models map budgets to the nearest effort and keep auto plain
    // adaptive; Opus 4.6 still takes a budget.
    for model in [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-fable-5",
        "claude-fable-5-1",
    ] {
        let base = format!(r#"{{"model":"{model}","max_tokens":32000,{msgs}}}"#);
        let budget = format!(
            r#"{{"model":"{model}","max_tokens":32000,{msgs},"thinking":{{"type":"enabled","budget_tokens":16384}}}}"#
        );
        for (suffixed, body, effort) in [
            (model.to_owned(), &budget, serde_json::json!("high")),
            (format!("{model}(64000)"), &base, serde_json::json!("xhigh")),
            (format!("{model}(auto)"), &base, Value::Null),
        ] {
            let out = apply(&suffixed, body);
            assert_eq!(
                out["thinking"],
                serde_json::json!({"type": "adaptive"}),
                "{suffixed}: {out}"
            );
            assert_eq!(out["output_config"]["effort"], effort, "{suffixed}: {out}");
        }
    }
    let out = apply(
        "claude-opus-4-6",
        &format!(
            r#"{{"model":"claude-opus-4-6","max_tokens":32000,{msgs},"thinking":{{"type":"enabled","budget_tokens":16384}}}}"#
        ),
    );
    assert_eq!(
        out["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": 16384}),
        "{out}"
    );
    // Not in the catalog: the fixed list keeps effort `low` where Go sends `disabled`.
    let out = apply(
        "claude-mythos-5",
        &format!(r#"{{"model":"claude-mythos-5",{msgs},"thinking":{{"type":"disabled"}}}}"#),
    );
    assert_eq!(out["thinking"], serde_json::json!({"type": "adaptive"}), "{out}");
    assert_eq!(out["output_config"], serde_json::json!({"effort": "low"}), "{out}");
}
