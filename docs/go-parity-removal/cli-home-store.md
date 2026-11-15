# CLI, Home, and store migration ledger

All 73 original inventory entries in the assigned crates are mapped below. Five K entries remain direct behavior tests. No provider account traffic or real auth directory was used.

## `crates/cliproxy/src/discovery/tests.rs::spec_errors_match_go`

Replacement: `advertising_refuses_invalid_service_types_and_zero_ports_before_network_access`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::go_durations`

Replacement: `scan_duration_display_is_readable_at_supported_timeout_boundaries`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/main.rs::go_int_matches_strconv_base_zero`

Replacement: `numeric_cli_arguments_accept_saved_spellings_and_refuse_out_of_range_values`. Explicit flag/config/local URL precedence; boolean flags and operand boundaries; all supported single/double-dash flags; actual CLI parsing checks saved numeric forms and overflow without Go underscore quirks.

## `crates/cliproxy/src/tui/client.rs::json_errors_read_like_go`

Replacement: `malformed_management_json_returns_an_actionable_decode_error`. Malformed JSON and wrong collection element types refuse meaningfully. Existing local HTTP redirect/method/credential-isolation checks remain; JSON map output is compared semantically.

## `crates/cliproxy/src/tui/config.rs::atoi_follows_go`

Replacement: `integer_input_accepts_decimal_signs_and_refuses_overflow`. Retained explicit decimal integer grammar and overflow table. This was already a direct behavior test.

## `crates/cpa-home/src/applog.rs::unsupported_matches_go`

Replacement: `protocol_capability_errors_disable_forwarding_but_operational_errors_do_not`. Five directly constructed log events check semantic JSON, level, instant/offset, app-log destination, and optional request ID; capability positive/negative table owns disable policy.

## `crates/cpa-home/src/cert_tests.rs::tls_config_errors_match_go`

Replacement: `tls_configuration_refuses_incomplete_keys_missing_ca_and_invalid_pem`. Same incomplete key pair, missing CA, and invalid PEM cases still refuse; category/actionable context replaces exact diagnostic prose.

## `crates/cpa-home/src/gojson.rs::matches_go_marshal`

Replacement: `object_writer_preserves_json_values_and_omits_empty_options`. Independent serde Value checks preserve string, number, array, null and omitted optional fields without key order/HTML escape requirements.

## `crates/cpa-home/src/inflight.rs::rfc3339_nano_trims_like_go`

Replacement: `published_timestamps_round_trip_the_instant_at_nanosecond_precision`. 22 historical scenario inputs retain no recorded frames. Independent aggregate counts, whole-group partitioning, multipart/detail/revision/string caps, execution-backed safe details, deterministic encoding and truncation metadata. Additional directly typed snapshot and timestamp instant checks.

## `crates/cliproxy/src/discovery/mdns.rs::decodes_compressed_packets_from_go_zeroconf`

Replacement: `dns_compression_decodes_captured_service_records`. Historical compressed DNS packets remain inputs only. Expected host, port, TXT fields, and addresses are explicit; independent RFC 6762 assertions check unique/shared record class bits and goodbye TTL, without answer ordering.

## `crates/cliproxy/src/discovery/mdns.rs::lookup_answers_match_zeroconf_order_and_flush_bits`

Replacement: `mdns_unique_records_flush_caches_and_goodbye_records_expire_immediately`. Historical compressed DNS packets remain inputs only. Expected host, port, TXT fields, and addresses are explicit; independent RFC 6762 assertions check unique/shared record class bits and goodbye TTL, without answer ordering.

## `crates/cliproxy/src/discovery/tests.rs::txt_records_match_go`

Replacement: `txt_metadata_preserves_supported_fields_with_dns_sd_size_limits`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::txt_parsing_names_subtypes_and_service_types_match_go`

Replacement: `dns_sd_metadata_names_and_service_types_follow_explicit_boundary_rules`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::interface_name_rules_match_go`

Replacement: `interface_name_policy_identifies_lan_tunnels_and_explicit_patterns`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::discovered_entries_and_merges_match_go`

Replacement: `discovered_services_decode_metadata_and_merge_unique_addresses`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::discover_output_matches_go_byte_for_byte`

