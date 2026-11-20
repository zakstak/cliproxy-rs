//! Independent tests of the proxy's bounded conversation-affinity policy.
//! Identity, lineage and lease behavior are specified here without a foreign oracle.

use std::time::{Duration, Instant};

use super::*;

/// Caps must always admit the configured maximum single trajectory.
#[test]
fn limits_default_and_cover_max_turns() {
    let m = Matcher::new(Limits {
        max_turns: 10,
        max_prefixes: 2,
        ..Limits::default()
    });
    let l = m.limits();
    assert_eq!(
        (l.ttl, l.max_turns, l.max_groups, l.max_prefixes),
        (Duration::from_secs(3600), 10, 4096, 10)
    );
}

/// Parallel tool calls have deterministic ordering even when their values tie.
#[test]
fn tool_parts_sort_by_value_then_digest() {
    let part = |digest: &str| Part {
        kind: "tool:call".into(),
        value: b"same_value".to_vec(),
        digest: digest.into(),
        ..Part::default()
    };
    let one = Turn {
        role: "assistant".into(),
        parts: vec![part("digest_b"), part("digest_a")],
    };
    let two = Turn {
        role: "assistant".into(),
        parts: vec![part("digest_a"), part("digest_b")],
    };
    assert_eq!(normalize_turn(&one).parts[0].digest, "digest_a");
    assert_eq!(normalize_turn(&two).parts[0].digest, "digest_a");
    assert_eq!(fingerprint(&one), fingerprint(&two));
}

fn texts(values: &[&str]) -> Prepared {
    let turns: Vec<Turn> = values
        .iter()
        .map(|v| Turn {
            role: "user".into(),
            parts: vec![Part {
                kind: "text".into(),
                value: v.as_bytes().to_vec(),
                ..Part::default()
            }],
        })
        .collect();
    Prepared::new(&turns)
}

/// One session bound to two credentials reports both, sorted.
#[test]
fn lookup_reports_every_credential_of_a_session() {
    let now = Instant::now();
    let ns = "lcp:v1::openai::gpt-4o::caller";
    let mut m = Matcher::new(Limits {
        ttl: Duration::from_secs(600),
        ..Limits::default()
    });
    let first = m.bind(ns, &texts(&["turnA"]), "auth-b", now).unwrap();
    // The extension inherits the session; a failover binds it to another credential.
    let grown = m.bind(ns, &texts(&["turnA", "turnB"]), "auth-a", now).unwrap();
    assert_eq!(grown.session, first.session);
    assert_eq!(
        m.lookup(&first.session, now),
        Some((vec!["auth-a".to_owned(), "auth-b".to_owned()], ns.to_owned()))
    );
}

/// Lookup drops expired groups, keeps live ones untouched and refreshes nothing.
#[test]
fn lookup_drops_expired_groups_without_refreshing() {
    let t0 = Instant::now();
    let minutes = |n: u64| t0 + Duration::from_secs(60 * n);
    let ns = "lcp:v1::openai::gpt-4o::caller";
    let mut m = Matcher::new(Limits {
        ttl: Duration::from_secs(600),
        ..Limits::default()
    });
    let session = m.bind(ns, &texts(&["turn1"]), "auth-a", t0).unwrap().session;
    // Both groups live until minute 15; matching the longer one keeps only it alive.
    m.bind(ns, &texts(&["turn1", "turn2"]), "auth-a", minutes(5)).unwrap();
    let hit = m.find(ns, &texts(&["turn1", "turn2", "turn3"]), minutes(12)).unwrap();
    assert_eq!((hit.prefix_length, hit.fork), (2, false));
    assert_eq!(
        m.lookup(&session, minutes(16)),
        Some((vec!["auth-a".to_owned()], ns.to_owned()))
    );
    assert!(
        m.find(ns, &texts(&["turn1"]), minutes(16)).is_some(),
        "the extension keeps the prefix"
    );
    let generation = m.find(ns, &texts(&["turn1", "turn2"]), minutes(17)).unwrap().access;
    m.lookup(&session, minutes(18));
    assert_eq!(
        m.find(ns, &texts(&["turn1", "turn2"]), minutes(19)).unwrap().access,
        generation + 1
    );
    assert_eq!(m.lookup(&session, minutes(30)), None, "lookup never refreshed it");
}

