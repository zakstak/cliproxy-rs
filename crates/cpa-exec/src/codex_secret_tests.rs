//! Generated inputs (no recorded provider traffic): the guard's reading of original
//! bytes, bound to the values one request carried.

use super::*;
use cpa_core::exec::{Delivery, ExecError, FailureScope};
use http::HeaderValue;

const SELECTED: &str = "at-SELECTED.token_9";

fn bearer(token: &str) -> Secrets {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    h.insert("chatgpt-account-id", HeaderValue::from_static("acct-PUBLIC"));
    h.insert("session-id", HeaderValue::from_static("sess-PUBLIC"));
    Secrets::from_request(&h, "https://chatgpt.example/backend-api/codex/responses")
}

#[test]
fn values_come_from_what_the_request_carried() {
    let s = bearer(SELECTED);
    assert!(s.exposes(SELECTED.as_bytes()));
    // Routing, account and session identities are no authentication values.
    for public in ["acct-PUBLIC", "sess-PUBLIC", "Bearer", "chatgpt.example"] {
        assert!(!s.exposes(public.as_bytes()), "{public}");
    }
    // Another account's token, or the same text that was never sent, is no secret here.
    assert!(!s.exposes(b"at-OTHER.token_9"));
    assert!(!bearer("at-OTHER").exposes(SELECTED.as_bytes()));
    // Empty values match nothing.
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("Bearer  "));
    h.insert("cookie", HeaderValue::from_static("a=; b= ;"));
    let empty = Secrets::from_request(&h, "http://127.0.0.1:1/x");
    assert!(!empty.exposes(b"anything at all, Bearer, a=, b"));
    // A scheme-less value, cookie values, API-key headers and a URL password.
    let mut h = HeaderMap::new();
    h.insert("authorization", HeaderValue::from_static("rawtoken-1"));
    h.insert("cookie", HeaderValue::from_static("sid=cookie-2; other=cookie-3"));
    h.insert("x-api-key", HeaderValue::from_static("key-4"));
    let s = Secrets::from_request(&h, "https://user:p%40ss-5@relay.example/v1");
    for value in ["rawtoken-1", "cookie-2", "cookie-3", "key-4", "p@ss-5", "p%40ss-5"] {
        assert!(s.exposes(format!("x {value} y").as_bytes()), "{value}");
    }
    assert!(!s.exposes(b"sid other user relay.example"));
    assert_eq!(format!("{s:?}"), "Secrets(6 redacted)");
}

#[test]
fn every_spelling_of_the_original_bytes_counts() {
    let s = bearer("at-FAKE");
    for raw in [
        // As written, in a member a last-key reader drops, in an array, malformed.
        r#"{"type":"error","error":{"message":"at-FAKE"},"error":{"message":"safe"}}"#,
        r#"["response.failed",null,null,{"error":["x","y","bad at-FAKE"]}]"#,
        r#"{"type":"response.failed","response":{"error":{"message":"at-FAKE""#,
        // JSON escapes, including a surrogate-free \u spelling of every letter.
        r#"{"m":"at\u002dFAKE"}"#,
        r#"{"m":"\u0061\u0074\u002d\u0046\u0041\u004b\u0045"}"#,
        r#"{"m":"at\-FAKE at\/x at-FA\u004bE"}"#,
        // A private raw value's string, and one escaped inside it.
        r#"{"error":{"$serde_json::private::RawValue":"{\"token\":\"at-FAKE\"}"}}"#,
        r#"{"error":{"$serde_json::private::RawValue":"{\"token\":\"at\\u002dFAKE\"}"}}"#,
        r#"{"a":"{\"b\":\"{\\\"c\\\":\\\"at\\\\u002dFAKE\\\"}\"}"}"#,
    ] {
        assert!(s.exposes(raw.as_bytes()), "{raw}");
    }
    for raw in [
        r#"{"type":"error","error":{"message":"at-FAK","code":"FAKE at-"}}"#,
        r#"{"m":"at\u002dFAKF","n":"\ud800 lone \\ \q"}"#,
        r#"{"m":"at\\-FAKE"}"#,
    ] {
        assert!(!s.exposes(raw.as_bytes()), "{raw}");
    }
    // A run of backslashes that keeps unescaping past the last round counts.
    let deep = format!("{{\"m\":\"{}\"}}", "\\".repeat(1 << 18));
    assert!(s.exposes(deep.as_bytes()));
    let shallow = format!("{{\"m\":\"{}\"}}", "\\".repeat(1 << 10));
    assert!(!s.exposes(shallow.as_bytes()));
}

