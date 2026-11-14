//! Tool declarations promoted from historical `additional_tools` (Go v8.0.67 0c61b12c,
//! issue #86). Bodies follow v8.0.67 TestXAIExecutorDeduplicatesToolsAcrossAdditionalTools,
//! TestPromoteXAIAdditionalTools_DeduplicatesAndPreservesWinningNamespaceRef and
//! TestXAIShouldFoldNamespaceTools_DeduplicatesAcrossAdditionalTools.

use serde_json::{Value, json};

use super::*;
use crate::xai_response::NamespaceRestorer;

/// `collectXAINamespaceToolRefs`, `promoteXAIAdditionalTools(normalizeXAITools(body))`:
/// the fold decision, the refs and the tools sent, as `prepare` computes them.
fn prepared(body: &Value) -> (bool, NamespaceRefs, Value) {
    let raw = serde_json::to_vec(body).unwrap();
    let fold = should_fold_namespace_tools(&raw, false);
    let refs = collect_namespace_refs(&raw, fold);
    let out = promote_additional_tools(normalize_tools(raw, fold));
    (fold, refs, serde_json::from_slice(&out).unwrap())
}

fn names(out: &Value) -> Vec<String> {
    out["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn tools(prefix: &str, range: std::ops::Range<usize>) -> Vec<Value> {
    range
        .map(|i| json!({"type": "function", "name": format!("{prefix}{i}"), "parameters": {"type": "object"}}))
        .collect()
}

fn namespace(name: &str, children: Vec<Value>) -> Value {
    json!({"type": "namespace", "name": name, "tools": children})
}

fn body(top: Vec<Value>, history: Vec<Value>) -> Value {
    json!({
        "model": "grok-4.7",
        "tools": top,
        "input": [
            {"type": "additional_tools", "role": "developer", "tools": history},
            {"role": "user", "content": "hi"},
        ],
    })
}

fn sleep(description: &str, param: &str) -> Value {
    json!({"type": "function", "name": "sleep", "description": description,
        "parameters": {"type": "object", "properties": {param: {"type": "string"}}, "required": [param]}})
}

/// The same namespaced tool in the top-level tools and in history is sent once, the
/// top-level declaration winning whether or not the copies agree.
#[test]
fn historical_copies_of_a_top_level_tool_are_dropped() {
    let top = namespace("clock", vec![sleep("top declaration", "duration_ms")]);
    for history in [
        top.clone(),
        namespace("clock", vec![sleep("history declaration", "seconds")]),
    ] {
        let (fold, refs, out) = prepared(&body(vec![top.clone()], vec![history]));
        assert!(!fold);
        assert_eq!(names(&out), ["clock__sleep"], "{out}");
        assert_eq!(out["tools"][0]["description"], "top declaration");
        assert!(
            out["tools"][0]["parameters"]["properties"]["duration_ms"].is_object(),
            "{out}"
        );
        assert!(
            out["tools"][0]["parameters"]["properties"].get("seconds").is_none(),
            "{out}"
        );
        assert!(
            out["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|i| i["type"] != "additional_tools"),
            "{out}"
        );
        assert_eq!(refs["clock__sleep"].namespace, "clock");
    }
}

/// A flat top-level `clock__sleep` wins over a historical `clock` namespace child of
/// the same flattened name, and is no namespace ref, so its calls are not renamed.
#[test]
fn a_flat_top_level_tool_wins_over_a_historical_namespace_child() {
    let flat = json!({"type": "function", "name": "clock__sleep", "description": "flat top", "parameters": {"type": "object"}});
    let history = namespace("clock", vec![sleep("namespaced history", "seconds")]);
    let (_, refs, out) = prepared(&body(vec![flat], vec![history]));
    assert_eq!(names(&out), ["clock__sleep"], "{out}");
    assert_eq!(out["tools"][0]["description"], "flat top");
    assert!(!refs.contains_key("clock__sleep"), "{refs:?}");
}

/// Duplicate names are per namespace: the same child name in two namespaces is two
/// tools, and a historical namespace adds only the children the top level lacks.
#[test]
fn duplicate_child_names_across_namespaces_stay_distinct() {
    let top = vec![
        namespace("a", vec![sleep("a sleep", "x")]),
        namespace("b", vec![sleep("b sleep", "y")]),
    ];
    let history = vec![
        namespace(
            "a",
            vec![
                sleep("a history", "z"),
                json!({"type": "function", "name": "wake", "parameters": {"type": "object"}}),
            ],
        ),
        namespace("c", vec![sleep("c sleep", "w")]),
    ];
    let (_, refs, out) = prepared(&body(top, history));
    assert_eq!(names(&out), ["a__sleep", "b__sleep", "a__wake", "c__sleep"], "{out}");
    assert_eq!(out["tools"][0]["description"], "a sleep");
    for (qualified, ns) in [
        ("a__sleep", "a"),
        ("b__sleep", "b"),
        ("a__wake", "a"),
        ("c__sleep", "c"),
    ] {
        assert_eq!(
            (refs[qualified].namespace.as_str(), refs[qualified].name.as_str()),
            (ns, qualified.split("__").nth(1).unwrap())
        );
    }
}

/// Overlapping copies count once toward xAI's 200-tool limit: 120 tools declared in
/// both places do not fold.
#[test]
fn overlapping_history_counts_once_toward_the_fold_limit() {
    let ns = namespace("mcp__app", tools("tool_", 0..120));
    let (fold, _, out) = prepared(&body(vec![ns.clone()], vec![ns]));
    assert!(!fold, "120 distinct tools must not fold");
    assert_eq!(out["tools"].as_array().unwrap().len(), 120);
}

/// Folded, a namespace declared in both places becomes one dispatcher whose catalog
/// keeps the top-level child and adds history's new children.
#[test]
fn a_namespace_declared_twice_folds_into_one_dispatcher() {
    let app = namespace("mcp__app", tools("tool_", 0..120));
    let top_clock = namespace(
        "clock",
        vec![
            json!({"type": "function", "name": "sleep", "description": "top sleep", "parameters": {"type": "object"}}),
        ],
    );
    let history_clock = namespace(
        "clock",
        vec![
            json!({"type": "function", "name": "sleep", "description": "history sleep", "parameters": {"type": "object"}}),
            json!({"type": "function", "name": "alarm", "description": "history alarm", "parameters": {"type": "object"}}),
        ],
    );
    let extra = namespace("mcp__extra", tools("tool_", 120..210));
    let (fold, refs, out) = prepared(&body(vec![top_clock, app], vec![history_clock, extra]));
    assert!(fold);
    assert_eq!(names(&out), ["clock", "mcp__app", "mcp__extra"], "{out}");
    let clock = out["tools"][0]["description"].as_str().unwrap();
    assert!(
        clock.contains("top sleep") && !clock.contains("history sleep"),
        "{clock}"
    );
    assert!(clock.contains("history alarm"), "{clock}");
    let alarm = restore(&refs, call("clock", r#"{"name":"alarm","arguments":{}}"#));
    assert_eq!(
        (&alarm["item"]["namespace"], &alarm["item"]["name"]),
        (&json!("clock"), &json!("alarm")),
        "{alarm}"
    );
}

fn restore(refs: &NamespaceRefs, event: Value) -> Value {
    let raw = NamespaceRestorer::new(refs.clone()).restore(serde_json::to_vec(&event).unwrap());
    serde_json::from_slice(&raw).unwrap()
}

fn call(name: &str, arguments: &str) -> Value {
    json!({"type": "response.output_item.done", "item": {"type": "function_call", "name": name, "call_id": "c", "arguments": arguments}})
}

/// Folded, a child's flattened name (`clock` + `sleep` = `clock__sleep`) does not keep
/// a namespace named `clock__sleep` from its own dispatcher; calls to both restore.
#[test]
fn a_namespace_named_like_a_flattened_child_keeps_its_dispatcher() {
    let app = namespace("mcp__app", tools("tool_", 0..120));
    let top = vec![
        namespace(
            "clock",
            vec![json!({"type": "function", "name": "sleep", "parameters": {"type": "object"}})],
        ),
        app,
    ];
    let history = vec![
        namespace(
            "clock__sleep",
            vec![json!({"type": "function", "name": "alarm", "parameters": {"type": "object"}})],
        ),
        namespace("mcp__extra", tools("tool_", 120..210)),
    ];
    let (fold, refs, _) = prepared(&body(top, history));
    assert!(fold);
    let r = &refs["clock__sleep"];
    assert!(r.dispatcher && r.namespace == "clock__sleep", "{r:?}");
    let clock = restore(
        &refs,
        call("clock", r#"{"name":"sleep","arguments":{"duration_ms":5}}"#),
    );
    assert_eq!(
        (
            &clock["item"]["namespace"],
            &clock["item"]["name"],
            &clock["item"]["arguments"]
        ),
        (&json!("clock"), &json!("sleep"), &json!(r#"{"duration_ms":5}"#)),
        "{clock}"
    );
    let alarm = restore(
        &refs,
        call("clock__sleep", r#"{"name":"alarm","arguments":{"ring":true}}"#),
    );
    assert_eq!(
        (
            &alarm["item"]["namespace"],
            &alarm["item"]["name"],
            &alarm["item"]["arguments"]
        ),
        (&json!("clock__sleep"), &json!("alarm"), &json!(r#"{"ring":true}"#)),
        "{alarm}"
    );
}

/// Folded, a flat function `clock__sleep` is never restored as namespace `clock`,
/// child `sleep`, whichever is declared first.
#[test]
fn folded_namespaces_never_claim_a_flat_functions_calls() {
    let app = namespace("mcp__app", tools("tool_", 0..120));
    let extra = namespace("mcp__extra", tools("tool_", 120..210));
    let flat =
        json!({"type": "function", "name": "clock__sleep", "description": "flat", "parameters": {"type": "object"}});
    let clock = namespace(
        "clock",
        vec![json!({"type": "function", "name": "sleep", "parameters": {"type": "object"}})],
    );
    for (top, history) in [
        (vec![flat.clone(), app.clone()], vec![clock.clone(), extra.clone()]),
        (vec![clock.clone(), flat.clone(), app.clone()], vec![extra.clone()]),
    ] {
        let (fold, refs, _) = prepared(&body(top, history));
        assert!(fold);
        assert!(!refs.contains_key("clock__sleep"), "{refs:?}");
        let restored = restore(&refs, call("clock__sleep", r#"{"raw":true}"#));
        assert_eq!(restored["item"]["name"], "clock__sleep", "{restored}");
        assert!(restored["item"].get("namespace").is_none(), "{restored}");
    }
}

/// Unknown members of the winning declaration reach xAI untouched; only exact name
/// repeats are dropped, never hosted tools.
#[test]
fn winning_declarations_keep_unknown_members_and_hosted_tools_stay() {
    let top = vec![
        json!({"type": "function", "name": "lookup", "parameters": {"type": "object"}, "x_vendor": {"keep": [1, 2]}}),
        json!({"type": "web_search"}),
    ];
    let history = vec![
        json!({"type": "function", "name": "lookup", "parameters": {"type": "object"}, "x_vendor": "history"}),
        json!({"type": "web_search"}),
    ];
    let (_, _, out) = prepared(&body(top, history));
    let tools = out["tools"].as_array().unwrap();
    assert_eq!(tools.iter().filter(|t| t["name"] == "lookup").count(), 1, "{out}");
    assert_eq!(tools[0]["x_vendor"], json!({"keep": [1, 2]}));
    assert_eq!(tools.iter().filter(|t| t["type"] == "web_search").count(), 2, "{out}");
}

/// A long history repeating the same namespace stays one set of tools, folded or not.
/// The work is linear: names are looked up in sets and a folded namespace's dispatcher
/// is built once, not once per repetition.
#[test]
fn long_histories_of_repeated_namespaces_deduplicate() {
    let app = namespace("mcp__app", tools("tool_", 0..150));
    let mut repeated = body(vec![app.clone()], vec![]);
    let input = repeated["input"].as_array_mut().unwrap();
    input.clear();
    for _ in 0..300 {
        input.push(json!({"type": "additional_tools", "role": "developer", "tools": [app.clone()]}));
    }
    input.push(json!({"role": "user", "content": "hi"}));
    let (fold, _, out) = prepared(&repeated);
    assert!(!fold);
    assert_eq!(out["tools"].as_array().unwrap().len(), 150);

    repeated["tools"]
        .as_array_mut()
        .unwrap()
        .push(namespace("mcp__extra", tools("extra_", 0..60)));
    let (fold, refs, out) = prepared(&repeated);
    assert!(fold);
    assert_eq!(names(&out), ["mcp__app", "mcp__extra"], "{out}");
    let last = restore(&refs, call("mcp__app", r#"{"name":"tool_149","arguments":{}}"#));
    assert_eq!(
        (&last["item"]["namespace"], &last["item"]["name"]),
        (&json!("mcp__app"), &json!("tool_149")),
        "{last}"
    );
}

/// A namespace that grows across history (each item re-declares it with one more
/// child) folds into one dispatcher listing every child exactly once, sent once, whose
/// calls restore for the first and the last child. Go emits a full copy of the merged
/// dispatcher at every occurrence (issue #86 review).
#[test]
fn a_namespace_growing_across_history_folds_once() {
    let n = 500;
    let mut grown = body(vec![], vec![]);
    let input = grown["input"].as_array_mut().unwrap();
    input.clear();
    for i in 0..n {
        input.push(json!({"type": "additional_tools", "tools": [namespace("app", tools("tool_", i..i + 1))]}));
    }
    input.push(json!({"role": "user", "content": "hi"}));
    let (fold, refs, out) = prepared(&grown);
    assert!(fold);
    assert_eq!(names(&out), ["app"], "{out}");
    let catalog = out["tools"][0]["description"].as_str().unwrap();
    let listed: Vec<&str> = catalog.lines().filter(|line| line.starts_with("- tool_")).collect();
    let want: Vec<String> = (0..n).map(|i| format!("- tool_{i}")).collect();
    assert_eq!(listed, want, "every child once, in history order");
    for child in ["tool_0".to_owned(), format!("tool_{}", n - 1)] {
        let restored = restore(&refs, call("app", &format!(r#"{{"name":"{child}","arguments":{{}}}}"#)));
        assert_eq!(
            (&restored["item"]["namespace"], &restored["item"]["name"]),
            (&json!("app"), &json!(child))
        );
    }
}

/// Emitting a folded dispatcher only at the namespace's first occurrence keeps Go's
/// first-wins outcome: a flat tool of the same name declared earlier, in the same list
/// or the top level, still wins, and one declared after the first occurrence loses.
#[test]
fn a_folded_dispatcher_keeps_first_wins_against_flat_tools() {
    let filler = namespace("mcp__extra", tools("tool_", 0..210));
    let flat = |description: &str| json!({"type": "function", "name": "clock", "description": description, "parameters": {"type": "object"}});
    let clock = namespace(
        "clock",
        vec![json!({"type": "function", "name": "sleep", "parameters": {"type": "object"}})],
    );
    for (top, history, winner) in [
        (
            vec![flat("flat top"), filler.clone()],
            vec![clock.clone()],
            Some("flat top"),
        ),
        (
            vec![filler.clone()],
            vec![flat("flat history"), clock.clone(), clock.clone()],
            Some("flat history"),
        ),
        (
            vec![clock.clone(), filler.clone()],
            vec![flat("flat later"), clock.clone()],
            None,
        ),
        (vec![filler.clone()], vec![clock.clone(), flat("flat later")], None),
    ] {
        let (fold, _, out) = prepared(&body(top, history));
        assert!(fold);
        let clocks: Vec<&Value> = out["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["name"] == "clock")
            .collect();
        assert_eq!(clocks.len(), 1, "{out}");
        match winner {
            Some(description) => assert_eq!(clocks[0]["description"], description, "{out}"),
            None => assert!(
                clocks[0]["description"]
                    .as_str()
                    .unwrap()
                    .contains("Available tools in this namespace"),
                "the dispatcher wins: {out}"
            ),
        }
    }
}