Replacement: `discovery_reports_semantic_json_safe_text_errors_and_effective_scan_options`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/discovery/tests.rs::terminal_sanitizing_and_filters_match_go`

Replacement: `discovery_sanitizes_terminal_controls_and_respects_explicit_interface_filters`. Handwritten DNS-SD field/size/boundary tables, direct interface policy and merge assertions, fake browser options, semantic JSON, terminal control refusal, and explicit YAML filter precedence. Address decode cap remains asserted.

## `crates/cliproxy/src/dotenv.rs::parses_like_godotenv`

Replacement: `dotenv_supports_quoting_comments_prior_key_expansion_and_atomic_refusal`. Handwritten supported quoting, comments, export/colon, CRLF, interpolation, prior-key, duplicate-key, and escape cases. Invalid input refuses atomically without Go diagnostic text.

## `crates/cliproxy/src/home.rs::busy_and_invalid_concurrency_replies_follow_go`

Replacement: `busy_and_invalid_concurrency_replies_preserve_retry_and_single_dispatch`. Retained independent fake-Home behavior for retry/identity/hierarchy; accounted tuple and defaults no longer load Go fixtures. K entries remain unchanged.

## `crates/cliproxy/src/home.rs::dispatch_sessions_follow_go`

Replacement: `dispatch_sessions_choose_primary_and_parent_identity`. Retained independent fake-Home behavior for retry/identity/hierarchy; accounted tuple and defaults no longer load Go fixtures. K entries remain unchanged.

## `crates/cliproxy/src/home.rs::home_session_hierarchies_follow_go`

Replacement: `home_sessions_preserve_parent_child_relationships`. Retained independent fake-Home behavior for retry/identity/hierarchy; accounted tuple and defaults no longer load Go fixtures. K entries remain unchanged.

## `crates/cliproxy/src/main.rs::management_base_url_follows_go`

Replacement: `management_url_uses_flag_then_configuration_then_local_port`. Explicit flag/config/local URL precedence; boolean flags and operand boundaries; all supported single/double-dash flags; actual CLI parsing checks saved numeric forms and overflow without Go underscore quirks.

## `crates/cliproxy/src/main.rs::argv_prescan_matches_go`

Replacement: `discover_mode_prescan_respects_boolean_values_operands_and_value_flags`. Explicit flag/config/local URL precedence; boolean flags and operand boundaries; all supported single/double-dash flags; actual CLI parsing checks saved numeric forms and overflow without Go underscore quirks.

## `crates/cliproxy/src/main.rs::every_go_flag_parses_in_single_and_double_dash_form`

Replacement: `operator_flags_accept_single_and_double_dash_forms`. Explicit flag/config/local URL precedence; boolean flags and operand boundaries; all supported single/double-dash flags; actual CLI parsing checks saved numeric forms and overflow without Go underscore quirks.

## `crates/cliproxy/src/tui/client.rs::redirects_follow_go_http_client`

Replacement: `redirects_preserve_http_method_rules_and_isolate_credentials`. Malformed JSON and wrong collection element types refuse meaningfully. Existing local HTTP redirect/method/credential-isolation checks remain; JSON map output is compared semantically.

## `crates/cliproxy/src/tui/client.rs::base_urls_follow_go`

Replacement: `management_urls_preserve_base_paths_and_query_parameters`. Malformed JSON and wrong collection element types refuse meaningfully. Existing local HTTP redirect/method/credential-isolation checks remain; JSON map output is compared semantically.

## `crates/cliproxy/src/tui/golden.rs::tabs_render_like_go`

Replacement: `tabs_render_visible_state_actions_and_masked_secrets`. All 29 historical scenario inputs retained without recorded output lines. Handwritten visible-content assertions, interaction state, logs, password/key masking, and TestBackend drawing replace Go layout equality. Added inputs for all four supported key sections. Removed parity-only Dashboard fit override.

## `crates/cliproxy/src/tui/style.rs::formats_like_go`

Replacement: `display_formats_keep_numeric_value_and_mask_secrets`. Numeric display preserves represented values; existing rounding, key masking, width/wrapping assertions remain. Removed arbitrary Go fmt map/list/nil/exponent spelling obligations.

## `crates/cliproxy/src/tui/widgets.rs::viewport_scrolls_like_bubbles`

Replacement: `viewport_scroll_clamps_to_visible_content`. Retained viewport clamp and edit/cursor/masking cases; added accented, CJK, and emoji editing. These are direct interaction contracts.

## `crates/cliproxy/src/tui/widgets.rs::text_input_edits_like_bubbles`

Replacement: `text_input_preserves_cursor_unicode_and_secret_masking`. Retained viewport clamp and edit/cursor/masking cases; added accented, CJK, and emoji editing. These are direct interaction contracts.

## `crates/cliproxy/tests/store_mode.rs::a_failing_store_logs_go_s_line_and_starts_nothing`

Replacement: `a_failing_store_reports_the_failure_redacts_credentials_and_starts_nothing`. Private startup process must report postgres failure, redact password, create only its private spool, and open no listener. Go log sentence and successful exit requirement removed.

## `crates/cpa-home/src/applog.rs::payloads_match_go`

Replacement: `app_log_payloads_preserve_line_level_instant_and_optional_request_id`. Five directly constructed log events check semantic JSON, level, instant/offset, app-log destination, and optional request ID; capability positive/negative table owns disable policy.

## `crates/cpa-home/src/client_tests.rs::dispatch_request_json_matches_go_bytes`

Replacement: `dispatch_requests_preserve_required_fields_and_optional_distinctions`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::kv_set_args_match_go`

