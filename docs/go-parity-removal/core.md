# Core and management parity migration

All 31 core inventory entries and all 13 management inventory entries are resolved. No Go expected-output fixture is consumed by these tests. Production behavior is unchanged except for a scoped mapped-address trusted-proxy security fix authorized by the parent after review. A stale recorder instruction was also removed from the legacy path table comment.

## Core original-to-replacement accounting

| Original inventory test | Replacement / retained behavior | Deliberately removed obligation |
| --- | --- | --- |
| accepts_and_rejects_like_go | http_endpoints_preserve_authority_and_reject_ambiguous_hosts: supported authorities, invalid host refusal | Go URL parser as oracle |
| new_request_removes_an_empty_port | endpoint_normalization_omits_empty_ports_and_redacts_userinfo | Go NewRequest implementation identity |
| matches_go_case_rules | ascii_protocol_names_have_stable_case_identity; existing Unicode identities remain checked at config, credential and registry boundaries | Exhaustive Unicode helper implementation parity |
| image_generation_mode_reads_go_spellings | image_generation_mode_accepts_operator_spellings: typed modes and save/reload | Exact decoder wording/escaping |
| defaults_match_go_loader | empty_config_uses_documented_defaults plus round trip | Go loader authority |
| integers_beyond_64_bits_read_as_in_go | oversized_numbers_keep_text_at_string_boundaries: opaque strings preserve spelling; typed positive integer overflow refused | Arbitrary Go float normalization |
| underscore_numbers_read_as_in_go | numeric_config_values_have_explicit_types_and_round_trip: supported operator numeric spellings remain typed | Malformed radix/underscore parser matrix |
| plain_timestamps_read_as_in_go | timestamps_preserve_string_settings_and_serialized_instants: string settings, normal instants, aliases, reload and hashing coexist | Go permissive timestamp parser quirks |
| scalar_settings_keep_their_yaml_text | Independent Rust matrix across legacy/v8 string fields, typed payload values and round trip | Golden expected outputs |
| config_models_reach_the_registry_like_go | configured_models_register_names_aliases_and_display_names | Go registry synthesis authority |
| scalar_model_names_keep_their_yaml_text | Direct legacy/grouped model identity, routing aliases, exclusions, OAuth aliases and round trip | Golden model-name outputs |
| file_type_and_exclusions_lowercase_like_go | saved_credentials_keep_existing_unicode_normalization: explicit saved identity behavior | Go helper as oracle |
| request_uri_follows_go | request_targets_escape_paths_and_preserve_existing_escapes | Go RequestURI as oracle |
| resolve_path_matches_go | relative_paths_remove_dot_segments_without_escaping_root: RFC3986-style examples | Go path resolution implementation |
| mapped_entries_and_v4_cidrs_follow_go_byte_lengths | trusted_network_boundaries_reject_spoofed_forwarding: explicit IPv4/IPv6/mapped CIDRs and outside-boundary spoof refusal | Mapped-address byte-length quirk fixed; exact-host and forwarding-spoof regressions added |
| absolute_paths_follow_go_filepath_is_abs | absolute_paths_follow_supported_unix_policy: standard-library path cross-check | Go filepath implementation |
| durations_parse_like_go | duration_units_and_overflow_have_explicit_bounds: unit equivalence and retained bounds | Go duration parser authority |
| validate_catalog_follows_go | retained_catalog_sections_reject_invalid_model_identities | Go catalog validation authority |
| models_decode_like_go_model_info | model_schema_preserves_capabilities_and_rejects_invalid_field_types | Exact Go serialization/key order |
| lookup_follows_go_search_order_across_providers | lookup_has_explicit_provider_priority_for_duplicate_ids: synthetic colliding IDs | Historical embedded catalog ordering/counts |
| state_keys_compare_under_go_lower | state_keys_share_identity_across_existing_case_variants | Go helper authority; existing public Unicode identity preserved |
| static_channel_lookup_matches_go | static_channels_accept_supported_operator_aliases | Go channel authority |
| account_info_matches_go | account_information_uses_credential_kind_and_trimmed_identity | Go account-info authority |
| oauth_claude_registers_go_catalog_minus_disabled | oauth_claude_registers_current_catalog_for_enabled_credentials: current catalog identity set | Fixed historical catalog count |
| exclusions_aliases_forks_and_prefixes_follow_go | model_aliases_exclusions_and_prefixes_compose | Go synthesis authority |
| exclusions_match_with_go_lowercasing | model_exclusions_keep_existing_unicode_identity | Go helper authority |
| wildcard_matches_go | wildcard_patterns_are_anchored_and_ordered | Go wildcard implementation identity |
| binary_scalars_decode_like_go | binary_and_tagged_inputs_follow_config_boundary_policy plus generated UTF8/base64 decoding | Golden outputs; 85 historical inputs remain stimuli only |
| prepare_for_upload_matches_go | upload_inputs_preserve_effective_fields_and_never_save_plaintext and ordinary_upload_preserves_comments_and_reloads | Golden outputs; 116 historical inputs remain stimuli only |
| in_flight_bounds_follow_go (K) | in_flight_limits_enforce_snapshot_safety: existing resource-safety bounds retained | Name only; coverage retained |
| providers_fold_like_go (K) | removed_provider_names_accept_operator_aliases: migration aliases and kept provider refusal retained | Name only; coverage retained |