/// A client turn nested far deeper than any conversation skips the matcher instead of
/// recursing once per level, on a stack the size of an async worker's (2 MiB).
#[test]
fn deeply_nested_turns_skip_the_matcher_without_overflowing() {
    let deep = |levels: usize| {
        format!(
            r#"{{"messages":[{{"role":"user","content":{}{{"text":"hi"}}{}}}]}}"#,
            "[".repeat(levels),
            "]".repeat(levels)
        )
    };
    let object = |levels: usize| {
        format!(
            r#"{{"messages":[{{"role":"user","content":[{{"tool_call":{}1{}}}]}}]}}"#,
            r#"{"a":"#.repeat(levels),
            "}".repeat(levels)
        )
    };
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(move || {
            assert!(Request::new("openai", deep(200_000).as_bytes(), "client-key").is_none());
            assert!(Request::new("openai", object(3_000).as_bytes(), "client-key").is_none());
            // At the ceiling the request is still matched.
            assert!(Request::new("openai", deep(MAX_NESTING - 4).as_bytes(), "client-key").is_some());
            assert!(Request::new("openai", deep(MAX_NESTING - 3).as_bytes(), "client-key").is_none());
        })
        .unwrap()
        .join()
        .unwrap();
}

#[path = "lcp_model_tests.rs"]
mod model;

// Session affinity is local proxy policy, not a provider wire contract. These
// examples are authored from that policy and use no recorded implementation output.
fn text_turn(role: &str, text: &str) -> Turn {
    Turn {
        role: role.into(),
        parts: vec![Part {
            kind: "text".into(),
            value: text.as_bytes().to_vec(),
            original_size: text.len() as i64,
            ..Part::default()
        }],
    }
}

fn frame_fields(fields: &[&[u8]]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in fields {
        bytes.extend_from_slice(format!("{}:", value.len()).as_bytes());
        bytes.extend_from_slice(value);
        bytes.push(0);
    }
    bytes
}

