//! Realtime behavior tests with independently authored JSON, MIME, and URL examples.
//!
//! `docs/codex/CONTRACT.md` excludes realtime from its Responses payload contract.
//! These tests preserve the proxy's supported realtime adaptation policy. SDP/MIME
//! examples follow RFC 8866, RFC 7578, and RFC 2045 section 6.7. Session envelope
//! fields follow https://developers.openai.com/api/docs/guides/realtime-webrtc
//! and https://developers.openai.com/api/reference/resources/realtime/subresources/client_secrets/methods/create.
//! Codex model aliases and legacy sideband paths are explicit proxy policy, not
//! claims about first-party Responses requests. No Go-produced outputs are read.

use super::*;
use serde_json::{Value, json};

fn value(body: &[u8]) -> Value {
    serde_json::from_slice(body).expect("valid JSON output")
}

fn invalid_shape(result: Result<Call, ShapeError>) {
    assert!(matches!(result, Err(ShapeError::Invalid(_))));
}

const OFFER: &str = "v=0\r\no=- 7 2 IN IP4 127.0.0.1\r\ns=-\r\na=mid:0\r\n";
const ANSWER: &str = "v=0\r\no=- 8 2 IN IP4 192.0.2.1\r\ns=-\r\na=mid:0\r\n";

// Test-only MIME construction. Explicit field bytes never use the shaping code.
fn multipart(parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, headers, bytes) in parts {
        body.extend_from_slice(
            format!("--test-boundary\r\nContent-Disposition: form-data; name=\"{name}\"\r\n{headers}\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"--test-boundary--\r\n");
    body
}
const MULTIPART: &str = "multipart/form-data; boundary=test-boundary";

#[test]
fn preparation_preserves_non_multipart_bytes_and_defaults_only_missing_metadata() {
    for body in [
        b"".as_slice(),
        b"  ",
        b"not json",
        b"\xff\xfe",
        b"null",
        b"[]",
        br#" { "sdp" : "v=0", "x":1.00, "nested":{"z":true} } "#,
        br#"{"session":{"model":"custom-live"}}"#,
        br#"{"model":"gpt-realtime"}"#,
        OFFER.as_bytes(),
    ] {
        for ct in [
            "",
            "application/json",
            "application/json; charset=utf-8",
            "application/sdp",
            "text/plain",
            "application/octet-stream",
            "multipart/mixed; boundary=b",
        ] {
            let call = prepare(body, ct).unwrap();
            assert_eq!(call.body, body, "{ct}");
            assert_eq!(call.content_type, if ct.is_empty() { "application/json" } else { ct });
            let expected = if body == br#"{"session":{"model":"custom-live"}}"# {
                "custom-live"
            } else if body == br#"{"model":"gpt-realtime"}"# {
                "gpt-realtime"
            } else {
                "gpt-live-1-codex"
            };
            assert_eq!(call.model, expected);
        }
    }
}