#[test]
fn a_failure_keeps_its_classification_and_loses_its_delivery() {
    let s = bearer("at-FAKE");
    let frame = Bytes::from_static(br#"{"type":"error","status":400,"error":{"message":"bad at-FAKE"}}"#);
    let mut error = ExecError::local(400, FailureScope::Request, r#"{"error":{"message":"bad"}}"#);
    error.delivery = Delivery::AsSent(Box::new(frame));
    error.direct = true;
    error.delivered_status = Some(400);
    let guarded = s.guarded(error);
    assert_eq!(guarded.delivery, Delivery::Confidential);
    assert_eq!((guarded.status, guarded.scope), (400, FailureScope::Request));
    assert_eq!(&guarded.body[..], br#"{"error":{"message":"bad"}}"#);
    assert!(!guarded.direct && guarded.delivered_status.is_none() && guarded.addon.is_none());
    // A Withheld account failure whose body carries the value becomes Confidential.
    let mut account = ExecError::local(429, FailureScope::Credential, "limited at-FAKE");
    account.delivery = Delivery::Withheld;
    assert_eq!(s.guarded(account).delivery, Delivery::Confidential);
    // An addon header carrying it.
    let mut headers = HeaderMap::new();
    headers.insert("x-echo", HeaderValue::from_static("at-FAKE"));
    let mut addon = ExecError::local(503, FailureScope::Credential, "busy");
    addon.addon = Some(Box::new(headers));
    assert_eq!(s.guarded(addon).delivery, Delivery::Confidential);
    // A failure without the value keeps its delivery, bytes and all.
    let safe = Bytes::from_static(br#"{ "type":"error","error":{"message":"at-FAKF"} , "n":1.50 }"#);
    let mut error = ExecError::local(400, FailureScope::Request, "x");
    error.delivery = Delivery::AsSent(Box::new(safe.clone()));
    assert_eq!(s.guarded(error).delivered_event(), Some(&safe));
}

#[tokio::test]
async fn a_stream_drops_frames_that_carry_a_value_and_goes_on() {
    let s = bearer("at-FAKE");
    let safe1 = Bytes::from_static(br#"{"type":"response.created","response":{"id":"r"},"x":[1,1.0]}"#);
    let leak = Bytes::from_static(br#"{"type":"error","status":200,"error":{"message":"at\u002dFAKE"}}"#);
    let safe2 = Bytes::from_static(br#"{"type":"response.completed","response":{"id":"r"}}"#);
    let failure = ExecError::local(502, FailureScope::Credential, "upstream said at-FAKE");
    let items: Vec<Result<Bytes, ExecError>> = vec![Ok(safe1.clone()), Ok(leak), Ok(safe2.clone()), Err(failure)];
    let out: Vec<_> = s
        .guard_stream(futures_util::stream::iter(items).boxed())
        .collect()
        .await;
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].as_ref().unwrap(), &safe1);
    assert_eq!(out[1].as_ref().unwrap(), &safe2);
    let error = out[2].as_ref().unwrap_err();
    assert_eq!((error.status, error.delivery.clone()), (502, Delivery::Confidential));
}

#[test]
fn a_buffered_answer_with_a_value_fails_request_scoped() {
    let s = bearer("at-FAKE");
    let response = |body: &'static [u8]| {
        let mut headers = HeaderMap::new();
        headers.insert("x-echo", HeaderValue::from_static("Bearer at-FAKE"));
        headers.insert("x-request-id", HeaderValue::from_static("req-1"));
        Ok(ExecResponse {
            status: 200,
            headers,
            body: ResponseBody::Buffered(Bytes::from_static(body)),
        })
    };
    let error = s
        .guard_response(response(br#"{"id":"r","output":"at-FAKE"}"#))
        .err()
        .unwrap();
    assert_eq!((error.status, error.scope), (502, FailureScope::Request));
    assert_eq!(error.delivery, Delivery::Confidential);
    let ok = s.guard_response(response(br#"{"id":"r" , "output":"safe"}"#)).unwrap();
    assert!(ok.headers.get("x-echo").is_none());
    assert_eq!(ok.headers.get("x-request-id").unwrap(), "req-1");
    let ResponseBody::Buffered(body) = ok.body else {
        panic!()
    };
    assert_eq!(&body[..], br#"{"id":"r" , "output":"safe"}"#);
}

#[test]
fn non_direct_handshake_headers_are_guarded_before_limit_rendering() {
    for token in ["at-FAKE", "at-OTHER"] {
        let mut error = ExecError::local(
            429,
            FailureScope::Credential,
            r#"{"error":{"type":"usage_limit_reached"}}"#,
        );
        error
            .headers
            .insert("x-codex-active-limit", HeaderValue::from_str(token).unwrap());
        assert!(!error.direct && error.addon.is_none());
        let guarded = bearer("at-FAKE").guarded(error);
        assert_eq!(guarded.status, 429);
        assert_eq!(guarded.confidential(), token == "at-FAKE");
    }
}
