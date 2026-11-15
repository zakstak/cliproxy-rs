# Realtime Codex migration ledger

Only `crates/cpa-exec/src/codex_live_tests.rs` changed in this additional assignment. Production behavior remains unchanged. The former file read 844 Go-produced output cases across 19 operation kinds. The replacement reads no fixture and constructs every input and expectation by hand.

`docs/codex/CONTRACT.md` places realtime outside the Responses payload contract. The tests state that distinction and cite RFC 8866, RFC 7578, RFC 2045 section 6.7, the OpenAI realtime WebRTC guide, and the client-secret API reference. Codex model aliases, legacy sideband paths, default expiry, and error variants are explicit existing proxy policy. These tests do not claim that Go or the Responses client defines realtime behavior.

## Original inventory entries

| Original test | Replacement and value retained |
| --- | --- |
| `shaping_matches_go` | Seventeen focused tests cover all 19 former operation kinds below. Assertions preserve bytes on passthrough, compare changed JSON semantically, preserve unknown fields and raw tokens where relevant, classify malformed input with typed errors, and check security/resource boundaries. |
| `fixture_covers_error_and_panic_paths` | Removed the recorder-count check. Actual malformed JSON, nonobject bodies, null-map variants, invalid session type, MIME truncation/headers, bad SDP, invalid secret expiry and invalid call IDs are executed directly. `ShapeError::NilMap`, `ShapeError::Invalid`, `SessionError::Invalid`, and `SessionError::Unsupported` are independently asserted. No caught Go panic or count of error strings supplies an expectation. |

## Former operation coverage

| Former kind | Historical cases | Independent replacement |
| --- | ---: | --- |
| `prepare` | 75 | `preparation_preserves_non_multipart_bytes_and_defaults_only_missing_metadata` crosses ten bodies with seven media types. Multipart tests cover SDP/session/unknown fields, duplicate SDP, quoted-printable, preamble/epilogue, file upload, LF framing, missing fields/boundary, invalid JSON, truncation, malformed headers, and the 10,000-header cap. |
| `pipeline` | 225 | `client_secret_session_replaces_only_the_session_and_pipeline_uses_its_model` independently defines raw SDP and JSON output fields, preserves extensions, replaces the key's session, and selects its mapped model. Empty secret stays byte-identical. Dedicated preparation/rewrite/refusal tests cover each stage. |
| `rewrite_model` | 75 | `model_rewrite_changes_only_the_selected_model_location` covers session versus top-level model, extensions, raw SDP/absent-model/null/whitespace passthrough, malformed types, and null session's typed refusal. |
| `model_from_json` | 75 | `model_selection_prefers_a_nonempty_session_model_and_refuses_invalid_field_types` defines nested-over-top precedence, whitespace/null/empty fallback, extensions, scalar/nonobject/malformed documents, and invalid field types. |
| `call_request_sdp` | 75 | `offers_and_answers_preserve_sdp_and_require_a_nonempty_string` exercises JSON strings, UTF-8, escaped credentials, SDP/plain media types, charset/case, empty offers, malformed JSON, nonobjects, wrong types and unsupported media. |
| `response_sdp` | 75 | Same test checks represented answers independently, including absent Content-Type raw responses and nonempty validation. |
| `replace_sdp` | 75 | `relay_offer_replacement_changes_only_sdp_in_the_json_envelope` defines rewritten SDP and preserved session/unknown fields; raw SDP becomes the documented envelope; invalid/nonobject/null/unsupported input refuses by typed category. |
| `codex_model` | 11 | `codex_realtime_alias_policy_preserves_custom_models` defines standard, dated, mini and preview aliases; custom models remain trimmed and mapping is idempotent. |
| `call_id_from_location` | 21 | `call_ids_use_a_bounded_ascii_grammar_and_locations_cannot_supply_unsafe_ids` defines bare IDs, live/calls paths, query precedence, percent-decoded paths, and refusal of separators, spaces and header injection. |
| `call_id_valid` | 7 | Same test checks exact 1/128/129-byte boundaries, allowed ASCII, Unicode/whitespace/path/query/control refusal. |
| `normalize_secret` | 18 | `client_secret_normalization_keeps_client_model_and_unknown_fields` defines default sessions, empty kind/model fallback, supported/custom models, nested extensions and Unicode, and typed invalid/unsupported refusals. |
| `session_response` | 18 | `session_response_adds_local_identity_and_update_omits_immutable_secret_fields` defines local ID, object and expiry overwrite, preserves session/extension fields, and refuses nonobjects/malformed data. |
| `session_update` | 18 | Same test defines the five immutable/secret fields omitted from updates, preserves remaining fields and numeric/string tokens, handles null and refuses malformed/nonobject input. |
| `sideband_url` | 9 | `realtime_urls_preserve_base_paths_and_encode_query_values` defines all three path styles over slash variants and validates query encoding with an independent URL parser. |
| `direct_url` | 3 | Same test checks model query round-trip with whitespace, ampersand, CJK, preserved base path, and ws/wss-to-http conversion for hangup. |
| `bearer` | 9 | `bearer_extraction_accepts_case_and_whitespace_without_other_auth_schemes` uses an explicit accepted/refused header table. |
| `lifetime` | 8 | `client_secret_lifetime_has_explicit_default_anchor_and_inclusive_bounds` checks default, both supported anchors, inclusive 10/7200-second bounds, negatives, overflow-sized signed values and invalid anchor. |
| `secret_request` | 22 | `secret_requests_decode_typed_expiry_and_keep_raw_session_bytes` checks exact raw session, typed optional expiry, absent/null/default/unknown fields, wrong types, fractional/exponent integers, overflow and separation of decode/range validation. |
| `quoted_printable` | 25 | `quoted_printable_decodes_mime_escapes_soft_breaks_and_rejects_corrupt_controls` defines RFC hex/soft-break/UTF-8/line-ending examples, trailing whitespace, illegal control refusal and propagation of a part-reader error. |

Six independent existing header/identity/SDP-redaction tests remain unchanged. Their behavior includes client credential isolation, truthful proxy identity, escaped SDP credentials, multipart log framing, and many credential markers.

## Deliberately retired obligations and limits

The replacement does not retain all 844 examples one-for-one. Go diagnostic strings, Go panic serialization, JSON key order/HTML escaping, arbitrary number formatting, case-fold/duplicate-object decoder quirks, and malformed MIME/quoted-printable permissiveness have no independent provider requirement. Their generated outputs are retired. Production parsing is unchanged, so this change alters no accepted operator or client behavior.

The tests prove local shaping and security/resource behavior only. They send no network requests, use no auth directory, and prove no live provider acceptance. The Codex owner removes unused `codex_live_go.json` and Go recorders after confirming there are no consumers.

## Verification

`unshare --user --map-root-user --net sh -c 'ip link set lo up; CMAKE=/home/zak/.local/share/mise/installs/cmake/4.5.0/cmake-4.5.0-linux-x86_64/bin/cmake CARGO_BUILD_JOBS=2 nice -n 19 cargo test --offline -p cpa-exec --lib codex_live::tests'`

Final outcome: 23 passed, 0 failed. `rustfmt --edition 2024` and `git diff --check` passed for the changed file. Log: `/tmp/cliproxy-parity-migration/codex-live-tests.log`.