Replacement: `redis_set_arguments_preserve_options_and_round_expiry_up`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::jwt_claims_match_go`

Replacement: `enrollment_jwt_validates_required_claims_and_accepts_base64url_padding`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::cluster_nodes_sort_like_go`

Replacement: `cluster_nodes_ignore_unusable_addresses_and_sort_by_load_then_address`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::subscription_parameters_match_go_per_recovery_state`

Replacement: `subscription_arguments_follow_revision_membership_and_recovery_state`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::every_command_matches_go_on_the_wire`

Replacement: `home_commands_preserve_resp_arguments_and_decode_reply_variants`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::membership_args_and_takeover_eligibility_follow_go`

Replacement: `subscription_arguments_follow_revision_membership_and_recovery_state`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::membership_errors_classify_like_go`

Replacement: `membership_errors_distinguish_takeover_refusal_from_legacy_arity`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/client_tests.rs::production_timeouts_match_go`

Replacement: `production_operations_and_subscription_have_configured_deadlines`. Typed dispatch cases cover omissions/defaults/exclusions/retry/headers; direct Redis options and JWT claims; direct cluster tuples; all 16 subscription recovery combinations; handwritten 30-operation RESP table retains argument order, semantic JSON, missing/error replies, CAS latching, membership and deadlines.

## `crates/cpa-home/src/config.rs::defaults_match_go_limiter_config`

Replacement: `limiter_defaults_define_the_home_protocol_policy`. Retained direct typed defaults, accepted lifecycle boundaries, invalid-value refusal, and explicit/default port policy. No output fixtures were involved.

## `crates/cpa-home/src/config.rs::the_lifecycle_fixture_validates_like_go`

Replacement: `lifecycle_configuration_accepts_valid_boundaries_and_refuses_invalid_values`. Retained direct typed defaults, accepted lifecycle boundaries, invalid-value refusal, and explicit/default port policy. No output fixtures were involved.

## `crates/cpa-home/src/config.rs::home_port_defaults_like_go`

Replacement: `home_port_uses_default_unless_explicitly_overridden`. Retained direct typed defaults, accepted lifecycle boundaries, invalid-value refusal, and explicit/default port policy. No output fixtures were involved.

## `crates/cpa-home/src/dispatch.rs::concurrency_fixture_round_trips`

Replacement: `accounted_concurrency_tuple_requires_matching_credential_identity`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/dispatch.rs::busy_fixture_maps_to_429_with_retry_after`

Replacement: `concurrency_busy_errors_preserve_retry_hints_and_round_up_public_seconds`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/dispatch.rs::error_codes_map_to_go_statuses`

Replacement: `dispatch_error_codes_define_public_status_and_retry_policy`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/dispatch.rs::dispatch_errors_match_go`

Replacement: `dispatch_errors_preserve_scope_message_and_retry_metadata`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/dispatch.rs::model_keys_match_go`

Replacement: `model_keys_normalize_recognized_suffixes_and_bound_shared_identity`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/dispatch.rs::envelopes_match_go`

Replacement: `concurrency_envelopes_distinguish_absent_invalid_and_accounted_tuples`. Direct accounted/busy tuples; identity refusal; error code/status/scope/message/retry table; malformed/absent/type-fallback variants; normalized model boundary/idempotence and typed optional envelope distinctions.