#[test]
fn model_selection_prefers_a_nonempty_session_model_and_refuses_invalid_field_types() {
    for (body, expected) in [
        (r#"{"model":" top "}"#, "top"),
        (r#"{"model":"top","session":{"model":" session "}}"#, "session"),
        (r#"{"model":"top","session":{"model":" "}}"#, "top"),
        (r#"{"model":"top","session":null}"#, "top"),
        (
            r#"{"session":{"model":"future-live","unknown":[1,true]}}"#,
            "future-live",
        ),
        (r#"{"model":null}"#, ""),
        (r#"{}"#, ""),
        (r#"null"#, ""),
        (r#"[]"#, ""),
        (r#"true"#, ""),
        (r#"12"#, ""),
        (r#""text""#, ""),
        (r#"{"model":4,"session":{"model":"valid"}}"#, ""),
        (r#"{"model":"top","session":[]}"#, ""),
        (r#"{"model":"top","session":{"model":false}}"#, ""),
        (r#"{"model":"cut""#, ""),
        (r#"{} trailing"#, ""),
    ] {
        assert_eq!(model_from_json(body.as_bytes()), expected, "{body}");
    }
}

#[test]
fn multipart_preparation_preserves_sdp_session_extensions_and_last_field_values() {
    for session in [
        None,
        Some(br#"{"model":"custom-live","instructions":"<help>&","future":{"x":[1,true]}}"#.as_slice()),
    ] {
        let mut parts = vec![
            ("sdp", "Content-Type: application/sdp\r\n", OFFER.as_bytes()),
            ("ignored", "", b"discard".as_slice()),
        ];
        if let Some(session) = session {
            parts.push(("session", "Content-Type: application/json\r\n", session));
        }
        let body = multipart(&parts);
        for ct in [MULTIPART, "MULTIPART/FORM-DATA; boundary=\"test-boundary\""] {
            let call = prepare(&body, ct).unwrap();
            let actual = value(&call.body);
            assert_eq!(actual["sdp"], OFFER);
            assert_eq!(actual.get("ignored"), None);
            assert_eq!(call.content_type, "application/json");
            assert_eq!(
                call.model,
                if session.is_some() {
                    "custom-live"
                } else {
                    "gpt-live-1-codex"
                }
            );
            if let Some(session) = session {
                assert_eq!(actual["session"], value(session));
            } else {
                assert!(actual.get("session").is_none());
            }
        }
    }
    let body = multipart(&[("sdp", "", b"first"), ("sdp", "", OFFER.as_bytes())]);
    assert_eq!(value(&prepare(&body, MULTIPART).unwrap().body)["sdp"], OFFER);
    // A browser may send the offer as a file upload. Its filename is metadata.
    let file = format!(
        "--test-boundary\r\nContent-Disposition: form-data; name=\"sdp\"; filename=\"offer.sdp\"\r\nContent-Type: application/sdp\r\n\r\n{OFFER}\r\n--test-boundary--\r\n"
    );
    assert_eq!(value(&prepare(file.as_bytes(), MULTIPART).unwrap().body)["sdp"], OFFER);
    let lf = b"--test-boundary\nContent-Disposition: form-data; name=\"sdp\"\n\nv=0\na=mid:0\n--test-boundary--\n";
    assert_eq!(value(&prepare(lf, MULTIPART).unwrap().body)["sdp"], "v=0\na=mid:0");
    let qp = multipart(&[
        (
            "sdp",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            b"v=3D0=0D=0Aa=3Dmid:0=0D=0A",
        ),
        (
            "session",
            "Content-Transfer-Encoding: quoted-printable\r\n",
            b"{=22model=22:=22custom-live=22}",
        ),
    ]);
    let call = prepare(&qp, MULTIPART).unwrap();
    assert_eq!(
        value(&call.body),
        json!({"sdp":"v=0\r\na=mid:0\r\n","session":{"model":"custom-live"}})
    );
    assert_eq!(call.model, "custom-live");
    // MIME preamble and epilogue are framing, not an SDP field.
    let mut body = b"preamble\r\n".to_vec();
    body.extend(multipart(&[("sdp", "", OFFER.as_bytes())]));
    body.extend_from_slice(b"epilogue");
    assert_eq!(value(&prepare(&body, MULTIPART).unwrap().body)["sdp"], OFFER);
}

#[test]
fn multipart_refuses_missing_fields_invalid_json_truncation_and_excessive_headers() {
    for ct in ["multipart/form-data", "multipart/form-data; boundary=\"\""] {
        invalid_shape(prepare(b"", ct));
    }
    for body in [
        b"".to_vec(),
        b"no boundary".to_vec(),
        multipart(&[("session", "", b"{}")]),
        multipart(&[("sdp", "", OFFER.as_bytes()), ("session", "", b"{invalid")]),
        multipart(&[("sdp", "", OFFER.as_bytes()), ("session", "", b"")]),
        b"--test-boundary\r\nbroken header\r\n\r\nvalue\r\n--test-boundary--\r\n".to_vec(),
        b"--test-boundary\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\ntruncated".to_vec(),
        b"--test-boundary\r\nContent-Disposition: form-data; name=\"sdp\"\r\n".to_vec(),
    ] {
        invalid_shape(prepare(&body, MULTIPART));
    }
    let headers = "X-Test: value\r\n".repeat(10_001);
    invalid_shape(prepare(&multipart(&[("sdp", &headers, OFFER.as_bytes())]), MULTIPART));
}

#[test]
fn offers_and_answers_preserve_sdp_and_require_a_nonempty_string() {
    for sdp in [OFFER, ANSWER, "v=0\na=mid:é猫\n", "v=0\r\na=ice-pwd:a<&>\\b\r\n"] {
        let body = serde_json::to_vec(&json!({"sdp":sdp,"future":true})).unwrap();
        for ct in ["application/json", "Application/JSON; charset=utf-8"] {
            assert_eq!(request_sdp(&body, ct).unwrap(), sdp);
            assert_eq!(response_sdp(&body, ct).unwrap(), sdp);
        }
        for ct in ["application/sdp", "Application/SDP; charset=utf-8", "text/plain"] {
            assert_eq!(request_sdp(sdp.as_bytes(), ct).unwrap(), sdp);
            assert_eq!(response_sdp(sdp.as_bytes(), ct).unwrap(), sdp);
        }
        // Some upstreams omit an answer Content-Type. Raw answer bytes are still SDP.
        assert_eq!(response_sdp(sdp.as_bytes(), "").unwrap(), sdp);
    }
    for body in [
        b"".as_slice(),
        b" ",
        b"{}",
        b"null",
        b"[]",
        b"true",
        b"12",
        br#"{"sdp":null}"#,
        br#"{"sdp":4}"#,
        br#"{"sdp":[]}"#,
        br#"{"sdp":false}"#,
        br#"{"sdp":"  "}"#,
        br#"{"sdp":"cut""#,
        b"{} trailing",
    ] {
        assert!(request_sdp(body, "application/json").is_err(), "{body:?}");
        assert!(response_sdp(body, "application/json").is_err(), "{body:?}");
    }
    for ct in ["application/sdp", "text/plain"] {
        assert!(request_sdp(b" \r\n", ct).is_err());
        assert!(response_sdp(b" \r\n", ct).is_err());
    }
    assert!(request_sdp(OFFER.as_bytes(), "application/octet-stream").is_err());
}

#[test]
fn relay_offer_replacement_changes_only_sdp_in_the_json_envelope() {
    let body =
        br#"{"sdp":"old","session":{"model":"custom-live","instructions":"keep"},"extra":{"a":[1,2]},"number":1.00}"#;
    let (rewritten, ct) = replace_sdp(body, "application/json", OFFER).unwrap();
    let mut expected = value(body);
    expected["sdp"] = OFFER.into();
    assert_eq!(value(&rewritten), expected);
    assert_eq!(ct, "application/json");
    for ct in ["application/sdp", "text/plain"] {
        let (body, ct) = replace_sdp(b"old", ct, OFFER).unwrap();
        assert_eq!(value(&body), json!({"sdp":OFFER}));
        assert_eq!(ct, "application/json");
    }
    for body in [b"[]".as_slice(), b"false", b"\"text\"", b"{", b"{} trailing"] {
        assert!(matches!(
            replace_sdp(body, "application/json", OFFER),
            Err(ShapeError::Invalid(_))
        ));
    }
    assert!(matches!(
        replace_sdp(b"null", "application/json", OFFER),
        Err(ShapeError::NilMap)
    ));
    assert!(replace_sdp(b"x", "application/octet-stream", OFFER).is_err());
}

#[test]
fn codex_realtime_alias_policy_preserves_custom_models() {
    for (input, expected) in [
        ("", "gpt-live-1-codex"),
        ("  ", "gpt-live-1-codex"),
        ("gpt-realtime", "gpt-live-1-codex"),
        (" GPT-Realtime ", "gpt-live-1-codex"),
        ("gpt-realtime-mini-2026", "gpt-live-1-codex"),
        ("gpt-4o-realtime-preview", "gpt-live-1-codex"),
        (" custom-live ", "custom-live"),
        ("gpt-live-2-codex", "gpt-live-2-codex"),
    ] {
        assert_eq!(codex_model(input), expected);
        assert_eq!(codex_model(expected), expected);
    }
}

#[test]
fn model_rewrite_changes_only_the_selected_model_location() {
    for body in [
        json!({"model":"gpt-realtime","sdp":OFFER,"extra":{"x":true}}),
        json!({"model":"outer","session":{"model":"gpt-realtime","instructions":"keep"},"sdp":OFFER}),
    ] {
        let call = Call {
            body: serde_json::to_vec(&body).unwrap(),
            content_type: "application/json".into(),
            model: "gpt-realtime".into(),
        };
        let actual = rewrite_model(call).unwrap();
        let mut expected = body;
        if expected.get("session").is_some() {
            expected["session"]["model"] = "gpt-live-1-codex".into();
        } else {
            expected["model"] = "gpt-live-1-codex".into();
        }
        assert_eq!(value(&actual.body), expected);
        assert_eq!(actual.model, "gpt-live-1-codex");
        assert_eq!(actual.content_type, "application/json");
    }
    for (body, ct) in [
        (OFFER.as_bytes(), "application/sdp"),
        (br#" { "unknown" : 1.00 } "#.as_slice(), "application/json"),
        (b"null".as_slice(), "application/json"),
        (b"  ".as_slice(), "application/json"),
    ] {
        let actual = rewrite_model(Call {
            body: body.to_vec(),
            content_type: ct.into(),
            model: "gpt-realtime".into(),
        })
        .unwrap();
        assert_eq!(actual.body, body);
        assert_eq!(actual.content_type, ct);
        assert_eq!(actual.model, "gpt-live-1-codex");
    }
    for body in [b"[]".as_slice(), b"{", br#"{"session":4}"#, br#"{"session":[]}"#] {
        invalid_shape(rewrite_model(Call {
            body: body.to_vec(),
            content_type: "application/json".into(),
            model: "gpt-realtime".into(),
        }));
    }
    assert!(matches!(
        rewrite_model(Call {
            body: br#"{"session":null}"#.to_vec(),
            content_type: "application/json".into(),
            model: "gpt-realtime".into()
        }),
        Err(ShapeError::NilMap)
    ));
}

#[test]
fn client_secret_session_replaces_only_the_session_and_pipeline_uses_its_model() {
    let session =
        br#"{"type":"realtime","model":"gpt-realtime-mini","instructions":"secret policy","future":{"x":true}}"#;
    for (body, ct, expected) in [
        (
            OFFER.as_bytes(),
            "application/sdp",
            json!({"sdp":OFFER,"session":value(session)}),
        ),
        (
            br#"{"sdp":"v=0","session":{"model":"old"},"outside":[1,true]}"#.as_slice(),
            "application/json",
            json!({"sdp":"v=0","session":value(session),"outside":[1,true]}),
        ),
    ] {
        let prepared = prepare(body, ct).unwrap();
        assert_eq!(apply_client_secret(prepared.clone(), b"").unwrap(), prepared);
        let bound = apply_client_secret(prepared, session).unwrap();
        assert_eq!(value(&bound.body), expected);
        assert_eq!(bound.model, "gpt-realtime-mini");
        assert_eq!(bound.content_type, "application/json");
        let actual = rewrite_model(bound).unwrap();
        let mut expected = expected;
        expected["session"]["model"] = "gpt-live-1-codex".into();
        assert_eq!(value(&actual.body), expected);
        assert_eq!(actual.model, "gpt-live-1-codex");
    }
    for body in [b"{".as_slice(), b"[]", b"false"] {
        invalid_shape(apply_client_secret(
            Call {
                body: body.to_vec(),
                content_type: "application/json".into(),
                model: "m".into(),
            },
            session,
        ));
    }
    assert!(matches!(
        apply_client_secret(
            Call {
                body: b"null".to_vec(),
                content_type: "application/json".into(),
                model: "m".into()
            },
            session
        ),
        Err(ShapeError::NilMap)
    ));
    invalid_shape(apply_client_secret(
        Call {
            body: b"x".to_vec(),
            content_type: "application/octet-stream".into(),
            model: "m".into(),
        },
        session,
    ));
}

#[test]
fn client_secret_normalization_keeps_client_model_and_unknown_fields() {
    for session in [
        b"".as_slice(),
        b" \n",
        b"null",
        b"{}",
        br#"{"type":""}"#,
        br#"{"model":" "}"#,
    ] {
        let (client, upstream) = normalize_client_secret_session(session).unwrap();
        assert_eq!(value(&client), json!({"type":"realtime","model":"gpt-realtime"}));
        assert_eq!(value(&upstream), json!({"type":"realtime","model":"gpt-live-1-codex"}));
    }
    for model in ["gpt-realtime-mini", "custom-live"] {
        let session = json!({"type":"realtime","model":model,"instructions":"<help>&","future":{"flag":true,"array":[1,null,"猫"]}});
        let (client, upstream) = normalize_client_secret_session(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert_eq!(value(&client), session);
        let mut expected = session;
        expected["model"] = if model == "custom-live" {
            "custom-live"
        } else {
            "gpt-live-1-codex"
        }
        .into();
        assert_eq!(value(&upstream), expected);
    }
    for session in [b"[]".as_slice(), b"false", b"4", b"\"text\"", b"{", b"{} trailing"] {
        assert!(matches!(
            normalize_client_secret_session(session),
            Err(SessionError::Invalid(_))
        ));
    }
    assert!(matches!(
        normalize_client_secret_session(br#"{"type":"transcription"}"#),
        Err(SessionError::Unsupported(_))
    ));
}

#[test]
fn secret_requests_decode_typed_expiry_and_keep_raw_session_bytes() {
    for body in [b"".as_slice(), b" \n", b"null", b"{}", br#"{"expires_after":null}"#] {
        assert_eq!(decode_secret_request(body), Some(SecretRequest::default()));
    }
    let body=br#"{"session":{ "model":"custom-live", "extra":1.00 },"expires_after":{"anchor":"created_at","seconds":7200},"ignored":true}"#;
    let request = decode_secret_request(body).unwrap();
    assert_eq!(request.session, br#"{ "model":"custom-live", "extra":1.00 }"#);
    assert_eq!(request.expires_after, Some(("created_at".into(), 7200)));
    for body in [
        b"[]".as_slice(),
        b"false",
        b"{",
        b"{} trailing",
        br#"{"expires_after":[]}"#,
        br#"{"expires_after":{"seconds":"60"}}"#,
        br#"{"expires_after":{"seconds":60.0}}"#,
        br#"{"expires_after":{"seconds":6e1}}"#,
        br#"{"expires_after":{"seconds":9223372036854775808}}"#,
        br#"{"expires_after":{"anchor":4}}"#,
    ] {
        assert!(decode_secret_request(body).is_none(), "{body:?}");
    }
    // Decoding and range validation are separate boundaries.
    assert_eq!(
        decode_secret_request(br#"{"expires_after":{"seconds":-1}}"#)
            .unwrap()
            .expires_after,
        Some(("".into(), -1))
    );
}

#[test]
fn client_secret_lifetime_has_explicit_default_anchor_and_inclusive_bounds() {
    assert_eq!(client_secret_lifetime(None).unwrap(), Duration::from_secs(600));
    for anchor in ["", "created_at"] {
        for seconds in [10, 60, 600, 7200] {
            assert_eq!(
                client_secret_lifetime(Some(&(anchor.into(), seconds))).unwrap(),
                Duration::from_secs(seconds as u64)
            );
        }
        for seconds in [i64::MIN, -1, 0, 9, 7201, i64::MAX] {
            assert!(client_secret_lifetime(Some(&(anchor.into(), seconds))).is_err());
        }
    }
    assert!(client_secret_lifetime(Some(&("expires_at".into(), 60))).is_err());
}

#[test]
fn session_response_adds_local_identity_and_update_omits_immutable_secret_fields() {
    let session = json!({"type":"realtime","model":"custom-live","id":"old","object":"old","expires_at":1,"client_secret":{"value":"fake-private"},"instructions":"keep","future":[true,1]});
    let bytes = serde_json::to_vec(&session).unwrap();
    let actual = session_response(&bytes, "sess_local", 1_700_000_000).unwrap().marshal();
    let mut expected = session.clone();
    expected["id"] = "sess_local".into();
    expected["object"] = "realtime.session".into();
    expected["expires_at"] = 1_700_000_000.into();
    assert_eq!(value(&actual), expected);
    assert_eq!(
        value(&session_update(&bytes).unwrap()),
        json!({"type":"realtime","instructions":"keep","future":[true,1]})
    );
    let raw = br#"{"future": { "n":1.00,"s":"\u732b" }, "model":"m"}"#;
    let updated = session_update(raw).unwrap();
    assert!(
        updated
            .windows(br#""n":1.00,"s":"\u732b""#.len())
            .any(|w| w == br#""n":1.00,"s":"\u732b""#)
    );
    for body in [b"[]".as_slice(), b"false", b"4", b"{", b"{} trailing"] {
        assert!(session_response(body, "s", 1).is_none());
        assert!(session_update(body).is_err());
    }
    assert!(session_response(b"null", "s", 1).is_none());
    assert_eq!(session_update(b"null").unwrap(), b"null");
}

#[test]
fn call_ids_use_a_bounded_ascii_grammar_and_locations_cannot_supply_unsafe_ids() {
    for id in ["a".to_owned(), "rtc_A-1".to_owned(), "x".repeat(128)] {
        assert!(valid_call_id(&id));
    }
    for id in [
        "".to_owned(),
        "x".repeat(129),
        "a b".into(),
        "a/b".into(),
        "é".into(),
        "a?b".into(),
        "a\nb".into(),
    ] {
        assert!(!valid_call_id(&id), "{id:?}");
    }
    for (location, expected) in [
        (" rtc_1 ", "rtc_1"),
        ("/v1/live/rtc_1", "rtc_1"),
        ("/v1/realtime/calls/rtc_2/", "rtc_2"),
        ("https://api.example.invalid/v1/realtime/calls/rtc_3?x=1", "rtc_3"),
        ("/v1/realtime?call_id=rtc_4", "rtc_4"),
        ("/calls/path_id?call_id=query_id", "query_id"),
        ("/calls/a%2Db", "a-b"),
        ("/other/id", ""),
        ("/live/", ""),
        ("/calls/a%20b", ""),
        ("?call_id=a%2Fb", ""),
        ("/calls/%2F", ""),
        ("/calls/a\r\nHost:evil", ""),
    ] {
        assert_eq!(call_id_from_location(location), expected, "{location:?}");
    }
}

#[test]
fn realtime_urls_preserve_base_paths_and_encode_query_values() {
    for base in [
        "wss://api.example.invalid/v1",
        "wss://api.example.invalid/v1/",
        "wss://api.example.invalid/v1//",
    ] {
        assert_eq!(
            sideband_url(base, Sideband::Frameless, "rtc_A-1"),
            "wss://api.example.invalid/v1/live/rtc_A-1"
        );
        assert_eq!(
            sideband_url(base, Sideband::Calls, "rtc_A-1"),
            "wss://api.example.invalid/v1/realtime/calls/rtc_A-1"
        );
        let query = url::Url::parse(&sideband_url(base, Sideband::Query, "a b&c")).unwrap();
        assert_eq!(query.path(), "/v1/realtime");
        assert_eq!(
            query.query_pairs().collect::<Vec<_>>(),
            vec![
                ("intent".into(), "quicksilver".into()),
                ("call_id".into(), "a b&c".into())
            ]
        );
        let direct = url::Url::parse(&direct_url(base, " custom model&猫 ")).unwrap();
        assert_eq!(direct.path(), "/v1/realtime");
        assert_eq!(
            direct.query_pairs().collect::<Vec<_>>(),
            vec![("model".into(), "custom model&猫".into())]
        );
        assert_eq!(http_base(base), "https://api.example.invalid/v1");
    }
    assert_eq!(
        http_url("ws://127.0.0.1:123/v1/live/a?x=1"),
        "http://127.0.0.1:123/v1/live/a?x=1"
    );
    assert_eq!(http_url("https://example.invalid/v1"), "https://example.invalid/v1");
}

#[test]
fn bearer_extraction_accepts_case_and_whitespace_without_other_auth_schemes() {
    for (authorization, expected) in [
        ("Bearer fake-key", "fake-key"),
        (" bearer  fake-key  ", "fake-key"),
        ("BEARER fake-key", "fake-key"),
        ("Bearer ", ""),
        ("", ""),
        ("Basic dXNlcjpwYXNz", ""),
        ("BearerX key", ""),
        ("Bearer\tkey", ""),
    ] {
        assert_eq!(bearer_token(authorization), expected);
    }
}

#[test]
fn quoted_printable_decodes_mime_escapes_soft_breaks_and_rejects_corrupt_controls() {
    // RFC 2045 section 6.7. Line endings and trailing whitespace are MIME framing.
    for (encoded, decoded) in [
        (b"".as_slice(), b"".as_slice()),
        (b"v=3D0=0D=0A", b"v=0\r\n"),
        (b"a=3db", b"a=b"),
        (b"hello=\r\nworld", b"helloworld"),
        (b"hello=\nworld", b"helloworld"),
        (b"=E2=82=AC", "€".as_bytes()),
        (b"line \t\r\nnext", b"line\r\nnext"),
        (b"a\tb\n", b"a\tb\n"),
    ] {
        assert_eq!(form::quoted_printable(encoded, None).unwrap(), decoded, "{encoded:?}");
    }
    for encoded in [b"\x01".as_slice(), b"\x7f", b"a=\rx", b"="] {
        assert!(form::quoted_printable(encoded, None).is_err(), "{encoded:?}");
    }
    assert!(form::quoted_printable(b"plain", Some("truncated MIME field".into())).is_err());
}

#[test]
fn protocol_headers_keep_values_and_canonical_names() {
    let mut client = HeaderMap::new();
    client.append("openai-alpha", "quicksilver=v2".parse().unwrap());
    client.append("x-session-id", "a".parse().unwrap());
    client.append("x-session-id", "b".parse().unwrap());
    client.append("authorization", "Bearer client-key".parse().unwrap());
    let headers = protocol_headers(&client);
    assert_eq!(
        headers,
        vec![
            ("Openai-Alpha".to_owned(), "quicksilver=v2".to_owned()),
            ("X-Session-Id".to_owned(), "a".to_owned()),
            ("X-Session-Id".to_owned(), "b".to_owned()),
        ],
        "only the live protocol headers, never the client key"
    );
    let direct = direct_headers(&client);
    assert!(direct.iter().all(|(n, _)| n != "Openai-Alpha"));
    // Go defaults to `Codex Desktop`; the proxy names itself (#26).
    assert!(direct.contains(&("Originator".to_owned(), "cliproxy-rs".to_owned())));
    assert!(!direct.iter().any(|(_, v)| v.contains("Codex Desktop")));
    client.insert("originator", "Codex CLI".parse().unwrap());
    assert!(direct_headers(&client).contains(&("Originator".to_owned(), "Codex CLI".to_owned())));
}

#[test]
fn redact_sdp_hides_ice_credentials_in_every_wrapping() {
    let raw = b"v=0\r\na=ice-ufrag:Ufr4g\r\na=ice-pwd:s3cret+pass/word\r\na=mid:0\r\n";
    assert_eq!(
        &*redact_sdp(raw),
        b"v=0\r\na=ice-ufrag:[REDACTED]\r\na=ice-pwd:[REDACTED]\r\na=mid:0\r\n"
    );
    // Bare LF line ends and a value at the end of the body.
    assert_eq!(
        &*redact_sdp(b"a=ice-pwd:p1\na=ice-pwd:p2"),
        b"a=ice-pwd:[REDACTED]\na=ice-pwd:[REDACTED]"
    );
    // JSON strings: the value ends at an escape or the closing quote; `\/` is part of it.
    let json = br#"{"sdp":"v=0\r\na=ice-pwd:ab\/cd\r\na=ice-ufrag:uf","session":{}}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"v=0\r\na=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]","session":{}}"#
    );
    // A multipart field keeps its boundary lines.
    let multipart = b"--b\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\na=ice-pwd:mp\r\n--b--\r\n";
    assert_eq!(
        &*redact_sdp(multipart),
        b"--b\r\nContent-Disposition: form-data; name=\"sdp\"\r\n\r\na=ice-pwd:[REDACTED]\r\n--b--\r\n"
    );
    // Other bodies, and an empty value, stay as they are, without a copy.
    assert!(matches!(
        redact_sdp(b"v=0\r\na=mid:0\r\n"),
        std::borrow::Cow::Borrowed(_)
    ));
    assert_eq!(&*redact_sdp(b"a=ice-pwd:\r\n"), b"a=ice-pwd:\r\n");
}

#[test]
fn redact_sdp_follows_json_escapes_to_the_end_of_the_credential() {
    // An escaped character is part of the credential; the escaped line end is not.
    let json = br#"{"sdp":"a=ice-pwd:\u0073ecret\"x\\y\r\na=ice-ufrag:u\u0066rag\u000d\u000Aa=mid:0"}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"a=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]\u000d\u000Aa=mid:0"}"#
    );
    // A trailing lone backslash or a cut-off escape ends with the body.
    assert_eq!(&*redact_sdp(br"a=ice-pwd:ab\"), b"a=ice-pwd:[REDACTED]");
    assert_eq!(&*redact_sdp(br"a=ice-pwd:ab\u00"), b"a=ice-pwd:[REDACTED]");
}

#[test]
fn redact_sdp_redacts_every_one_of_many_markers() {
    let markers = 100_000;
    let body = "a=ice-pwd:secret\r\na=ice-ufrag:frag\r\n".repeat(markers / 2);
    let redacted = redact_sdp(body.as_bytes());
    assert_eq!(
        &*redacted,
        "a=ice-pwd:[REDACTED]\r\na=ice-ufrag:[REDACTED]\r\n"
            .repeat(markers / 2)
            .as_bytes()
    );
}

#[test]
fn redact_sdp_recognises_an_escaped_equals_sign() {
    let json = br#"{"sdp":"a\u003dice-pwd:secret\r\na\u003Dice-ufrag:frag\r\na\u003dmid:0","note":"a\u003dice"}"#;
    assert_eq!(
        &*redact_sdp(json),
        br#"{"sdp":"a\u003dice-pwd:[REDACTED]\r\na\u003Dice-ufrag:[REDACTED]\r\na\u003dmid:0","note":"a\u003dice"}"#
    );
}

/// Live requests are formulated by the proxy: the proxy's `User-Agent` (Go sent
/// net/http's `Go-http-client` default), the selected credential's bearer and account,
/// and none of the operator headers the identity policy refuses (#26).
#[test]
fn live_requests_carry_the_proxy_identity_and_the_selected_credential() {
    let mut credential = cpa_core::credential::Credential::from_file(
        std::path::Path::new("/fake"),
        std::path::Path::new("/fake/codex-live.json"),
        serde_json::json!({"type": "codex", "access_token": "at-FAKE", "account_id": "acct-SELECTED"})
            .as_object()
            .unwrap()
            .clone(),
    )
    .unwrap();
    for (k, v) in [
        ("header:User-Agent", "codex-tui/0.154.0 (Mac OS 26.5.2; arm64)"),
        ("header:X-Copied", "$Authorization"),
        ("header:X-Static", "kept"),
    ] {
        credential.attributes.insert(k.into(), v.into());
    }
    let view = crate::codex_request::View::new(&credential);
    let mut client = HeaderMap::new();
    client.insert("authorization", "Bearer client-key-FAKE".parse().unwrap());
    let caller = vec![
        ("Authorization".to_owned(), "Bearer client-key-FAKE".to_owned()),
        ("Chatgpt-Account-Id".to_owned(), "acct-CALLER".to_owned()),
    ];
    let headers = CodexExecutor::live_credential_headers(&view, &client, caller);
    let get = |name: &str| {
        headers
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(get("User-Agent"), [crate::codex_identity::USER_AGENT]);
    assert_eq!(get("Authorization"), ["Bearer at-FAKE"]);
    assert_eq!(get("Chatgpt-Account-Id"), ["acct-SELECTED"]);
    assert_eq!(get("X-Static"), ["kept"]);
    assert!(get("X-Copied").is_empty());
    assert!(
        headers
            .iter()
            .all(|(_, v)| !v.contains("client-key-FAKE") && !v.contains("Mac OS")),
        "{headers:?}"
    );
    assert!(
        crate::codex_identity::check_credential(&credential).is_err(),
        "live entry points refuse it"
    );
}