## Management original-to-replacement accounting

All replacements live in crates/cliproxy-internal/tests/gateway_management_contracts.rs (formerly manage_go.rs).

| Original inventory test | Independent replacement / behavior retained |
| --- | --- |
| trusted_proxy_validation_errors_match_go_text | invalid_config_boundaries_report_actionable_fields retains all 83 historical validation inputs without recorded error prose |
| yaml_bool_spellings_and_durations_load_like_go | config_boolean_spellings_and_opaque_host_text_have_explicit_types; duration unit/bound checks in core; valid duration boundaries remain in validation inputs |
| typed_scalars_load_like_go | retry_numbers_preserve_supported_operator_spellings; typed string/boolean boundaries |
| yaml_merge_keys_load_like_go | config_merges_resolve_explicit_values_before_inherited_values, including nested merges, aliases, credential overrides and round trip |
| client_ip_matches_gin_for_every_trusted_remote_and_header_combination | forwarded_client_identity_requires_a_trusted_socket_peer: trusted chain traversal, untrusted socket refusal, IPv4/IPv6 and fallback |
| forwarded_header_bytes_and_zoned_peers_match_gin | malformed_forwarding_bytes_fall_back_without_trusting_spoofed_addresses; removed exact empty-string answer for a zoned socket peer |
| oauth_only_presence_matches_go | oauth_only_settings_do_not_reach_api_key_credentials |
| config_and_file_credentials_match_go_synthesizers | grouped_credentials_inherit_defaults_and_keep_explicit_overrides; saved_credentials_preserve_unknown_metadata_and_skip_removed_providers |
| every_go_access_scenario_replays_through_the_router | management_authentication_enforces_credentials_remote_policy_and_ip_bans: header precedence, configured/env/local secrets, trusted peers, spoof refusal, per-IP bans, success reset, isolation, errors and no secret leakage |
| go_server_routing_preflight_and_availability_replay | management_routes_preflight_and_errors_follow_http_contracts; management_availability_changes_on_reload_with_environment_override: CORS tokens, methods/statuses, headers, reload and private auth dirs |
| go_config_v8_writes_replay_value_for_value | Five config_contract tests below; oracle interpreter deleted |
| go_credential_management_replay | Seven credential_contract tests below; oracle interpreter deleted |
| dashboard_probes_are_rejected_without_side_effects (K) | credential_refusals_and_dashboard_probes_have_no_side_effects keeps all nine probe route families and independently checks errors, disk bytes, config bytes and credential revisions after each refusal |

### Config-write families replaced

- rejected_config_writes_are_transactional_and_identify_the_boundary: malformed body/shape/type, unknown/read-only path, removed plugin field, typed fields, invalid catalogs, malformed OAuth maps, invalid merge and binary uploads. Refusals preserve bytes on disk, effective config and the live port; errors identify actionable boundaries and exclude secrets.
- config_edits_round_trip_and_preserve_unrelated_settings: grouped provider keys, aliases, exclusions, client keys, payload rules, nested PATCH, null reset, DELETE, ancestor pruning and comment preservation.
- config_nulls_restore_typed_defaults_and_scalar_updates_remain_typed: documented scalar defaults, request retry/port typed behavior, numeric text, supported boolean spellings and opaque HTML/Unicode text semantics.
- config_uploads_migrate_legacy_and_expand_merges_and_binary_scalars: legacy first-write migration and unknown-section archive, explicit-value precedence, YAML aliases/merges/binary inputs, secret hashing, repeated edits, root replacement, catalog sources/clearing and method refusal.
- management_secrets_are_redacted_preserved_by_identity_and_rotated_as_hashes: config view hides management/TURN secrets, URL identity preserves reordered TURN credentials, explicit empty values are respected, rotation hashes the secret and changes accepted auth.

These cover retained behavior across all 17 historical config scenario families. Removed obligations: recorder scenario/step totals; Go JSON key order/HTML escaping/raw generated JSON bytes; exact YAML decoder prose; Go default materialization and archive layout byte identity; removed-provider/plugin implementation; malformed parser precedence cases without an operator contract. Comments, migration archive data, valid operator syntax, atomicity, typed values and redaction still have explicit assertions.

### Credential-management families replaced