## `crates/cpa-home/src/error.rs::classifiers_match_go_message_checks`

Replacement: `server_error_classification_recognizes_protocol_capabilities`. Positive/negative protocol capability classifiers remain direct. Timeout and ambiguity use typed classifications rather than Go display equality.

## `crates/cpa-home/src/inflight.rs::frames_match_go_bytes`

Replacement: `snapshot_scenarios_preserve_complete_aggregates_with_bounded_safe_details`. 22 historical scenario inputs retain no recorded frames. Independent aggregate counts, whole-group partitioning, multipart/detail/revision/string caps, execution-backed safe details, deterministic encoding and truncation metadata. Additional directly typed snapshot and timestamp instant checks.

## `crates/cpa-home/src/inflight.rs::fixture_frames_decode_into_the_contract`

Replacement: `snapshot_counts_shared_credentials_and_preserves_each_request_instant`. 22 historical scenario inputs retain no recorded frames. Independent aggregate counts, whole-group partitioning, multipart/detail/revision/string caps, execution-backed safe details, deterministic encoding and truncation metadata. Additional directly typed snapshot and timestamp instant checks.

## `crates/cpa-home/src/kv.rs::hash_and_log_prefix_match_go`

Replacement: `shared_key_namespace_hashes_are_stable_and_logs_hide_the_input`. Retained fixed shared-key hash and log-prefix assertions because stored Home key interoperability and input redaction require exact values.

## `crates/cpa-home/src/refresh.rs::error_codes_map_to_go_statuses`

Replacement: `refresh_errors_map_to_public_statuses`. Retained direct error-code to public-status table and malformed error-envelope behavior as Home protocol policy.

## `crates/cpa-home/src/release.rs::frame_matches_go_fixture`

Replacement: `release_frame_carries_identity_model_and_cumulative_sequence`. Direct identity/model/cumulative release sequence cases 1, 3 and i64::MAX compare semantic payload fields.

## `crates/cpa-home/src/resp.rs::encodes_like_go_encode_resp_array`

Replacement: `resp_arrays_count_bytes_and_preserve_argument_order`. Retained exact RESP lengths and order as protocol bytes; added empty argument and multibyte UTF-8 example.

## `crates/cpa-home/src/session_alias.rs::aliases_match_go`

Replacement: `alias_groups_join_merge_expire_and_keep_hierarchy_separate`. Independent fixed-clock joining, shared prompt, expiry, solo-group join, prompt rotation, merge, blank/default TTL and hierarchy operations replace recorded aliases.

## `crates/cpa-home/src/subscriber.rs::decodes_go_redis_pubsub_shapes`

Replacement: `resp_pubsub_decodes_subscription_and_message_variants`. Retained directly authored RESP subscription/message variants as Redis protocol behavior.

## `crates/cpa-store/src/lib.rs::selection_follows_go_main`

Replacement: `store_selection_respects_explicit_operator_precedence`. Retained direct environment/operator store-selection precedence assertions.

## `crates/cpa-store/src/object.rs::endpoints_parse_like_go_main`

Replacement: `s3_endpoints_accept_documented_operator_forms`. Retained supported endpoint/operator forms with unsupported-scheme category checks; local S3 error peers still supply location regions.

## `crates/cpa-store/src/object.rs::regions_come_from_location_errors_like_minio`

Replacement: `s3_location_errors_supply_the_bucket_region`. Retained supported endpoint/operator forms with unsupported-scheme category checks; local S3 error peers still supply location regions.

## `crates/cpa-store/src/pgconn.rs::sslmode_maps_to_pgx_verification`

Replacement: `postgres_sslmode_enforces_certificate_and_hostname_policy`. Retained PostgreSQL sslmode certificate/hostname security policy and connection runtime parameter assertions.

## `crates/cpa-store/src/pgconn.rs::other_keys_travel_as_runtime_parameters_like_pgx`

Replacement: `postgres_connection_parameters_preserve_application_settings`. Retained PostgreSQL sslmode certificate/hostname security policy and connection runtime parameter assertions.

## `crates/cpa-store/src/postgres.rs::identifiers_and_paths_follow_go`

Replacement: `sql_identifiers_are_quoted_and_spool_paths_are_resolved`. Retained direct SQL quote escaping and supported Linux spool path resolution examples.

## `crates/cpa-store/src/sigv4.rs::signatures_match_minio_go`