#[test]
fn supported_client_formats_reduce_to_the_same_conversation() {
    let examples = [
        (
            "openai",
            serde_json::json!({"messages":[{"role":"developer","content":"policy"},{"role":"user","content":"hello"},{"role":"assistant","content":[{"type":"text","text":"world"}]}]}),
        ),
        (
            "claude",
            serde_json::json!({"system":[{"type":"text","text":"policy"}],"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]},{"role":"assistant","content":"world"}]}),
        ),
        (
            "openai-response",
            serde_json::json!({"instructions":"policy","input":[{"role":"user","content":[{"type":"input_text","text":"hello"}]},{"role":"assistant","content":[{"type":"output_text","text":"world"}]}]}),
        ),
    ];
    let expected = vec![
        text_turn("system", "policy"),
        text_turn("user", "hello"),
        text_turn("assistant", "world"),
    ];
    for (format, body) in examples {
        let body = serde_json::to_vec(&body).unwrap();
        assert_eq!(extract(format, &body), expected, "{format}");
        assert_eq!(extract("", &body), expected, "inferred {format}");
        let prepared = Prepared::new(&extract(format, &body));
        assert_eq!(prepared.min_prefix, 2);
        assert!(prepared.usable());
    }
    assert_eq!(
        extract("codex", br#"{"input":"hello"}"#),
        vec![text_turn("user", "hello")]
    );
    assert_eq!(
        extract(
            "openai",
            br#"{"messages":[{"role":" AI ","content":"hello"},{"role":"function","content":"done"}]}"#
        ),
        vec![text_turn("assistant", "hello"), text_turn("tool", "done")]
    );
    for bad in [b"".as_slice(), b"not json", b"null", b"{}", br#"{"messages":[]}"#] {
        assert!(extract("openai", bad).is_empty());
    }
}

#[test]
fn extraction_ignores_reasoning_and_keeps_tools_media_and_opaque_data() {
    let body = serde_json::json!({"messages":[{"role":"assistant","content":[
        {"type":"thinking","thinking":"private"}, {"type":"reasoning","text":"private"},
        {"type":"text","text":"answer"}, {"type":"tool_use","id":"c1","name":"read","input":{"file":"a"}},
        {"type":"image","source":{"type":"base64","data":"AA=="}}, {"future":"kept"}, 7, null
    ]}]});
    let turns = extract("claude", &serde_json::to_vec(&body).unwrap());
    assert_eq!(turns.len(), 1);
    let parts = &turns[0].parts;
    assert_eq!(
        parts.iter().map(|p| p.kind.as_str()).collect::<Vec<_>>(),
        vec!["text", "tool:tool_use", "media", "json", "value", "value"]
    );
    assert_eq!(parts[0], text_turn("assistant", "answer").parts[0]);
    for (part, expected) in parts[1..].iter().zip([
        serde_json::json!({"type":"tool_use","id":"c1","name":"read","input":{"file":"a"}}),
        serde_json::json!({"type":"image","source":{"type":"base64","data":"AA=="}}),
        serde_json::json!({"future":"kept"}),
        serde_json::json!(7),
        serde_json::Value::Null,
    ]) {
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&part.value).unwrap(),
            expected
        );
    }
    let response = extract("codex", br#"{"input":[{"type":"reasoning","summary":[]},{"type":"function_call","name":"read","arguments":"{}"},{"type":"function_call_output","output":"done"}]}"#);
    assert_eq!(
        response.iter().map(|t| t.role.as_str()).collect::<Vec<_>>(),
        vec!["assistant", "tool"]
    );
    assert!(response.iter().all(|t| t.parts[0].kind.starts_with("tool:")));
}

#[test]
fn fingerprints_use_unambiguous_versioned_byte_framing_and_normalization() {
    let t = text_turn("user", "hello");
    let framed = frame_fields(&[b"cpa-session-turn-v1", b"user", b"text", b"", b"5", b"", b"hello"]);
    assert_eq!(fingerprint(&t), format!("{:x}", Sha256::digest(framed)));
    assert_ne!(fingerprint(&t), fingerprint(&text_turn("assistant", "hello")));
    assert_ne!(fingerprint(&t), fingerprint(&text_turn("user", "hello!")));
    assert_ne!(frame_fields(&[b"ab", b"c"]), frame_fields(&[b"a", b"bc"]));
    let noisy = text_turn(
        " Developer ",
        "  at 2026-11-14T12:34:56Z id 12345678-1234-4234-9234-123456789abc\r\n<think>private</think> policy ",
    );
    let clean = text_turn("system", "at <timestamp> id <uuid> policy");
    assert_eq!(fingerprint(&noisy), fingerprint(&clean));
    assert_ne!(
        fingerprint(&text_turn("user", "2026-11-14T12:34:56Z")),
        fingerprint(&text_turn("user", "2026-11-15T12:34:56Z")),
        "user dates are meaningful"
    );
    let mut decorated = t.clone();
    decorated.parts[0].kind = " TEXT ".into();
    decorated.parts[0].mime = " Text/Plain ".into();
    decorated.parts.push(Part::default());
    let mut normalized = t;
    normalized.parts[0].mime = "text/plain".into();
    assert_eq!(fingerprint(&decorated), fingerprint(&normalized));
}

#[test]
fn large_parts_are_bounded_but_changes_outside_the_sample_change_identity() {
    let mut text = "a".repeat(LARGE_PART + 8000);
    let body =
        |value: &str| serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":value}]})).unwrap();
    let original = extract("openai", &body(&text));
    let part = &original[0].parts[0];
    assert!(part.sampled);
    assert_eq!(part.original_size, text.len() as i64);
    let third = SPARSE_BYTES / 3;
    let middle = (text.len() - third) / 2;
    let expected = [
        &text.as_bytes()[..third],
        &text.as_bytes()[middle..middle + third],
        &text.as_bytes()[text.len() - third..],
    ]
    .concat();
    assert_eq!(part.value, expected);
    assert_eq!(part.digest, format!("{:x}", Sha256::digest(text.as_bytes())));
    text.replace_range(third + 1..third + 2, "b");
    let changed = extract("openai", &body(&text));
    assert_eq!(original[0].parts[0].value, changed[0].parts[0].value);
    assert_ne!(fingerprint(&original[0]), fingerprint(&changed[0]));
    // System prompt dynamic values must not leak back through the sampled digest.
    let mut a = text_turn("system", "stable <timestamp>");
    a.parts[0].sampled = true;
    a.parts[0].original_size = 100_000;
    a.parts[0].digest = "timestamp-a".into();
    let mut b = a.clone();
    b.parts[0].digest = "timestamp-b".into();
    assert_eq!(fingerprint(&a), fingerprint(&b));
    a.role = "user".into();
    b.role = "user".into();
    assert_ne!(fingerprint(&a), fingerprint(&b));
}

#[test]
fn preparation_tracks_all_system_context_and_the_actual_trailing_window() {
    let turns = [
        text_turn("system", "policy"),
        text_turn("user", "question"),
        text_turn("system", "later policy"),
    ];
    let prepared = Prepared::new(&turns);
    assert_eq!(prepared.min_prefix, 2);
    let system_fingerprints = [fingerprint(&turns[0]), fingerprint(&turns[2])];
    let environment = frame_fields(&[system_fingerprints[0].as_bytes(), system_fingerprints[1].as_bytes()]);
    assert_eq!(prepared.environment, format!("{:x}", Sha256::digest(environment)));
    assert_eq!(prepared.tails, prepared.fingerprints);
    assert!(!Prepared::new(&[text_turn("developer", "instructions")]).usable());
    assert!(!Prepared::new(&[]).usable());
    let long = (0..MAX_TURNS + 3)
        .map(|i| text_turn("user", &format!("turn {i}")))
        .collect::<Vec<_>>();
    let prepared = Prepared::new(&long);
    assert_eq!(prepared.fingerprints.len(), MAX_TURNS);
    assert_eq!(
        prepared.tails,
        long[long.len() - 32..].iter().map(fingerprint).collect::<Vec<_>>()
    );
    assert_ne!(prepared.tails.last(), prepared.fingerprints.last());
}

#[test]
fn extraction_caps_turns_and_parts_without_silently_equating_truncated_input() {
    let messages = (0..MAX_TURNS + 10)
        .map(|i| serde_json::json!({"role":"user","content":i.to_string()}))
        .collect::<Vec<_>>();
    let result = extract(
        "openai",
        &serde_json::to_vec(&serde_json::json!({"messages":messages})).unwrap(),
    );
    assert_eq!(result.len(), MAX_TURNS);
    assert_eq!(result[MAX_TURNS - 1], text_turn("user", &(MAX_TURNS - 1).to_string()));
    let parts = (0..MAX_PARTS + 11)
        .map(|i| serde_json::json!({"type":"text","text":i.to_string()}))
        .collect::<Vec<_>>();
    let result = extract(
        "openai",
        &serde_json::to_vec(&serde_json::json!({"messages":[{"role":"user","content":parts}]})).unwrap(),
    );
    assert_eq!(result[0].parts.len(), MAX_PARTS + 1);
    assert_eq!(result[0].parts.last().unwrap().value, b"<truncated:11 parts>");
    assert_eq!(result[0].parts.last().unwrap().original_size, 11);
    assert_ne!(
        fingerprint(&result[0]),
        fingerprint(&Turn {
            role: "user".into(),
            parts: result[0].parts[..MAX_PARTS].to_vec()
        })
    );
}

#[test]
fn requests_isolate_callers_and_models_and_skip_missing_identity_or_user_turns() {
    let body = br#"{"messages":[{"role":"user","content":"hello"}]}"#;
    let a = Request::new("openai", body, "client-a").unwrap();
    let b = Request::new("openai", body, "client-b").unwrap();
    assert_ne!(a.namespace("model-a"), b.namespace("model-a"));
    assert_ne!(a.namespace("model-a"), a.namespace("model-b"));
    assert_eq!(a.prepared(), b.prepared());
    assert!(Request::new("openai", body, "").is_none());
    assert!(Request::new("openai", b"{}", "private-client").is_none());
    assert!(Request::new("claude", br#"{"system":"instructions"}"#, "private-client").is_none());
    assert!(
        !a.namespace("model-a").contains("client-a"),
        "namespace scopes hash the client secret"
    );
}

fn sequence(values: &[&str], environment: &str) -> Prepared {
    Prepared {
        fingerprints: values.iter().map(|s| (*s).into()).collect(),
        min_prefix: 1,
        tails: Vec::new(),
        environment: environment.into(),
    }
}

#[test]
fn forks_keep_parent_identity_and_rebinding_does_not_mutate_ancestors() {
    let now = Instant::now();
    let mut matcher = Matcher::default();
    let original = sequence(&["root", "known", "tail"], "policy");
    let root = matcher.bind("caller/model", &original, "parent-auth", now).unwrap();
    let branch = sequence(&["root", "branch", "question"], "policy");
    let fork = matcher.find("caller/model", &branch, now).unwrap();
    assert_eq!(fork.node(), ("fork", true, false));
    assert_eq!(fork.prefix_length, 1);
    assert_eq!(fork.auth, "parent-auth");
    assert_eq!(fork.parent, root.session);
    assert_ne!(fork.session, root.session);
    let bound = matcher.bind("caller/model", &branch, "branch-auth", now).unwrap();
    assert_eq!(
        (bound.session.as_str(), bound.parent.as_str()),
        (fork.session.as_str(), root.session.as_str())
    );
    assert_eq!(
        matcher.find("caller/model", &original, now).unwrap().auth,
        "parent-auth"
    );
    let grown = sequence(&["root", "branch", "question", "answer"], "policy");
    let continuation = matcher.find("caller/model", &grown, now).unwrap();
    assert_eq!(continuation.session, fork.session);
    assert_eq!(continuation.parent, root.session);
    assert_eq!(continuation.auth, "branch-auth");
    assert_eq!(continuation.node(), ("", false, false));
    let rebound = matcher.bind("caller/model", &branch, "failover", now).unwrap();
    assert_eq!(rebound.session, fork.session);
    assert!(!matcher.touch("caller/model", &branch, "branch-auth", now));
    assert_eq!(matcher.find("caller/model", &branch, now).unwrap().auth, "failover");
}

#[test]
fn compaction_accepts_only_contiguous_trailing_history_at_the_summary_boundary() {
    let now = Instant::now();
    let parent = sequence(&["old", "a", "b", "c"], "policy");
    for values in [
        vec!["summary", "b", "c", "new"],
        vec!["b", "c"],
        vec!["old", "summary", "b", "c", "new"],
        vec!["old", "a", "summary", "b", "c"],
    ] {
        let mut matcher = Matcher::default();
        let original = matcher.bind("ns", &parent, "credential", now).unwrap();
        let request = sequence(&values, "policy");
        let found = matcher.find("ns", &request, now).unwrap();
        assert_eq!(found.node(), ("compaction", false, true), "{values:?}");
        assert_eq!(found.prefix_length, 2);
        assert_eq!(found.parent, original.session);
        assert_ne!(found.session, original.session);
        assert_eq!(found.auth, "credential");
        let repeated = matcher.find("ns", &request, now).unwrap();
        assert_eq!(repeated.session, found.session, "identity is deterministic");
        let bound = matcher.bind("ns", &request, "credential", now).unwrap();
        assert_eq!(bound.session, found.session);
        let continuation = matcher.find("ns", &request, now).unwrap();
        assert_eq!(continuation.session, bound.session);
        assert_eq!(continuation.node(), ("compaction", false, true));
    }
    for (values, environment) in [
        (vec!["summary", "c", "new"], "policy"),
        (vec!["summary", "noise", "b", "c", "new"], "policy"),
        (vec!["summary", "b", "changed", "c"], "policy"),
        (vec!["summary", "b", "c"], "different-policy"),
    ] {
        let mut matcher = Matcher::default();
        matcher.bind("ns", &parent, "credential", now).unwrap();
        assert!(
            matcher.find("ns", &sequence(&values, environment), now).is_none(),
            "{values:?} {environment}"
        );
    }
}

#[test]
fn compaction_prefers_the_lineage_leaf_and_refuses_unrelated_equal_overlaps() {
    let now = Instant::now();
    let mut matcher = Matcher::default();
    let original = sequence(&["history", "b", "c"], "policy");
    let root = matcher.bind("ns", &original, "root-auth", now).unwrap();
    let condensed = sequence(&["summary", "b", "c"], "policy");
    let child = matcher.bind("ns", &condensed, "child-auth", now).unwrap();
    assert_eq!(child.parent, root.session);
    let next = sequence(&["new-summary", "b", "c", "question"], "policy");
    let found = matcher.find("ns", &next, now).unwrap();
    assert_eq!(found.parent, child.session);
    assert_eq!(found.auth, "child-auth");
    // Independently created trajectory, later moved into the same environment by
    // a successful request. It is not an ancestor or descendant of either session.
    let mut competitor = sequence(&["unrelated", "b", "c"], "other-policy");
    let unrelated = matcher.bind("ns", &competitor, "competitor", now).unwrap();
    assert!(unrelated.parent.is_empty());
    competitor.environment = "policy".into();
    assert!(matcher.touch("ns", &competitor, "competitor", now));
    assert!(
        matcher.find("ns", &next, now).is_none(),
        "two unrelated leaves are ambiguous"
    );
    matcher.invalidate("competitor");
    assert_eq!(matcher.find("ns", &next, now).unwrap().parent, child.session);
}

#[test]
fn compaction_bucket_overflow_is_conservative_and_recovers_after_removal_or_expiry() {
    let t0 = Instant::now();
    for expire in [false, true] {
        let mut matcher = Matcher::new(Limits {
            ttl: Duration::from_secs(10),
            ..Limits::default()
        });
        // All entries belong to one lineage. Below the cap the unique leaf wins;
        // above the cap the matcher refuses even that otherwise valid continuation.
        let mut leaf = String::new();
        for i in 0..=MAX_TAILS_PER_KEY {
            let values = [format!("summary-{i}"), "x".into(), "y".into()];
            let request = sequence(&values.iter().map(String::as_str).collect::<Vec<_>>(), "policy");
            let bound = matcher.bind("ns", &request, &format!("auth-{i}"), t0).unwrap();
            if i < MAX_TAILS_PER_KEY {
                leaf = bound.session;
            }
        }
        let candidate = sequence(&["next-summary", "x", "y", "new"], "policy");
        assert!(matcher.find("ns", &candidate, t0).is_none());
        if expire {
            // Keep the previous leaf live; all the other colliding entries expire.
            let previous = sequence(&[&format!("summary-{}", MAX_TAILS_PER_KEY - 1), "x", "y"], "policy");
            assert!(matcher.touch(
                "ns",
                &previous,
                &format!("auth-{}", MAX_TAILS_PER_KEY - 1),
                t0 + Duration::from_secs(5)
            ));
            let recovered = matcher.find("ns", &candidate, t0 + Duration::from_secs(10)).unwrap();
            assert_eq!(recovered.parent, leaf);
        } else {
            matcher.invalidate(&format!("auth-{MAX_TAILS_PER_KEY}"));
            assert_eq!(matcher.find("ns", &candidate, t0).unwrap().parent, leaf);
        }
    }
}

#[test]
fn long_histories_use_actual_tails_and_reject_divergence_outside_the_probe_window() {
    let now = Instant::now();
    let mut matcher = Matcher::new(Limits {
        max_turns: 8,
        ..Limits::default()
    });
    let full = (0..1100)
        .map(|i| text_turn("user", &format!("turn-{i}")))
        .collect::<Vec<_>>();
    let parent = Prepared::new(&full);
    let root = matcher.bind("ns", &parent, "auth", now).unwrap();
    let mut compacted = vec![text_turn("user", "summary")];
    compacted.extend_from_slice(&full[1097..]);
    compacted.push(text_turn("user", "next"));
    let hit = matcher.find("ns", &Prepared::new(&compacted), now).unwrap();
    assert_eq!(hit.node(), ("compaction", false, true));
    assert_eq!(hit.prefix_length, 3);
    assert_eq!(hit.parent, root.session);
    // The request can itself exceed the capped prefix history. The real tail,
    // rather than the eighth indexed turn, determines continuation.
    let mut long_candidate = vec![text_turn("user", "summary")];
    long_candidate.extend_from_slice(&full[2..]);
    long_candidate.push(text_turn("user", "next"));
    assert!(long_candidate.len() > 1024);
    let mut long_matcher = Matcher::new(Limits {
        max_turns: 1024,
        ..Limits::default()
    });
    long_matcher.bind("ns", &parent, "auth", now).unwrap();
    let long_hit = long_matcher.find("ns", &Prepared::new(&long_candidate), now).unwrap();
    assert_eq!(long_hit.parent, root.session);
    assert_eq!(long_hit.prefix_length, 31);
    assert!(long_hit.compaction);
    // The large candidate's divergent head may not be hidden by its matching tail:
    // an overlap far beyond the allowed summary boundary is not compaction.
    let mut divergent = (0..100)
        .map(|i| text_turn("user", &format!("other-{i}")))
        .collect::<Vec<_>>();
    divergent.extend_from_slice(&full[1097..]);
    assert!(matcher.find("ns", &Prepared::new(&divergent), now).is_none());
}

#[test]
fn environment_changes_through_touch_isolate_system_context_even_outside_the_prefix() {
    let now = Instant::now();
    let mut matcher = Matcher::default();
    let old = [
        text_turn("system", "first-policy"),
        text_turn("user", "old"),
        text_turn("system", "middle-policy"),
        text_turn("user", "x"),
        text_turn("assistant", "y"),
    ];
    let bound = matcher.bind("ns", &Prepared::new(&old), "auth", now).unwrap();
    let candidate = |first: &str, middle: Option<&str>| {
        let mut turns = vec![text_turn("system", first), text_turn("user", "summary")];
        if let Some(middle) = middle {
            turns.push(text_turn("system", middle));
        }
        turns.extend_from_slice(&old[3..]);
        Prepared::new(&turns)
    };
    let same = candidate("first-policy", Some("middle-policy"));
    assert_eq!(matcher.find("ns", &same, now).unwrap().parent, bound.session);
    assert!(
        matcher
            .find("ns", &candidate("changed", Some("middle-policy")), now)
            .is_none()
    );
    assert!(
        matcher
            .find("ns", &candidate("first-policy", Some("changed")), now)
            .is_none()
    );
    assert!(matcher.find("ns", &candidate("first-policy", None), now).is_none());
    // A capped head can remain identical while a later system instruction changes.
    let mut capped = Matcher::new(Limits {
        max_turns: 2,
        ..Limits::default()
    });
    capped.bind("ns", &Prepared::new(&old), "auth", now).unwrap();
    let mut updated = old.clone();
    updated[2] = text_turn("system", "new-policy");
    assert!(capped.touch("ns", &Prepared::new(&updated), "auth", now));
    let changed = candidate("first-policy", Some("new-policy"));
    assert!(capped.find("ns", &same, now).is_none());
    assert!(capped.find("ns", &changed, now).unwrap().compaction);
}

#[test]
fn generations_survive_clear_and_reset_and_peek_never_extends_a_lease() {
    let t0 = Instant::now();
    let limits = Limits {
        ttl: Duration::from_secs(10),
        ..Limits::default()
    };
    let p = sequence(&["turn"], "policy");
    let mut matcher = Matcher::new(limits);
    let old = matcher.bind("ns", &p, "auth", t0).unwrap();
    matcher.clear();
    let fresh = matcher.bind("ns", &p, "auth", t0).unwrap();
    assert!(fresh.access > old.access);
    assert!(!matcher.remove("ns", &p.fingerprints, "auth", old.access, t0));
    matcher.reset(limits);
    let reset = matcher.bind("ns", &p, "auth", t0).unwrap();
    assert!(reset.access > fresh.access);
    assert!(!matcher.remove("ns", &p.fingerprints, "auth", fresh.access, t0));
    assert!(matcher.peek("ns", &p, t0 + Duration::from_secs(9)).is_some());
    assert!(matcher.find("ns", &p, t0 + Duration::from_secs(10)).is_none());
    let earlier = t0 - Duration::from_secs(1);
    let rebound = matcher.bind("ns", &p, "auth", earlier).unwrap();
    assert!(
        rebound.access > reset.access,
        "wall-clock order never reverses generations"
    );
}

#[test]
fn instruction_only_prefixes_and_invalid_context_never_acquire_a_binding() {
    let now = Instant::now();
    let mut matcher = Matcher::new(Limits {
        max_turns: 2,
        ..Limits::default()
    });
    for prepared in [
        Prepared {
            fingerprints: vec![],
            min_prefix: 1,
            tails: vec![],
            environment: String::new(),
        },
        Prepared {
            fingerprints: vec!["system".into()],
            min_prefix: 0,
            tails: vec![],
            environment: String::new(),
        },
        Prepared {
            fingerprints: vec!["system".into(), "system2".into(), "user".into()],
            min_prefix: 3,
            tails: vec![],
            environment: String::new(),
        },
    ] {
        assert!(matcher.bind("ns", &prepared, "auth", now).is_none());
        assert!(matcher.find("ns", &prepared, now).is_none());
        assert!(!matcher.touch("ns", &prepared, "auth", now));
    }
    let p = sequence(&["user"], "policy");
    assert!(matcher.bind("", &p, "auth", now).is_none());
    assert!(matcher.bind("ns", &p, " ", now).is_none());
    assert!(!matcher.touch("ns", &p, "", now));
    assert!(!matcher.remove("ns", &[], "auth", 0, now));
    assert!(matcher.lookup("", now).is_none());
    // Two requests sharing only instructions must not inherit each other's auth.
    let mut a = sequence(&["system", "question-a"], "policy");
    a.min_prefix = 2;
    let mut b = sequence(&["system", "question-b"], "policy");
    b.min_prefix = 2;
    matcher.bind("ns", &a, "a", now).unwrap();
    assert!(matcher.find("ns", &b, now).is_none());
}

#[test]
fn parallel_tools_are_order_independent_without_reordering_text_parts() {
    let calls = [
        serde_json::json!({"type":"tool_use","id":"b","name":"read","input":{"path":"b"}}),
        serde_json::json!({"type":"tool_use","id":"a","name":"read","input":{"path":"a"}}),
        serde_json::json!({"type":"tool_use","id":"c","name":"read","input":{"path":"c"}}),
    ];
    let make = |order: [usize; 3]| {
        let content = vec![
            serde_json::json!({"type":"text","text":"before"}),
            calls[order[0]].clone(),
            serde_json::json!({"type":"text","text":"between"}),
            calls[order[1]].clone(),
            calls[order[2]].clone(),
        ];
        extract(
            "claude",
            &serde_json::to_vec(&serde_json::json!({"messages":[{"role":"assistant","content":content}]})).unwrap(),
        )
    };
    let first = make([0, 1, 2]);
    for order in [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]] {
        assert_eq!(make(order), first);
    }
    assert_eq!(first[0].parts[0].value, b"before");
    assert_eq!(first[0].parts[2].value, b"between");
}
