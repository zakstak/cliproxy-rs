use super::*;

fn slots(payload: &str) -> Vec<(String, &str)> {
    occurrences(payload)
        .into_iter()
        .map(|(slot, range)| {
            let name = match slot {
                Slot::Top(n) => n.to_owned(),
                Slot::Response => "response".to_owned(),
                Slot::ResponseError => "response.error".to_owned(),
                Slot::Field(Object::ResponseError, n) => format!("response.error.{n}"),
                Slot::Field(_, n) => format!("error.{n}"),
            };
            (name, &payload[range])
        })
        .collect()
}

#[test]
fn occurrences_are_the_members_the_failure_readings_read() {
    assert_eq!(
        slots(
            r#"{"type":"a","x":{"type":"b"},"t\u0079pe":"c","error":{"code":"d","type":"e","message":{"m":1}},"response":{"error":"f","id":"g"},"status":429}"#
        ),
        [
            ("type".to_owned(), r#""a""#),
            ("type".to_owned(), r#""c""#),
            ("error.code".to_owned(), r#""d""#),
            ("error.type".to_owned(), r#""e""#),
            ("error.message".to_owned(), r#"{"m":1}"#),
            ("error".to_owned(), r#"{"code":"d","type":"e","message":{"m":1}}"#),
            ("response.error".to_owned(), r#""f""#),
            ("response".to_owned(), r#"{"error":"f","id":"g"}"#),
            ("status".to_owned(), "429"),
        ]
    );
    // Inside a string, nothing is a member; a nested `error` is no error object.
    assert!(slots(r#"{"note":"\"type\":\"error\"","meta":{"error":{"type":"x"}}}"#).is_empty());
    // An array element and an array root read nothing either.
    assert!(slots(r#"[{"type":"error"}]"#).is_empty());
    assert_eq!(
        slots(r#"{"items":[{"type":"x"}],"type":"y"}"#),
        [("type".to_owned(), r#""y""#)]
    );
    // Invalid JSON is read as far as it goes; a value cut off ends with the input.
    assert_eq!(
        slots(r#"{"type":"a","type":"b","error":{"type":"c""#),
        [
            ("type".to_owned(), r#""a""#),
            ("type".to_owned(), r#""b""#),
            ("error.type".to_owned(), r#""c""#),
            ("error".to_owned(), r#"{"type":"c""#),
        ]
    );
    assert_eq!(slots(r#"{"type":"error"#), [("type".to_owned(), r#""error"#)]);
    // A key that does not unescape names nothing.
    assert!(slots(r#"{"type\ud800":"error"}"#).is_empty());
}

#[test]
fn any_occurrence_of_a_repeated_member_counts() {
    for frame in [
        r#"{"type":"response.future","type":"error","type":"response.future","status":429,"error":{"type":"rate_limit_error","message":"limited"}}"#,
        r#"{"type":"response.future","type":"error","status":400,"status":429,"status":400,"error":{"code":"other"}}"#,
        r#"{"type":"response.future","type":"error","status_code":400,"status_code":401,"status_code":400}"#,
        r#"{"type":"error","type":"x","error":{"type":"invalid_request_error","type":"authentication_error","type":"invalid_request_error"}}"#,
        r#"{"type":"error","type":"x","error":{"code":"other","code":"invalid_api_key","code":"other"}}"#,
        r#"{"type":"error","type":"x","error":{"code":"other"},"error":{"type":"usage_limit_reached"},"error":{"code":"other"}}"#,
        r#"{"type":"response.future","\u0074ype":"error","type":"response.future","status":429}"#,
        r#"{"type":"a","\u0074\u0079\u0070\u0065":"error","type":"b","type":"c","s\u0074atus":403}"#,
        r#"{"type":"x","type":"response.failed","type":"x","response":{"error":{"code":"rate_limit_exceeded"}}}"#,
        r#"{"type":"x","type":"response.failed","response":{"error":{"status":402}},"response":{"error":{"status":400}}}"#,
        r#"{"type":"x","type":"error","error":{"message":"m"},"error":{"message":"The model is at capacity"}}"#,
        r#"{"type":"x","type":"error","code":"other","code":"forbidden"}"#,
        r#"{"type":"x","type":"error","error_type":"permission_error"}"#,
        r#"{"type":"response.future","type":"error","type":"response.future","status":429,"error":{"type":"rate_limit_error""#,
    ] {
        assert!(ambiguous_account(frame), "{frame}");
    }
    for frame in [
        // Nothing repeats: the first and last readings already decide.
        r#"{"type":"error","delta":5,"status":400,"error":{"status":400,"type":"rate_limit_error"}}"#,
        r#"{"type":"response.future","delta":5,"status":429,"error":{"type":"rate_limit_error"}}"#,
        // Repeated, but no occurrence is an account failure.
        r#"{"type":"response.future","type":"error","status":400,"error":{"code":"other","message":"bad input"}}"#,
        r#"{"type":"a","type":"b","type":"c","status":429,"error":{"type":"rate_limit_error"}}"#,
        r#"{"type":"x","type":"x","note":"\"type\":\"error\",\"status\":429","error":{"type":"rate_limit_error"}}"#,
        r#"{"type":"x","type":"error","status":400,"meta":{"status":429,"error":{"type":"rate_limit_error"}},"error":{"code":"other"}}"#,
        r#"{"type":"error","type":"error","status":200,"error":{"code":"other","type":"invalid_request_error"}}"#,
    ] {
        assert!(!ambiguous_account(frame), "{frame}");
    }
}

/// Members to generate frames from, account-shaped and not.
const ROOT: [(&str, &[&str]); 8] = [
    ("type", &[r#""error""#, r#""response.failed""#, r#""response.future""#]),
    ("status", &["400", "429", "200", r#""401""#]),
    ("status_code", &["403", "500"]),
    ("code", &[r#""other""#, r#""invalid_api_key""#]),
    ("message", &[r#""fine""#, r#""model is at capacity""#]),
    ("error_type", &[r#""x""#, r#""usage_limit_reached""#]),
    ("error", &["E"]),
    ("response", &["R"]),
];
const ERROR: [(&str, &[&str]); 4] = [
    (
        "type",
        &[
            r#""invalid_request_error""#,
            r#""rate_limit_error""#,
            r#""usage_limit_reached""#,
            r#""authentication_error""#,
        ],
    ),
    (
        "code",
        &[
            r#""other""#,
            r#""rate_limit_exceeded""#,
            r#""cyber_policy""#,
            r#""context_length_exceeded""#,
        ],
    ),
    ("status", &["400", "403", "429"]),
    ("message", &[r#""m""#, r#""model at capacity""#]),
];

/// A deterministic generator (xorshift), so a failure reproduces.
struct Rng(u64);
impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// An object as its members, in order, each a key and its value's alternatives: the
/// value is one JSON text, or a nested object.
#[derive(Clone)]
enum Value {
    Text(String),
    Object(Vec<(String, Value)>),
}

fn object(rng: &mut Rng, pool: &[(&str, &[&str])], depth: usize) -> Vec<(String, Value)> {
    let n = 2 + rng.below(6);
    (0..n)
        .map(|_| {
            let (key, values) = pool[rng.below(pool.len())];
            let value = match values[rng.below(values.len())] {
                "E" if depth == 0 => Value::Object(object(rng, &ERROR, 1)),
                "R" if depth == 0 => Value::Object(vec![("error".to_owned(), Value::Object(object(rng, &ERROR, 1)))]),
                "E" | "R" => Value::Text("{}".to_owned()),
                text => Value::Text(text.to_owned()),
            };
            (key.to_owned(), value)
        })
        .collect()
}

fn render(members: &[(String, Value)]) -> String {
    let body: Vec<String> = members
        .iter()
        .map(|(k, v)| match v {
            Value::Text(t) => format!(r#""{k}":{t}"#),
            Value::Object(m) => format!(r#""{k}":{}"#, render(m)),
        })
        .collect();
    format!("{{{}}}", body.join(","))
}

/// Every reading that keeps one occurrence of each repeated key, in every object.
fn projections(members: &[(String, Value)]) -> Vec<Vec<(String, Value)>> {
    let mut keys: Vec<&str> = Vec::new();
    for (k, _) in members {
        if !keys.contains(&k.as_str()) {
            keys.push(k);
        }
    }
    let mut out: Vec<Vec<(String, Value)>> = vec![Vec::new()];
    for key in keys {
        let mut choices = Vec::new();
        for (k, v) in members.iter().filter(|(k, _)| k == key) {
            match v {
                Value::Text(_) => choices.push((k.clone(), v.clone())),
                Value::Object(inner) => {
                    for p in projections(inner) {
                        choices.push((k.clone(), Value::Object(p)));
                    }
                }
            }
        }
        out = out
            .into_iter()
            .flat_map(|prefix| {
                choices.iter().map(move |c| {
                    let mut next = prefix.clone();
                    next.push(c.clone());
                    next
                })
            })
            .collect();
    }
    out
}

/// No single-occurrence reading of a frame is an account failure that the frame's own
/// check misses: generated frames with repeated members at the frame, inside `error` and
/// inside `response.error`, against every projection read by the exact readings
/// (`codex_ws::account_shaped` with nothing repeated).
#[test]
fn no_reading_of_a_repeated_member_escapes_the_check() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut checked, mut account, mut beyond_first_and_last) = (0, 0, 0);
    while checked < 3000 {
        let members = object(&mut rng, &ROOT, 0);
        let all = projections(&members);
        if all.len() > 512 {
            continue;
        }
        checked += 1;
        let frame = render(&members);
        let any = all.iter().any(|p| crate::codex_ws::account_shaped(&render(p), true));
        if any {
            account += 1;
            assert!(
                crate::codex_ws::account_shaped(&frame, false),
                "{frame}: a reading is an account failure"
            );
            // Neither the first nor the last of each repeated key is one.
            if !crate::codex_ws::account_shaped(&frame, true) {
                beyond_first_and_last += 1;
            }
        }
    }
    eprintln!("{account} of {checked} frames have an account reading, {beyond_first_and_last} past the first and last");
    // The generator reaches both outcomes, and readings past the first and the last.
    assert!(account > 100 && account < 2900, "{account} of {checked}");
    assert!(beyond_first_and_last > 10, "{beyond_first_and_last} of {account}");
}

/// The pass is linear: a frame with very many repeated members is checked at once.
#[test]
fn many_repeated_members_are_checked_in_linear_time() {
    let mut frame = String::from("{");
    for _ in 0..200_000 {
        frame.push_str(r#""type":"response.future","error":{"code":"other"},"#);
    }
    frame.push_str(r#""type":"error","status":429}"#);
    assert!(ambiguous_account(&frame));
    frame.truncate(frame.len() - r#""type":"error","status":429}"#.len());
    frame.push_str(r#""status":429}"#);
    assert!(!ambiguous_account(&frame));
}

/// A failure reading composes one error body from several members
/// (`response::terminal_failure_body`): the error object as sent, the frame's `message`,
/// `code`, `error_type` and `param`, and the other error object's message. Its capacity
/// check reads the whole body, so pieces that are no capacity message alone can make one
/// together. Each frame's middle reading of a repeated `type` is a 429 by the exact
/// readings (`codex_ws::account_shaped` of that projection), and the frame's own check
/// finds it; the first and last readings do not. `<BS>` stands for a backslash.
#[test]
fn capacity_messages_composed_from_several_members_count() {
    let middle = |frame: &str| frame.replacen(r#""type":"x","type":"error","type":"x""#, r#""type":"error""#, 1);
    for frame in [
        r#"{"type":"x","type":"error","type":"x","code":"model","message":"at capacity"}"#,
        r#"{"type":"x","type":"error","type":"x","param":"model at capacity"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"type":"model"},"response":{"error":{"message":"at capacity"}}}"#,
        r#"{"type":"x","type":"error","type":"x","error_type":"model","param":"at capacity"}"#,
        r#"{"type":"x","type":"error","type":"x","message":"mod<BS>u0065l at<BS>u0020capacity"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"model":1,"x":"at capacity"}}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"code":"mod<BS>u0065l_at_capacity"}}"#,
        r#"{"type":"x","type":"error","type":"x","error":"model at capacity"}"#,
        r#"{"type":"x","type":"error","type":"x","code":{"m":"model at capacity"}}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"$serde_json::private::RawValue":"{<BS>"type<BS>":<BS>"rate_limit_error<BS>"}"}}"#,
    ] {
        let frame = frame.replace("<BS>", "\\");
        assert!(
            crate::codex_ws::account_shaped(&middle(&frame), true),
            "{frame}: the middle reading is an account failure"
        );
        assert!(
            !crate::codex_ws::account_shaped(&frame, true),
            "{frame}: first and last readings"
        );
        assert!(ambiguous_account(&frame), "{frame}");
    }
    for frame in [
        // The pieces sit where no reading composes them, or are no capacity message.
        r#"{"type":"x","type":"error","type":"x","meta":{"code":"model","message":"at capacity"},"error":{"code":"other"}}"#,
        r#"{"type":"x","type":"error","type":"x","code":"model","message":"busy"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"message":"at capacity"},"note":"model"}"#,
        r#"{"type":"x","type":"error","type":"x","error":{"x":"mod<BS>u0065l","y":"at capacity"}}"#,
    ] {
        let frame = frame.replace("<BS>", "\\");
        assert!(!crate::codex_ws::account_shaped(&middle(&frame), true), "{frame}");
        assert!(!ambiguous_account(&frame), "{frame}");
    }
}

/// Members whose pieces compose capacity messages, and others.
const COMPOSED_ROOT: [(&str, &[&str]); 9] = [
    ("type", &[r#""error""#, r#""response.failed""#, r#""response.future""#]),
    ("code", &[r#""model""#, r#""other""#, r#""model_at_capacity""#]),
    ("message", &[r#""at capacity""#, r#""fine""#, r#""mod<BS>u0065l""#]),
    ("error_type", &[r#""model""#, r#""x""#]),
    ("param", &[r#""at capacity""#, r#""p""#, r#""model at capacity""#]),
    ("error", &["E", r#""model""#, r#""at capacity""#]),
    ("response", &["R"]),
    ("note", &[r#""model at capacity""#]),
    ("status", &["400"]),
];
const COMPOSED_ERROR: [(&str, &[&str]); 5] = [
    ("type", &[r#""model""#, r#""invalid_request_error""#]),
    ("code", &[r#""other""#, r#""model""#]),
    ("message", &[r#""at capacity""#, r#""m""#]),
    ("note", &[r#""model""#, r#""at capacity""#, r#""x""#]),
    ("status", &["400"]),
];

fn composed_object(rng: &mut Rng, pool: &[(&str, &[&str])], depth: usize) -> Vec<(String, Value)> {
    let n = 2 + rng.below(6);
    (0..n)
        .map(|_| {
            let (key, values) = pool[rng.below(pool.len())];
            let value = match values[rng.below(values.len())] {
                "E" if depth == 0 => Value::Object(composed_object(rng, &COMPOSED_ERROR, 1)),
                "R" if depth == 0 => Value::Object(vec![(
                    "error".to_owned(),
                    Value::Object(composed_object(rng, &COMPOSED_ERROR, 1)),
                )]),
                "E" | "R" => Value::Text("{}".to_owned()),
                text => Value::Text(text.replace("<BS>", "\\")),
            };
            (key.to_owned(), value)
        })
        .collect()
}

/// The soundness of the composed capacity check: generated frames whose pieces interact
/// (split phrases, `param`, an escaped phrase, an error string, a message from the other
/// error object, the same pieces in an unrelated member), against every projection read
/// by the exact readings (`codex_ws::account_shaped` with nothing repeated).
#[test]
fn no_composed_reading_of_a_repeated_member_escapes_the_check() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let (mut checked, mut account, mut beyond_first_and_last) = (0, 0, 0);
    while checked < 3000 {
        let members = composed_object(&mut rng, &COMPOSED_ROOT, 0);
        let all = projections(&members);
        if all.len() > 512 {
            continue;
        }
        checked += 1;
        let frame = render(&members);
        if all.iter().any(|p| crate::codex_ws::account_shaped(&render(p), true)) {
            account += 1;
            assert!(
                crate::codex_ws::account_shaped(&frame, false),
                "{frame}: a reading is an account failure"
            );
            if !crate::codex_ws::account_shaped(&frame, true) {
                beyond_first_and_last += 1;
            }
        }
    }
    eprintln!("{account} of {checked} frames have an account reading, {beyond_first_and_last} past the first and last");
    assert!(account > 100 && account < 2900, "{account} of {checked}");
    assert!(beyond_first_and_last > 10, "{beyond_first_and_last} of {account}");
}

/// The composed check stays linear: very many repeated pieces are summarized at once.
#[test]
fn many_composed_pieces_are_checked_in_linear_time() {
    let mut frame = String::from("{");
    for _ in 0..100_000 {
        frame.push_str(r#""type":"response.future","code":"model","error":{"type":"x"},"#);
    }
    frame.push_str(r#""type":"error","message":"at capacity"}"#);
    assert!(ambiguous_account(&frame));
    frame.truncate(frame.len() - r#""type":"error","message":"at capacity"}"#.len());
    frame.push_str(r#""type":"error","message":"busy"}"#);
    assert!(!ambiguous_account(&frame));
}

/// A member repeats only when its path does: the same key twice in one object, or in two
/// occurrences of a repeated ancestor. The same name in distinct objects is two members,
/// so a frame with each once is read by its first and last readings alone and reaches
/// the client when those classify no account failure (#25 review, safe unique frame:
/// its first and last readings select the root error, a 400). A repeated ancestor still
/// counts, with every occurrence: two `error` objects, two `response` objects (the
/// middle one carrying the only `response.error`), or two `error`s inside one `response`.
#[test]
fn only_a_repeated_path_is_a_repeated_member() {
    let unique = r#"{"type":"error","item_id":23,"error":{"type":"invalid_request_error","message":"bad input"},"response":{"error":{"type":"model_at_capacity"}}}"#;
    for frame in [
        unique,
        r#"{"type":"error","error":{"code":"other","status":400},"response":{"error":{"code":"rate_limit_exceeded","status":429}}}"#,
        r#"{"type":"response.failed","error":{"type":"invalid_request_error"},"response":{"error":{"message":"bad"},"id":"r1"},"meta":{"type":"rate_limit_error"}}"#,
    ] {
        assert!(!ambiguous_account(frame), "{frame}");
    }
    assert!(!crate::codex_ws::account_shaped(unique, false));
    for frame in [
        r#"{"type":"error","error":{"type":"invalid_request_error"},"error":{"type":"invalid_request_error"},"response":{"error":{"type":"model_at_capacity"}}}"#,
        r#"{"type":"response.failed","response":{"id":"a"},"response":{"error":{"code":"rate_limit_exceeded"}},"response":{"id":"b"}}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"other"},"error":{"type":"permission_error"},"error":{"code":"other"}}}"#,
        r#"{"type":"error","error":{"type":"invalid_request_error","type":"invalid_request_error"},"response":{"error":{"type":"rate_limit_error"}}}"#,
    ] {
        assert!(ambiguous_account(frame), "{frame}");
    }
    let middle = r#"{"type":"response.failed","response":{"id":"a"},"response":{"error":{"code":"rate_limit_exceeded"}},"response":{"id":"b"}}"#;
    assert!(crate::codex_ws::account_shaped(middle, false));
}