- credential_inventory_filters_paginates_edits_and_deletes_persistently: deterministic file names, name/index selection and intersection, pagination, no token leakage, status and metadata persistence, Codex token/unknown-field preservation, model definition availability, cooldown reset, single/all deletion and unrelated-file safety.
- config_credentials_change_status_through_config_without_creating_auth_files: config keys stay out of auth-file listings; status writes model exclusions through config, keeps key identity, re-enables, refuses generic file-field edits and creates no auth files.
- credential_refusals_and_dashboard_probes_have_no_side_effects: positive pagination, download/upload traversal, extension constraints, invalid metadata, missing targets, removed-provider upload refusal, all dashboard probes and no disk/config/revision changes.
- credential_uploads_preserve_bytes_and_metadata_while_edits_change_requested_fields: raw Claude upload/download bytes, attachment metadata, no-op status, nested unknown fields and ordinary JSON numbers, provider token preservation under Codex metadata edits, multipart partial success and truncated-upload atomicity, bulk-delete partial results.
- api_call_substitutes_only_the_selected_credential_and_relays_local_upstream: selected token in headers/body, JSON-escaped token substitution, payload and headers observed by a local upstream, 307 body/method preservation, status/body/chunk relay, invalid URL/auth refusal, connection failure and usage schema/count validation.
- oauth_sessions_validate_callback_identity_and_support_denial_and_cancellation: state/PKCE challenge and documented Claude scopes, identity mismatch refusal, asynchronous provider denial, repeated callback conflict, cancellation and unknown-state behavior; no token exchange/live account traffic.
- application_logs_support_tail_cursor_download_and_clear_without_exposing_other_files: chronological rotations, continuation lines, tail limits, cursor append, timestamp filtering, error inventory, request/error download bodies, invalid traversal/missing file handling, malformed cursor reset, clear/truncate behavior and logging-disabled refusal.

These cover retained behavior across credential inventory/edits, save bytes, api-call/usage, OAuth sessions, logs-disabled and logs-enabled families. Runtime-only AI Studio relay credentials remain removed under #49. Removed obligations: Go float64 quantization/number spelling for arbitrary metadata, Go JSON serializer order/escaping, incidental Date/client-generated headers, exact error prose, hard-coded catalog contents, Go lock-file scenario counts and multipart parser trivia such as duplicate malformed-header precedence. Raw passthrough is still checked where it is a product contract.

## Fixture/recorder cleanup

- Removed core scalar_settings_go.json, scalar_model_names_go.json, provider_key_go.json, binary_strings_go.json and prepare_upload_go.json after confirming no active consumers.
- Removed all core tests/reference recorder directories, including removed Vertex/provider-key generators. Retained production legacy_paths.json as supported configuration migration data and removed its stale regeneration instruction.
- New input-only core fixtures: yaml_binary_inputs.json (85 historical stimuli) and upload_inputs.json (116 historical stimuli). New management validation fixture: manage_config_inputs.json (83 historical stimuli). They contain no Go expected results.
- Removed manage_go.json and tests/reference/manage. management_contracts.rs has no Go fixture loader, snapshots or adaptation helpers.
- Shared server helper modules forward_port/plugins_removed/removed_settings are no longer declared by management_contracts.rs. Other agent owns their remaining consumers/cleanup.

## Validation

- CARGO_BUILD_JOBS=2 nice -n 19 cargo test -p cliproxy-internal --offline passed: 95 unit tests, 2 binary integration tests, 2 upload integration tests, doc tests.
- CARGO_BUILD_JOBS=2 nice -n 19 cargo test -p cliproxy-internal --test management_contracts --offline passed: 24 tests. Final clean rerun also passed after removing an unused import. A separate run observed a transient concurrent runtime.rs rewrite and failed before the stable rerun.
- Focused config/credential contract run passed all 13 selected tests before deleting replay interpreters.
- git diff --check passed for owned paths before final cleanup. Scoped rustfmt applied; no global format.

## Mapped-address trust security fix

Removing the old Go expectation exposed a concrete security bug: a bare mapped address selected prefix /32 from its reduced IPv4 byte length, then appended it to the original IPv6 spelling. TrustedProxies::new(["::ffff:127.0.0.1"]) therefore constructed ::/32, trusted unrelated IPv6 peers, and failed to match canonical 127.0.0.1. Parent authorized a scoped production correction.

The constructor now selects /32 or /128 from the parsed address family before the existing mapped-address canonicalization. Generated tests for three mapped hosts prove exact canonical+mapped matches, neighbor and ::1/::2 refusals, and untrusted forwarded-loopback spoof refusal. Management auth now tests the configured mapped proxy in both socket spellings and refuses unrelated IPv4/IPv6 peers attempting a forwarded-loopback bypass. Explicit CIDRs keep their semantics. Added an operator-visible DIFFERENCES-FROM-GO.md section.

The new exact-host regression failed before the fix and the focused core trusted suite passes afterward (2 tests). Focused management auth verification also passes (1 test), recorded in mapped-management.log. The final owned-path diff check is clean. No other production behavior changed here.