Replacement: `s3_signatures_match_independent_aws_canonical_request_vectors`. Six independently signed AWS canonical request cases use Python stdlib hashlib/hmac and public placeholder keys. Explicit canonical URI/query/headers plus header case/order/whitespace metamorphic checks replace MinIO Go outputs.

## `crates/cpa-store/tests/postgres.rs::postgres_store_reports_go_errors`

Replacement: `postgres_store_reports_actionable_failures`. Failure context and password redaction replace exact Go prose. Concurrent per-credential cooldown merge assertions remain; cluster-backed execution depends on installed PostgreSQL binaries.

## `crates/cpa-store/tests/postgres.rs::cooldown_instances_merge_like_go`

Replacement: `concurrent_cooldown_instances_merge_each_credential_state`. Failure context and password redaction replace exact Go prose. Concurrent per-credential cooldown merge assertions remain; cluster-backed execution depends on installed PostgreSQL binaries.

## `crates/cliproxy/src/home.rs::overlay_forces_go_home_runtime_settings`

Replacement: `overlay_forces_go_home_runtime_settings`. Retained independent fake-Home behavior for retry/identity/hierarchy; accounted tuple and defaults no longer load Go fixtures. K entries remain unchanged.

## `crates/cliproxy/src/home.rs::meta_mints_go_through_home`

Replacement: `meta_mints_go_through_home`. Retained independent fake-Home behavior for retry/identity/hierarchy; accounted tuple and defaults no longer load Go fixtures. K entries remain unchanged.

## `crates/cliproxy/src/tui/i18n.rs::locales_fall_back_like_go`

Replacement: `locales_fall_back_like_go`. K: retained existing direct fallback/visible-string behavior. No output fixture or Go oracle.

## `crates/cliproxy/src/tui/logs.rs::filters_follow_go_match_level`

Replacement: `filters_follow_go_match_level`. K: retained existing direct log level filtering. No output fixture or Go oracle.

## `crates/cliproxy/src/tui/oauth.rs::poll_errors_fail_like_go`

Replacement: `poll_errors_fail_like_go`. K: retained existing poll failure threshold. No output fixture or Go oracle.

## Fixture disposition

Removed obsolete owned Go JSON outputs, Home and SigV4 recorders, and all `cliproxy/tests/reference` Go generation sources. Historical provenance remains in Git history. Only historical DNS packet inputs, input-only Home/TUI scenarios, and independent AWS signatures are consumed. Fixture README files describe each source. No Go generator is required.

## Verification

- Network namespace denies external networking and enables loopback only. CMAKE uses `/home/zak/.local/share/mise/installs/cmake/4.5.0/cmake-4.5.0-linux-x86_64/bin/cmake`; `CARGO_BUILD_JOBS=2 nice -n 19`; Cargo uses `--offline`.
- `cargo test --offline -p cliproxy --bin cliproxy`: 146 passed, 0 failed, after final source changes. Log `/tmp/cliproxy-parity-migration/cli-tests.log`.
- `cargo test --offline -p cpa-home --lib`: 132 passed, 0 failed in initial validation; final post-cleanup rerun recorded in `owned-final-tests.log`.
- `cargo test --offline -p cpa-store --lib`: 41 passed, 0 failed in initial and final validation.
- Final rerun: Home 132/132, store lib 41/41, store_mode 2/2. PostgreSQL test process reported 6/6 but five cluster-backed tests returned early because PostgreSQL binaries are absent; only the failure/redaction test executed its database-independent assertions. Log `/tmp/cliproxy-parity-migration/owned-final-tests.log`.
- PostgreSQL executables were not found through PATH, `/usr/lib/postgresql`, `/usr`, `/opt`, or `/home/zak/.local`. Do not count skipped cluster-backed tests as executed.
- `git diff --check -- crates/cliproxy crates/cpa-home crates/cpa-store`: passed. Owned changed Rust files formatted with rustfmt.

## Parent integration follow-up

The parent subsequently provisioned temporary PostgreSQL 18.6 binaries without installing a service and ran all six PostgreSQL integration tests with `CPA_TEST_NO_SKIP=1`; all passed with networking denied. TUI layout tests were renamed from `tui/golden.rs` to `tui/layout_tests.rs`; original identifiers in this table describe the audit snapshot. Final workspace verification is recorded in [the migration report](../GO-PARITY-REMOVAL.md).
