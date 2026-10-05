# Parity status, item by item

Audit of 2026-10-05 against CLIProxyAPI `6fecc6e`. Milestones audited: M1, M2, M3, M4, M5, M6 (every item in [checklist.md](checklist.md)). [docs/PARITY.md](../PARITY.md) explains the milestones and sums them up.

Statuses:

- **covered**: implemented, and Rust tests or Go-generated fixtures exercise it. A note starting "Deliberate difference" marks an owner-approved divergence from Go; it counts as covered because there is no gap to close.
- **partial**: implemented in part, or implemented without tests that pin Go's behaviour. For Go test suites: the behaviour exists and is exercised, but not every Go case is ported.
- **missing**: not implemented.

Area names the part of the code base that would close a partial or missing item.

Method: `docs/parity-audit/audit.py` regenerates this file. Routes come from `probe.py`, which starts the binary and requests every listed method and path without credentials (routed pairs answer from the auth guard or handler, unrouted ones 404/405). A route counts as covered when a test requests it. Go test suites are matched case by case against Go test names cited in Rust code and in Go-generated fixtures (fixture names carry `TestName:line`). Every other item, and every suite the matcher cannot see, was judged by reading the Rust code and tests; those judgments live in `docs/parity-audit/manual.tsv` with their evidence. "Not ported by name" means the area is implemented and tested through other cases (usually Go-generated end-to-end scenarios), but the Go suite's own cases are not reproduced one by one.

## Summary

| Milestone | Items | covered | partial | missing |
|---|---:|---:|---:|---:|
| M1 | 118 | 53 | 65 | 0 |
| M2 | 158 | 111 | 40 | 7 |
| M3 | 350 | 190 | 112 | 48 |
| M4 | 510 | 232 | 258 | 20 |
| M5 | 303 | 180 | 102 | 21 |
| M6 | 248 | 69 | 130 | 49 |

### Gaps by area

| Area | missing | partial | Missing items |
|---|---:|---:|---|
| google | 46 | 13 | M3-0019, M3-0033, M3-0036, M3-0048, M3-0054, M3-0058, M3-0059, M3-0060, M3-0061, M3-0062, M3-0063, M3-0222, M3-0226, M3-0227, M3-0228, M3-0229, M3-0230, M3-0231, M3-0232, M3-0233, M3-0234, M3-0235, M3-0236, M3-0237, M3-0238, M3-0239, M3-0240, M3-0241, M3-0242, M3-0269, M3-0270, M3-0283, M3-0287, M3-0329, M3-0334, M3-0335, M3-0336, M3-0342, M3-0344, M4-0063, M4-0415, M5-0021, M5-0032, M5-0235, M5-0300, M6-0210 |
| plugins | 39 | 108 | M5-0078, M5-0079, M5-0080, M5-0081, M5-0082, M6-0137, M6-0138, M6-0139, M6-0140, M6-0141, M6-0142, M6-0143, M6-0144, M6-0145, M6-0146, M6-0150, M6-0151, M6-0152, M6-0153, M6-0154, M6-0169, M6-0170, M6-0171, M6-0174, M6-0183, M6-0187, M6-0188, M6-0192, M6-0194, M6-0195, M6-0211, M6-0213, M6-0220, M6-0239, M6-0240, M6-0241, M6-0242, M6-0247, M6-0248 |
| manage | 13 | 125 | M5-0090, M5-0091, M5-0097, M5-0098, M5-0099, M5-0100, M5-0128, M5-0129, M5-0130, M5-0228, M5-0276, M6-0155, M6-0162 |
| codex | 12 | 63 | M2-0145, M2-0146, M2-0147, M2-0148, M2-0149, M3-0142, M3-0246, M3-0262, M3-0263, M4-0268, M4-0269, M6-0214 |
| observe | 12 | 1 | M4-0001, M4-0002, M4-0003, M4-0004, M4-0005, M4-0006, M4-0007, M4-0008, M4-0009, M4-0010, M4-0011, M4-0485 |
| openai-xai | 8 | 27 | M2-0139, M2-0150, M3-0056, M3-0057, M3-0154, M4-0259, M6-0216, M6-0217 |
| home | 5 | 22 | M6-0173, M6-0225, M6-0230, M6-0231, M6-0237 |
| server | 4 | 221 | M3-0221, M4-0307, M4-0371, M4-0372 |
| tui | 4 | 10 | M5-0087, M6-0058, M6-0059, M6-0218 |
| device-providers | 1 | 20 | M6-0215 |
| realtime | 1 | 7 | M3-0209 |
| claude | 0 | 60 | — |
| translate | 0 | 30 | — |

## M1

### M1: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0001 | GET /healthz | covered | probe: GET /healthz -> 200; tests: crates/cliproxy/tests/plugin_wiring.rs, crates/cpa-server/src/observability.rs (+5) |  |  |
| M1-0002 | HEAD /healthz | covered | probe: HEAD /healthz -> 200; tests: crates/cliproxy/tests/plugin_wiring.rs, crates/cpa-server/src/observability.rs (+5) |  |  |
| M1-0003 | GET /v1/models | partial | probe GET /v1/models -> 401; crates/cpa-server/tests/routes.rs, claude_passthrough.rs; Codex client_version catalog in crates/cpa-server/src/codex_models.rs (tests/codex_models.rs) | openai-xai | The Grok Shell catalog falls back to the OpenAI list (ponytail in crates/cpa-server/src/models.rs). |
| M1-0004 | POST /v1/messages | covered | probe: POST /v1/messages -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-exec/src/claude.rs (+10) |  |  |
| M1-0005 | POST /v1/messages/count_tokens | partial | probe POST /v1/messages/count_tokens -> 401; executor count path: claude scenarios count-oauth-mid-system, claude::tests::custom_origin_counts_locally_without_sending_credentials | server | No server test drives the route to a Claude credential. |
| M1-0006 | GET / | covered | crates/cpa-server/tests/routes.rs misc_routes_match_go (GET /) |  |  |
| M1-0007 | GET /anthropic/callback | covered | probe: GET /anthropic/callback -> 200; tests: crates/cpa-server/tests/routes.rs |  |  |

### M1: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0008 | Local OAuth listener `ANY /callback` (browser flow uses GET; ServeMux registration has no … | covered | crates/cpa-exec/src/claude_login_tests.rs callback_server_answers_like_go, success_page_escapes_the_platform_url |  |  |
| M1-0009 | Local OAuth listener `ANY /success` (browser flow uses GET; ServeMux registration has no m… | covered | crates/cpa-exec/src/claude_login_tests.rs callback_server_answers_like_go (302 to /success, success page) |  |  |
| M1-0010 | Client auth input compatibility: Bearer Authorization, `X-Api-Key`, `X-Goog-Api-Key`, and … | covered | crates/cpa-server/src/access.rs (Bearer, X-Goog-Api-Key, X-Api-Key, key/auth_token query; url.ParseQuery tests); gemini_routes.rs; routes.rs 401 envelope |  |  |

### M1: 3. Upstream providers, auth flows, persisted records, and special behavior

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0011 | Claude | covered | crates/cpa-exec/src/oauth_tests.rs pkce_and_authorize_url, code_exchange_login_layout_and_atomic_permissions, refresh_*; claude_login_tests.rs browser_login_writes_go_file_and_migrates_the_legacy_one |  |  |

### M1: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0012 | Claude on-disk typed core | covered | crates/cpa-exec/src/oauth_tests.rs code_exchange_login_layout_and_atomic_permissions, missing_rotated_refresh_and_profile_fields_keep_saved_values; claude_login_tests.rs browser_login_writes_go_file_and_migrates_the_legacy_one |  |  |

### M1: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0013 | Native-vs-cloaked policy must distinguish strong verified CLI signals, entrypoint, exact m… | covered | crates/cpa-exec/src/claude/detect.rs; claude scenarios native-confirmed-stream, apikey-firstparty-default, apikey-firstparty-cli, apikey-cloak-always |  |  |
| M1-0014 | Cloaking builds Claude Code billing and identity system blocks, assigns stable credential … | covered | crates/cpa-exec/src/claude/cloak.rs; claude scenarios oauth-plain, oauth-opus55-complex, oauth-strict-sensitive, oauth-subagent, oauth-legacy-stream |  |  |
| M1-0015 | CCH billing signing is automatic only on native supported origins (Anthropic and Vertex); … | covered | crates/cpa-exec/src/claude/signing.rs; cch values in claude scenarios and crates/cpa-exec/src/claude/testdata/go_captures.json |  |  |
| M1-0016 | Stable header/software baseline: claude-cli/2.1.280 (external, cli), Stainless 0.112.1, ru… | partial | crates/cpa-exec/src/claude/profile.rs (BASELINE, 7-day PROFILE_TTL); claude scenarios | home | Home KV profile mode (shared profiles, 5 s write lock) is not ported (ponytail in profile.rs). |
| M1-0017 | Do not substitute generic browser TLS for native CLI: Claude Messages/count_tokens uses de… | covered | crates/cpa-exec/src/tls.rs capture_both_clienthellos_against_go_source_profile, transport_lru_bound; harness ClientHello and resumption comparison (harness run on 2026-10-02: 38 hellos structurally identical) |  |  |
| M1-0018 | OAuth acquisition/profile inspection has its own ordered HTTP/1.1 header profiles and comp… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case; crates/cpa-exec/src/tls.rs ClientHello capture |  |  |
| M1-0019 | MCP tool aliasing rewrites names consistently in declarations, tool_use/tool_result histor… | partial | crates/cpa-exec/src/claude/alias.rs; {{ALIAS}}/{{DRIFT}} replies in claude scenarios | claude | Go's legacy sjson rewrite for malformed JSON is not ported; such bodies pass through unaliased (ponytail in alias.rs). |
| M1-0020 | Thinking replay and signature caches distinguish validated signed thinking, redacted think… | covered | crates/cpa-exec/src/claude/replay.rs (5 tests); claude::tests::compat_replay_sequence_matches_go; claude scenarios replay-1-store, replay-2-restore, oauth-signature-history; cpa_common::signature recorded replay |  |  |
| M1-0021 | Anthropic quota rejection is not every 429: shared 5h/7d/7d_oi limits, utilization/status,… | covered | crates/cpa-exec/src/quota.rs model_shared_and_fast_entitlement_scopes, overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not, latest_relevant_deadline_fractional_seconds_and_dates; claude scenarios oauth-fast-*, oauth-unified-429, oauth-model-429; crates/cpa-server/tests/scheduler_attempts.rs |  |  |
| M1-0022 | Exact Messages header order: `Accept` → `Authorization` → `Content-Type` → `User-Agent` → … | covered | crates/cpa-exec/src/claude/headers.rs; harness wire captures (upstream requests differ only in x-client-request-id) |  |  |
| M1-0023 | Exact count_tokens header order: `Accept` → `Authorization` → `Content-Type` → `User-Agent… | covered | crates/cpa-exec/src/claude/headers.rs; harness count-tokens capture (crates/cpa-exec/src/claude/testdata/go_captures.json count-tokens) |  |  |
| M1-0024 | Exact OAuth token header order: `Accept` → `Content-Type` → `User-Agent` → `Content-Length… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case |  |  |
| M1-0025 | Exact OAuth inspection header order: `Accept` → `Content-Type` → `Authorization` → `Cache-… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case |  |  |
| M1-0026 | Managed beta spelling inventory (conditional, NOT all sent on every request): `token-count… | covered | crates/cpa-exec/src/claude/betas.rs; anthropic-beta headers in every claude scenario |  |  |

### M1: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0027 | oauth.providers.claude.claude-code.disable-cloaking-model-list | partial | read in crates/cpa-server/src/models.rs | server | No test sets it. |
| M1-0028 | access.api-keys | covered | crates/cpa-core/src/config.rs; crates/cpa-server/tests/routes.rs and claude_passthrough.rs configure access.api-keys |  | heuristic: key name match |
| M1-0029 | api-keys.claude[].keys[].api-key | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios configure api-keys.claude |  | heuristic: key name match |
| M1-0030 | api-keys.claude[].base-url | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios apikey-gateway-cli-stream, apikey-cloak-always (base-url) |  | heuristic: key name match |
| M1-0031 | api-keys.claude[].keys[].models[].name | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios with model aliases (apikey-firstparty-oauth-cli-alias) |  | heuristic: key name match |
| M1-0032 | api-keys.claude[].keys[].models[].display-name | covered | crates/cpa-core/src/config/credentials.rs config_models_reach_the_registry_like_go (display names reach the registry; the test configures codex and vertex keys, Claude keys take the same path); crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M1-0033 | api-keys.claude[].keys[].models[].max-context-length | covered | crates/cpa-core/src/registry/dynamic.rs max_context_length, advertised by crates/cpa-server/src/codex_models.rs; tests/codex_models.rs client_version_selects_the_codex_catalog |  | heuristic: key name match |
| M1-0034 | api-keys.claude[].keys[].models[].is-compat | covered | claude scenarios apikey-compat-openai-reasoning, apikey-plain-openai-reasoning; compat scenarios claude_is_compat_keeps_thinking, claude_not_compat_drops_thinking |  | heuristic: key name match |
| M1-0035 | api-keys.claude[].keys[].models[].thinking.min | covered | claude scenarios apikey-resolved-thinking-in-range, apikey-resolved-thinking-out-of-range |  | heuristic: key name match |
| M1-0036 | api-keys.claude[].keys[].models[].thinking.max | covered | claude scenarios apikey-resolved-thinking-in-range, apikey-resolved-thinking-out-of-range |  | heuristic: key name match |
| M1-0037 | api-keys.claude[].keys[].models[].thinking.zero-allowed | partial | plumbed through crates/cpa-core/src/registry.rs to cpa_common::thinking (recorded replay covers the thinking logic) | server | No test configures it through config.yaml. |
| M1-0038 | api-keys.claude[].keys[].models[].thinking.dynamic-allowed | partial | plumbed through crates/cpa-core/src/registry.rs to cpa_common::thinking (recorded replay covers the thinking logic) | server | No test configures it through config.yaml. |
| M1-0039 | api-keys.claude[].keys[].models[].thinking.levels | partial | crates/cpa-core/src/registry/dynamic.rs; crates/cpa-translate/tests/registry_overlay.rs (overlay levels) | server | No test configures it through config.yaml. |
| M1-0040 | api-keys.claude[].keys[].headers | covered | cpa_common::headers custom_headers; claude scenario with headers config |  | heuristic: key name match |
| M1-0041 | api-keys.claude[].keys[].rebuild-mid-system-message | partial | crates/cpa-exec/src/claude/settings.rs, reconcile.rs | claude | No test sets rebuild-mid-system-message. |
| M1-0042 | api-keys.claude[].keys[].cloak.mode | covered | claude scenario apikey-cloak-always (cloak.mode always) |  | heuristic: key name match |
| M1-0043 | api-keys.claude[].keys[].cloak.strict-mode | partial | crates/cpa-exec/src/claude/settings.rs; behaviour covered through credential attributes (claude scenario oauth-strict-sensitive, cloak_strict_mode) | claude | The config.yaml path is not exercised. |
| M1-0044 | api-keys.claude[].keys[].cloak.sensitive-words | partial | crates/cpa-exec/src/claude/settings.rs; behaviour covered through credential attributes (claude scenario oauth-strict-sensitive, cloak_sensitive_words) | claude | The config.yaml path is not exercised. |
| M1-0045 | api-keys.claude[].keys[].cloak.cache-user-id | covered | claude scenario apikey-cloak-always (cache-user-id: true) |  | heuristic: key name match |
| M1-0046 | api-keys.claude[].keys[].fingerprint-profile | covered | claude scenarios apikey-firstparty-cli, apikey-gateway-cli-stream, apikey-firstparty-oauth-cli-alias (fingerprint-profile) |  | heuristic: key name match |
| M1-0047 | api-keys.claude[].keys[].experimental-cch-signing | covered | accepted by crates/cpa-core/src/config/schema.json; Go gives it no runtime effect |  |  |
| M1-0048 | oauth.providers.claude.header-defaults.user-agent | partial | crates/cpa-exec/src/claude/settings.rs, profile.rs | claude | No test sets header-defaults. |
| M1-0049 | oauth.providers.claude.header-defaults.package-version | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0050 | oauth.providers.claude.header-defaults.runtime-version | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0051 | oauth.providers.claude.header-defaults.os | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0052 | oauth.providers.claude.header-defaults.arch | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0053 | oauth.providers.claude.header-defaults.timeout | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0054 | oauth.providers.claude.header-defaults.timezone | partial | crates/cpa-exec/src/claude/settings.rs, claude.rs | claude | No test sets header-defaults; only UTC and the process zone resolve without a tz database (ponytail in claude.rs). |
| M1-0055 | oauth.providers.claude.header-defaults.stabilize-device-profile | partial | crates/cpa-exec/src/claude/settings.rs | claude | No test sets header-defaults. |
| M1-0056 | oauth.providers.claude.disable-claude-cloak-mode | partial | crates/cpa-exec/src/claude/settings.rs; crates/cpa-server/tests/manage_go.rs (oauth-only scoping) | claude | No executor scenario disables cloaking globally. |

### M1: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0057 | Fallback/validation source internal/runtime/executor/helps/claude_device_profile.go:1-639:… | covered | crates/cpa-exec/src/claude/profile.rs BASELINE; claude scenarios assert claude-cli/2.1.280, Stainless 0.112.1, v26.3.0, MacOS/arm64 |  |  |

### M1: CLI flags

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0058 | --claude-login | covered | crates/cliproxy/src/main.rs --claude-login -> cpa_exec::claude_login::login (claude_login_tests.rs) |  |  |
| M1-0059 | --no-browser | covered | crates/cliproxy/src/main.rs --no-browser -> LoginOptions.no_browser (claude_login_tests.rs pasted-callback tests) |  |  |
| M1-0060 | --oauth-callback-port | covered | crates/cliproxy/src/main.rs --oauth-callback-port (0 -> provider default); claude_login_tests.rs a_busy_port_is_go_port_in_use |  |  |
| M1-0061 | --config | covered | crates/cliproxy/src/main.rs every_go_flag_parses_in_single_and_double_dash_form, argv_prescan_matches_go; --config resolves to config.yaml in the working directory like Go's empty default |  |  |
| M1-0062 | --local-model | covered | crates/cliproxy/src/main.rs (--local-model -> Runtime::set_local_model); every_go_flag_parses_in_single_and_double_dash_form |  |  |
| M1-0063 | Startup loads .env automatically (do not require a real .env for parity fixtures), config-… | covered | crates/cliproxy/src/main.rs: .env (dotenv.rs parses_like_godotenv), cloud standby (cloud_mode_treats_missing_empty_and_broken_files_as_empty_config), auth-dir expansion, logins; cpa_store::select/bootstrap for PGSTORE_*, OBJECTSTORE_*, GITSTORE_* (crates/cpa-store tests) |  |  |

### M1: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M1-0064 | internal/auth/claude/anthropic_auth_test.go | partial | crates/cpa-exec/src/oauth_tests.rs token_key_singleflight_survives_canceled_waiter, refresh_429_backoff_and_error_redaction, refresh_rotation_preserves_identity_on_optional_profile_failure, missing_rotated_refresh_and_profile_fields_keep_saved_values | claude | Equivalents cover dedupe, 429 backoff, profile-failure tolerance and account preservation; no Go case is ported by name and the timeout cases are untested. |
| M1-0065 | internal/auth/claude/filename_test.go | covered | crates/cpa-exec/src/claude_login_tests.rs file_names_match_go, legacy_matching_follows_go_identity_rules |  |  |
| M1-0066 | internal/auth/claude/identity_test.go | partial | crates/cpa-exec/src/claude/identity.rs; device pools in claude scenarios | claude | Pool repair/migration cases are not ported. |
| M1-0067 | internal/auth/claude/oauth_response_test.go | partial | crates/cpa-exec/src/oauth.rs (response decoding) | claude | Stacked/advertised encoding cases are not ported. |
| M1-0068 | internal/auth/claude/token_test.go | partial | crates/cpa-exec/src/oauth_tests.rs missing_rotated_refresh_and_profile_fields_keep_saved_values | claude | Custom metadata preservation on save is not tested directly. |
| M1-0069 | internal/auth/claude/utls_transport_test.go | partial | crates/cpa-exec/src/tls.rs capture_both_clienthellos_against_go_source_profile, transport_lru_bound; oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case | claude | Handshake bound and resumption wire-safety cases are not ported. |
| M1-0070 | internal/cache/claude_thinking_replay_cache_test.go | partial | crates/cpa-exec/src/claude/replay.rs (5 tests) | claude | Not ported by name. |
| M1-0071 | internal/client/claude/models/models_test.go | partial | crates/cpa-server/src/claude.rs dd_model_ids_round_trip_like_go (model ID prefix); crates/cpa-server/src/models.rs claude_list | server | BuildResponse cases (cloaking on/off, empty) are untested. |
| M1-0072 | internal/runtime/executor/claude_cloaked_cache_repro_test.go | partial | claude scenarios oauth-turn-1, oauth-turn-2 (prefix and continuity) | claude | Fingerprint edge cases and UTF-16 index cases are not ported. |
| M1-0073 | internal/runtime/executor/claude_executor_auth_race_test.go | covered | n/a in Rust: Go data-race invariants on shared credential metadata; Rust credentials are immutable snapshots with MetadataPatch commits |  |  |
| M1-0074 | internal/runtime/executor/claude_executor_auth_test.go | partial | crates/cpa-exec/src/oauth_tests.rs prepare_forces_a_refresh_outside_the_lead_window; crates/cpa-exec/src/claude/identity.rs | claude | Setup-token, 403 scope fallback and skip-profile cases are not ported. |
| M1-0075 | internal/runtime/executor/claude_executor_beta_passthrough_test.go | partial | crates/cpa-exec/src/claude/betas.rs; claude scenarios | claude | Not ported by name. |
| M1-0076 | internal/runtime/executor/claude_executor_beta_policy_test.go | partial | crates/cpa-exec/src/claude/betas.rs, quota.rs; claude scenarios | claude | Not ported by name. |
| M1-0077 | internal/runtime/executor/claude_executor_cloaking_display_test.go | partial | crates/cpa-exec/src/claude/reconcile.rs tests | claude | Not ported by name. |
| M1-0078 | internal/runtime/executor/claude_executor_diagnostics_test.go | partial | crates/cpa-exec/src/claude/signals.rs, session.rs; claude scenarios oauth-turn-1, oauth-turn-2 | claude | Not ported by name. |
| M1-0079 | internal/runtime/executor/claude_executor_fable_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs (overage-only, model scope); crates/cpa-server/tests/scheduler_attempts.rs | claude | Fable-only rejection and model-level cooling cases are not ported. |
| M1-0080 | internal/runtime/executor/claude_executor_fast_error_test.go | partial | claude scenarios decode-fast-bad-gzip-429, decode-fast-truncated-503, oauth-fast-500; crates/cpa-core/src/exec.rs direct error responses | claude | Not ported by name. |
| M1-0081 | internal/runtime/executor/claude_executor_native_helper_test.go | partial | crates/cpa-exec/src/claude/detect.rs; go_captures.json native-signals | claude | Not ported by name. |
| M1-0082 | internal/runtime/executor/claude_executor_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs; claude scenarios oauth-unified-429, oauth-model-429; scheduler_attempts.rs | claude | Not ported by name. |
| M1-0083 | internal/runtime/executor/claude_executor_request_remap_test.go | partial | crates/cpa-exec/src/claude/alias.rs; claude scenarios with alias drift | claude | Malformed-JSON fallback is not ported; mangled-alias recovery cases are not ported by name. |
| M1-0084 | internal/runtime/executor/claude_executor_stream_terminal_test.go | partial | crates/cpa-exec/src/claude/stream.rs (stops after message_stop) | claude | Client disconnect after the terminal event is untested. |
| M1-0085 | internal/runtime/executor/claude_executor_subagent_ttl_regression_test.go | partial | claude scenario oauth-subagent | claude | API-key subagent and stream variants are not ported. |
| M1-0086 | internal/runtime/executor/claude_executor_test.go | partial | crates/cpa-exec/src/claude.rs and claude/*; 46 claude scenarios, go_captures.json, harness | claude | 221 Go cases; behaviour is exercised through Go-generated scenarios, not ported case by case. |
| M1-0087 | internal/runtime/executor/claude_executor_thinking_signature_test.go | partial | claude scenarios oauth-strict-sensitive, oauth-signature-history | claude | Not ported by name. |
| M1-0088 | internal/runtime/executor/claude_executor_wire_casing_test.go | partial | crates/cpa-exec/src/claude/headers.rs; harness wire captures | claude | Not ported by name. |
| M1-0089 | internal/runtime/executor/claude_fingerprint_policy_test.go | partial | crates/cpa-exec/src/claude/detect.rs, claude.rs; claude scenarios apikey-firstparty-*, apikey-gateway-cli-stream | claude | 24 Go cases; not ported by name. |
| M1-0090 | internal/runtime/executor/claude_issue_6120_test.go | partial | claude scenario oauth-plain (direct Messages OAuth is cloaked) | claude | Stream variant not ported. |
| M1-0091 | internal/runtime/executor/claude_issue_6193_test.go | partial | crates/cpa-exec/src/claude/betas.rs | claude | Not ported by name. |
| M1-0092 | internal/runtime/executor/claude_messages_passthrough_test.go | partial | claude scenarios native-confirmed-stream, apikey-firstparty-default | claude | Not ported by name. |
| M1-0093 | internal/runtime/executor/claude_mid_system_model_test.go | partial | crates/cpa-exec/src/claude/reconcile.rs tests; claude scenario count-oauth-mid-system | claude | Not ported by name. |
| M1-0094 | internal/runtime/executor/claude_signing_test.go | partial | crates/cpa-exec/src/claude/signing.rs; cch values in claude scenarios | claude | Known-vector cases are not ported by name. |
| M1-0095 | internal/runtime/executor/claude_thinking_replay_test.go | partial | claude::tests::compat_replay_sequence_matches_go; claude scenarios replay-1-store, replay-2-restore | claude | Not ported by name. |
| M1-0096 | internal/runtime/executor/helps/claude_builtin_tools_test.go | partial | crates/cpa-exec/src/claude/alias.rs is_server_tool_type | claude | Not ported by name. |
| M1-0097 | internal/runtime/executor/helps/claude_cli_identity_seed_test.go | partial | crates/cpa-exec/src/claude/identity.rs | claude | Not ported by name. |
| M1-0098 | internal/runtime/executor/helps/claude_client_detection_test.go | partial | crates/cpa-exec/src/claude/detect.rs; claude scenarios | claude | 19 Go cases; not ported by name. |
| M1-0099 | internal/runtime/executor/helps/claude_code_session_test.go | partial | cpa_common::session; crates/cpa-exec/src/claude/session.rs | claude | Not ported by name. |
| M1-0100 | internal/runtime/executor/helps/claude_credential_identity_race_test.go | covered | n/a in Rust: Go data-race invariants on shared credential metadata |  |  |
| M1-0101 | internal/runtime/executor/helps/claude_credential_identity_test.go | partial | crates/cpa-exec/src/claude/identity.rs, session.rs | claude | Not ported by name; the Home KV case belongs to M6 Home. |
| M1-0102 | internal/runtime/executor/helps/claude_device_profile_test.go | partial | crates/cpa-exec/src/claude/profile.rs | claude | Local profile cases are not ported by name; the Home cases belong to M6 Home. |
| M1-0103 | internal/runtime/executor/helps/claude_diagnostics_test.go | partial | crates/cpa-exec/src/claude/signals.rs, session.rs | claude | Not ported by name. |
| M1-0104 | internal/runtime/executor/helps/claude_input_tokens_test.go | partial | claude::tests::custom_origin_counts_locally_without_sending_credentials; local count in crates/cpa-exec/src/claude.rs | claude | message_start input-token patching cases are not ported. |
| M1-0105 | internal/runtime/executor/helps/claude_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs latest_relevant_deadline_fractional_seconds_and_dates, overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not | claude | Equivalent cases; not ported by name. |
| M1-0106 | internal/runtime/executor/helps/claude_upstream_test.go | partial | crates/cpa-exec/src/claude.rs DEFAULT_BASE_URL handling | claude | Not ported by name. |
| M1-0107 | internal/signature/claude_messages_sanitize_compat_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M1-0108 | internal/signature/claude_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/22 cases also cited by name) |  |  |
| M1-0109 | internal/thinking/claude_enabled_effort_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (internal/thinking calls) |  |  |
| M1-0110 | internal/util/claude_attribution_test.go | partial | crates/cpa-translate/src/common.rs is_claude_code_attribution_text; translator goldens | claude | StripClaudeCodeAttributionSystem is ported in crates/cpa-exec/src/claude.rs (strip_attribution_system) without Go's case; IsClaudeCodeAttributionSystemText replays Go's calls (crates/cpa-translate/tests/fixtures/go_helpers.json). |
| M1-0111 | internal/util/claude_model_test.go | partial | inlined in crates/cpa-translate/src/antigravity_claude.rs; claude-antigravity goldens | translate | Unit cases not ported. |
| M1-0112 | internal/util/claude_schema_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M1-0113 | internal/util/claude_tool_id_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M1-0114 | internal/util/claude_tool_result_test.go | partial | crates/cpa-translate/src/gemini.rs; translator goldens | translate | Unit cases not ported. |
| M1-0115 | sdk/api/handlers/claude/code_handlers_error_test.go | partial | crates/cpa-server/src/claude.rs, errors.rs; routes.rs failover_stop_rules_and_cooldown_contracts | server | Not ported by name. |
| M1-0116 | sdk/api/handlers/claude/code_handlers_model_test.go | partial | crates/cpa-server/src/claude.rs dd_model_ids_round_trip_like_go | server | Display-name and model-list cloaking cases are untested. |
| M1-0117 | test/claude_code_compatibility_sentinel_test.go | covered | n/a: the Go test checks its own fixture maps, not production code |  |  |
| M1-0118 | test/codex_claude_parallel_function_calls_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |

## M2

### M2: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M2-0001 | POST /v1/chat/completions | covered | probe: POST /v1/chat/completions -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-exec/src/kimi_tests.rs (+5) |  |  |
| M2-0002 | POST /v1/completions | covered | probe: POST /v1/completions -> 401; tests: crates/cpa-server/tests/routes.rs |  |  |
| M2-0003 | POST /v1/responses | covered | probe: POST /v1/responses -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-exec/src/codex_capture.rs (+13) |  |  |
| M2-0004 | POST /v1/responses/compact | covered | probe: POST /v1/responses/compact -> 401; tests: crates/cpa-server/tests/openai_compat_routes.rs, crates/cpa-server/tests/routes.rs |  |  |
| M2-0005 | POST /backend-api/codex/responses | covered | probe: POST /backend-api/codex/responses -> 401; tests: crates/cpa-exec/src/codex_tls_tests.rs, crates/cpa-server/src/observability.rs |  |  |
| M2-0006 | POST /backend-api/codex/responses/compact | partial | probe: POST /backend-api/codex/responses/compact -> 401; no test requests this path | server |  |
| M2-0007 | GET /v1beta/models | covered | probe: GET /v1beta/models -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/safe_mode.rs (+3) |  |  |
| M2-0008 | POST /v1beta/interactions | covered | probe: POST /v1beta/interactions -> 401; tests: crates/cpa-server/tests/gemini_routes.rs, crates/cpa-server/tests/routes.rs |  |  |
| M2-0009 | POST /v1beta/models/*action | covered | probe: POST /v1beta/models/*action -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/tests/aistudio_relay.rs (+2) |  |  |
| M2-0010 | GET /v1beta/models/*action | covered | probe: GET /v1beta/models/*action -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/tests/aistudio_relay.rs (+2) |  |  |

### M2: 2. Registered translator matrix and LOC

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M2-0011 | Gemini → Claude | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-claude.json (reference_goldens) |  |  |
| M2-0012 | Interactions → Claude | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-claude.json |  |  |
| M2-0013 | OpenAI → Claude | covered | crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0014 | OpenaiResponse → Claude | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0015 | Claude → Gemini | covered | crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0016 | Gemini → Gemini | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-gemini.json |  |  |
| M2-0017 | Interactions → Interactions | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-interactions.json |  |  |
| M2-0018 | Interactions → Gemini | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-gemini.json |  |  |
| M2-0019 | Gemini → Interactions | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-interactions.json |  |  |
| M2-0020 | OpenAI → Gemini | covered | crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0021 | OpenaiResponse → Gemini | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0022 | Claude → Interactions | covered | crates/cpa-translate/tests/fixtures/pairs/claude-interactions.json |  |  |
| M2-0023 | Claude → OpenAI | covered | crates/cpa-translate/tests/fixtures/pairs/claude-openai.json |  |  |
| M2-0024 | Gemini → OpenAI | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-openai.json |  |  |
| M2-0025 | OpenAI → Interactions | covered | crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0026 | Interactions → OpenAI | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-openai.json |  |  |
| M2-0027 | OpenaiResponse → Interactions | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0028 | Interactions → OpenaiResponse | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-openai-response.json |  |  |
| M2-0029 | OpenAI → OpenAI | covered | crates/cpa-translate/tests/fixtures/pairs/openai-openai.json |  |  |
| M2-0030 | OpenaiResponse → OpenAI | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0031 | Registry fallback is identity plus resolved top-level model rewrite, not implicit universa… | covered | crates/cpa-translate/tests/sdk_translator.rs (registration matrix, fallback vectors); tool_error goldens; envelope goldens |  |  |
| M2-0032 | Canonical reasoning pipeline: parse model suffix (suffix wins body), extract canonical thi… | covered | cpa_common::thinking recorded replay (go_calls.jsonl.gz, go_thinking_matrix.jsonl.gz); summary pipeline in translator goldens |  |  |
| M2-0033 | Translation fidelity includes tool input JSON, parallel tool IDs, role/system ordering, mu… | covered | 9,435 translator byte goldens (crates/cpa-translate/tests/fixtures/pairs) |  |  |

### M2: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M2-0034 | requests.streaming.keepalive-seconds | partial | crates/cpa-server/src/respond.rs, websocket.rs | server | No test sets keepalive-seconds. |
| M2-0035 | requests.streaming.bootstrap-retries | covered | crates/cpa-server/tests/routes.rs bootstrap_retries_rerun_a_stream_that_broke_before_its_first_payload |  | heuristic: key name match |
| M2-0036 | requests.nonstream-keepalive-interval | covered | crates/cpa-server/src/dispatch.rs nonstream_keepalive; tests/routes.rs nonstream_keepalive_commits_like_go (Go golden nonstream_keepalive in server_go.json) |  | heuristic: key name match |

### M2: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M2-0037 | internal/config/gemini_keys_normalization_test.go | partial | crates/cpa-core/src/config/credentials.rs sanitized() keeps empty keys with a base URL | manage | Not ported by name. |
| M2-0038 | internal/runtime/executor/gemini_executor_signature_test.go | partial | cpa_common::signature (recorded replay) used by crates/cpa-exec/src/gemini.rs | google | Executor-level signature cases (Gemini and Vertex) are not ported. |
| M2-0039 | internal/runtime/executor/gemini_executor_test.go | partial | gemini scenarios (75): gen_cap_*, gen_boundary_user_turns, gen_payload_rules, stream_* | google | 28 Go cases; equivalents exist for capping, boundary turns and payload rules; not ported by name. |
| M2-0040 | internal/runtime/executor/gemini_interactions_translate_test.go | covered | n/a: Go slice-reuse and plugin-call invariants; Interactions request translation covered by gemini scenarios int_* |  |  |
| M2-0041 | internal/runtime/executor/helps/gemini_content_turns_test.go | covered | gemini scenarios gen_boundary_user_turns, stream_boundary_user_turns, gen_trailing_function_response_kept |  |  |
| M2-0042 | internal/runtime/executor/helps/openai_compat_max_tokens_test.go | covered | compat scenarios chat_max_tokens_to_max_completion_tokens, chat_both_limits_keep_max_completion_tokens, chat_max_completion_tokens_to_max_tokens, chat_requested_alias_selects_limit_mode |  |  |
| M2-0043 | internal/runtime/executor/helps/openai_compat_tool_results_test.go | covered | compat scenarios chat_text_only_tool_results, chat_image_model_keeps_tool_images |  |  |
| M2-0044 | internal/runtime/executor/helps/thinking_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (helps package calls) |  |  |
| M2-0045 | internal/runtime/executor/openai_compat_executor_compact_test.go | partial | compat scenarios compact_*, chat_prompt_cache_key_*, chat_execution_session_* | openai-xai | 21 Go cases; prompt-cache and compact equivalents exist; config-index scoping cases not ported. |
| M2-0046 | internal/runtime/executor/openai_compat_executor_images_test.go | partial | compat scenarios images_* | openai-xai | Executor image paths are ported; the /v1/images routes are not (M3-0001, M3-0002). |
| M2-0047 | internal/runtime/executor/openai_compat_executor_max_tokens_test.go | covered | compat scenarios chat_max_tokens_* (non-stream and stream) |  |  |
| M2-0048 | internal/runtime/executor/openai_compat_executor_reasoning_test.go | covered | compat scenarios claude_is_compat_keeps_thinking, claude_not_compat_drops_thinking |  |  |
| M2-0049 | internal/runtime/executor/openai_compat_executor_retry_test.go | covered | compat scenarios error_429_retry_after_* |  |  |
| M2-0050 | internal/runtime/executor/openai_compat_executor_tool_results_test.go | covered | compat scenarios chat_text_only_tool_results, chat_image_model_keeps_tool_images |  |  |
| M2-0051 | internal/runtime/executor/openai_compat_executor_video_test.go | covered | compat scenario chat_video_input_passthrough |  |  |
| M2-0052 | internal/runtime/executor/openai_responses_signature_test.go | partial | crates/cpa-exec/src/codex_request.rs | codex | Not ported by name. |
| M2-0053 | internal/signature/gemini_sanitize_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (internal/signature calls) |  |  |
| M2-0054 | internal/signature/gemini_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/29 cases also cited by name) |  |  |
| M2-0055 | internal/thinking/apply_configured_api_key_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/15 cases also cited by name) |  |  |
| M2-0056 | internal/thinking/summary_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/12 cases also cited by name) |  |  |
| M2-0057 | internal/translator/claude/gemini/claude_gemini_request_test.go | covered | 13/13 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-claude.json |  |  |
| M2-0058 | internal/translator/claude/gemini/claude_gemini_response_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-claude.json |  |  |
| M2-0059 | internal/translator/claude/gemini/noop_optimization_test.go | partial | 5/5 cases: crates/cpa-translate/tests/fixtures/go_helpers.json | translate | Byte results of all five cases are replayed; TestLowercaseClaudeToolSchemaTypesReusesLowercaseSchema also asserts that the input buffer is returned without a copy, which the replay does not check (it compares returned bytes only). |
| M2-0060 | internal/translator/claude/interactions/interactions_claude_test.go | covered | 12/12 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-claude.json |  |  |
| M2-0061 | internal/translator/claude/openai/chat-completions/claude_openai_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0062 | internal/translator/claude/openai/chat-completions/claude_openai_request_test.go | covered | 33/33 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0063 | internal/translator/claude/openai/chat-completions/claude_openai_response_test.go | covered | 12/12 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0064 | internal/translator/claude/openai/chat-completions/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0065 | internal/translator/claude/openai/responses/claude_openai-responses_citations_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0066 | internal/translator/claude/openai/responses/claude_openai-responses_interleaved_search_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0067 | internal/translator/claude/openai/responses/claude_openai-responses_reasoning_order_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0068 | internal/translator/claude/openai/responses/claude_openai-responses_request_test.go | partial | 64/67 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestClaudeMessageInvariantProblems, TestSplitResponsesQualifiedFunctionCallFromAdditionalTools, TestConvertOpenAIResponsesRequestToClaudeWithCompat_FablePreservesAssistantPrefill | translate |  |
| M2-0069 | internal/translator/claude/openai/responses/claude_openai-responses_response_test.go | covered | 54/54 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0070 | internal/translator/claude/openai/responses/claude_openai-responses_server_tool_test.go | covered | 17/17 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0071 | internal/translator/claude/openai/responses/claude_openai-responses_tool_names_test.go | partial | 1/5 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestBuildClaudeToolNames_RoundTripAndStability, TestBuildClaudeToolNames_DeclarationOrderInvariance, TestBuildClaudeToolNames_SingleLongName, TestBuildClaudeToolNames_DeclaredToolsPrecedeHistory | translate |  |
| M2-0072 | internal/translator/claude/openai/responses/claude_openai_responses_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0073 | internal/translator/claude/openai/responses/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0074 | internal/translator/common/apply_patch_events_test.go | partial | 2/5 cases: crates/cpa-translate/tests/fixtures/go_helpers.json; not matched: TestApplyPatchCallStateFailureIsolation, TestApplyPatchEventsPayloads, TestApplyPatchFailureSanitizesClientError | translate |  |
| M2-0075 | internal/translator/common/apply_patch_identity_test.go | covered | 3/3 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json |  |  |
| M2-0076 | internal/translator/common/apply_patch_input_test.go | partial | 5/12 cases: crates/cpa-translate/tests/fixtures/go_helpers.json; not matched: TestApplyPatchInputDecoderStringFragments, TestApplyPatchInputDecoderRejectsInvalidArguments, TestApplyPatchInputDecoderInvalidValueFailsImmediately, TestApplyPatchInputDecoderInvalidPendingCharactersDoNotEmitReplacement … | translate |  |
| M2-0077 | internal/translator/common/apply_patch_responses_test.go | covered | 17/17 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json, crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0078 | internal/translator/common/bytes_test.go | partial | crates/cpa-translate/src/common.rs, sse.rs; cpa_common::json | translate | Unit cases not ported. |
| M2-0079 | internal/translator/common/cache_control_test.go | covered | 7/7 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0080 | internal/translator/common/claude_messages_test.go | partial | implementation cites claude_messages.go (crates/cpa-translate/src/common.rs); no case matched by name | translate |  |
| M2-0081 | internal/translator/common/claude_system_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0082 | internal/translator/common/claude_user_id_test.go | covered | 19/19 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0083 | internal/translator/common/file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0084 | internal/translator/common/gemini_test.go | covered | 6/6 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0085 | internal/translator/common/openai_tools_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M2-0086 | internal/translator/common/request_test.go | partial | 2/3 cases: crates/cpa-translate/tests/fixtures/go_helpers.json; not matched: TestGenerateClaudeToolCallID | translate |  |
| M2-0087 | internal/translator/common/responses_test.go | partial | implementation cites responses.go (crates/cpa-translate/src/claude_responses.rs); no case matched by name | translate |  |
| M2-0088 | internal/translator/gemini/claude/gemini_claude_compat_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0089 | internal/translator/gemini/claude/gemini_claude_request_test.go | covered | 15/15 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0090 | internal/translator/gemini/claude/gemini_claude_response_test.go | covered | 6/6 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0091 | internal/translator/gemini/gemini/gemini_gemini_request_test.go | partial | 2/8 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-gemini.json; not matched: TestConvertGeminiRequestToGeminiReusesLargeNormalizedPayload, TestBackfillEmptyFunctionResponseNames_Single, TestBackfillEmptyFunctionResponseNames_Parallel, TestBackfillEmptyFunctionResponseNames_PreservesExisting … | translate |  |
| M2-0092 | internal/translator/gemini/interactions/interactions_gemini_common_test.go | partial | 42/56 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-interactions.json, crates/cpa-translate/tests/fixtures/pairs/interactions-gemini.json; not matched: TestConvertGeminiResponseToInteractionsNonStream, TestConvertGeminiResponseToInteractionsNonStreamSnakeCaseUsage, TestConvertGeminiResponseToInteractionsNonStreamFunctionCall, TestConvertGeminiResponseToInteractionsNonStreamFunctionCallPreservesCallID … | translate |  |
| M2-0093 | internal/translator/gemini/interactions/interactions_gemini_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-gemini.json |  |  |
| M2-0094 | internal/translator/gemini/openai/chat-completions/gemini_openai_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0095 | internal/translator/gemini/openai/chat-completions/gemini_openai_request_test.go | covered | 22/22 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0096 | internal/translator/gemini/openai/chat-completions/gemini_openai_response_test.go | covered | 8/8 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0097 | internal/translator/gemini/openai/chat-completions/gemini_openai_signature_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0098 | internal/translator/gemini/openai/chat-completions/noop_optimization_test.go | covered | 3/3 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0099 | internal/translator/gemini/openai/responses/apply_patch_review_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0100 | internal/translator/gemini/openai/responses/apply_patch_test.go | covered | 10/10 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0101 | internal/translator/gemini/openai/responses/gemini_openai-responses_request_test.go | partial | 75/76 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestReorderOpenAIResponsesDetachedReasoningDoesNotCrossUserMessage | translate |  |
| M2-0102 | internal/translator/gemini/openai/responses/gemini_openai-responses_response_test.go | covered | 46/46 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0103 | internal/translator/gemini/openai/responses/gemini_openai-responses_web_search_test.go | partial | 33/42 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestBuildResponsesURLCitations_RuneOffsetConversion, TestHasValidWebGrounding, TestModelSupportsWebSearch_StaticVetoTakesPrecedence, TestAllowsResponsesWebSearchToolChoice_AllowedTools … | translate |  |
| M2-0104 | internal/translator/gemini/openai/responses/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0105 | internal/translator/gemini/openai/responses/signature_carrier_test.go | partial | 7/10 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestGeminiResponsesCarrierRoundTrip, TestNormalizeGeminiResponsesCarriersDropsMalformedEnvelope, TestDecodeGeminiResponsesCarrierRejectsNestedEnvelope | translate |  |
| M2-0106 | internal/translator/gemini/openai/responses/trailing_signature_test.go | covered | 7/7 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0107 | internal/translator/interactions/claude/interactions_claude_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/claude-interactions.json |  |  |
| M2-0108 | internal/translator/interactions/claude/interactions_claude_test.go | covered | 14/14 cases: crates/cpa-translate/tests/fixtures/pairs/claude-interactions.json |  |  |
| M2-0109 | internal/translator/interactions/import_boundary_test.go | covered | n/a: Go package import boundary check |  |  |
| M2-0110 | internal/translator/openai/claude/openai_claude_compat_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json |  |  |
| M2-0111 | internal/translator/openai/claude/openai_claude_request_test.go | covered | 28/28 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json |  |  |
| M2-0112 | internal/translator/openai/claude/openai_claude_response_test.go | partial | 41/42 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json; not matched: TestExtractOpenAIUsage | translate |  |
| M2-0113 | internal/translator/openai/gemini/openai_gemini_request_test.go | covered | 13/13 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-openai.json |  |  |
| M2-0114 | internal/translator/openai/gemini/openai_gemini_response_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-openai.json |  |  |
| M2-0115 | internal/translator/openai/interactions/chat-completions/interactions_openai_request_test.go | covered | 12/12 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai.json, crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0116 | internal/translator/openai/interactions/chat-completions/interactions_openai_response_test.go | covered | 17/17 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai.json, crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0117 | internal/translator/openai/interactions/chat-completions/openai_interactions_file_data_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0118 | internal/translator/openai/interactions/responses/apply_patch_identity_test.go | covered | 6/6 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0119 | internal/translator/openai/interactions/responses/apply_patch_rereview_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0120 | internal/translator/openai/interactions/responses/apply_patch_review_test.go | covered | 19/19 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0121 | internal/translator/openai/interactions/responses/apply_patch_source_stop_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0122 | internal/translator/openai/interactions/responses/apply_patch_test.go | covered | 16/16 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0123 | internal/translator/openai/interactions/responses/interactions_openai_responses_request_test.go | covered | 30/30 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai-response.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0124 | internal/translator/openai/interactions/responses/interactions_openai_responses_response_test.go | covered | 37/37 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai-response.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0125 | internal/translator/openai/openai/chat-completions/openai_openai_request_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-openai.json |  |  |
| M2-0126 | internal/translator/openai/openai/chat-completions/openai_openai_response_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-openai.json |  |  |
| M2-0127 | internal/translator/openai/openai/responses/custom_tool_namespace_recovery_test.go | partial | 2/3 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json; not matched: TestNamespaceRecoveryDoesNotGuessAmbiguousOrOverrideExactNames | translate |  |
| M2-0128 | internal/translator/openai/openai/responses/openai_openai-responses_request_test.go | partial | 59/64 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json; not matched: TestResponsesSingleCustomToolName_CountsDeduplicatedTools, TestSplitResponsesQualifiedFunctionCallFromRequest_FirstDeclarationWins, TestSplitResponsesQualifiedFunctionCallFromRequest_MatchesMergedToolIdentity, TestResponsesCustomToolNames_FollowsMergedDeclaration … | translate |  |
| M2-0129 | internal/translator/openai/openai/responses/openai_openai-responses_response_test.go | covered | 44/44 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0130 | internal/translator/openai/openai/responses/openai_openai-responses_video_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0131 | internal/translator/openai/openai/responses/responses_compatibility_digest_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0132 | internal/translator/openai/openai/responses/responses_request_state_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0133 | internal/util/gemini_schema_test.go | covered | crates/cpa-common/tests/fixtures/gemini_schema.json: every JSON literal mined from gemini_schema_test.go run through Go's cleaners (crates/cpa-common/tests/gemini_schema.rs) |  |  |
| M2-0134 | internal/watcher/diff/openai_compat_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M2-0135 | sdk/api/handlers/gemini/gemini_handlers_stream_error_test.go | partial | crates/cpa-server/src/dispatch.rs (bootstrap); routes.rs mid_stream_failure_ends_with_route_error_frame | server | Not ported for the Gemini handler. |
| M2-0136 | sdk/api/handlers/gemini/gemini_models_display_name_test.go | partial | crates/cpa-server/src/models.rs gemini_list display_name | server | Untested. |
| M2-0137 | sdk/api/handlers/gemini/interactions_handlers_test.go | partial | crates/cpa-server/tests/gemini_routes.rs (4 tests), routes.rs misc_routes_match_go (Interactions validation) | server | 11 Go cases; target parsing and agent selection are not ported by name. |
| M2-0138 | sdk/api/handlers/openai/openai_handlers_stream_error_test.go | partial | crates/cpa-server/src/dispatch.rs; routes.rs bootstrap and mid-stream tests | server | Not ported by name. |
| M2-0139 | sdk/api/handlers/openai/openai_images_handlers_test.go | missing | POST /v1/images/* not routed (probe 404) | openai-xai | Images handlers (xAI and compat) are not ported. |
| M2-0140 | sdk/api/handlers/openai/openai_responses_compact_test.go | covered | 5/5 cases: crates/cpa-server/tests/openai_compat_routes.rs |  |  |
| M2-0141 | sdk/api/handlers/openai/openai_responses_handlers_stream_error_test.go | partial | crates/cpa-server/src/openai.rs (response.failed), crates/cpa-translate/src/stream.rs ResponsesFramer | server | 21 Go cases; not ported by name. |
| M2-0142 | sdk/api/handlers/openai/openai_responses_handlers_stream_test.go | partial | crates/cpa-translate/src/stream.rs responses_framer_matches_recorded_go_frames; crates/cpa-server/src/openai.rs output repair | server | 23 Go cases; framer recorded frames only. |
| M2-0143 | sdk/api/handlers/openai/openai_responses_multi_agent_test.go | partial | cpa_common::codex_client rewrites; crates/cpa-server/src/openai.rs | codex | Not ported by name. |
| M2-0144 | sdk/api/handlers/openai/openai_responses_signature_test.go | partial | crates/cpa-server/src/openai.rs (no handler-side validation) | server | Untested. |
| M2-0145 | sdk/api/handlers/openai/openai_responses_steering_auth_test.go | missing | no Responses steering in Rust | codex | Responses WebSocket steering is not ported. |
| M2-0146 | sdk/api/handlers/openai/openai_responses_steering_error_test.go | missing | no Responses steering in Rust | codex | Responses WebSocket steering is not ported. |
| M2-0147 | sdk/api/handlers/openai/openai_responses_steering_integration_test.go | missing | no Responses steering in Rust | codex | Responses WebSocket steering is not ported. |
| M2-0148 | sdk/api/handlers/openai/openai_responses_steering_test.go | missing | no Responses steering in Rust | codex | Responses WebSocket steering is not ported. |
| M2-0149 | sdk/api/handlers/openai/openai_responses_steering_validation_test.go | missing | no Responses steering in Rust | codex | Responses WebSocket steering is not ported. |
| M2-0150 | sdk/api/handlers/openai/openai_videos_handlers_test.go | missing | /v1/videos and /openai/v1/videos not routed (probe 404) | openai-xai | Videos handlers are not ported. |
| M2-0151 | sdk/api/handlers/openai/permanent_oauth_classification_test.go | partial | crates/cpa-exec/src/codex_oauth.rs (refresh_token_reused classification) | server | Manager-level permanent-failure handling is untested. |
| M2-0152 | sdk/api/handlers/openai_responses_stream_error_test.go | partial | crates/cpa-server/src/openai.rs response.failed chunks | server | Not ported by name. |
| M2-0153 | sdk/cliproxy/auth/openai_compat_pool_test.go | partial | crates/cpa-server/src/dispatch.rs alias-pool rotation; routes.rs config_models_alias_and_force_mapping_reach_upstream_and_client | server | 20 Go cases; not ported by name. |
| M2-0154 | sdk/cliproxy/openai_compat_config_models_test.go | partial | crates/cpa-server/src/models.rs, crates/cpa-core/src/registry/dynamic.rs input modalities | server | Untested. |
| M2-0155 | sdk/translator/registry_bytes_test.go | covered | Rust transforms return bytes; every translator golden compares bytes |  |  |
| M2-0156 | sdk/translator/registry_summary_test.go | partial | summary cases through sdk.TranslateRequest in translator goldens (matrix summary variants) | plugins | The 3 plugin-hook cases need plugin hooks (M6). |
| M2-0157 | sdk/translator/registry_test.go | partial | crates/cpa-translate/tests/sdk_translator.rs (registration matrix, fallback vectors); apply_patch nil goldens | plugins | 8 of 15 cases need plugin hooks or runtime (un)registration (M6). |
| M2-0158 | test/thinking_conversion_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (Thinking\|Summary\|Signature tests in ./test/) |  |  |

## M3

### M3: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0001 | POST /v1/images/generations | covered | probe: POST /v1/images/generations -> 401; tests: crates/cpa-exec/src/codex_tests.rs, crates/cpa-server/tests/codex_routed_images.rs (+1) |  | The xAI and OpenAI-compatible executors implement image requests, but no /v1/images route reaches them. |
| M3-0002 | POST /v1/images/edits | covered | probe: POST /v1/images/edits -> 401; tests: crates/cpa-server/src/observability.rs |  | The xAI and OpenAI-compatible executors implement image requests, but no /v1/images route reaches them. |
| M3-0003 | POST /v1/videos | covered | probe: POST /v1/videos -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/observability.rs (+1) |  | xAI videos handlers are not routed. |
| M3-0004 | POST /v1/videos/generations | covered | probe: POST /v1/videos/generations -> 401; tests: crates/cliproxy/src/home.rs |  | xAI videos handlers are not routed. |
| M3-0005 | POST /v1/videos/edits | partial | probe: POST /v1/videos/edits -> 401; no test requests this path | openai-xai | xAI videos handlers are not routed. |
| M3-0006 | POST /v1/videos/extensions | partial | probe: POST /v1/videos/extensions -> 401; no test requests this path | openai-xai | xAI videos handlers are not routed. |
| M3-0007 | GET /v1/videos/:request_id | covered | probe: GET /v1/videos/:request_id -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/tests/routes.rs |  | xAI videos handlers are not routed. |
| M3-0008 | POST /v1/alpha/search | covered | probe POST /v1/alpha/search -> 401; same handler as /backend-api/codex/alpha/search (crates/cpa-server/src/codex_alpha.rs routes()); crates/cpa-server/tests/codex_alpha.rs alpha_search_uses_policy_eligible_credential_and_passes_upstream_through; codex_go.json alpha_search |  |  |
| M3-0009 | POST /openai/v1/videos | covered | probe: POST /openai/v1/videos -> 401; tests: crates/cpa-server/src/observability.rs |  | OpenAI videos handlers are not routed. |
| M3-0010 | GET /openai/v1/videos/:video_id/content | partial | probe: GET /openai/v1/videos/:video_id/content -> 401; no test requests this path | openai-xai | OpenAI videos handlers are not routed. |
| M3-0011 | GET /openai/v1/videos/:video_id | partial | probe: GET /openai/v1/videos/:video_id -> 401; no test requests this path | openai-xai | OpenAI videos handlers are not routed. |
| M3-0012 | POST /backend-api/codex/alpha/search | covered | probe: POST /backend-api/codex/alpha/search -> 401; tests: crates/cpa-server/tests/codex_alpha.rs |  |  |
| M3-0013 | GET /codex/callback | covered | probe: GET /codex/callback -> 200; tests: crates/cpa-server/src/management/oauth.rs |  |  |
| M3-0014 | GET /antigravity/callback | partial | probe GET /antigravity/callback -> 200 (shared callback page) | google | Untested; there is no Antigravity login flow behind it. |
| M3-0015 | GET /callback | covered | probe: GET /callback -> 400; tests: crates/cpa-exec/src/claude_login_tests.rs, crates/cpa-exec/src/codex_oauth_tests.rs (+4) |  |  |
| M3-0016 | GET /devin/callback | covered | probe: GET /devin/callback -> 400; tests: crates/cpa-server/tests/routes.rs |  |  |

### M3: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0017 | Local OAuth listener `ANY /auth/callback` (browser flow uses GET; ServeMux registration ha… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener, pasted_callbacks_parse_like_go |  |  |
| M3-0018 | Local OAuth listener `ANY /success` (browser flow uses GET; ServeMux registration has no m… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener |  |  |
| M3-0019 | Local OAuth listener `ANY /oauth-callback` (browser flow uses GET; ServeMux registration h… | missing | no Antigravity login in Rust | google |  |
| M3-0020 | Local OAuth listener `ANY /callback` (browser flow uses GET; ServeMux registration has no … | partial | crates/cpa-exec/src/devin_auth.rs login listener; devin_tests.rs code_exchange_matches_go_fixtures; devin_auth.rs callback_page_escapes_the_error | device-providers | Listener routes are not tested case by case. |
| M3-0021 | Local OAuth listener `ANY /` (browser flow uses GET; ServeMux registration has no method r… | partial | crates/cpa-exec/src/devin_auth.rs login listener | device-providers | The catch-all 404 at / is untested. |

### M3: 2. Registered translator matrix and LOC

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0022 | Claude → Codex | covered | crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0023 | Gemini → Codex | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-codex.json |  |  |
| M3-0024 | Interactions → Codex | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-codex.json |  |  |
| M3-0025 | OpenAI → Codex | covered | crates/cpa-translate/tests/fixtures/pairs/openai-codex.json |  |  |
| M3-0026 | OpenaiResponse → Codex | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json |  |  |
| M3-0027 | Claude → Antigravity | covered | crates/cpa-translate/tests/fixtures/pairs/claude-antigravity.json |  |  |
| M3-0028 | Gemini → Antigravity | covered | crates/cpa-translate/tests/fixtures/pairs/gemini-antigravity.json |  |  |
| M3-0029 | Interactions → Antigravity | covered | crates/cpa-translate/tests/fixtures/pairs/interactions-antigravity.json |  |  |
| M3-0030 | OpenAI → Antigravity | covered | crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json |  |  |
| M3-0031 | OpenaiResponse → Antigravity | covered | crates/cpa-translate/tests/fixtures/pairs/openai-response-antigravity.json |  |  |

### M3: 3. Upstream providers, auth flows, persisted records, and special behavior

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0032 | Codex | covered | crates/cpa-exec/src/codex_oauth.rs; codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go, exchange_and_refresh_wire_and_results_match_go, device_flow_polls_through_pending_and_exchanges_like_go; crates/cpa-server/tests/management.rs codex_login_completes_through_the_main_listener_callback |  |  |
| M3-0033 | Antigravity | missing | no Antigravity login, executor or credential type in Rust (translators only) | google |  |
| M3-0034 | Gemini API keys / native Interactions keys | covered | crates/cpa-exec/src/gemini.rs; gemini scenarios (gen_*, stream_*, count_*, int_*); crates/cpa-server/tests/gemini_routes.rs |  |  |
| M3-0035 | Vertex | covered | crates/cpa-exec/src/vertex.rs, vertex_auth.rs; vertex_tests.rs go_reference_scenarios (tests/fixtures/vertex_go.json), vertex_import_matches_go |  | Go's Vertex executor has no separate Claude transport; Claude clients only get Claude input-token accounting. |
| M3-0036 | AI Studio | missing | no AI Studio WebSocket relay | google |  |
| M3-0037 | Kimi (.com and .ai) | covered | crates/cpa-exec/src/kimi_auth.rs (domain_resolution tests), kimi_tests.rs; crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0038 | xAI | covered | crates/cpa-exec/src/xai_auth.rs (xai_auth_tests.rs, xai_auth_go.json); executor crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) |  |  |
| M3-0039 | Meta | covered | crates/cpa-exec/src/meta.rs, meta_auth.rs; meta_tests.rs login_matches_go_requests_and_file, mint_errors_follow_go, execution_matches_go_byte_for_byte |  |  |
| M3-0040 | Devin | covered | crates/cpa-exec/src/devin_auth.rs (PKCE login), crates/cpa-exec/src/devin*.rs; devin_tests.rs (17 Go-fixture tests, tests/device_fixtures/devin: 46 recorded cases) |  |  |
| M3-0041 | OpenAI-compatible | covered | crates/cpa-exec/src/openai_compat.rs; compat scenarios (108); crates/cpa-server/tests/openai_compat_routes.rs |  |  |

### M3: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0042 | Codex on-disk typed core | covered | crates/cpa-exec/src/codex_oauth_tests.rs login_file_bytes_match_go_storage_serializer, existing_file_keeps_user_fields_but_never_old_tokens |  |  |
| M3-0043 | Kimi on-disk typed core | covered | crates/cpa-exec/src/kimi_auth.rs; crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0044 | xAI on-disk typed core | covered | crates/cpa-exec/src/xai_auth_tests.rs login_matches_go_manager_and_file_store |  |  |
| M3-0045 | Meta on-disk typed core | covered | crates/cpa-exec/src/meta_tests.rs file_names_and_writer_match_go, login_matches_go_requests_and_file |  |  |
| M3-0046 | Vertex on-disk typed core | covered | crates/cpa-exec/src/vertex_auth.rs import; vertex_tests.rs vertex_import_matches_go, key_file_folded_duplicates_follow_sorted_order |  |  |
| M3-0047 | Meta has a custom writer, not ordinary omitempty serialization: expires_in and dca_expires… | covered | crates/cpa-exec/src/meta_tests.rs file_names_and_writer_match_go, remint_patch_matches_go_metadata, login_merge_skips_an_existing_file_go_cannot_decode |  |  |
| M3-0048 | Antigravity on-disk metadata | missing | no Antigravity credentials | google |  |
| M3-0049 | Devin on-disk metadata | covered | crates/cpa-exec/src/devin_tests.rs auth_record_matches_go_fixtures, login_save_merges_like_go_manager |  |  |
| M3-0050 | AI Studio | partial | YAML-originated records: crates/cpa-core/src/config/credentials.rs (config_models_reach_the_registry_like_go, resolve_api_key_entry_prefers_the_matching_config_index) | google | AI Studio relay records are absent. |
| M3-0051 | Built-in legacy Gemini CLI OAuth credentials are not an additional upstream to implement: … | covered | crates/cpa-core/src/credential.rs drops type gemini-cli files like the Go file synthesizer |  |  |

### M3: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0052 | Codex WS codex.rate_limits events (and error headers) normalize primary/secondary/code-rev… | partial | crates/cpa-exec/src/codex_response.rs, codex_quota.rs; codex_tests.rs, codex_ws_tests.rs | codex | Not ported case by case. |

### M3: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0053 | Codex native fidelity: instructions, tools (including apply_patch/spawn_agent), encrypted … | partial | crates/cpa-exec/src/codex*.rs; codex_go.json executor (18), replay; codex_client vectors; codex_tokens_go.json; codex_tls_tests.rs, codex_ws_tests.rs | codex | The image tool and direct Images API are absent. |
| M3-0054 | Antigravity request sanitization includes project envelope, thinking-signature validation/… | missing | no Antigravity executor (the translators port the request-side sanitizing) | google |  |

### M3: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0055 | multimedia.disable-image-generation | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-exec/src/codex_request.rs (+1); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M3-0056 | multimedia.gpt-image-2-base-model | missing | read in crates/cpa-exec/src/codex_request.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) | openai-xai | Images handlers are not ported. |
| M3-0057 | multimedia.video-result-auth-cache-ttl | missing | read in crates/cpa-server/src/videos.rs; no test sets it | openai-xai | Videos handlers are not ported. |
| M3-0058 | oauth.providers.antigravity.signature-cache-enabled | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0059 | oauth.providers.antigravity.signature-bypass-strict | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0060 | oauth.providers.antigravity.sensitive-words | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0061 | oauth.providers.antigravity.connection-pool.enabled | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0062 | oauth.providers.antigravity.connection-pool.idle-conn-timeout | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0063 | oauth.providers.antigravity.connection-pool.max-idle-conns-per-host | missing | no antigravity executor in crates/cpa-exec | google |  |
| M3-0064 | oauth.providers.devin.sensitive-words | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/devin.rs (+3); set in crates/cpa-exec/tests/device_fixtures/devin/chat-sensitive-words.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M3-0065 | api-keys.gemini[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+16) |  | heuristic: key name match |
| M3-0066 | api-keys.gemini[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+18) |  | heuristic: key name match |
| M3-0067 | api-keys.gemini[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/discovery_cmd_go.json (+67) |  | heuristic: key name match |
| M3-0068 | api-keys.gemini[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/vertex_go.json (+2) |  | heuristic: key name match |
| M3-0069 | api-keys.gemini[].keys[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0070 | api-keys.gemini[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+3) |  | heuristic: key name match |
| M3-0071 | api-keys.gemini[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+7) |  | heuristic: key name match |
| M3-0072 | api-keys.gemini[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+15) |  | heuristic: key name match |
| M3-0073 | api-keys.gemini[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0074 | api-keys.gemini[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0075 | api-keys.gemini[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+10) |  | heuristic: key name match |
| M3-0076 | api-keys.gemini[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+32) |  | heuristic: key name match |
| M3-0077 | api-keys.interactions[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-exec/tests/fixtures/gemini_go.json (+5) |  | heuristic: key name match |
| M3-0078 | api-keys.interactions[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-exec/tests/fixtures/gemini_go.json (+5) |  | heuristic: key name match |
| M3-0079 | api-keys.interactions[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/tests/fixtures/discovery_go.json, crates/cpa-common/src/thinking/mod.rs (+33) |  | heuristic: key name match |
| M3-0080 | api-keys.interactions[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0081 | api-keys.interactions[].keys[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0082 | api-keys.interactions[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cpa-exec/tests/fixtures/gemini_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M3-0083 | api-keys.interactions[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0084 | api-keys.interactions[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+7) |  | heuristic: key name match |
| M3-0085 | api-keys.interactions[].keys[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | google | heuristic: key name match |
| M3-0086 | api-keys.interactions[].keys[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | google | heuristic: key name match |
| M3-0087 | api-keys.interactions[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+6) |  | heuristic: key name match |
| M3-0088 | api-keys.interactions[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-exec/src/devin_tests.rs (+17) |  | heuristic: key name match |
| M3-0089 | api-keys.codex[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+29) |  | heuristic: key name match |
| M3-0090 | api-keys.codex[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+29) |  | heuristic: key name match |
| M3-0091 | api-keys.codex[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-exec/src/xai_ws_tests.rs (+8) |  | heuristic: key name match |
| M3-0092 | api-keys.codex[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M3-0093 | api-keys.codex[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_cli_go.json (+101) |  | heuristic: key name match |
| M3-0094 | api-keys.codex[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/vertex_go.json (+2) |  | heuristic: key name match |
| M3-0095 | api-keys.codex[].keys[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/codex_models.rs, crates/cpa-server/tests/fixtures/codex_models_go.json (+2) |  | heuristic: key name match |
| M3-0096 | api-keys.codex[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/codex_request.rs (+6) |  | heuristic: key name match |
| M3-0097 | api-keys.codex[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/src/dispatch.rs (+1) |  | heuristic: key name match |
| M3-0098 | api-keys.codex[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+8) |  | heuristic: key name match |
| M3-0099 | api-keys.codex[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+12) |  | heuristic: key name match |
| M3-0100 | api-keys.codex[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0101 | api-keys.codex[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0102 | api-keys.codex[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+13) |  | heuristic: key name match |
| M3-0103 | api-keys.codex[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-common/src/codex_client_tests.rs (+69) |  | heuristic: key name match |
| M3-0104 | api-keys.codex[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs (+2); set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M3-0105 | api-keys.xai[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/xai_ws_tests.rs (+10) |  | heuristic: key name match |
| M3-0106 | api-keys.xai[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/xai_ws_tests.rs (+9) |  | heuristic: key name match |
| M3-0107 | api-keys.xai[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_ws_tests.rs, crates/cpa-exec/tests/fixtures/xai_ws_go.json (+2) |  | heuristic: key name match |
| M3-0108 | api-keys.xai[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M3-0109 | api-keys.xai[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_cli_go.json (+23) |  | heuristic: key name match |
| M3-0110 | api-keys.xai[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0111 | api-keys.xai[].keys[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0112 | api-keys.xai[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M3-0113 | api-keys.xai[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0114 | api-keys.xai[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0115 | api-keys.xai[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+5) |  | heuristic: key name match |
| M3-0116 | api-keys.xai[].keys[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | openai-xai | heuristic: key name match |
| M3-0117 | api-keys.xai[].keys[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | openai-xai | heuristic: key name match |
| M3-0118 | api-keys.xai[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/thinking/mod.rs (+7) |  | heuristic: key name match |
| M3-0119 | api-keys.xai[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/xai.rs (+12) |  | heuristic: key name match |
| M3-0120 | api-keys.xai[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs (+2); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M3-0121 | api-keys.meta[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+19) |  | heuristic: key name match |
| M3-0122 | api-keys.meta[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+21) |  | heuristic: key name match |
| M3-0123 | api-keys.meta[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_ws_tests.rs, crates/cpa-plugin/tests/fixtures/pluginhost_go.json (+5) |  | heuristic: key name match |
| M3-0124 | api-keys.meta[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M3-0125 | api-keys.meta[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_cli_go.json (+147) |  | heuristic: key name match |
| M3-0126 | api-keys.meta[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/tests/fixtures/vertex_go.json (+2) |  | heuristic: key name match |
| M3-0127 | api-keys.meta[].keys[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/fixtures/codex_models_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M3-0128 | api-keys.meta[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cliproxy/src/home.rs, crates/cpa-core/src/registry/dynamic.rs (+6) |  | heuristic: key name match |
| M3-0129 | api-keys.meta[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0130 | api-keys.meta[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+6) |  | heuristic: key name match |
| M3-0131 | api-keys.meta[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cliproxy/src/home.rs, crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz (+15) |  | heuristic: key name match |
| M3-0132 | api-keys.meta[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0133 | api-keys.meta[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0134 | api-keys.meta[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz (+9) |  | heuristic: key name match |
| M3-0135 | api-keys.meta[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+94) |  | heuristic: key name match |
| M3-0136 | api-keys.meta[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs (+2); set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M3-0137 | oauth.providers.xai.inject-x-search | covered | read in crates/cpa-exec/src/xai_request.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-exec/tests/fixtures/xai_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M3-0138 | oauth.providers.codex.disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs (+2); set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M3-0139 | oauth.providers.codex.stream-bootstrap-buffering | covered | read in crates/cpa-exec/src/codex_request.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+2) |  | heuristic: key name match |
| M3-0140 | oauth.providers.codex.stream-bootstrap-timeout | covered | read in crates/cpa-exec/src/codex_request.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M3-0141 | oauth.providers.codex.orphan-delegation-compatibility | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-exec/src/codex_client_tests.rs (+4) |  | heuristic: key name match |
| M3-0142 | oauth.providers.codex.response-steering | missing | read in crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/codex_duplex_tests.rs, crates/cpa-exec/src/codex_ws_tests.rs (+4) | codex | Responses steering is not ported. |
| M3-0143 | oauth.providers.codex.header-defaults.user-agent | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-exec/src/claude/detect.rs (+13); set in crates/cliproxy/src/home.rs, crates/cpa-common/src/codex_client_tests.rs (+10) |  | heuristic: key name match |
| M3-0144 | oauth.providers.codex.header-defaults.beta-features | covered | read in crates/cpa-exec/src/codex_request.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M3-0145 | api-keys.openai-compatibility[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+18) |  | heuristic: key name match |
| M3-0146 | api-keys.openai-compatibility[].disabled | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+8) |  | heuristic: key name match |
| M3-0147 | api-keys.openai-compatibility[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+14) |  | heuristic: key name match |
| M3-0148 | api-keys.openai-compatibility[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+12) |  | heuristic: key name match |
| M3-0149 | api-keys.openai-compatibility[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+18) |  | heuristic: key name match |
| M3-0150 | api-keys.openai-compatibility[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+2) |  | heuristic: key name match |
| M3-0151 | api-keys.openai-compatibility[].models[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/fixtures/codex_models_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M3-0152 | api-keys.openai-compatibility[].models[].image | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/session.rs (+38); set in crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+3) |  | heuristic: key name match |
| M3-0153 | api-keys.openai-compatibility[].models[].input-modalities | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/openai_compat_payload.rs (+1); set in crates/cpa-exec/tests/fixtures/openai_compat_go.json, crates/cpa-server/tests/fixtures/codex_models_go.json |  | heuristic: key name match |
| M3-0154 | api-keys.openai-compatibility[].models[].output-modalities | missing | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-home/src/dispatch.rs; no test sets it | openai-xai | heuristic: key name match |
| M3-0155 | api-keys.openai-compatibility[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs (+3); set in crates/cliproxy/src/home.rs, crates/cpa-core/src/registry/dynamic.rs (+3) |  | heuristic: key name match |
| M3-0156 | api-keys.openai-compatibility[].models[].use-max-completion-tokens | covered | read in crates/cpa-exec/src/openai_compat_payload.rs, crates/cpa-home/src/dispatch.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/openai_compat_payload.rs (+1) |  | heuristic: key name match |
| M3-0157 | api-keys.openai-compatibility[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0158 | api-keys.openai-compatibility[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cliproxy/src/home.rs, crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz (+4) |  | heuristic: key name match |
| M3-0159 | api-keys.openai-compatibility[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | openai-xai | heuristic: key name match |
| M3-0160 | api-keys.openai-compatibility[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | openai-xai | heuristic: key name match |
| M3-0161 | api-keys.openai-compatibility[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz (+6) |  | heuristic: key name match |
| M3-0162 | api-keys.openai-compatibility[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+11) |  | heuristic: key name match |
| M3-0163 | api-keys.openai-compatibility[].support-prompt-cache-key | covered | read in crates/cpa-exec/src/openai_compat_payload.rs, crates/cpa-server/src/config_diff.rs (+2); set in crates/cpa-exec/src/openai_compat_payload.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+1) |  | heuristic: key name match |
| M3-0164 | api-keys.vertex[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+7) |  | heuristic: key name match |
| M3-0165 | api-keys.vertex[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/main.rs, crates/cliproxy/tests/fixtures/tui_go.json (+8) |  | heuristic: key name match |
| M3-0166 | api-keys.vertex[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/vertex_tests.rs (+6) |  | heuristic: key name match |
| M3-0167 | api-keys.vertex[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/tests/fixtures/plugin_cli_go.json, crates/cliproxy/tests/fixtures/tui_go.json (+9) |  | heuristic: key name match |
| M3-0168 | api-keys.vertex[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/vertex_go.json (+2) |  | heuristic: key name match |
| M3-0169 | api-keys.vertex[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/tests/fixtures/vertex_go.json (+3) |  | heuristic: key name match |
| M3-0170 | api-keys.vertex[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+8); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/tests/fixtures/vertex_go.json (+2) |  | heuristic: key name match |
| M3-0171 | api-keys.vertex[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0172 | api-keys.vertex[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/tests/fixtures/vertex_go.json |  | heuristic: key name match |
| M3-0173 | api-keys.vertex[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+2); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/tests/fixtures/vertex_go.json (+3) |  | heuristic: key name match |

### M3: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0174 | Fallback/validation source internal/config/codex_live.go:1-106: `DefaultCodexLiveMediaMaxS… | covered | crates/cpa-server/src/realtime/media.rs, relay.rs (media_tests.rs); crates/cpa-exec/src/codex_live.rs |  |  |
| M3-0175 | Fallback/validation source internal/config/vertex_compat.go:1-130: defaults/normalization … | covered | crates/cpa-core/src/config/credentials.rs (SanitizeVertexCompatKeys port); Go goldens crates/cpa-server/tests/fixtures/server_go.json, manage_go.json |  |  |

### M3: CLI flags

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0176 | --codex-login | covered | crates/cliproxy/src/main.rs --codex-login -> codex_oauth::login (codex_oauth_tests.rs) |  |  |
| M3-0177 | --codex-device-login | covered | crates/cliproxy/src/main.rs --codex-device-login -> codex_oauth::device_login (device_flow_polls_through_pending_and_exchanges_like_go) |  |  |
| M3-0178 | --antigravity-login | partial | crates/cliproxy/src/main.rs parses -antigravity-login, then exits with 'not supported' | google | The Antigravity login itself is absent. |
| M3-0179 | --kimi-login | covered | crates/cliproxy/src/main.rs --kimi-login -> kimi_auth::login |  |  |
| M3-0180 | --kimi-ai-login | covered | crates/cliproxy/src/main.rs --kimi-ai-login -> kimi_auth::login("kimi-ai") |  |  |
| M3-0181 | --xai-login | covered | crates/cliproxy/src/main.rs --xai-login -> xai_auth::login (login_matches_go_manager_and_file_store) |  |  |
| M3-0182 | --devin-login | covered | crates/cliproxy/src/main.rs --devin-login -> cpa_exec::devin_auth::login (every_go_flag_parses_in_single_and_double_dash_form; devin_tests.rs code_exchange_matches_go_fixtures) |  |  |
| M3-0183 | --meta-login | covered | crates/cliproxy/src/main.rs --meta-login -> meta_auth::login (login_matches_go_requests_and_file) |  |  |
| M3-0184 | --vertex-import | covered | crates/cliproxy/src/main.rs calls cpa_exec::vertex_auth::import; vertex_tests.rs vertex_import_matches_go |  |  |
| M3-0185 | --vertex-import-prefix | covered | crates/cliproxy/src/main.rs passes the prefix to cpa_exec::vertex_auth::import; vertex_import_matches_go |  |  |

### M3: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M3-0186 | internal/api/server_devin_oauth_test.go | covered | crates/cpa-server/tests/management.rs devin_login_completes_through_the_main_listener_callback |  |  |
| M3-0187 | internal/api/server_kimi_oauth_test.go | covered | crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0188 | internal/auth/antigravity/auth_test.go | partial | implementation cites auth.go (crates/cpa-plugin/src/store/auth.rs); no case matched by name | google |  |
| M3-0189 | internal/auth/codex/filename_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0190 | internal/auth/codex/jwt_parser_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0191 | internal/auth/codex/openai_auth_test.go | partial | crates/cpa-exec/src/codex_oauth_tests.rs exchange_and_refresh_wire_and_results_match_go, refresh_retry_policy_matches_go_attempt_counts, concurrent_refreshes_of_one_token_share_one_exchange | codex | Not ported by name. |
| M3-0192 | internal/auth/codex/token_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs existing_file_keeps_user_fields_but_never_old_tokens |  |  |
| M3-0193 | internal/auth/devin/devin_auth_test.go | partial | crates/cpa-exec/src/devin*.rs; devin_tests.rs (17 Go-fixture tests, tests/device_fixtures/devin: 46 recorded cases) | device-providers | Not ported by name. |
| M3-0194 | internal/auth/devin/record_test.go | partial | crates/cpa-exec/src/devin_tests.rs auth_record_matches_go_fixtures | device-providers | Not ported by name. |
| M3-0195 | internal/auth/devin/user_status_test.go | partial | crates/cpa-exec/src/devin_tests.rs user_status_matches_go | device-providers | Not ported by name. |
| M3-0196 | internal/auth/kimi/kimi_proxy_test.go | partial | crates/cpa-exec/src/kimi_http.rs, crate::proxy | device-providers | Proxy cases are not ported. |
| M3-0197 | internal/auth/kimi/kimi_refresh_test.go | covered | 1/1 cases: crates/cpa-exec/src/kimi_tests.rs |  |  |
| M3-0198 | internal/auth/kimi/kimi_test.go | partial | 1/6 cases: crates/cpa-exec/src/kimi_auth.rs; not matched: TestKimiDomainResolution, TestKimiAuthCreationAndEndpoints, TestKimiCreateTokenStorageAndSave, TestRefreshToken_KimiAIEndpoint … | device-providers |  |
| M3-0199 | internal/auth/meta/meta_auth_test.go | partial | crates/cpa-exec/src/meta_tests.rs login_matches_go_requests_and_file, device_flow_errors_and_slow_down_follow_go, poll_errors_use_go_partial_decoding_and_key_folding | device-providers | Equivalents; not ported by name. |
| M3-0200 | internal/auth/xai/xai_auth_test.go | partial | crates/cpa-exec/src/xai_auth_tests.rs (9 tests from xai_auth_go.json) | openai-xai | Equivalents; not ported by name. |
| M3-0201 | internal/cache/antigravity_reasoning_replay_cache_test.go | partial | crates/cpa-translate/src/replay_cache.rs (in-process store, tombstones, purge; 4 tests) | google | Owner of internal/cache with the Antigravity executor; snapshots, CAS and Home KV are not ported. |
| M3-0202 | internal/cache/codex_reasoning_replay_cache_test.go | partial | crates/cpa-exec/src/codex_replay.rs (cache per model and Claude Code agent/session); codex_replay_tests.rs replay_scenarios_match_go (Go-generated codex_go.json replay scenarios), entries_keep_the_newest_turns | codex | Not ported by name. |
| M3-0203 | internal/cache/kimi_thinking_replay_cache_test.go | partial | crates/cpa-exec/src/kimi_replay.rs family_shares_k3_variants_only, conditional_writes_keep_newer_content | device-providers | Not ported by name. |
| M3-0204 | internal/cache/xai_reasoning_replay_cache_test.go | partial | implementation cites xai_reasoning_replay_cache.go (crates/cpa-exec/src/xai_replay.rs); no case matched by name | openai-xai |  |
| M3-0205 | internal/client/codex/apply-patch/tool_test.go | covered | 11/11 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M3-0206 | internal/client/codex/live/capabilities_test.go | partial | 3/5 cases: crates/cpa-server/tests/realtime.rs; not matched: TestHandleHangupForwardsUnauthorizedHomeResponseWithoutRefresh, TestHandleHangupReportsUnauthorizedWhenResponseReadFails | realtime |  |
| M3-0207 | internal/client/codex/live/client_secret_test.go | partial | 9/10 cases: crates/cpa-server/tests/realtime.rs; not matched: TestLiveSelectionHeadersRemoveLocalClientSecret | realtime |  |
| M3-0208 | internal/client/codex/live/live_test.go | partial | 21/25 cases: crates/cpa-server/src/realtime/calls.rs, crates/cpa-server/src/realtime/home_tests.rs (+3); not matched: TestHandlerForwardsUnauthorizedHomeResponseWithoutRefresh, TestHandlerReportsUnauthorizedBeforeEarlyReturn, TestHandleSidebandForwardsUnauthorizedHomeHandshakeWithoutRefresh, TestHandleSidebandDialErrorPreservesBodyReturnedWithReadError | realtime |  |
| M3-0209 | internal/client/codex/live/media_test.go | missing | the WebRTC media relay is not wired (ponytail in crates/cpa-server/src/realtime/http.rs) | realtime |  |
| M3-0210 | internal/client/codex/live/tcp_proxy_test.go | partial | 1/11 cases: crates/cpa-server/src/realtime/media_tests.rs; not matched: TestPrepareProxiedUpstreamAnswerRestrictsAndRewritesCandidates, TestPrepareProxiedUpstreamAnswerRejectsUnsafeTargets, TestPrepareProxiedUpstreamAnswerLimitsCandidateCount, TestReadValidatedICEBindingFrame … | realtime |  |
| M3-0211 | internal/client/codex/models/apply_patch_test.go | partial | cpa_common::codex_catalog (catalogs_match_go, validation_matches_go, versions_compare_like_go); served on GET /v1/models?client_version= by crates/cpa-server/src/codex_models.rs (tests/codex_models.rs client_version_selects_the_codex_catalog) | codex | Not ported by name. |
| M3-0212 | internal/client/codex/models/models_test.go | partial | cpa_common::codex_catalog (catalogs_match_go, validation_matches_go, versions_compare_like_go); served on GET /v1/models?client_version= by crates/cpa-server/src/codex_models.rs (tests/codex_models.rs client_version_selects_the_codex_catalog) | codex | 31 Go cases; not ported by name. |
| M3-0213 | internal/client/codex/models/web_search_capability_test.go | partial | cpa_common::codex_catalog (catalogs_match_go, validation_matches_go, versions_compare_like_go); served on GET /v1/models?client_version= by crates/cpa-server/src/codex_models.rs (tests/codex_models.rs client_version_selects_the_codex_catalog) | codex | Not ported by name. |
| M3-0214 | internal/client/codex/optimize-multi-agent-v2/optimize_multi_agent_v2_test.go | partial | cpa_common::codex_client multi-agent v2 rewrites; crates/cpa-common/tests/fixtures/codex_client_go.json (52 Go vectors); crates/cpa-exec/src/codex_client_tests.rs replays_go_translation_and_optimization | codex | 35 Go cases; not ported by name. |
| M3-0215 | internal/client/codex/optimize-multi-agent-v2/orphan_delegation_test.go | partial | cpa_common::codex_client orphan delegation (oauth.providers.codex.orphan-delegation-compatibility); crates/cpa-exec/src/codex_client_tests.rs orphan_delegation_written_oauth_only_skips_api_keys; codex_client_go.json vectors | codex | Not ported by name. |
| M3-0216 | internal/client/codex/tool-schema/tool_schema_test.go | partial | cpa_common::payload normalize_codex_tool_integer_types | codex | Not ported by name. |
| M3-0217 | internal/config/codex_live_test.go | partial | crates/cpa-core/src/config/validate.rs (codex live bounds) | realtime | Validation only; the live relay is absent. |
| M3-0218 | internal/config/config_meta_test.go | partial | crates/cpa-core/src/config.rs | manage | Not ported by name. |
| M3-0219 | internal/config/xai_alpha_search_test.go | partial | crates/cpa-core/src/config/credentials.rs (xAI alpha-search); xai_go.json | openai-xai | Not ported by name. |
| M3-0220 | internal/config/xai_api_key_test.go | partial | crates/cpa-core/src/config/credentials.rs (xAI keys); xai_go.json config_key_base_url_and_headers | openai-xai | Not ported by name. |
| M3-0221 | internal/logging/requestmeta_test.go | missing | no request metadata logging | server |  |
| M3-0222 | internal/misc/antigravity_version_test.go | missing | 0/9 cases matched; antigravity_version.go not cited | google |  |
| M3-0223 | internal/registry/codex_client_models_test.go | covered | crates/cpa-exec/src/codex_catalog_updater.rs (refresh_falls_through_sources_and_validates); started by crates/cpa-server/src/model_updater.rs |  |  |
| M3-0224 | internal/registry/devin_models_test.go | partial | implementation cites devin_models.go (crates/cpa-core/src/registry/devin.rs, crates/cpa-exec/src/devin_models.rs); no case matched by name | device-providers |  |
| M3-0225 | internal/runtime/executor/aistudio_executor_test.go | partial | implementation cites aistudio_executor.go (crates/cpa-exec/src/aistudio.rs); no case matched by name | google |  |
| M3-0226 | internal/runtime/executor/antigravity_executor_buildrequest_test.go | missing | 0/14 cases matched; antigravity_executor_buildrequest.go not cited | google |  |
| M3-0227 | internal/runtime/executor/antigravity_executor_compaction_test.go | missing | 0/5 cases matched; antigravity_executor_compaction.go not cited | google |  |
| M3-0228 | internal/runtime/executor/antigravity_executor_credits_test.go | missing | 0/15 cases matched; antigravity_executor_credits.go not cited | google |  |
| M3-0229 | internal/runtime/executor/antigravity_executor_disable_cooling_test.go | missing | 0/5 cases matched; antigravity_executor_disable_cooling.go not cited | google |  |
| M3-0230 | internal/runtime/executor/antigravity_executor_finish_reason_test.go | missing | 0/4 cases matched; antigravity_executor_finish_reason.go not cited | google |  |
| M3-0231 | internal/runtime/executor/antigravity_executor_interactions_test.go | missing | 0/1 cases matched; antigravity_executor_interactions.go not cited | google |  |
| M3-0232 | internal/runtime/executor/antigravity_executor_keepalive_test.go | missing | 0/6 cases matched; antigravity_executor_keepalive.go not cited | google |  |
| M3-0233 | internal/runtime/executor/antigravity_executor_signature_test.go | missing | 0/24 cases matched; antigravity_executor_signature.go not cited | google |  |
| M3-0234 | internal/runtime/executor/antigravity_executor_split_usage_test.go | missing | 0/2 cases matched; antigravity_executor_split_usage.go not cited | google |  |
| M3-0235 | internal/runtime/executor/antigravity_executor_transport_test.go | missing | 0/22 cases matched; antigravity_executor_transport.go not cited | google |  |
| M3-0236 | internal/runtime/executor/antigravity_preupstream_rewrite_differential_test.go | missing | 0/7 cases matched; antigravity_preupstream_rewrite_differential.go not cited | google |  |
| M3-0237 | internal/runtime/executor/antigravity_reasoning_replay_clear_test.go | missing | 0/1 cases matched; antigravity_reasoning_replay_clear.go not cited | google |  |
| M3-0238 | internal/runtime/executor/antigravity_reasoning_replay_index_test.go | missing | 0/19 cases matched; antigravity_reasoning_replay_index.go not cited | google |  |
| M3-0239 | internal/runtime/executor/antigravity_reasoning_replay_test.go | missing | 0/70 cases matched; antigravity_reasoning_replay.go not cited | google |  |
| M3-0240 | internal/runtime/executor/antigravity_refresh_issue6199_test.go | missing | 0/9 cases matched; antigravity_refresh_issue6199.go not cited | google |  |
| M3-0241 | internal/runtime/executor/antigravity_refresh_test.go | missing | 0/2 cases matched; antigravity_refresh.go not cited | google |  |
| M3-0242 | internal/runtime/executor/antigravity_schema_sanitize_test.go | missing | 0/18 cases matched; antigravity_schema_sanitize.go not cited | google |  |
| M3-0243 | internal/runtime/executor/codex_executor_auth_test.go | partial | crates/cpa-exec/src/codex_oauth_tests.rs refresh_patch_matches_go_executor_refresh | codex | Not ported by name. |
| M3-0244 | internal/runtime/executor/codex_executor_cache_test.go | partial | codex_go.json executor oauth_execution_session_prompt_cache, apikey_payload_rules_and_session_headers | codex | Not ported by name. |
| M3-0245 | internal/runtime/executor/codex_executor_compact_test.go | covered | codex_go.json executor oauth_compact |  |  |
| M3-0246 | internal/runtime/executor/codex_executor_grokbuild_keepalive_test.go | missing | no Grok Build keepalive in the Codex executor | codex |  |
| M3-0247 | internal/runtime/executor/codex_executor_imagegen_test.go | partial | image_generation handling in crates/cpa-exec/src/codex_request.rs | codex | 16 Go cases; not ported by name. |
| M3-0248 | internal/runtime/executor/codex_executor_input_ids_test.go | partial | crates/cpa-exec/src/codex_request.rs input IDs | codex | Not ported by name. |
| M3-0249 | internal/runtime/executor/codex_executor_instructions_test.go | partial | codex_go.json executor oauth_nonstream_no_instructions_free_plan | codex | Not ported by name. |
| M3-0250 | internal/runtime/executor/codex_executor_parallel_tool_calls_test.go | partial | crates/cpa-exec/src/codex_request.rs parallel_tool_calls | codex | Not ported by name. |
| M3-0251 | internal/runtime/executor/codex_executor_reasoning_replay_cache_test.go | partial | crates/cpa-exec/src/codex_replay.rs; codex_replay_tests.rs replay_scenarios_match_go (Go-generated end-to-end scenarios) | codex | 24 Go cases; not ported by name. |
| M3-0252 | internal/runtime/executor/codex_executor_retry_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover; crates/cpa-exec/src/codex_tests.rs (invalid_grant) | codex | Not ported by name. |
| M3-0253 | internal/runtime/executor/codex_executor_routing_hint_test.go | partial | crates/cpa-exec/src/codex_request.rs routing hint | codex | Not ported by name. |
| M3-0254 | internal/runtime/executor/codex_executor_signature_test.go | partial | crates/cpa-exec/src/codex_request.rs reasoning sanitizing | codex | Not ported by name. |
| M3-0255 | internal/runtime/executor/codex_executor_spawn_agent_test.go | partial | cpa_common::codex_client spawn_agent/send_message/followup_task rewrites; crates/cpa-common/tests/fixtures/codex_client_go.json (52 Go vectors), crates/cpa-exec/tests/fixtures/codex_client_translate_go.json | codex | Not ported by name. |
| M3-0256 | internal/runtime/executor/codex_executor_stream_alloc_test.go | covered | n/a: Go allocation count test |  |  |
| M3-0257 | internal/runtime/executor/codex_executor_stream_output_test.go | partial | codex_go.json executor oauth_stream_native, oauth_stream_terminal_failed_in_stream, oauth_stream_truncated, oauth_capacity_in_stream_500 | codex | 21 Go cases; not ported by name. |
| M3-0258 | internal/runtime/executor/codex_executor_tokens_test.go | covered | crates/cpa-exec/src/codex_tokens.rs; codex_tokens_tests.rs count_tokens_matches_go, encodings_follow_go_model_prefixes (codex_tokens_go.json) |  |  |
| M3-0259 | internal/runtime/executor/codex_executor_tool_schema_test.go | partial | codex_go.json executor oauth_tool_schema_enum_collapse | codex | Not ported by name. |
| M3-0260 | internal/runtime/executor/codex_executor_translate_test.go | partial | crates/cpa-exec/src/codex.rs via cpa_translate | codex | Not ported by name. |
| M3-0261 | internal/runtime/executor/codex_native_fidelity_test.go | partial | crates/cpa-exec/src/codex_tls_tests.rs chatgpt_clienthello_matches_go_chrome_profile; codex_go.json executor | codex | Not ported by name. |
| M3-0262 | internal/runtime/executor/codex_openai_images_extract_test.go | missing | no Codex Images API | codex |  |
| M3-0263 | internal/runtime/executor/codex_openai_images_test.go | missing | no Codex Images API | codex |  |
| M3-0264 | internal/runtime/executor/codex_per_credential_cloaking_issue6034_test.go | partial | disable-codex-cloaking per credential (crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs); codex_go.json apikey_stream_cloak_disabled_headers | codex | Not ported by name. |
| M3-0265 | internal/runtime/executor/codex_response_model_test.go | partial | crates/cpa-server/src/dispatch.rs response model rewrite | codex | Not ported by name. |
| M3-0266 | internal/runtime/executor/codex_stream_bootstrap_buffering_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover, oauth_bootstrap_time_budget_spent, oauth_bootstrap_holds_then_releases | codex | 49 Go cases; not ported by name. |
| M3-0267 | internal/runtime/executor/devin_executor_test.go | partial | implementation cites devin_executor.go (crates/cpa-exec/src/devin.rs, crates/cpa-exec/src/devin_request.rs (+1)); no case matched by name | device-providers |  |
| M3-0268 | internal/runtime/executor/gemini_vertex_executor_test.go | partial | implementation cites gemini_vertex_executor.go (crates/cpa-exec/src/vertex.rs); no case matched by name | google |  |
| M3-0269 | internal/runtime/executor/helps/antigravity_compaction_test.go | missing | 0/7 cases matched; antigravity_compaction.go not cited | google |  |
| M3-0270 | internal/runtime/executor/helps/antigravity_grounding_urls_test.go | missing | 0/1 cases matched; antigravity_grounding_urls.go not cited | google |  |
| M3-0271 | internal/runtime/executor/helps/codex_input_ids_test.go | partial | crates/cpa-exec/src/codex_request.rs input IDs | codex | Not ported by name. |
| M3-0272 | internal/runtime/executor/helps/codex_multi_agent_v2_summary_test.go | partial | cpa_common::codex_client | codex | Not ported by name. |
| M3-0273 | internal/runtime/executor/helps/codex_multi_agent_v2_test.go | partial | cpa_common::codex_client | codex | Not ported by name. |
| M3-0274 | internal/runtime/executor/helps/codex_quota_test.go | partial | crates/cpa-exec/src/codex_quota.rs; codex_go.json quota | codex | 15 Go cases; not ported by name. |
| M3-0275 | internal/runtime/executor/helps/codex_terminal_incomplete_test.go | partial | codex_go.json executor oauth_nonstream_empty_incomplete | codex | Not ported by name. |
| M3-0276 | internal/runtime/executor/helps/codex_tool_schema_batch_test.go | partial | cpa_common::payload | codex | Not ported by name. |
| M3-0277 | internal/runtime/executor/helps/codex_tool_schema_test.go | partial | cpa_common::payload normalize_codex_tool_integer_types; crates/cpa-common/tests/payload.rs | codex | 22 Go cases; not ported by name. |
| M3-0278 | internal/runtime/executor/helps/devin_models_test.go | partial | implementation cites devin_models.go (crates/cpa-core/src/registry/devin.rs, crates/cpa-exec/src/devin_models.rs); no case matched by name | device-providers |  |
| M3-0279 | internal/runtime/executor/helps/devin_wire_test.go | partial | implementation cites devin_wire.go (crates/cpa-exec/src/devin_wire.rs); no case matched by name | device-providers |  |
| M3-0280 | internal/runtime/executor/helps/kimi_responses_test.go | partial | 1/4 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestResolveKimiChatURL, TestResolveKimiClaudeBaseURL, TestNormalizeKimiResponsesInput | device-providers |  |
| M3-0281 | internal/runtime/executor/helps/meta_tools_test.go | partial | crates/cpa-exec/src/meta_wire.rs | device-providers | Not ported by name. |
| M3-0282 | internal/runtime/executor/helps/payload_helpers_codex_integer_test.go | partial | crates/cpa-common/tests/payload.rs (payload_go.json) | server | Not ported by name. |
| M3-0283 | internal/runtime/executor/helps/vertex_payload_helpers_test.go | missing | 0/2 cases matched; vertex_payload_helpers.go not cited | google |  |
| M3-0284 | internal/runtime/executor/kimi_executor_test.go | partial | 1/41 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestNewKimiExecutorInitializesDelegatedClaudeConfig, TestKimiExecutorRequestToFormatMatchesWireProtocol, TestKimiExecutorResponsesPassthrough, TestKimiExecutorResponsesStreamPassthrough … | device-providers |  |
| M3-0285 | internal/runtime/executor/kimi_thinking_replay_test.go | partial | crates/cpa-exec/src/kimi_replay.rs; device fixtures (crates/cpa-exec/tests/device_fixtures/kimi) | device-providers | Not ported by name. |
| M3-0286 | internal/runtime/executor/meta_executor_test.go | partial | 3/27 cases: crates/cpa-exec/src/meta_tests.rs; not matched: TestMetaExecutor_Identifier, TestMetaExecutor_ExecuteSuccessAndRateLimit, TestMetaExecutor_Refresh_RequiresManagerAcceptance, TestMetaExecutor_PrepareConcurrentAccounts … | device-providers |  |
| M3-0287 | internal/runtime/executor/vertex_proxy_token_test.go | missing | 0/1 cases matched; vertex_proxy_token.go not cited | google |  |
| M3-0288 | internal/runtime/executor/xai_client_version_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | openai-xai | Not ported by name. |
| M3-0289 | internal/runtime/executor/xai_configuration_update_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | openai-xai | Not ported by name; the configuration-update intent is a ponytail in xai_request.rs. |
| M3-0290 | internal/runtime/executor/xai_executor_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | openai-xai | 146 Go cases; not ported by name. The apply_patch path still uses the seam in xai_apply_patch.rs (cpa_translate::apply_patch_responses is now on master). |
| M3-0291 | internal/runtime/executor/xai_status_err_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | openai-xai | Not ported by name. |
| M3-0292 | internal/signature/kimi_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/13 cases also cited by name) |  |  |
| M3-0293 | internal/thinking/apply_codex_usage_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/5 cases also cited by name) |  |  |
| M3-0294 | internal/thinking/kimi_max_clamp_repro_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M3-0295 | internal/translator/antigravity/claude/antigravity_claude_request_test.go | partial | 93/110 cases: crates/cpa-translate/tests/fixtures/pairs/claude-antigravity.json; not matched: TestValidateBypassMode_AcceptsClaudeSingleAndDoubleLayer, TestValidateBypassMode_RejectsGeminiSignature, TestValidateBypassMode_RejectsMissingSignature, TestValidateBypassMode_RejectsNonREPrefix … | translate |  |
| M3-0296 | internal/translator/antigravity/claude/antigravity_claude_response_test.go | partial | 37/39 cases: crates/cpa-translate/tests/fixtures/go_helpers.json, crates/cpa-translate/tests/fixtures/pairs/claude-antigravity.json; not matched: TestWebSearchResultsFromGrounding_DeduplicatesAndSkipsEmptyURLs, TestBuildWebSearchCitedTextBlocks_TrimsOverlappingGroundingSupports | translate |  |
| M3-0297 | internal/translator/antigravity/claude/signature_validation_test.go | partial | crates/cpa-translate/src/antigravity_claude.rs; claude-antigravity goldens (bypass modes, signature carriers) | translate | Unit cases not ported. |
| M3-0298 | internal/translator/antigravity/gemini/antigravity_gemini_request_test.go | partial | 24/30 cases: crates/cpa-translate/tests/fixtures/go_helpers.json, crates/cpa-translate/tests/fixtures/pairs/gemini-antigravity.json; not matched: TestSanitizeAntigravityClaudeGeminiRequestSignatures_PreservesNumberPrecision, TestSanitizeAntigravityClaudeGeminiRequestSignatures_StripsFunctionCallSignatureForClaudeModel, TestSanitizeAntigravityClaudeGeminiRequestSignatures_StrictTypeChecks, TestSanitizeAntigravityClaudeGeminiRequestSignatures_StripsDuplicateSignatureKeys … | translate |  |
| M3-0299 | internal/translator/antigravity/gemini/antigravity_gemini_response_test.go | covered | 10/10 cases: crates/cpa-translate/tests/fixtures/go_helpers.json, crates/cpa-translate/tests/fixtures/pairs/gemini-antigravity.json |  |  |
| M3-0300 | internal/translator/antigravity/gemini/noop_optimization_test.go | partial | 6/7 cases: crates/cpa-translate/tests/fixtures/go_helpers.json; not matched: TestConvertGeminiRequestToAntigravityBoundsLargePayloadCopies | translate | The unmatched case bounds Go allocations for a 4 MiB payload; it has no Rust counterpart. Three matched cases also assert that the input buffer is returned without a copy, which the replay does not check (it compares returned bytes only). |
| M3-0301 | internal/translator/antigravity/interactions/interactions_antigravity_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-antigravity.json |  |  |
| M3-0302 | internal/translator/antigravity/interactions/interactions_antigravity_test.go | covered | 21/21 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-antigravity.json |  |  |
| M3-0303 | internal/translator/antigravity/interactions/noop_optimization_test.go | partial | 2/2 cases: crates/cpa-translate/tests/fixtures/go_helpers.json | translate | Byte results of both cases are replayed; TestRewriteInteractionsFunctionNamesReusesNormalizedPayload also asserts that the input buffer is returned without a copy, which the replay does not check (it compares returned bytes only). |
| M3-0304 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json |  |  |
| M3-0305 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_request_test.go | covered | 24/24 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json |  |  |
| M3-0306 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_response_test.go | covered | 13/13 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json |  |  |
| M3-0307 | internal/translator/antigravity/openai/chat-completions/noop_optimization_test.go | partial | 1/1 cases: crates/cpa-translate/tests/fixtures/go_helpers.json | translate | The byte result is replayed; the case also asserts that the input buffer is returned without a copy, which the replay does not check (it compares returned bytes only). |
| M3-0308 | internal/translator/antigravity/openai/responses/antigravity_openai-responses_request_test.go | partial | 38/39 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-antigravity.json; not matched: TestConvertOpenAIResponsesRequestEnvelopeToAntigravity | translate |  |
| M3-0309 | internal/translator/antigravity/openai/responses/antigravity_openai-responses_response_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-antigravity.json |  |  |
| M3-0310 | internal/translator/codex/claude/codex_claude_compat_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0311 | internal/translator/codex/claude/codex_claude_parallel_function_calls_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0312 | internal/translator/codex/claude/codex_claude_request_test.go | covered | 22/22 cases: crates/cpa-translate/tests/fixtures/go_helpers.json, crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0313 | internal/translator/codex/claude/codex_claude_response_test.go | partial | 34/35 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json; not matched: TestExtractResponsesUsage | translate |  |
| M3-0314 | internal/translator/codex/claude/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0315 | internal/translator/codex/gemini/codex_gemini_request_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-codex.json |  |  |
| M3-0316 | internal/translator/codex/gemini/codex_gemini_response_test.go | covered | 7/7 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-codex.json |  |  |
| M3-0317 | internal/translator/codex/gemini/noop_optimization_test.go | partial | 2/3 cases: crates/cpa-translate/tests/fixtures/go_helpers.json; not matched: TestSetCodexToolChoiceFromGeminiToolConfigReusesAutoChoice | translate | The unmatched case asserts Go slice reuse (setCodexToolChoiceFromGeminiToolConfig keeps the same backing array); its value result is in the pair goldens. |
| M3-0318 | internal/translator/codex/interactions/interactions_codex_test.go | covered | 10/10 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-codex.json |  |  |
| M3-0319 | internal/translator/codex/interactions/noop_optimization_test.go | partial | 2/2 cases: crates/cpa-translate/tests/fixtures/go_helpers.json | translate | Byte results of both cases are replayed; TestSetInteractionsCodexRawIfDifferentReusesMatchingValue also asserts that the input buffer is returned without a copy, which the replay does not check (it compares returned bytes only). |
| M3-0320 | internal/translator/codex/openai/chat-completions/codex_openai_request_test.go | covered | 33/33 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json |  |  |
| M3-0321 | internal/translator/codex/openai/chat-completions/codex_openai_response_test.go | covered | 31/31 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json |  |  |
| M3-0322 | internal/translator/codex/openai/chat-completions/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json |  |  |
| M3-0323 | internal/translator/codex/openai/responses/codex_openai-responses_request_test.go | covered | 22/22 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json |  |  |
| M3-0324 | internal/translator/codex/openai/responses/codex_openai-responses_response_test.go | covered | 3/3 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json |  |  |
| M3-0325 | internal/translator/common/antigravity_tools_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M3-0326 | internal/translator/common/devin_tools_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/go_helpers.json |  |  |
| M3-0327 | sdk/api/handlers/handlers_metadata_test.go | partial | crates/cpa-server/src/session.rs, dispatch.rs (execution metadata) | server | 15 Go cases; not ported by name. |
| M3-0328 | sdk/api/handlers/openai/codex_client_models_test.go | partial | cpa_common::codex_catalog (catalogs_match_go, validation_matches_go, versions_compare_like_go); served on GET /v1/models?client_version= by crates/cpa-server/src/codex_models.rs (tests/codex_models.rs client_version_selects_the_codex_catalog) | codex | Not ported by name. |
| M3-0329 | sdk/auth/antigravity_headless_test.go | missing | 0/9 cases matched; antigravity_headless.go not cited | google |  |
| M3-0330 | sdk/auth/codex_auth_record_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs login_file_bytes_match_go_storage_serializer |  |  |
| M3-0331 | sdk/auth/devin_test.go | partial | implementation cites devin.go (crates/cpa-exec/src/devin_auth.rs); no case matched by name | device-providers |  |
| M3-0332 | sdk/auth/meta_test.go | partial | implementation cites meta.go (crates/cpa-exec/src/meta_auth.rs); no case matched by name | device-providers |  |
| M3-0333 | sdk/auth/xai_test.go | partial | implementation cites xai.go (crates/cpa-exec/src/xai_auth.rs); no case matched by name | openai-xai |  |
| M3-0334 | sdk/cliproxy/antigravity_models_dedup_test.go | missing | 0/9 cases matched; antigravity_models_dedup.go not cited | google |  |
| M3-0335 | sdk/cliproxy/antigravity_models_timeout_test.go | missing | 0/10 cases matched; antigravity_models_timeout.go not cited | google |  |
| M3-0336 | sdk/cliproxy/auth/antigravity_credits_test.go | missing | 0/6 cases matched; antigravity_credits.go not cited | google |  |
| M3-0337 | sdk/cliproxy/auth/codex_forcemap_ws_forward_test.go | partial | crates/cpa-server/src/websocket.rs force-mapping forwarding | codex | Not ported by name. |
| M3-0338 | sdk/cliproxy/auth/codex_model_not_found_cooldown_test.go | partial | crates/cpa-server/src/classify.rs, crates/cpa-exec/src/codex_response.rs (model not found) | server | Not ported by name. |
| M3-0339 | sdk/cliproxy/auth/meta_refresh_test.go | covered | crates/cpa-exec/src/meta_tests.rs prepare_remints_and_matches_go_refresh |  |  |
| M3-0340 | sdk/cliproxy/auth/metadata_keys_test.go | partial | crates/cpa-core/src/credential.rs | server | Not ported by name. |
| M3-0341 | sdk/cliproxy/auth/metadata_merge_test.go | partial | crates/cpa-core/src/credential.rs MetadataPatch | server | Not ported by name. |
| M3-0342 | sdk/cliproxy/auth/response_model_rewriter_antigravity_sim_test.go | missing | 0/3 cases matched; response_model_rewriter_antigravity_sim.go not cited | google |  |
| M3-0343 | sdk/cliproxy/auth/selected_auth_metadata_test.go | partial | crates/cpa-server/src/dispatch.rs | server | Not ported by name. |
| M3-0344 | sdk/cliproxy/auth/selector_antigravity_subagent_test.go | missing | 0/8 cases matched; selector_antigravity_subagent.go not cited | google |  |
| M3-0345 | sdk/cliproxy/auth/session_affinity_metadata_test.go | partial | crates/cpa-server/src/affinity.rs (3 tests), server_go.json affinity | server | Not ported by name. |
| M3-0346 | sdk/cliproxy/service_codex_executor_binding_test.go | partial | crates/cpa-exec/src/lib.rs Executors binding | codex | Not ported by name. |
| M3-0347 | sdk/cliproxy/service_codex_models_test.go | partial | cpa_common::codex_catalog (catalogs_match_go, validation_matches_go, versions_compare_like_go); served on GET /v1/models?client_version= by crates/cpa-server/src/codex_models.rs (tests/codex_models.rs client_version_selects_the_codex_catalog) | codex | Not ported by name; the catalog updater is not started. |
| M3-0348 | test/codex_incomplete_stream_error_type_test.go | partial | crates/cpa-server/src/openai.rs; codex_go.json oauth_stream_truncated | codex | Not ported by name. |
| M3-0349 | test/codex_quota_failover_test.go | partial | crates/cpa-server/tests/routes.rs failover_stop_rules_and_cooldown_contracts; codex_go.json oauth_usage_limit_429 | server | Not ported by name. |
| M3-0350 | test/codex_stream_disconnect_failover_test.go | partial | crates/cpa-server/tests/routes.rs empty_stream_fails_over_and_reports_empty_stream, bootstrap retries | server | Not ported by name. |

## M4

### M4: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0001 | Optional separate pprof listener `ANY /debug/pprof/` (ServeMux imposes no method constrain… | missing | no pprof listener yet | observe | To be implemented for real (listener, index, cmdline, symbol). |
| M4-0002 | Optional separate pprof listener `ANY /debug/pprof/cmdline` (ServeMux imposes no method co… | missing | no pprof listener yet | observe | To be implemented for real (listener, index, cmdline, symbol). |
| M4-0003 | Optional separate pprof listener `ANY /debug/pprof/profile` (ServeMux imposes no method co… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: the CPU profile sits behind a non-default cargo feature. |
| M4-0004 | Optional separate pprof listener `ANY /debug/pprof/symbol` (ServeMux imposes no method con… | missing | no pprof listener yet | observe | To be implemented for real (listener, index, cmdline, symbol). |
| M4-0005 | Optional separate pprof listener `ANY /debug/pprof/trace` (ServeMux imposes no method cons… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0006 | Optional separate pprof listener `ANY /debug/pprof/allocs` (ServeMux imposes no method con… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0007 | Optional separate pprof listener `ANY /debug/pprof/block` (ServeMux imposes no method cons… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0008 | Optional separate pprof listener `ANY /debug/pprof/goroutine` (ServeMux imposes no method … | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0009 | Optional separate pprof listener `ANY /debug/pprof/heap` (ServeMux imposes no method const… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0010 | Optional separate pprof listener `ANY /debug/pprof/mutex` (ServeMux imposes no method cons… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |
| M4-0011 | Optional separate pprof listener `ANY /debug/pprof/threadcreate` (ServeMux imposes no meth… | missing | no pprof listener yet | observe | Planned deliberate difference once the pprof listener ships: Go-runtime profiles answer 501. |

### M4: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0012 | Generic auth JSON is open-ended: `type`, `disabled`, `proxy_url`, `prefix`, `email`, `note… | covered | crates/cpa-core/src/credential.rs keeps open metadata; runtime.rs patch_persists_atomically_preserves_unknown_fields_and_rejects_stale; manage_go.json credentials and synth |  |  |
| M4-0013 | Exact shared metadata spelling normalization: api-key → api_key, base-url → base_url, disa… | partial | crates/cpa-core/src/config/credentials.rs, crates/cpa-server/src/management/auth_files.rs | manage | Spelling normalization is not tested case by case. |
| M4-0014 | Shared auth override value shapes: disabled boolean; proxy_url/prefix/email/note/base_url/… | partial | crates/cpa-core/src/credential.rs; manage_go.json credentials | manage | Not tested case by case. |
| M4-0015 | Storage merge contract: ordinary typed writers marshal their JSON tags then flatten Metada… | partial | runtime.rs first_use_persists_disabled_in_go_marshal_form, patch_persists_atomically_preserves_unknown_fields_and_rejects_stale; provider file writers (codex_oauth_tests, meta_tests, xai_auth_tests, devin_tests) | server | FileStore equal-rewrite skipping and creation intent are not tested. |

### M4: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0016 | Passive quota snapshots are provider-scoped, replacement rather than merge: Claude Anthrop… | partial | Codex quota observations (crates/cpa-exec/src/codex_quota.rs, management delivery 7); Claude quota headers in crates/cpa-exec/src/quota.rs | server | The 64-header/512-byte snapshot bounds and replacement semantics are not tested. |
| M4-0017 | Claude and Codex model-level-cooling defaults false; credential-wide quota propagation mus… | partial | crates/cpa-exec/src/quota.rs model_shared_and_fast_entitlement_scopes; model-level-cooling read in claude/settings.rs and codex_response.rs; scheduler cooldown_deadlines_floor_backoff_and_sibling_success | server | Antigravity credits fallback is absent (no Antigravity executor). |

### M4: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0018 | Round-robin maintains previous selected identity per provider/model, not monotonically ind… | covered | crates/cpa-server/src/scheduler.rs rotation_tracks_previous_identity_and_provider_model_keys, smooth_weights_keep_signed_credits_across_temporary_exclusions |  |  |
| M4-0019 | Highest available priority tier wins cold selection; priority default 0. Integer weight om… | partial | scheduler.rs normalization_matches_go_runtime_not_example_config, smooth weights; runtime.rs attribute_only_reload_changes_priority_and_rejects_stale_outcome | server | YAML weight rejection (float, string, overflow) is not tested. |
| M4-0020 | Established session binding outranks recovered higher-priority credentials; fail over only… | covered | scheduler.rs affinity_beats_recovered_priority_until_unusable_or_expired, session_affinity_matches_go_selector; server_go.json affinity |  |  |
| M4-0021 | Session identity precedence: explicit Claude Code/Codex/OpenCode/pi headers; prompt_cache_… | covered | crates/cpa-server/src/session.rs identity_matches_go (server_go.json session, 53 vectors); cpa_common::session |  |  |
| M4-0022 | Retry rounds: initial round 0 then request-retry additional rounds; each credential admitt… | covered | crates/cpa-server/tests/scheduler_attempts.rs cap_is_per_round_and_skipped_credentials_age_by_round; runtime.rs retry_wait_obeys_exact_cap_and_attempted_quota_floor; routes.rs cooling_selection_waits_for_the_next_retry_round |  |  |
| M4-0023 | Do not conflate classification layers: configured status+body substring/regex rules choose… | covered | crates/cpa-server/src/classify.rs request_faults_follow_go_status_and_body_rules; scheduler.rs rules_are_ordered_status_and_body_matches_with_canonical_precedence; scheduler_attempts.rs stop_continue_and_force_cooldown_are_independent |  |  |
| M4-0024 | Default cooldown status policy includes 401/402/403 30m, unsupported 404 12h absent Retry-… | covered | scheduler.rs cooldowns_match_go_mark_result (server_go.json cooldown, 25 sequences), cooldown_deadlines_floor_backoff_and_sibling_success |  |  |
| M4-0025 | Transport/lifecycle failures and request-scoped faults must not poison credential quota; c… | covered | scheduler_attempts.rs transport_and_precommit_faults_fail_over_without_poisoning, postcommit_stream_fault_is_terminal_and_never_replayed |  |  |
| M4-0026 | Cooling override precedence: credential metadata/attributes, provider settings, global; v8… | partial | crates/cpa-server/src/cooldown_store.rs (server_go.json cooldown_files); scheduler.rs forced_cooling_uses_fallback_and_quota_does_not_reuse_transient_state | server | Override precedence across credential, provider and global is not tested case by case. |
| M4-0027 | OAuth refresh scheduler has one replaceable loop, 5s check interval, default 16 workers, p… | partial | crates/cpa-server/src/refresh.rs pending_failure_ineffective_rotation_and_invalid_grant_backoffs; runtime.rs auth-auto-refresh-workers default 16 | server | Loop interval and lifecycle (disabled, removal) cases are not tested. |
| M4-0028 | Model matching honors per-key prefixes, force-model-prefix exceptions, OAuth aliases vs AP… | covered | crates/cpa-core/src/registry/dynamic.rs exclusions_aliases_forks_and_prefixes_follow_go, config_models_and_force_mapping; dispatch.rs force_mapping_rewrites_json_and_sse_model_fields; routes.rs config_models_alias_and_force_mapping_reach_upstream_and_client |  |  |
| M4-0029 | Proxy priority: execution/request override, credential proxy, global proxy, injected conte… | covered | crates/cpa-exec/src/proxy.rs effective_proxy_precedence, proxy_parsing_scheme_mapping_and_redaction, environment proxy tests; tests/fixtures/proxy_go.json (source, redirects, lines) |  |  |
| M4-0030 | Custom header map supports literal values and $Header references copied from downstream; m… | partial | cpa_common::headers resolves_literals_references_and_session; crates/cpa-common/tests/payload.rs custom_headers_match_go | server | requests.passthrough-headers (response header passthrough allowlist) is not read. |
| M4-0031 | Payload rules operate on final provider payload after protocol translation, using default/… | covered | crates/cpa-common/tests/payload.rs payload_rules_match_go (payload_go.json, 2037 config cases) |  |  |

### M4: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0032 | client.codex.optimize-multi-agent-v2 | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-server/src/codex_models.rs (+1); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-exec/src/codex_client_tests.rs (+4) |  | heuristic: key name match |
| M4-0033 | client.codex.enable-apply-patch | covered | read in crates/cpa-server/src/codex_models.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0034 | requests.proxy-url | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+19); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+11) |  | heuristic: key name match |
| M4-0035 | routing.force-model-prefix | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-core/src/config.rs (+3); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/registry/dynamic.rs (+4) |  | heuristic: key name match |
| M4-0036 | observability.logs.request-log | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-home/src/client.rs (+5); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-home/tests/fixtures/go_home_golden.json (+8) |  | heuristic: key name match |
| M4-0037 | requests.passthrough-headers | covered | read in crates/cpa-server/src/respond.rs; set in crates/cliproxy/tests/fixtures/plugin_wiring_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0038 | server.host | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/discovery/tests.rs (+17); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/src/home.rs (+39) |  | heuristic: key name match |
| M4-0039 | server.port | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/discovery/tests.rs (+6); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/src/home.rs (+31) |  | heuristic: key name match |
| M4-0040 | server.trusted-proxies | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-server/src/observability.rs, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0041 | server.tls.enable | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/listener.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-home/src/cert_tests.rs (+4) |  | heuristic: key name match |
| M4-0042 | server.tls.cert | covered | read in crates/cpa-server/src/listener.rs; set in crates/cpa-home/src/cert_tests.rs, crates/cpa-home/src/client_tests.rs (+2) |  | heuristic: key name match |
| M4-0043 | server.tls.key | covered | read in crates/cpa-exec/src/meta_auth.rs, crates/cpa-plugin/src/api.rs (+6); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/app.rs (+64) |  | heuristic: key name match |
| M4-0044 | oauth.auth-dir | covered | read in crates/cpa-core/src/config.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+20) |  | heuristic: key name match |
| M4-0045 | observability.logs.debug | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+5); set in crates/cliproxy/src/tui/client.rs, crates/cliproxy/tests/fixtures/tui_go.json (+10) |  | heuristic: key name match |
| M4-0046 | observability.pprof.enable | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/listener.rs (+2); set in crates/cliproxy/src/home.rs, crates/cpa-home/src/cert_tests.rs (+4) |  | heuristic: key name match |
| M4-0047 | observability.pprof.addr | covered | read in crates/cpa-home/src/client.rs, crates/cpa-server/src/config_diff.rs (+1); set in crates/cliproxy/src/main.rs, crates/cpa-exec/src/gemini_tests.rs (+15) |  | heuristic: key name match |
| M4-0048 | server.commercial-mode | covered | read in crates/cpa-server/src/request_logging.rs; set in crates/cpa-server/src/request_logging.rs |  | It only disables request logging; it ships with request-log capture (M4-0250). |
| M4-0049 | observability.logs.logging-to-file | covered | read in crates/cliproxy/src/tui/app.rs, crates/cliproxy/src/tui/config.rs (+7); set in crates/cliproxy/src/tui/app.rs, crates/cliproxy/tests/fixtures/tui_go.json (+3) |  | heuristic: key name match |
| M4-0050 | observability.logs.logs-max-total-size-mb | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-server/src/config_diff.rs (+4); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0051 | observability.logs.error-logs-max-files | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-server/src/config_diff.rs (+4); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M4-0052 | observability.usage.usage-statistics-enabled | covered | read in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/config.rs (+5); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/src/observability.rs (+10) |  | heuristic: key name match |
| M4-0053 | observability.usage.redis-usage-queue-retention-seconds | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-server/src/usage.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M4-0054 | routing.cooldown.disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/meta_auth.rs (+6) |  | heuristic: key name match |
| M4-0055 | routing.cooldown.save-cooldown-status | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+1); set in crates/cpa-server/src/runtime.rs, crates/cpa-server/src/scheduler.rs (+2) |  | heuristic: key name match |
| M4-0056 | routing.cooldown.transient-error-cooldown-seconds | covered | read in crates/cpa-core/src/config.rs, crates/cpa-server/src/config_diff.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+3) |  | heuristic: key name match |
| M4-0057 | oauth.auth-auto-refresh-workers | covered | read in crates/cpa-server/src/runtime.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M4-0058 | routing.retry.request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+10) |  | heuristic: key name match |
| M4-0059 | routing.retry.max-retry-credentials | covered | read in crates/cpa-core/src/config.rs, crates/cpa-server/src/config_diff.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0060 | routing.retry.max-retry-interval | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-core/src/config.rs (+3); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+6) |  | heuristic: key name match |
| M4-0061 | quota-exceeded.switch-project | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-server/src/config_diff.rs (+2); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | Go reads it only in the v0 management handlers and the reload diff. |
| M4-0062 | quota-exceeded.switch-preview-model | covered | read in crates/cliproxy/src/tui/config.rs, crates/cpa-server/src/config_diff.rs (+2); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | Go reads it only in the v0 management handlers and the reload diff. |
| M4-0063 | oauth.providers.antigravity.antigravity-credits | missing | no antigravity executor in crates/cpa-exec | google |  |
| M4-0064 | routing.strategy | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+4); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config.rs (+7) |  | heuristic: key name match |
| M4-0065 | routing.session-affinity | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0066 | routing.session-affinity-ttl | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/runtime.rs (+3) |  | heuristic: key name match |
| M4-0067 | routing.session-affinity-subagents | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0068 | api-keys.gemini[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+11) |  | heuristic: key name match |
| M4-0069 | api-keys.gemini[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-plugin/tests/fixtures/pluginhost_go.json (+5) |  | heuristic: key name match |
| M4-0070 | api-keys.gemini[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+34) |  | heuristic: key name match |
| M4-0071 | api-keys.gemini[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+5) |  | heuristic: key name match |
| M4-0072 | api-keys.gemini[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+13) |  | heuristic: key name match |
| M4-0073 | api-keys.gemini[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0074 | api-keys.gemini[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0075 | api-keys.gemini[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/src/management/legacy/lists.rs, crates/cpa-server/src/scheduler.rs (+4) |  | heuristic: key name match |
| M4-0076 | api-keys.gemini[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+7) |  | heuristic: key name match |
| M4-0077 | api-keys.gemini[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+27) |  | heuristic: key name match |
| M4-0078 | api-keys.gemini[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/lcp_tests.rs (+5) |  | heuristic: key name match |
| M4-0079 | api-keys.gemini[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0080 | api-keys.gemini[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+6) |  | heuristic: key name match |
| M4-0081 | api-keys.interactions[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0082 | api-keys.interactions[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0083 | api-keys.interactions[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-exec/tests/fixtures/gemini_go.json (+15) |  | heuristic: key name match |
| M4-0084 | api-keys.interactions[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0085 | api-keys.interactions[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/tests/fixtures/gemini_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+5) |  | heuristic: key name match |
| M4-0086 | api-keys.interactions[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0087 | api-keys.interactions[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0088 | api-keys.interactions[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0089 | api-keys.interactions[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0090 | api-keys.interactions[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-exec/src/gemini_tests.rs (+14) |  | heuristic: key name match |
| M4-0091 | api-keys.interactions[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0092 | api-keys.interactions[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0093 | api-keys.interactions[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M4-0094 | api-keys.codex[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+14) |  | heuristic: key name match |
| M4-0095 | api-keys.codex[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-plugin/tests/fixtures/pluginhost_go.json (+5) |  | heuristic: key name match |
| M4-0096 | api-keys.codex[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+23) |  | heuristic: key name match |
| M4-0097 | api-keys.codex[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+8) |  | heuristic: key name match |
| M4-0098 | api-keys.codex[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cpa-core/src/config/credentials.rs (+21) |  | heuristic: key name match |
| M4-0099 | api-keys.codex[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0100 | api-keys.codex[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0101 | api-keys.codex[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M4-0102 | api-keys.codex[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+8) |  | heuristic: key name match |
| M4-0103 | api-keys.codex[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+62) |  | heuristic: key name match |
| M4-0104 | api-keys.codex[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+7) |  | heuristic: key name match |
| M4-0105 | api-keys.codex[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0106 | api-keys.codex[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+7) |  | heuristic: key name match |
| M4-0107 | api-keys.xai[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+6) |  | heuristic: key name match |
| M4-0108 | api-keys.xai[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/tests/fixtures/xai_auth_go.json (+3) |  | heuristic: key name match |
| M4-0109 | api-keys.xai[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+5) |  | heuristic: key name match |
| M4-0110 | api-keys.xai[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0111 | api-keys.xai[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/xai.rs (+7) |  | heuristic: key name match |
| M4-0112 | api-keys.xai[].keys[].models[].force-mapping | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | openai-xai | heuristic: key name match |
| M4-0113 | api-keys.xai[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0114 | api-keys.xai[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0115 | api-keys.xai[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+4) |  | heuristic: key name match |
| M4-0116 | api-keys.xai[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/xai.rs (+16) |  | heuristic: key name match |
| M4-0117 | api-keys.xai[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+3) |  | heuristic: key name match |
| M4-0118 | api-keys.xai[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0119 | api-keys.xai[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M4-0120 | api-keys.meta[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+15) |  | heuristic: key name match |
| M4-0121 | api-keys.meta[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/meta_auth.rs (+7) |  | heuristic: key name match |
| M4-0122 | api-keys.meta[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-core/src/registry/dynamic.rs (+40) |  | heuristic: key name match |
| M4-0123 | api-keys.meta[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/codex_tests.rs, crates/cpa-exec/src/kimi_auth.rs (+9) |  | heuristic: key name match |
| M4-0124 | api-keys.meta[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+17) |  | heuristic: key name match |
| M4-0125 | api-keys.meta[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0126 | api-keys.meta[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M4-0127 | api-keys.meta[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-server/src/scheduler.rs (+4) |  | heuristic: key name match |
| M4-0128 | api-keys.meta[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/meta_auth.rs (+8) |  | heuristic: key name match |
| M4-0129 | api-keys.meta[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+95) |  | heuristic: key name match |
| M4-0130 | api-keys.meta[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/store_go.rs (+8) |  | heuristic: key name match |
| M4-0131 | api-keys.meta[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0132 | api-keys.meta[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0133 | oauth.providers.codex.model-level-cooling | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0134 | oauth.providers.claude.model-level-cooling | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0135 | api-keys.claude[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+17) |  | heuristic: key name match |
| M4-0136 | api-keys.claude[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+6) |  | heuristic: key name match |
| M4-0137 | api-keys.claude[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+32) |  | heuristic: key name match |
| M4-0138 | api-keys.claude[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+9) |  | heuristic: key name match |
| M4-0139 | api-keys.claude[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+22) |  | heuristic: key name match |
| M4-0140 | api-keys.claude[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0141 | api-keys.claude[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+2) |  | heuristic: key name match |
| M4-0142 | api-keys.claude[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+4) |  | heuristic: key name match |
| M4-0143 | api-keys.claude[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+9) |  | heuristic: key name match |
| M4-0144 | api-keys.claude[].keys[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+60) |  | heuristic: key name match |
| M4-0145 | api-keys.claude[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+7) |  | heuristic: key name match |
| M4-0146 | api-keys.claude[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0147 | api-keys.claude[].keys[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+7) |  | heuristic: key name match |
| M4-0148 | api-keys.openai-compatibility[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+8) |  | heuristic: key name match |
| M4-0149 | api-keys.openai-compatibility[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+8) |  | heuristic: key name match |
| M4-0150 | api-keys.openai-compatibility[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/src/home.rs, crates/cpa-server/src/management/legacy/lists.rs (+3) |  | heuristic: key name match |
| M4-0151 | api-keys.openai-compatibility[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+3) |  | heuristic: key name match |
| M4-0152 | api-keys.openai-compatibility[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+10) |  | heuristic: key name match |
| M4-0153 | api-keys.openai-compatibility[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M4-0154 | api-keys.openai-compatibility[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/src/management/legacy/lists.rs, crates/cpa-server/src/scheduler.rs (+3) |  | heuristic: key name match |
| M4-0155 | api-keys.openai-compatibility[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/tui_go.json (+4) |  | heuristic: key name match |
| M4-0156 | api-keys.openai-compatibility[].request-scoped-errors[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+11) |  | heuristic: key name match |
| M4-0157 | api-keys.openai-compatibility[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M4-0158 | api-keys.openai-compatibility[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0159 | api-keys.openai-compatibility[].request-scoped-errors[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M4-0160 | api-keys.vertex[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M4-0161 | api-keys.vertex[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0162 | api-keys.vertex[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+6) |  | heuristic: key name match |
| M4-0163 | api-keys.vertex[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-core/src/config/credentials.rs (+4) |  | heuristic: key name match |
| M4-0164 | api-keys.vertex[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/vertex_go.json (+5) |  | heuristic: key name match |
| M4-0165 | api-keys.vertex[].keys[].models[].force-mapping | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | google | heuristic: key name match |
| M4-0166 | api-keys.vertex[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0167 | api-keys.vertex[].keys[].disable-cooling | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+8); set in crates/cpa-server/tests/fixtures/config_diff_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M4-0168 | api-keys.vertex[].keys[].request-retry | covered | read in crates/cliproxy/src/tui/config.rs, crates/cliproxy/src/tui/dashboard.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M4-0169 | oauth.excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+7); set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+3) |  | heuristic: key name match |
| M4-0170 | oauth.model-alias.{key}[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0171 | oauth.model-alias.{key}[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+8); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+26) |  | heuristic: key name match |
| M4-0172 | oauth.model-alias.{key}[].fork | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/src/lcp_tests.rs (+7) |  | heuristic: key name match |
| M4-0173 | oauth.model-alias.{key}[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+4); set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+3) |  | heuristic: key name match |
| M4-0174 | oauth.model-alias.{key}[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0175 | oauth.request-scoped-errors.{key}[].status | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cliproxy/src/tui/client.rs (+43); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/client.rs (+203) |  | heuristic: key name match |
| M4-0176 | oauth.request-scoped-errors.{key}[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0177 | oauth.request-scoped-errors.{key}[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/config_diff_go.json |  | heuristic: key name match |
| M4-0178 | oauth.request-scoped-errors.{key}[].action | covered | read in crates/cliproxy/src/tui/golden.rs, crates/cpa-common/src/signature/tests.rs (+11); set in crates/cliproxy/tests/fixtures/tui_go.json, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+7) |  | heuristic: key name match |
| M4-0179 | oauth.settings.{key}[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0180 | oauth.settings.{key}[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+8); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+26) |  | heuristic: key name match |
| M4-0181 | oauth.settings.{key}[].max-context-length | covered | read in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs (+2); set in crates/cpa-server/tests/codex_models.rs, crates/cpa-server/tests/fixtures/codex_models_go.json (+2) |  | heuristic: key name match |
| M4-0182 | requests.payload.default[].models[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0183 | requests.payload.default[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+15) |  | heuristic: key name match |
| M4-0184 | requests.payload.default[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+17); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+212) |  | heuristic: key name match |
| M4-0185 | requests.payload.default[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0186 | requests.payload.default[].models[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0187 | requests.payload.default[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0188 | requests.payload.default[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0189 | requests.payload.default[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0190 | requests.payload.default[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M4-0191 | requests.payload.default-raw[].models[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0192 | requests.payload.default-raw[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+15) |  | heuristic: key name match |
| M4-0193 | requests.payload.default-raw[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+17); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+212) |  | heuristic: key name match |
| M4-0194 | requests.payload.default-raw[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0195 | requests.payload.default-raw[].models[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0196 | requests.payload.default-raw[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0197 | requests.payload.default-raw[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0198 | requests.payload.default-raw[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0199 | requests.payload.default-raw[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M4-0200 | requests.payload.override[].models[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0201 | requests.payload.override[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+15) |  | heuristic: key name match |
| M4-0202 | requests.payload.override[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+17); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+212) |  | heuristic: key name match |
| M4-0203 | requests.payload.override[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0204 | requests.payload.override[].models[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0205 | requests.payload.override[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0206 | requests.payload.override[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0207 | requests.payload.override[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0208 | requests.payload.override[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M4-0209 | requests.payload.override-raw[].models[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0210 | requests.payload.override-raw[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+15) |  | heuristic: key name match |
| M4-0211 | requests.payload.override-raw[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+17); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+212) |  | heuristic: key name match |
| M4-0212 | requests.payload.override-raw[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0213 | requests.payload.override-raw[].models[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0214 | requests.payload.override-raw[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0215 | requests.payload.override-raw[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0216 | requests.payload.override-raw[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0217 | requests.payload.override-raw[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M4-0218 | requests.payload.filter[].models[].name | covered | read in crates/cliproxy/src/discovery/tests.rs, crates/cliproxy/src/tui/auth.rs (+94); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+301) |  | heuristic: key name match |
| M4-0219 | requests.payload.filter[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+15) |  | heuristic: key name match |
| M4-0220 | requests.payload.filter[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+17); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+212) |  | heuristic: key name match |
| M4-0221 | requests.payload.filter[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0222 | requests.payload.filter[].models[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M4-0223 | requests.payload.filter[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0224 | requests.payload.filter[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0225 | requests.payload.filter[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M4-0226 | requests.payload.filter[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M4-0227 | config-version | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/text.rs (+1); set in crates/cliproxy/src/main.rs, crates/cliproxy/tests/fixtures/plugin_cli_go.json (+15) |  | heuristic: key name match |
| M4-0228 | api-keys.<family>[].name | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | server | heuristic: key name match |
| M4-0229 | Legacy/v8/mixed YAML precedence is presence-based, including false/zero/null/empty maps/li… | covered | crates/cpa-core/src/config.rs v8_wins_by_presence_even_when_null_or_empty, legacy_spellings_still_work, null_or_scalar_parents_are_rejected; document.rs tests; manage_go.json config_writes |  |  |
| M4-0230 | Codex multi-agent historical paths precedence: client.codex.optimize-multi-agent-v2; then … | covered | crates/cpa-exec/src/codex_client_tests.rs settings_read_go_config_paths |  |  |
| M4-0231 | Normalization/defaults beyond zero values: optional cloud config may be absent/empty/inval… | partial | crates/cliproxy/src/main.rs cloud standby; crates/cpa-server/src/management (bcrypt keys); crates/cpa-core/src/config/trusted.rs | manage | Clamps (queue retention, log sizes, caps) are not tested case by case. |

### M4: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0232 | Fallback/validation source internal/config/config_defaults.go:1-8: `DefaultPanelGitHubRepo… | covered | crates/cpa-core/src/config.rs defaults_match_go_loader |  |  |
| M4-0233 | Fallback/validation source internal/config/credential_concurrency.go:1-194: `defaultCPAHea… | covered | crates/cpa-home/src/config.rs defaults_match_go_limiter_config, invalid_limiter_values_are_rejected, lifecycle_sum_overflow_is_rejected |  |  |
| M4-0234 | Fallback/validation source internal/config/credential_in_flight.go:1-87: `DefaultInFlightM… | covered | crates/cpa-home/src/config.rs in_flight_reads_defaults_and_overrides |  |  |
| M4-0235 | Fallback/validation source internal/config/claude_fingerprint_profile.go:1-44: defaults/no… | partial | crates/cpa-exec/src/claude/settings.rs fingerprint-profile | claude | Normalization and validation are not tested by name. |
| M4-0236 | Fallback/validation source internal/config/disable_image_generation_mode.go:1-147: default… | partial | cpa_common::payload disable-image-generation (payload_go.json) | server | Not tested by name. |
| M4-0237 | Fallback/validation source internal/config/config_validation.go:1-79: defaults/normalizati… | partial | crates/cpa-core/src/config/validate.rs; manage_go.json load_errors (38) | manage | Not tested by name. |
| M4-0238 | Fallback/validation source internal/config/config_normalization.go:1-494: defaults/normali… | partial | crates/cpa-core/src/config/sanitize.rs, credentials.rs; manage_go.json materialized_defaults | manage | Not tested by name. |
| M4-0239 | Fallback/validation source internal/runtime/executor/helps/utls_client.go:1-430: defaults/… | partial | crates/cpa-exec/src/tls.rs, crates/cpa-exec/src/claude/headers.rs | claude | Not tested by name. |

### M4: CLI flags

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0240 | --discover | covered | crates/cliproxy/src/discovery (tests.rs discover_output_matches_go_byte_for_byte); main.rs every_go_flag_parses_in_single_and_double_dash_form |  |  |
| M4-0241 | --discover-timeout | covered | crates/cliproxy/src/discovery/tests.rs go_durations; main.rs flag parsing |  |  |
| M4-0242 | --discover-json | covered | crates/cliproxy/src/discovery/tests.rs discover_output_matches_go_byte_for_byte; main.rs argv_prescan_matches_go |  |  |
| M4-0243 | --discover-service-type | covered | crates/cliproxy/src/discovery/tests.rs txt_parsing_names_subtypes_and_service_types_match_go |  |  |
| M4-0244 | --standalone | partial | crates/cliproxy/src/main.rs parses --tui/--standalone | tui | The terminal UI is not available yet (main.rs prints 'TUI error'). |
| M4-0245 | Storage choices: file store default; PGSTORE_*, GITSTORE_*, OBJECTSTORE_* environment conf… | covered | crates/cpa-store (postgres.rs, object.rs with fake_s3, git.rs; tests/postgres.rs, tests/git.rs, object.rs unit tests); crates/cliproxy/src/main.rs cpa_store::select/bootstrap |  |  |
| M4-0246 | Watcher fsnotify: config Write/Create/Rename, immediate-child .json auth Create/Write/Remo… | partial | crates/cpa-server/src/watching.rs hash_cache_rereads_only_changed_or_racy_files; crates/cpa-server/tests/management.rs watcher_keeps_last_good_config_and_reconciles_disabled_deleted_and_self_writes | manage | Metadata polling stands in for fsnotify (ponytail in watching.rs). |
| M4-0247 | Reload updates provider executors, credential synthesis, model registry/aliases/exclusions… | partial | runtime.rs config_routing_drives_policy_at_startup_and_on_publish, reconcile_keeps_unchanged_revisions_and_never_reuses_old_ones; management.rs watcher tests | manage | Not tested for every reloadable subsystem. |
| M4-0248 | Watcher dispatcher queues/replaces auth updates, protects snapshots from stale concurrent … | partial | runtime.rs reconcile and stale-lease tests; management.rs watcher tests | manage | Not tested case by case. |
| M4-0249 | Application logging: logrus debug switch, stdout vs rotating file sink, log directory sele… | partial | crates/cpa-server/src/logging.rs lines_follow_go_log_formatter, rotating_file_follows_lumberjack, cleaner_removes_oldest_logs_but_keeps_main_log | manage | Commercial mode (dropping heavy request logging) is absent. |
| M4-0250 | Request logs capture inbound method/path/headers/body, selected account/provider, upstream… | partial | crates/cpa-server/src/management/logs.rs serves request and error logs | server | Request logs are never written: no capture of inbound/upstream requests and responses. |
| M4-0251 | Usage records track input/output/reasoning/cache tokens, model/alias/provider, credential/… | partial | crates/cpa-server/src/usage_record.rs queued_records_match_go; usage.rs; routes.rs usage_queue_records_every_attempt; server_go.json usage | server | No RESP subscribers (ponytail in usage.rs); plugin and Home usage hooks are not wired. |

### M4: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M4-0252 | cmd/server/main_test.go | partial | crates/cliproxy/src/main.rs argv_prescan_matches_go (TestArgvEnablesBoolFlag); model catalog plan in crates/cpa-server/src/model_updater.rs plan_matches_go | tui | Example-API-key safe mode and management base URL cases are not ported. |
| M4-0253 | internal/api/middleware/request_logging_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0254 | internal/api/middleware/response_writer_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0255 | internal/api/mux_listener_test.go | covered | crates/cpa-server/src/listener.rs serve_with_resp; tests/resp_protocol.rs |  | Go's muxListener queue (Put after Close, drain on Close) is an implementation detail: Rust hands each sniffed connection straight to its protocol, so those cases have no counterpart. |
| M4-0256 | internal/api/protocol_multiplexer_test.go | covered | crates/cpa-server/tests/resp_protocol.rs idle_connection_does_not_block_http (Go TestAcceptMuxNotBlockedByIdleConnection) |  |  |
| M4-0257 | internal/api/redis_queue_protocol_integration_test.go | partial | crates/cpa-server/tests/resp_protocol.rs: management_disabled_rejects_connection, subscribe_usage_sends_support_refresh, subscribe_errors_receives_error_events, auth_and_pop_contracts | server | TestRedisProtocol_HomeEnabled_DisablesConnection has no counterpart test (main.rs turns RESP off in Home mode). |
| M4-0258 | internal/api/server_apply_patch_config_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0259 | internal/api/server_grok_models_test.go | missing | /v1/models has no Grok Shell catalog (ponytail in crates/cpa-server/src/models.rs) | openai-xai |  |
| M4-0260 | internal/api/server_models_interceptor_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0261 | internal/api/server_multi_agent_config_test.go | partial | cpa_common::codex_client settings | codex | Not ported by name. |
| M4-0262 | internal/api/server_sdk_config_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0263 | internal/api/server_stop_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0264 | internal/api/server_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | server | Not ported by name. |
| M4-0265 | internal/auth/claude/anthropic_auth_proxy_test.go | partial | Claude OAuth through crate::proxy (crates/cpa-exec/src/oauth.rs) | claude | Not ported by name. |
| M4-0266 | internal/cache/bounded_lru_test.go | partial | bounded caches (crates/cpa-exec/src/proxy.rs client_cache_is_bounded_lru, affinity eviction) | server | Not ported by name. |
| M4-0267 | internal/cache/signature_cache_test.go | partial | crates/cpa-translate/src/replay_cache.rs (signature cache adapter, 4 tests) | google | Owner of internal/cache with the Antigravity executor. |
| M4-0268 | internal/client/grokbuild/grokbuild_test.go | missing | no Grok Build keepalive transform | codex |  |
| M4-0269 | internal/client/grokbuild/keepalive_test.go | missing | no Grok Build keepalive transform | codex |  |
| M4-0270 | internal/clienterror/client_error_test.go | partial | implementation cites client_error.go (crates/cpa-server/src/classify.rs); no case matched by name | server |  |
| M4-0271 | internal/cmd/discover_test.go | partial | crates/cliproxy/src/discovery (tests.rs, dns.rs, mdns.rs Go-derived cases) | tui | Not ported by name. |
| M4-0272 | internal/config/api_key_is_compat_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0273 | internal/config/claude_code_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0274 | internal/config/claude_fingerprint_profile_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0275 | internal/config/claude_header_defaults_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0276 | internal/config/client_optimize_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0277 | internal/config/client_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0278 | internal/config/cloak_save_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0279 | internal/config/clone_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0280 | internal/config/config_v8_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0281 | internal/config/cooling_override_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0282 | internal/config/credential_concurrency_fixture_test.go | partial | crates/cpa-home/src/config.rs (limiter and in-flight defaults) | home | Not ported by name. |
| M4-0283 | internal/config/credential_concurrency_test.go | partial | 3/5 cases: crates/cpa-home/src/config.rs; not matched: TestValidateCredentialConcurrencyAcceptsHomeAuthoritativeHeartbeat, TestValidateCredentialConcurrencyLifecycleRejectsSafetyOverflow; crates/cpa-home/src/config.rs (limiter and in-flight defaults) | home | Not ported by name. |
| M4-0284 | internal/config/credential_in_flight_test.go | partial | crates/cpa-home/src/config.rs (limiter and in-flight defaults) | home | Not ported by name. |
| M4-0285 | internal/config/disable_image_generation_mode_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0286 | internal/config/is_compat_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0287 | internal/config/max_context_length_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0288 | internal/config/model_display_name_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0289 | internal/config/oauth_model_alias_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0290 | internal/config/oauth_request_scoped_errors_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0291 | internal/config/oauth_scope_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0292 | internal/config/oauth_settings_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0293 | internal/config/request_retry_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0294 | internal/config/request_scoped_errors_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0295 | internal/config/trusted_proxies_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0296 | internal/config/use_max_completion_tokens_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0297 | internal/config/weight_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | manage | Not ported by name. |
| M4-0298 | internal/credentialweight/weight_test.go | partial | scheduler.rs smooth weights | server | Not ported by name. |
| M4-0299 | internal/htmlsanitize/htmlsanitize_test.go | covered | 2/2 cases: crates/cpa-plugin/src/management.rs |  |  |
| M4-0300 | internal/httpfetch/httpfetch_test.go | partial | latest-version and model catalog fetches (crates/cpa-server/src/model_updater.rs, management/observability.rs) | server | Not ported by name. |
| M4-0301 | internal/httpwire/ordered_conn_test.go | partial | ordered HTTP/1.1 writer (crates/cpa-exec/src/claude/headers.rs); harness wire captures | claude | Not ported by name. |
| M4-0302 | internal/logging/cpa_trace_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | server | Not ported by name. |
| M4-0303 | internal/logging/diagnostic_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | server | Not ported by name. |
| M4-0304 | internal/logging/gin_logger_test.go | partial | crates/cpa-server/src/access.rs, observability.rs (go_access_and_redaction_goldens, health_status_levels_and_stream_completion_follow_go) | observe | The recovery cases (ErrAbortHandler re-panic, panic logging) are not ported by name. |
| M4-0305 | internal/logging/global_logger_test.go | partial | crates/cpa-server/src/logging.rs (lumberjack rotation, cleaner, formatter tests) | manage | Not ported by name. |
| M4-0306 | internal/logging/log_dir_cleaner_test.go | partial | crates/cpa-server/src/logging.rs (lumberjack rotation, cleaner, formatter tests) | manage | Not ported by name. |
| M4-0307 | internal/logging/request_logger_collision_test.go | missing | request logs are never written | server |  |
| M4-0308 | internal/logging/requestid_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | server | Not ported by name. |
| M4-0309 | internal/misc/credentials_test.go | partial | MetadataPatch merges (crates/cpa-core/src/credential.rs) | server | Not ported by name. |
| M4-0310 | internal/modelconfig/model_info_test.go | partial | crates/cpa-core/src/registry/dynamic.rs | server | Not ported by name. |
| M4-0311 | internal/redisqueue/queue_test.go | partial | usage queue (crates/cpa-server/src/usage.rs) without RESP | server | Not ported by name. |
| M4-0312 | internal/registry/model_definitions_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0313 | internal/registry/model_registry_cache_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0314 | internal/registry/model_registry_credential_quota_regression_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0315 | internal/registry/model_registry_grok_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0316 | internal/registry/model_registry_hook_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0317 | internal/registry/model_registry_quota_refresh_regression_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0318 | internal/registry/model_registry_safety_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0319 | internal/registry/model_updater_test.go | covered | 2/2 cases: crates/cpa-core/src/registry.rs |  |  |
| M4-0320 | internal/registry/web_search_capability_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | server | Not ported by name. |
| M4-0321 | internal/runtime/executor/apply_patch_bridge_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0322 | internal/runtime/executor/apply_patch_capability_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0323 | internal/runtime/executor/apply_patch_identity_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0324 | internal/runtime/executor/apply_patch_integration_test.go | covered | Kimi and Meta run cpa_translate::apply_patch_responses::State (crates/cpa-exec/src/kimi.rs, meta.rs); kimi_tests apply_patch_failures_end_streams_like_go, meta_tests apply_patch_failures_end_meta_streams_like_go; apply_patch_responses.json Go scenarios |  |  |
| M4-0325 | internal/runtime/executor/apply_patch_repair_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0326 | internal/runtime/executor/apply_patch_source_stop_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0327 | internal/runtime/executor/caching_verify_test.go | partial | Claude cache-control placement (crates/cpa-exec/src/claude/cloak.rs); claude scenarios | claude | Not ported by name. |
| M4-0328 | internal/runtime/executor/custom_magic_headers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0329 | internal/runtime/executor/executor_payload_optimization_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0330 | internal/runtime/executor/helps/apply_patch_responses_test.go | covered | 19/19 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json |  |  |
| M4-0331 | internal/runtime/executor/helps/apply_patch_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0332 | internal/runtime/executor/helps/cache_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0333 | internal/runtime/executor/helps/claude_mcp_alias_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0334 | internal/runtime/executor/helps/derived_session_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0335 | internal/runtime/executor/helps/logging_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0336 | internal/runtime/executor/helps/model_capabilities_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0337 | internal/runtime/executor/helps/payload_helpers_disable_image_generation_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0338 | internal/runtime/executor/helps/payload_mutations_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0339 | internal/runtime/executor/helps/proxy_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0340 | internal/runtime/executor/helps/request_pair_compat_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0341 | internal/runtime/executor/helps/response_model_multiprovider_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0342 | internal/runtime/executor/helps/response_model_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0343 | internal/runtime/executor/helps/responses_ttft_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0344 | internal/runtime/executor/helps/responses_usage_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0345 | internal/runtime/executor/helps/session_id_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0346 | internal/runtime/executor/helps/stream_response_model_observer_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0347 | internal/runtime/executor/helps/transport_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0348 | internal/runtime/executor/helps/usage_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0349 | internal/runtime/executor/helps/user_id_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0350 | internal/runtime/executor/helps/utls_client_alpn_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0351 | internal/runtime/executor/helps/utls_client_resumption_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0352 | internal/runtime/executor/helps/utls_client_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0353 | internal/runtime/executor/oauth_scope_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0354 | internal/runtime/executor/request_proxy_priority_test.go | covered | crates/cpa-exec/src/proxy.rs effective_proxy_precedence; proxy_go.json source |  |  |
| M4-0355 | internal/runtime/executor/response_model_multiprovider_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | server | Not ported by name. |
| M4-0356 | internal/safemode/example_api_keys_test.go | partial | crates/cpa-server/src/safe_mode.rs templates_and_html_match_real_go (Go-generated fixtures, tests/fixtures/safe_mode_go.json), startup_reload_paths_query_and_cors_order | server | Not ported by name. |
| M4-0357 | internal/signature/gpt_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M4-0358 | internal/signature/grok_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/16 cases also cited by name) |  |  |
| M4-0359 | internal/signature/provider_compatibility_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/24 cases also cited by name) |  |  |
| M4-0360 | internal/store/disabled_login_save_test.go | partial | runtime.rs first_use_persists_disabled_in_go_marshal_form | server | Not ported by name. |
| M4-0361 | internal/store/gitstore_test.go | partial | crates/cpa-store/src/git.rs; tests/git.rs (the_first_node_initializes_an_empty_remote, two_nodes_share_auth_files_and_config) | home | Not every gitstore_test.go case is ported. |
| M4-0362 | internal/store/postgres_cooldown_store_test.go | covered | crates/cpa-store/src/postgres.rs; tests/postgres.rs postgres_store_round_trips_config_auth_and_cooldowns |  | The Postgres tests run only where PostgreSQL server binaries exist (CPA_TEST_PG_BIN or /usr/lib/postgresql) and pass vacuously otherwise. |
| M4-0363 | internal/util/github_test.go | partial | latest-version route (crates/cpa-server/src/management/observability.rs) | manage | Not ported by name. |
| M4-0364 | internal/util/gjson_test.go | covered | crates/cpa-common/tests/json.rs get_matches_gjson (Go-generated vectors) |  |  |
| M4-0365 | internal/util/header_helpers_test.go | partial | implementation cites header_helpers.go (crates/cpa-common/src/headers.rs, crates/cpa-common/src/lib.rs); no case matched by name | server |  |
| M4-0366 | internal/util/nocopy_invariant_test.go | covered | n/a: Go slice-aliasing invariants |  |  |
| M4-0367 | internal/util/responses_tools_test.go | partial | implementation cites responses_tools.go (crates/cpa-translate/src/responses_tools.rs); no case matched by name | server |  |
| M4-0368 | internal/util/sanitize_test.go | partial | crates/cpa-server/src/sanitize.rs sanitize_matches_go | server | Not ported by name. |
| M4-0369 | internal/watcher/diff/config_diff_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0370 | internal/watcher/diff/cooling_override_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0371 | internal/watcher/diff/model_compat_hash_test.go | missing | no model hash helpers or models_hash/excluded_models_hash auth attributes | server | Reload summaries are ported (crates/cpa-server/src/config_diff.rs). |
| M4-0372 | internal/watcher/diff/model_hash_test.go | missing | no model hash helpers or models_hash/excluded_models_hash auth attributes | server | Reload summaries are ported (crates/cpa-server/src/config_diff.rs). |
| M4-0373 | internal/watcher/diff/oauth_excluded_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0374 | internal/watcher/diff/oauth_model_alias_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0375 | internal/watcher/diff/oauth_request_scoped_errors_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0376 | internal/watcher/diff/oauth_settings_test.go | partial | crates/cpa-server/src/config_diff.rs real_go_reload_summaries_and_redaction (Go-generated fixtures, tests/fixtures/config_diff_go.json); watching.rs reload logging | server | Not ported by name. |
| M4-0377 | internal/watcher/dispatcher_snapshot_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0378 | internal/watcher/synthesizer/config_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0379 | internal/watcher/synthesizer/cooling_override_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0380 | internal/watcher/synthesizer/file_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0381 | internal/watcher/synthesizer/helpers_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0382 | internal/watcher/watcher_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | manage | Not ported by name. |
| M4-0383 | sdk/access/registry_test.go | partial | crates/cpa-server/src/access.rs | server | Not ported by name. |
| M4-0384 | sdk/api/handlers/apply_patch_capability_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0385 | sdk/api/handlers/handlers_error_response_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0386 | sdk/api/handlers/handlers_interceptors_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0387 | sdk/api/handlers/handlers_model_router_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0388 | sdk/api/handlers/handlers_request_details_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0389 | sdk/api/handlers/handlers_stream_bootstrap_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0390 | sdk/api/handlers/header_filter_test.go | covered | crates/cpa-server/src/respond.rs filter_upstream_headers, passthrough_headers; tests/routes.rs passthrough_headers_follow_go |  | Not ported by name. |
| M4-0391 | sdk/api/handlers/model_execution_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0392 | sdk/api/handlers/retry_deadline_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0393 | sdk/api/handlers/stream_forwarder_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | server | Not ported by name. |
| M4-0394 | sdk/auth/filestore_disabled_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | server | Not ported by name. |
| M4-0395 | sdk/auth/filestore_proxy_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | server | Not ported by name. |
| M4-0396 | sdk/auth/filestore_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | server | Not ported by name. |
| M4-0397 | sdk/auth/manager_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | server | Not ported by name. |
| M4-0398 | sdk/auth/refresh_registry_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | server | Not ported by name. |
| M4-0399 | sdk/cliproxy/auth/api_key_model_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0400 | sdk/cliproxy/auth/api_key_model_capabilities_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0401 | sdk/cliproxy/auth/api_key_model_compat_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0402 | sdk/cliproxy/auth/apply_patch_capability_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0403 | sdk/cliproxy/auth/auto_refresh_issue6199_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0404 | sdk/cliproxy/auth/auto_refresh_loop_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0405 | sdk/cliproxy/auth/catalog_credential_quota_regression_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0406 | sdk/cliproxy/auth/classification_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0407 | sdk/cliproxy/auth/claude_ratelimit_cooldown_test.go | partial | crates/cpa-exec/src/quota.rs; scheduler_attempts.rs | claude | Not ported by name. |
| M4-0408 | sdk/cliproxy/auth/conductor_alias_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0409 | sdk/cliproxy/auth/conductor_availability_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0410 | sdk/cliproxy/auth/conductor_claude_cancellation_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0411 | sdk/cliproxy/auth/conductor_cloudflare_520_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0412 | sdk/cliproxy/auth/conductor_compact_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0413 | sdk/cliproxy/auth/conductor_cooldown_monotonic_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0414 | sdk/cliproxy/auth/conductor_cooling_precedence_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0415 | sdk/cliproxy/auth/conductor_credits_candidates_test.go | missing | no Antigravity credits fallback | google |  |
| M4-0416 | sdk/cliproxy/auth/conductor_execution_error_priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0417 | sdk/cliproxy/auth/conductor_execution_quota_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0418 | sdk/cliproxy/auth/conductor_executor_replace_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0419 | sdk/cliproxy/auth/conductor_fast_error_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0420 | sdk/cliproxy/auth/conductor_force_mapping_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0421 | sdk/cliproxy/auth/conductor_oauth_alias_nofork_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0422 | sdk/cliproxy/auth/conductor_oauth_alias_suspension_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0423 | sdk/cliproxy/auth/conductor_oauth_request_scoped_errors_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0424 | sdk/cliproxy/auth/conductor_overrides_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0425 | sdk/cliproxy/auth/conductor_persist_failure_logging_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0426 | sdk/cliproxy/auth/conductor_quota_clock_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0427 | sdk/cliproxy/auth/conductor_recent_requests_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0428 | sdk/cliproxy/auth/conductor_refresh_disabled_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0429 | sdk/cliproxy/auth/conductor_refresh_executor_key_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0430 | sdk/cliproxy/auth/conductor_remove_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0431 | sdk/cliproxy/auth/conductor_request_scoped_errors_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0432 | sdk/cliproxy/auth/conductor_result_policy_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0433 | sdk/cliproxy/auth/conductor_retry_round_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0434 | sdk/cliproxy/auth/conductor_scheduler_cooldown_rebuild_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0435 | sdk/cliproxy/auth/conductor_scheduler_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0436 | sdk/cliproxy/auth/conductor_scheduler_targeted_update_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0437 | sdk/cliproxy/auth/conductor_selection_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0438 | sdk/cliproxy/auth/conductor_session_affinity_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0439 | sdk/cliproxy/auth/conductor_stream_overload_failover_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0440 | sdk/cliproxy/auth/conductor_stream_overload_status_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0441 | sdk/cliproxy/auth/conductor_stream_quota_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0442 | sdk/cliproxy/auth/conductor_subsecond_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0443 | sdk/cliproxy/auth/conductor_transport_retry_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0444 | sdk/cliproxy/auth/conductor_unauthorized_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0445 | sdk/cliproxy/auth/conductor_update_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0446 | sdk/cliproxy/auth/conductor_usage_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0447 | sdk/cliproxy/auth/conductor_warn_logging_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0448 | sdk/cliproxy/auth/conductor_weight_validation_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0449 | sdk/cliproxy/auth/config_apikey_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0450 | sdk/cliproxy/auth/connection_lifecycle_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0451 | sdk/cliproxy/auth/cooldown_backoff_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0452 | sdk/cliproxy/auth/cooldown_state_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0453 | sdk/cliproxy/auth/cooldown_view_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0454 | sdk/cliproxy/auth/custom_headers_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0455 | sdk/cliproxy/auth/error_events_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0456 | sdk/cliproxy/auth/errors_compat_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0457 | sdk/cliproxy/auth/force_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0458 | sdk/cliproxy/auth/oauth_model_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0459 | sdk/cliproxy/auth/persist_policy_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0460 | sdk/cliproxy/auth/priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0461 | sdk/cliproxy/auth/quota_signals_test.go | partial | 11/23 cases: crates/cpa-exec/src/quota.rs, crates/cpa-server/tests/quota_observation.rs; not matched: TestQuotaStateObserveResponseHeadersBoundsAndCanonicalizesValues, TestMarkResultCountTokensDoesNotReplaceObservation, TestResetModelStatePreservesObservationSignals, TestMergeModelStateKeepsNewestObservationSnapshot … | server |  |
| M4-0462 | sdk/cliproxy/auth/request_auth_prepare_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0463 | sdk/cliproxy/auth/request_proxy_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0464 | sdk/cliproxy/auth/request_termination_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0465 | sdk/cliproxy/auth/response_model_rewriter_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0466 | sdk/cliproxy/auth/retry_deadline_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0467 | sdk/cliproxy/auth/scheduler_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0468 | sdk/cliproxy/auth/selector_lcp_test.go | partial | crates/cpa-server/src/scheduler.rs session_affinity_matches_go_selector (14 LCP cases from Go SessionAffinitySelector: growth, fork lineage, compaction, failure removal, request faults, caller isolation, explicit session wins, system-only and anonymous fallbacks); lcp_tests.rs replays the matcher calls of selector_lcp_test.go; openai_compat_routes.rs lcp_session_reaches_custom_headers_and_usage_records | server | LookupAffinity (plugin affinity callbacks) and Home dispatch session aliases do not consult the matcher; the explicit-session cases are not ported by name. |
| M4-0469 | sdk/cliproxy/auth/selector_subagent_affinity_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0470 | sdk/cliproxy/auth/selector_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0471 | sdk/cliproxy/auth/session_affinity_lookup_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0472 | sdk/cliproxy/auth/session_affinity_priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0473 | sdk/cliproxy/auth/session_cache_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0474 | sdk/cliproxy/auth/types_cooling_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0475 | sdk/cliproxy/auth/types_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0476 | sdk/cliproxy/auth/weight_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | server | Not ported by name. |
| M4-0477 | sdk/cliproxy/builder_weight_validation_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0478 | sdk/cliproxy/config_model_display_name_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0479 | sdk/cliproxy/config_model_max_context_length_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0480 | sdk/cliproxy/executionregistry/concurrency_release_test.go | partial | crates/cpa-home/src/registry.rs | home | Not ported by name. |
| M4-0481 | sdk/cliproxy/executionregistry/observation_test.go | partial | crates/cpa-home/src/registry.rs | home | Not ported by name. |
| M4-0482 | sdk/cliproxy/executionregistry/registry_test.go | partial | crates/cpa-home/src/registry.rs | home | Not ported by name. |
| M4-0483 | sdk/cliproxy/executor/lifecycle_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0484 | sdk/cliproxy/executor/types_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0485 | sdk/cliproxy/pprof_server_test.go | missing | no pprof listener yet | observe |  |
| M4-0486 | sdk/cliproxy/rtprovider_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0487 | sdk/cliproxy/service_auth_sync_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0488 | sdk/cliproxy/service_config_weight_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0489 | sdk/cliproxy/service_cooldown_store_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0490 | sdk/cliproxy/service_excluded_models_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0491 | sdk/cliproxy/service_executionregistry_test.go | partial | crates/cpa-home/src/registry.rs | home | Not ported by name. |
| M4-0492 | sdk/cliproxy/service_executor_registration_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0493 | sdk/cliproxy/service_models_config_index_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0494 | sdk/cliproxy/service_oauth_model_alias_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0495 | sdk/cliproxy/service_oauth_settings_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0496 | sdk/cliproxy/service_result_policy_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0497 | sdk/cliproxy/service_stale_state_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0498 | sdk/cliproxy/service_stop_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0499 | sdk/cliproxy/session/identity_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | server | Not ported by name. |
| M4-0500 | sdk/cliproxy/session/info_duplicate_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | server | Not ported by name. |
| M4-0501 | sdk/cliproxy/session/info_performance_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | server | Not ported by name. |
| M4-0502 | sdk/cliproxy/session/info_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | server | Not ported by name. |
| M4-0503 | sdk/cliproxy/session/lcp_lookup_test.go | covered | crates/cpa-server/src/lcp_tests.rs lookup_reports_every_credential_of_a_session, lookup_drops_expired_groups_without_refreshing (both Go cases through the public API) |  |  |
| M4-0504 | sdk/cliproxy/session/lcp_test.go | covered | crates/cpa-server/src/lcp_tests.rs canonical_turns_and_fingerprints_match_go, matcher_replays_go_call_by_call (every call of the Go tests, recorded by tests/reference/lcp), limits_default_and_cover_max_turns, tool_parts_sort_by_value_then_digest |  | TestMerklePrefixMatcherConcurrentAccess is not replayed: its goroutines race; the Rust matcher sits behind the scheduler lock. |
| M4-0505 | sdk/cliproxy/usage/accounting_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0506 | sdk/cliproxy/usage/manager_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | server | Not ported by name. |
| M4-0507 | sdk/proxyutil/proxy_test.go | partial | crates/cpa-exec/src/proxy.rs; proxy_go.json | claude | Not ported by name. |
| M4-0508 | test/builtin_tools_translation_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M4-0509 | test/summary_intent_translation_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (Summary tests in ./test/) |  |  |
| M4-0510 | test/usage_logging_test.go | partial | crates/cpa-server/tests/routes.rs usage_queue_records_every_attempt | server | Not ported by name. |

## M5

### M5: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0001 | GET /management.html | covered | probe: GET /management.html -> 200; tests: crates/cpa-server/src/safe_mode.rs, crates/cpa-server/tests/management.rs |  | The page is the embedded dashboard, not Go's downloaded panel (deliberate difference, see M5-0283). |
| M5-0002 | GET /v1/responses | covered | probe: GET /v1/responses -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-exec/src/codex_capture.rs (+13) |  |  |
| M5-0003 | POST /v1/live | covered | probe: POST /v1/live -> 401; tests: crates/cpa-server/src/realtime.rs, crates/cpa-server/src/realtime/capture_tests.rs (+3) |  |  |
| M5-0004 | GET /v1/live/:call_id | covered | probe: GET /v1/live/:call_id -> 401; tests: crates/cpa-server/src/realtime.rs, crates/cpa-server/src/realtime/capture_tests.rs (+3) |  |  |
| M5-0005 | GET /v1/realtime | covered | probe: GET /v1/realtime -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0006 | POST /v1/realtime | covered | probe: POST /v1/realtime -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0007 | POST /v1/realtime/calls | covered | probe: POST /v1/realtime/calls -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0008 | GET /v1/realtime/calls/:call_id | covered | probe: GET /v1/realtime/calls/:call_id -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0009 | POST /v1/realtime/client_secrets | covered | probe: POST /v1/realtime/client_secrets -> 401; tests: crates/cpa-server/src/realtime.rs, crates/cpa-server/tests/realtime.rs |  |  |
| M5-0010 | POST /v1/realtime/sessions | covered | probe: POST /v1/realtime/sessions -> 401; tests: crates/cpa-server/src/realtime.rs |  |  |
| M5-0011 | POST /v1/realtime/transcription_sessions | covered | probe: POST /v1/realtime/transcription_sessions -> 401; tests: crates/cpa-server/src/realtime.rs |  |  |
| M5-0012 | GET /v1/realtime/translations | covered | probe: GET /v1/realtime/translations -> 401; tests: crates/cpa-server/src/realtime.rs |  |  |
| M5-0013 | POST /v1/realtime/translations | covered | probe: POST /v1/realtime/translations -> 401; tests: crates/cpa-server/src/realtime.rs |  |  |
| M5-0014 | POST /v1/realtime/translations/client_secrets | covered | probe: POST /v1/realtime/translations/client_secrets -> 401; tests: crates/cpa-server/src/realtime.rs |  |  |
| M5-0015 | POST /v1/realtime/calls/:call_id/hangup | covered | probe: POST /v1/realtime/calls/:call_id/hangup -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0016 | POST /v1/realtime/calls/:call_id/accept | covered | probe: POST /v1/realtime/calls/:call_id/accept -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0017 | POST /v1/realtime/calls/:call_id/reject | covered | probe: POST /v1/realtime/calls/:call_id/reject -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0018 | POST /v1/realtime/calls/:call_id/refer | covered | probe: POST /v1/realtime/calls/:call_id/refer -> 401; tests: crates/cliproxy/src/home.rs, crates/cpa-server/src/realtime.rs (+3) |  |  |
| M5-0019 | GET /backend-api/codex/responses | covered | probe: GET /backend-api/codex/responses -> 401; tests: crates/cpa-exec/src/codex_tls_tests.rs, crates/cpa-server/src/observability.rs |  |  |

### M5: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0020 | WebUI OAuth forwarder listener `ANY /*` on 0.0.0.0:54545 (Anthropic), :1455 (Codex), or :5… | covered | crates/cpa-server/src/management/oauth.rs forwarder_redirect_keeps_the_query, forwarder_connections_have_go_deadlines |  |  |
| M5-0021 | Dynamic relay route: `GET /v1/ws` by default, or normalized caller-supplied path, `AttachW… | missing | no AI Studio WebSocket relay route | google |  |
| M5-0022 | Optional SDK `GET /keep-alive`, `handleKeepAlive`; no body, keepalive token/timeout lifecy… | covered | crates/cpa-server/src/keepalive.rs (heartbeats_hold_off_the_timeout, dropping_the_router_stops_the_watcher, go_duration_strings) |  |  |
| M5-0023 | Realtime credentials distinguish configured proxy keys from short-lived client secrets; pr… | covered | crates/cpa-server/src/realtime/secrets.rs keys_expire_and_capacity_is_bounded; tests/realtime.rs realtime_http_matches_go, realtime_websockets_match_go (realtime_http_go.json, codex_live_ws_go.json) |  |  |

### M5: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0024 | Codex Responses WebSocket pooling, liveness deadlines, session affinity, force mapping, du… | partial | crates/cpa-exec/src/codex_ws.rs, crates/cpa-server/src/websocket*.rs; tests/ws_e2e.rs; websocket_requests_tests.rs (Go vectors) | codex | Duplex steering is absent (M2-0145..0149). |
| M5-0025 | Codex streaming bootstrap holds metadata/empty-added/heartbeats before generated output; o… | covered | codex_go.json executor oauth_bootstrap_overload_failover, oauth_bootstrap_time_budget_spent, oauth_bootstrap_holds_then_releases; stream-bootstrap keys read in crates/cpa-exec/src/codex_request.rs |  |  |

### M5: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0026 | management.allow-remote | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+1); set in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+5) |  | heuristic: key name match |
| M5-0027 | management.secret-key | covered | read in crates/cpa-core/src/config.rs, crates/cpa-server/src/management.rs; set in crates/cpa-home/src/kv.rs, crates/cpa-server/src/management/access.rs (+8) |  | heuristic: key name match |
| M5-0028 | management.disable-control-panel | covered | read in crates/cliproxy/src/home.rs, crates/cpa-core/src/config.rs (+1); set in crates/cliproxy/tests/fixtures/plugin_wiring_go.json, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M5-0029 | management.disable-auto-update-panel | covered | accepted by the config schema (crates/cpa-core/src/config.rs); no panel updater reads it |  | Deliberate difference (owner ruling): Rust serves its embedded dashboard and never downloads or auto-updates Go's panel; the periodic GitHub download runs third-party JavaScript with the admin key. |
| M5-0030 | management.panel-github-repository | covered | accepted by the config schema (crates/cpa-core/src/config.rs); nothing downloads from it |  | Deliberate difference (owner ruling): Rust serves its embedded dashboard and never downloads or auto-updates Go's panel; the periodic GitHub download runs third-party JavaScript with the admin key. |
| M5-0031 | management.base-url | covered | read in crates/cliproxy/src/main.rs, crates/cliproxy/src/tui/keys.rs (+13); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/main.rs (+39) |  | heuristic: key name match |
| M5-0032 | oauth.providers.aistudio.ws-auth | missing | no aistudio executor in crates/cpa-exec | google |  |
| M5-0033 | oauth.providers.codex.live-media-relay.enabled | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/tui/auth.rs (+23); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_cli_go.json (+16) |  | heuristic: key name match |
| M5-0034 | oauth.providers.codex.live-media-relay.max-sessions | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/relay.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+2) |  | heuristic: key name match |
| M5-0035 | oauth.providers.codex.live-media-relay.disable-private-remote-ips | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/http.rs, crates/cpa-server/src/realtime/relay.rs (+2) |  | heuristic: key name match |
| M5-0036 | oauth.providers.codex.live-media-relay.public-ip | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/relay.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M5-0037 | oauth.providers.codex.live-media-relay.udp-port-min | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/relay.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M5-0038 | oauth.providers.codex.live-media-relay.udp-port-max | covered | read in crates/cpa-server/src/config_diff.rs, crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/relay.rs, crates/cpa-server/tests/fixtures/config_diff_go.json (+1) |  | heuristic: key name match |
| M5-0039 | oauth.providers.codex.live-media-relay.ice-servers[].urls | covered | read in crates/cpa-server/src/realtime/relay.rs; set in crates/cpa-server/src/realtime/media_tests.rs, crates/cpa-server/src/realtime/relay.rs (+2) |  | heuristic: key name match |
| M5-0040 | oauth.providers.codex.live-media-relay.ice-servers[].username | covered | read in crates/cpa-plugin/src/store/auth.rs, crates/cpa-server/src/management.rs (+1); set in crates/cpa-server/src/realtime/relay.rs, crates/cpa-server/src/realtime/tunnel_tests.rs (+3) |  | heuristic: key name match |
| M5-0041 | oauth.providers.codex.live-media-relay.ice-servers[].credential | covered | read in crates/cpa-exec/src/claude/go_exec.rs, crates/cpa-exec/src/codex_request.rs (+5); set in crates/cliproxy/src/home.rs, crates/cpa-common/tests/fixtures/codex_catalog_go.json.gz (+46) |  | heuristic: key name match |

### M5: /v8/management

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0042 | GET /v8/management/oauth/callback | covered | probe: GET /v8/management/oauth/callback -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0043 | POST /v8/management/oauth/callback | covered | probe: POST /v8/management/oauth/callback -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0044 | GET /v8/management/config | covered | probe: GET /v8/management/config -> 200; tests: crates/cpa-server/src/safe_mode.rs, crates/cpa-server/tests/legacy_go.rs (+3) |  |  |
| M5-0045 | PUT /v8/management/config | covered | probe: PUT /v8/management/config -> 500; tests: crates/cpa-server/src/safe_mode.rs, crates/cpa-server/tests/legacy_go.rs (+3) |  |  |
| M5-0046 | PATCH /v8/management/config | covered | probe: PATCH /v8/management/config -> 200; tests: crates/cpa-server/src/safe_mode.rs, crates/cpa-server/tests/legacy_go.rs (+3) |  |  |
| M5-0047 | GET /v8/management/config.yaml | partial | probe: GET /v8/management/config.yaml -> 200; no test requests this path | manage |  |
| M5-0048 | PUT /v8/management/config.yaml | partial | probe: PUT /v8/management/config.yaml -> 200; no test requests this path | manage |  |
| M5-0049 | GET /v8/management/config/*path | covered | probe: GET /v8/management/config/*path -> 404; tests: crates/cpa-server/tests/legacy_go.rs, crates/cpa-server/tests/management.rs (+1) |  |  |
| M5-0050 | PUT /v8/management/config/*path | covered | probe: PUT /v8/management/config/*path -> 400; tests: crates/cpa-server/tests/legacy_go.rs, crates/cpa-server/tests/management.rs (+1) |  |  |
| M5-0051 | PATCH /v8/management/config/*path | covered | probe: PATCH /v8/management/config/*path -> 400; tests: crates/cpa-server/tests/legacy_go.rs, crates/cpa-server/tests/management.rs (+1) |  |  |
| M5-0052 | DELETE /v8/management/config/*path | covered | probe: DELETE /v8/management/config/*path -> 404; tests: crates/cpa-server/tests/legacy_go.rs, crates/cpa-server/tests/management.rs (+1) |  |  |
| M5-0053 | GET /v8/management/server/latest-version | covered | probe: GET /v8/management/server/latest-version -> 502; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0054 | POST /v8/management/requests/api-call | covered | probe: POST /v8/management/requests/api-call -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0055 | POST /v8/management/routing/cooldown/reset | partial | probe: POST /v8/management/routing/cooldown/reset -> 400; no test requests this path | manage |  |
| M5-0056 | GET /v8/management/routing/model-definitions/:channel | partial | probe: GET /v8/management/routing/model-definitions/:channel -> 400; no test requests this path | manage |  |
| M5-0057 | GET /v8/management/observability/logs | covered | probe: GET /v8/management/observability/logs -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0058 | DELETE /v8/management/observability/logs | covered | probe: DELETE /v8/management/observability/logs -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0059 | GET /v8/management/observability/logs/errors | covered | probe: GET /v8/management/observability/logs/errors -> 200; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0060 | GET /v8/management/observability/logs/errors/:name | partial | probe: GET /v8/management/observability/logs/errors/:name -> 404; no test requests this path | manage |  |
| M5-0061 | GET /v8/management/observability/logs/requests/:id | covered | probe: GET /v8/management/observability/logs/requests/:id -> 404; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0062 | GET /v8/management/observability/usage/api-keys | partial | probe: GET /v8/management/observability/usage/api-keys -> 200; no test requests this path | manage |  |
| M5-0063 | GET /v8/management/observability/usage/queue | covered | probe: GET /v8/management/observability/usage/queue -> 200; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0064 | GET /v8/management/credentials | covered | probe: GET /v8/management/credentials -> 200; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0065 | POST /v8/management/credentials | covered | probe: POST /v8/management/credentials -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0066 | DELETE /v8/management/credentials | covered | probe: DELETE /v8/management/credentials -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0067 | GET /v8/management/credentials/models | covered | probe: GET /v8/management/credentials/models -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0068 | GET /v8/management/credentials/download | partial | probe: GET /v8/management/credentials/download -> 400; no test requests this path | manage |  |
| M5-0069 | PATCH /v8/management/credentials/status | covered | probe: PATCH /v8/management/credentials/status -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0070 | PATCH /v8/management/credentials/fields | covered | probe: PATCH /v8/management/credentials/fields -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0071 | POST /v8/management/credentials/refresh | partial | probe: POST /v8/management/credentials/refresh -> 400; no test requests this path | manage |  |
| M5-0072 | POST /v8/management/oauth/import | covered | probe: POST /v8/management/oauth/import -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0073 | GET /v8/management/oauth/auth-url | covered | probe: GET /v8/management/oauth/auth-url -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0074 | GET /v8/management/oauth/status | partial | probe: GET /v8/management/oauth/status -> 200; no test requests this path | manage |  |
| M5-0075 | DELETE /v8/management/oauth/session | covered | probe: DELETE /v8/management/oauth/session -> 400; tests: crates/cpa-server/tests/management.rs |  |  |
| M5-0076 | GET /v8/management/plugins | partial | probe: GET /v8/management/plugins -> 200; no test requests this path | manage |  |
| M5-0077 | DELETE /v8/management/plugins/:id | partial | probe: DELETE /v8/management/plugins/:id -> 404; no test requests this path | manage |  |
| M5-0078 | GET /v8/management/plugins/store | missing | probe: GET /v8/management/plugins/store -> 404 (not routed) | plugins | Plugin store and plugin quota routes need the plugin store and the QuotaProvider capability wired into the server. |
| M5-0079 | POST /v8/management/plugins/store/:id/install | missing | probe: POST /v8/management/plugins/store/:id/install -> 404 (not routed) | plugins | Plugin store and plugin quota routes need the plugin store and the QuotaProvider capability wired into the server. |
| M5-0080 | GET /v8/management/plugins/:id/quota | missing | probe: GET /v8/management/plugins/:id/quota -> 404 (not routed) | plugins | Plugin store and plugin quota routes need the plugin store and the QuotaProvider capability wired into the server. |
| M5-0081 | POST /v8/management/plugins/:id/quota | missing | probe: POST /v8/management/plugins/:id/quota -> 404 (not routed) | plugins | Plugin store and plugin quota routes need the plugin store and the QuotaProvider capability wired into the server. |
| M5-0082 | DELETE /v8/management/plugins/:id/quota | missing | probe: DELETE /v8/management/plugins/:id/quota -> 404 (not routed) | plugins | Plugin store and plugin quota routes need the plugin store and the QuotaProvider capability wired into the server. |

### M5: /v0/management

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0083 | POST /v0/management/oauth-callback | partial | probe: POST /v0/management/oauth-callback -> 400; no test requests this path | manage |  |
| M5-0084 | GET /v0/management/oauth-callback | partial | probe: GET /v0/management/oauth-callback -> 400; no test requests this path | manage |  |
| M5-0085 | GET /v0/management/config | covered | probe: GET /v0/management/config -> 200; tests: crates/cliproxy/src/tui/client.rs, crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0086 | GET /v0/management/config.yaml | covered | probe: GET /v0/management/config.yaml -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0087 | PUT /v0/management/config.yaml | missing | probe: PUT /v0/management/config.yaml -> 404 (not routed) | tui | v0 serves GET config.yaml only (management.rs); v8 has both. |
| M5-0088 | GET /v0/management/latest-version | partial | probe: GET /v0/management/latest-version -> 502; no test requests this path | manage |  |
| M5-0089 | GET /v0/management/plugins | covered | probe: GET /v0/management/plugins -> 200; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0090 | GET /v0/management/plugin-store | missing | probe: GET /v0/management/plugin-store -> 404 (not routed) | manage |  |
| M5-0091 | POST /v0/management/plugin-store/:id/install | missing | probe: POST /v0/management/plugin-store/:id/install -> 404 (not routed) | manage |  |
| M5-0092 | DELETE /v0/management/plugins/:id | covered | probe: DELETE /v0/management/plugins/:id -> 404; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0093 | PATCH /v0/management/plugins/:id/enabled | covered | probe: PATCH /v0/management/plugins/:id/enabled -> 400; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0094 | GET /v0/management/plugins/:id/config | covered | probe: GET /v0/management/plugins/:id/config -> 404; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0095 | PUT /v0/management/plugins/:id/config | covered | probe: PUT /v0/management/plugins/:id/config -> 200; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0096 | PATCH /v0/management/plugins/:id/config | covered | probe: PATCH /v0/management/plugins/:id/config -> 200; tests: crates/cpa-server/tests/plugin_quota_cooldown.rs |  |  |
| M5-0097 | GET /v0/management/plugins/:id/quota | missing | probe: GET /v0/management/plugins/:id/quota -> 404 (not routed) | manage |  |
| M5-0098 | POST /v0/management/plugins/:id/quota | missing | probe: POST /v0/management/plugins/:id/quota -> 404 (not routed) | manage |  |
| M5-0099 | DELETE /v0/management/plugins/:id/quota | missing | probe: DELETE /v0/management/plugins/:id/quota -> 404 (not routed) | manage |  |
| M5-0100 | POST /v0/management/plugins/:id/quota/reset | missing | probe: POST /v0/management/plugins/:id/quota/reset -> 404 (not routed) | manage |  |
| M5-0101 | GET /v0/management/debug | covered | probe: GET /v0/management/debug -> 200; tests: crates/cliproxy/src/tui/client.rs, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  |  |
| M5-0102 | PUT /v0/management/debug | covered | probe: PUT /v0/management/debug -> 400; tests: crates/cliproxy/src/tui/client.rs, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  |  |
| M5-0103 | PATCH /v0/management/debug | covered | probe: PATCH /v0/management/debug -> 400; tests: crates/cliproxy/src/tui/client.rs, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  |  |
| M5-0104 | GET /v0/management/logging-to-file | covered | probe: GET /v0/management/logging-to-file -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0105 | PUT /v0/management/logging-to-file | covered | probe: PUT /v0/management/logging-to-file -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0106 | PATCH /v0/management/logging-to-file | covered | probe: PATCH /v0/management/logging-to-file -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0107 | GET /v0/management/logs-max-total-size-mb | covered | probe: GET /v0/management/logs-max-total-size-mb -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0108 | PUT /v0/management/logs-max-total-size-mb | covered | probe: PUT /v0/management/logs-max-total-size-mb -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0109 | PATCH /v0/management/logs-max-total-size-mb | covered | probe: PATCH /v0/management/logs-max-total-size-mb -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0110 | GET /v0/management/error-logs-max-files | covered | probe: GET /v0/management/error-logs-max-files -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0111 | PUT /v0/management/error-logs-max-files | covered | probe: PUT /v0/management/error-logs-max-files -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0112 | PATCH /v0/management/error-logs-max-files | covered | probe: PATCH /v0/management/error-logs-max-files -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0113 | GET /v0/management/usage-statistics-enabled | covered | probe: GET /v0/management/usage-statistics-enabled -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0114 | PUT /v0/management/usage-statistics-enabled | covered | probe: PUT /v0/management/usage-statistics-enabled -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0115 | PATCH /v0/management/usage-statistics-enabled | covered | probe: PATCH /v0/management/usage-statistics-enabled -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0116 | GET /v0/management/proxy-url | covered | probe: GET /v0/management/proxy-url -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0117 | PUT /v0/management/proxy-url | covered | probe: PUT /v0/management/proxy-url -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0118 | PATCH /v0/management/proxy-url | covered | probe: PATCH /v0/management/proxy-url -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0119 | DELETE /v0/management/proxy-url | covered | probe: DELETE /v0/management/proxy-url -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0120 | POST /v0/management/api-call | partial | probe: POST /v0/management/api-call -> 400; no test requests this path | manage |  |
| M5-0121 | GET /v0/management/quota-exceeded/switch-project | covered | probe: GET /v0/management/quota-exceeded/switch-project -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0122 | PUT /v0/management/quota-exceeded/switch-project | covered | probe: PUT /v0/management/quota-exceeded/switch-project -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0123 | PATCH /v0/management/quota-exceeded/switch-project | covered | probe: PATCH /v0/management/quota-exceeded/switch-project -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0124 | GET /v0/management/quota-exceeded/switch-preview-model | covered | probe: GET /v0/management/quota-exceeded/switch-preview-model -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0125 | PUT /v0/management/quota-exceeded/switch-preview-model | covered | probe: PUT /v0/management/quota-exceeded/switch-preview-model -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0126 | PATCH /v0/management/quota-exceeded/switch-preview-model | covered | probe: PATCH /v0/management/quota-exceeded/switch-preview-model -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0127 | POST /v0/management/reset-quota | partial | probe: POST /v0/management/reset-quota -> 400; no test requests this path | manage |  |
| M5-0128 | GET /v0/management/quota/providers | missing | probe: GET /v0/management/quota/providers -> 404 (not routed) | manage |  |
| M5-0129 | POST /v0/management/quota/fetch | missing | probe: POST /v0/management/quota/fetch -> 404 (not routed) | manage |  |
| M5-0130 | POST /v0/management/quota/reset | missing | probe: POST /v0/management/quota/reset -> 404 (not routed) | manage |  |
| M5-0131 | GET /v0/management/api-keys | covered | probe: GET /v0/management/api-keys -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0132 | PUT /v0/management/api-keys | covered | probe: PUT /v0/management/api-keys -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0133 | PATCH /v0/management/api-keys | covered | probe: PATCH /v0/management/api-keys -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0134 | DELETE /v0/management/api-keys | covered | probe: DELETE /v0/management/api-keys -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0135 | GET /v0/management/api-key-usage | partial | probe: GET /v0/management/api-key-usage -> 200; no test requests this path | manage |  |
| M5-0136 | GET /v0/management/usage-queue | partial | probe: GET /v0/management/usage-queue -> 200; no test requests this path | manage |  |
| M5-0137 | GET /v0/management/gemini-api-key | covered | probe: GET /v0/management/gemini-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0138 | PUT /v0/management/gemini-api-key | covered | probe: PUT /v0/management/gemini-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0139 | PATCH /v0/management/gemini-api-key | covered | probe: PATCH /v0/management/gemini-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0140 | DELETE /v0/management/gemini-api-key | covered | probe: DELETE /v0/management/gemini-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0141 | GET /v0/management/interactions-api-key | covered | probe: GET /v0/management/interactions-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0142 | PUT /v0/management/interactions-api-key | covered | probe: PUT /v0/management/interactions-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0143 | PATCH /v0/management/interactions-api-key | covered | probe: PATCH /v0/management/interactions-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0144 | DELETE /v0/management/interactions-api-key | covered | probe: DELETE /v0/management/interactions-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0145 | GET /v0/management/logs | covered | probe: GET /v0/management/logs -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0146 | DELETE /v0/management/logs | covered | probe: DELETE /v0/management/logs -> 400; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0147 | GET /v0/management/request-error-logs | covered | probe: GET /v0/management/request-error-logs -> 200; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0148 | GET /v0/management/request-error-logs/:name | partial | probe: GET /v0/management/request-error-logs/:name -> 404; no test requests this path | manage |  |
| M5-0149 | GET /v0/management/request-log-by-id/:id | covered | probe: GET /v0/management/request-log-by-id/:id -> 404; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0150 | GET /v0/management/request-log | covered | probe: GET /v0/management/request-log -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0151 | PUT /v0/management/request-log | covered | probe: PUT /v0/management/request-log -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0152 | PATCH /v0/management/request-log | covered | probe: PATCH /v0/management/request-log -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0153 | GET /v0/management/ws-auth | covered | probe: GET /v0/management/ws-auth -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0154 | PUT /v0/management/ws-auth | covered | probe: PUT /v0/management/ws-auth -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0155 | PATCH /v0/management/ws-auth | covered | probe: PATCH /v0/management/ws-auth -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0156 | GET /v0/management/request-retry | covered | probe: GET /v0/management/request-retry -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0157 | PUT /v0/management/request-retry | covered | probe: PUT /v0/management/request-retry -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0158 | PATCH /v0/management/request-retry | covered | probe: PATCH /v0/management/request-retry -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0159 | GET /v0/management/max-retry-credentials | covered | probe: GET /v0/management/max-retry-credentials -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0160 | PUT /v0/management/max-retry-credentials | covered | probe: PUT /v0/management/max-retry-credentials -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0161 | PATCH /v0/management/max-retry-credentials | covered | probe: PATCH /v0/management/max-retry-credentials -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0162 | GET /v0/management/max-retry-interval | covered | probe: GET /v0/management/max-retry-interval -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0163 | PUT /v0/management/max-retry-interval | covered | probe: PUT /v0/management/max-retry-interval -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0164 | PATCH /v0/management/max-retry-interval | covered | probe: PATCH /v0/management/max-retry-interval -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0165 | GET /v0/management/force-model-prefix | covered | probe: GET /v0/management/force-model-prefix -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0166 | PUT /v0/management/force-model-prefix | covered | probe: PUT /v0/management/force-model-prefix -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0167 | PATCH /v0/management/force-model-prefix | covered | probe: PATCH /v0/management/force-model-prefix -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0168 | GET /v0/management/routing/strategy | covered | probe: GET /v0/management/routing/strategy -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs (+1) |  |  |
| M5-0169 | PUT /v0/management/routing/strategy | covered | probe: PUT /v0/management/routing/strategy -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs (+1) |  |  |
| M5-0170 | PATCH /v0/management/routing/strategy | covered | probe: PATCH /v0/management/routing/strategy -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs (+1) |  |  |
| M5-0171 | GET /v0/management/claude-api-key | covered | probe: GET /v0/management/claude-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0172 | PUT /v0/management/claude-api-key | covered | probe: PUT /v0/management/claude-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0173 | PATCH /v0/management/claude-api-key | covered | probe: PATCH /v0/management/claude-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0174 | DELETE /v0/management/claude-api-key | covered | probe: DELETE /v0/management/claude-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0175 | GET /v0/management/codex-api-key | covered | probe: GET /v0/management/codex-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0176 | PUT /v0/management/codex-api-key | covered | probe: PUT /v0/management/codex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0177 | PATCH /v0/management/codex-api-key | covered | probe: PATCH /v0/management/codex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0178 | DELETE /v0/management/codex-api-key | covered | probe: DELETE /v0/management/codex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0179 | GET /v0/management/xai-api-key | covered | probe: GET /v0/management/xai-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0180 | PUT /v0/management/xai-api-key | covered | probe: PUT /v0/management/xai-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0181 | PATCH /v0/management/xai-api-key | covered | probe: PATCH /v0/management/xai-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0182 | DELETE /v0/management/xai-api-key | covered | probe: DELETE /v0/management/xai-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0183 | GET /v0/management/meta-api-key | covered | probe: GET /v0/management/meta-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0184 | PUT /v0/management/meta-api-key | covered | probe: PUT /v0/management/meta-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0185 | PATCH /v0/management/meta-api-key | covered | probe: PATCH /v0/management/meta-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0186 | DELETE /v0/management/meta-api-key | covered | probe: DELETE /v0/management/meta-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0187 | GET /v0/management/openai-compatibility | covered | probe: GET /v0/management/openai-compatibility -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0188 | PUT /v0/management/openai-compatibility | covered | probe: PUT /v0/management/openai-compatibility -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0189 | PATCH /v0/management/openai-compatibility | covered | probe: PATCH /v0/management/openai-compatibility -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0190 | DELETE /v0/management/openai-compatibility | covered | probe: DELETE /v0/management/openai-compatibility -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0191 | GET /v0/management/vertex-api-key | covered | probe: GET /v0/management/vertex-api-key -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0192 | PUT /v0/management/vertex-api-key | covered | probe: PUT /v0/management/vertex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0193 | PATCH /v0/management/vertex-api-key | covered | probe: PATCH /v0/management/vertex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0194 | DELETE /v0/management/vertex-api-key | covered | probe: DELETE /v0/management/vertex-api-key -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json, crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0195 | GET /v0/management/oauth-excluded-models | covered | probe: GET /v0/management/oauth-excluded-models -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0196 | PUT /v0/management/oauth-excluded-models | covered | probe: PUT /v0/management/oauth-excluded-models -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0197 | PATCH /v0/management/oauth-excluded-models | covered | probe: PATCH /v0/management/oauth-excluded-models -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0198 | DELETE /v0/management/oauth-excluded-models | covered | probe: DELETE /v0/management/oauth-excluded-models -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0199 | GET /v0/management/oauth-model-alias | covered | probe: GET /v0/management/oauth-model-alias -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0200 | PUT /v0/management/oauth-model-alias | covered | probe: PUT /v0/management/oauth-model-alias -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0201 | PATCH /v0/management/oauth-model-alias | covered | probe: PATCH /v0/management/oauth-model-alias -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0202 | DELETE /v0/management/oauth-model-alias | covered | probe: DELETE /v0/management/oauth-model-alias -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0203 | GET /v0/management/oauth-request-scoped-errors | covered | probe: GET /v0/management/oauth-request-scoped-errors -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0204 | PUT /v0/management/oauth-request-scoped-errors | covered | probe: PUT /v0/management/oauth-request-scoped-errors -> 200; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0205 | PATCH /v0/management/oauth-request-scoped-errors | covered | probe: PATCH /v0/management/oauth-request-scoped-errors -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0206 | DELETE /v0/management/oauth-request-scoped-errors | covered | probe: DELETE /v0/management/oauth-request-scoped-errors -> 400; tests: crates/cpa-server/tests/fixtures/legacy_go.json |  |  |
| M5-0207 | GET /v0/management/auth-files | covered | probe: GET /v0/management/auth-files -> 200; tests: crates/cpa-server/src/dispatch.rs, crates/cpa-server/tests/quota_observation.rs (+1) |  |  |
| M5-0208 | GET /v0/management/auth-files/models | partial | probe: GET /v0/management/auth-files/models -> 400; no test requests this path | manage |  |
| M5-0209 | GET /v0/management/model-definitions/:channel | partial | probe: GET /v0/management/model-definitions/:channel -> 400; no test requests this path | manage |  |
| M5-0210 | GET /v0/management/auth-files/download | partial | probe: GET /v0/management/auth-files/download -> 400; no test requests this path | manage |  |
| M5-0211 | POST /v0/management/auth-files | covered | probe: POST /v0/management/auth-files -> 400; tests: crates/cpa-server/src/dispatch.rs, crates/cpa-server/tests/quota_observation.rs (+1) |  |  |
| M5-0212 | DELETE /v0/management/auth-files | covered | probe: DELETE /v0/management/auth-files -> 400; tests: crates/cpa-server/src/dispatch.rs, crates/cpa-server/tests/quota_observation.rs (+1) |  |  |
| M5-0213 | PATCH /v0/management/auth-files/status | partial | probe: PATCH /v0/management/auth-files/status -> 400; no test requests this path | manage |  |
| M5-0214 | PATCH /v0/management/auth-files/fields | partial | probe: PATCH /v0/management/auth-files/fields -> 400; no test requests this path | manage |  |
| M5-0215 | POST /v0/management/auth-files/refresh | partial | probe: POST /v0/management/auth-files/refresh -> 400; no test requests this path | manage |  |
| M5-0216 | POST /v0/management/vertex/import | covered | probe: POST /v0/management/vertex/import -> 404; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0217 | GET /v0/management/anthropic-auth-url | partial | probe: GET /v0/management/anthropic-auth-url -> 200; no test requests this path | manage |  |
| M5-0218 | GET /v0/management/codex-auth-url | partial | probe: GET /v0/management/codex-auth-url -> 200; no test requests this path | manage |  |
| M5-0219 | GET /v0/management/antigravity-auth-url | covered | probe: GET /v0/management/antigravity-auth-url -> 404; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0220 | GET /v0/management/kimi-auth-url | partial | probe: GET /v0/management/kimi-auth-url -> 500; no test requests this path | manage |  |
| M5-0221 | GET /v0/management/kimi-ai-auth-url | partial | probe: GET /v0/management/kimi-ai-auth-url -> 500; no test requests this path | manage |  |
| M5-0222 | GET /v0/management/xai-auth-url | covered | probe: GET /v0/management/xai-auth-url -> 500; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0223 | GET /v0/management/devin-auth-url | covered | probe: GET /v0/management/devin-auth-url -> 200; tests: crates/cpa-server/tests/legacy_go.rs |  |  |
| M5-0224 | GET /v0/management/meta-auth-url | partial | probe: GET /v0/management/meta-auth-url -> 500; no test requests this path | manage |  |
| M5-0225 | GET /v0/management/get-auth-status | partial | probe: GET /v0/management/get-auth-status -> 200; no test requests this path | manage |  |
| M5-0226 | DELETE /v0/management/oauth-session | partial | probe: DELETE /v0/management/oauth-session -> 400; no test requests this path | manage |  |
| M5-0227 | Doc/code route reconciliation: all v8 operational rows in docs/management-api-v8.md have c… | partial | probe: 95 of 237 listed routes routed | manage | Plugin routes and the deprecated v0 config subroutes are not registered. |
| M5-0228 | Important v0-only surfaces are not aliases under v8: quota/providers, quota/fetch, quota/r… | missing | v0-only quota/providers, quota/fetch, quota/reset and plugin config routes are not routed (probe 404) | manage |  |
| M5-0229 | Management write protection and redaction: TURN credentials hidden only in JSON (YAML incl… | partial | crates/cpa-server/tests/management.rs (writes_normalize_only_what_they_touch, config_key_* tests); manage_go.json config_writes | manage | TURN credential and Home revision redaction are not tested. |
| M5-0230 | OAuth shared v8 dispatcher supports built-ins claude, codex, antigravity, kimi/kimi-ai, xa… | partial | crates/cpa-server/src/management/oauth.rs (claude, codex, kimi/kimi-ai, meta, xai, devin); tests/management.rs *_login_* tests | manage | Antigravity login (no executor) and plugin provider IDs are absent. The module header ponytail still says xai and devin answer provider_not_found; that is stale. |
| M5-0231 | Management credential operations preserve listing projections vs raw download, multipart a… | partial | crates/cpa-server/src/management/auth_files.rs, multipart.rs (hostile_bodies_never_panic); manage_go.json credentials; tests/manage_go.rs go_credential_management_replay | manage | Not ported by name. |
| M5-0232 | Management authentication has per-client-IP failed-attempt tracking: five credential failu… | covered | crates/cpa-server/src/management/access.rs (fifth failure bans for 30 minutes); tests/manage_go.rs every_go_access_scenario_replays_through_the_router (manage_go.json access) |  |  |

### M5: CLI flags

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0233 | --password | covered | crates/cliproxy/src/main.rs --password (loopback-only management password) -> serve; every_go_flag_parses_in_single_and_double_dash_form |  |  |
| M5-0234 | --management-base-url | partial | crates/cliproxy/src/main.rs parses --management-base-url | tui | TUI client mode is not available. |
| M5-0235 | AI Studio relay protocol carries HTTP requests/responses and streaming chunks over session… | missing | no AI Studio relay | google |  |
| M5-0236 | Realtime/Live includes SDP negotiation, short-lived client secrets, legacy sessions, worki… | partial | crates/cpa-server/src/realtime/* (media.rs, relay.rs), crates/cpa-exec/src/codex_live.rs; tests/realtime.rs (Go fixtures), realtime/media_tests.rs | realtime | webrtc-rs lacks some pion behaviours (ponytails in realtime/media.rs). |

### M5: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M5-0237 | internal/api/handlers/management/api_key_usage_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0238 | internal/api/handlers/management/api_tools_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0239 | internal/api/handlers/management/auth_files_batch_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0240 | internal/api/handlers/management/auth_files_cooldown_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0241 | internal/api/handlers/management/auth_files_delete_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0242 | internal/api/handlers/management/auth_files_devin_oauth_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0243 | internal/api/handlers/management/auth_files_download_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0244 | internal/api/handlers/management/auth_files_download_windows_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0245 | internal/api/handlers/management/auth_files_filter_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0246 | internal/api/handlers/management/auth_files_pagination_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0247 | internal/api/handlers/management/auth_files_patch_fields_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0248 | internal/api/handlers/management/auth_files_project_id_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0249 | internal/api/handlers/management/auth_files_provider_meta_oauth_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0250 | internal/api/handlers/management/auth_files_quota_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0251 | internal/api/handlers/management/auth_files_recent_requests_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0252 | internal/api/handlers/management/auth_files_refresh_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0253 | internal/api/handlers/management/auth_files_relogin_preserve_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0254 | internal/api/handlers/management/auth_files_status_sync_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0255 | internal/api/handlers/management/auth_files_upload_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0256 | internal/api/handlers/management/config_apikey_disable_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0257 | internal/api/handlers/management/config_basic_version_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0258 | internal/api/handlers/management/config_basic_weight_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0259 | internal/api/handlers/management/config_claude_key_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0260 | internal/api/handlers/management/config_codex_alpha_search_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0261 | internal/api/handlers/management/config_codex_disable_cloaking_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0262 | internal/api/handlers/management/config_disable_cooling_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0263 | internal/api/handlers/management/config_lists_delete_keys_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0264 | internal/api/handlers/management/config_meta_key_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0265 | internal/api/handlers/management/config_openai_compat_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0266 | internal/api/handlers/management/config_priority_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0267 | internal/api/handlers/management/config_v8_client_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0268 | internal/api/handlers/management/config_v8_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0269 | internal/api/handlers/management/config_weight_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0270 | internal/api/handlers/management/config_xai_key_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0271 | internal/api/handlers/management/handler_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0272 | internal/api/handlers/management/logs_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0273 | internal/api/handlers/management/oauth_callback_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0274 | internal/api/handlers/management/oauth_codex_concurrency_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0275 | internal/api/handlers/management/oauth_sessions_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0276 | internal/api/handlers/management/quota_test.go | missing | v0 quota routes are not routed (probe 404) | manage |  |
| M5-0277 | internal/api/handlers/management/test_main_test.go | covered | n/a: Go test harness setup |  |  |
| M5-0278 | internal/api/handlers/management/usage_test.go | partial | crates/cpa-server/src/management*; tests/management.rs, tests/manage_go.rs (manage_go.json) | manage | Not ported by name. |
| M5-0279 | internal/api/server_management_v8_test.go | partial | crates/cpa-server/src/management.rs router; tests/manage_go.rs go_server_routing_preflight_and_availability_replay | manage | Not ported by name. |
| M5-0280 | internal/client/codex/live/websocket_test.go | partial | crates/cpa-server/src/realtime/socket.rs; tests/realtime.rs realtime_websockets_match_go | realtime | Not ported by name. |
| M5-0281 | internal/config/codex_websocket_header_defaults_test.go | partial | crates/cpa-exec/src/codex_request.rs header defaults | codex | Not ported by name. |
| M5-0282 | internal/config/remote_management_test.go | partial | crates/cpa-core/src/config.rs ManagementConfig; tests/management.rs nonlocal_socket_requires_allow_remote_even_with_a_valid_key | manage | Not ported by name. |
| M5-0283 | internal/managementasset/updater_test.go | covered | crates/cpa-server/src/management.rs serves the embedded dashboard |  | Deliberate difference (owner ruling): Rust serves its embedded dashboard and never downloads or auto-updates Go's panel; the periodic GitHub download runs third-party JavaScript with the admin key. |
| M5-0284 | internal/runtime/executor/codex_websockets_duplex_bootstrap_input_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0285 | internal/runtime/executor/codex_websockets_duplex_credential_failure_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0286 | internal/runtime/executor/codex_websockets_duplex_health_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0287 | internal/runtime/executor/codex_websockets_duplex_initial_failure_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0288 | internal/runtime/executor/codex_websockets_duplex_rejection_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0289 | internal/runtime/executor/codex_websockets_duplex_successor_metadata_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0290 | internal/runtime/executor/codex_websockets_duplex_test.go | partial | crates/cpa-exec/src/codex_duplex.rs; cpa-server tests/ws_e2e.rs steering_forwards_steers_and_the_automatic_successor | codex | Not ported by name. |
| M5-0291 | internal/runtime/executor/codex_websockets_executor_store_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0292 | internal/runtime/executor/codex_websockets_executor_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0293 | internal/runtime/executor/codex_websockets_routing_hint_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0294 | internal/runtime/executor/codex_websockets_spawn_agent_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0295 | internal/runtime/executor/helps/websocket_observer_helpers_test.go | partial | crates/cpa-server/src/websocket*.rs | codex | Not ported by name. |
| M5-0296 | internal/runtime/executor/websocket_lifecycle_bind_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0297 | internal/runtime/executor/websocket_proxy_reuse_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0298 | internal/runtime/executor/websocket_session_target_test.go | partial | crates/cpa-exec/src/codex_ws.rs (codex_ws_tests.rs, codex_ws_errors.json); crates/cpa-server/tests/ws_e2e.rs | codex | Not ported by name. |
| M5-0299 | internal/runtime/executor/xai_websockets_executor_test.go | partial | crates/cpa-exec/src/xai_ws.rs (xai_ws_tests.rs turns_match_go, Go fixtures) | openai-xai | Not every xai_websockets_executor_test.go case is ported. |
| M5-0300 | internal/wsrelay/session_test.go | missing | no AI Studio WebSocket relay | google |  |
| M5-0301 | sdk/api/handlers/openai/openai_responses_websocket_requests_memory_test.go | partial | crates/cpa-server/src/websocket*.rs; websocket_requests_tests.rs (Go vectors, ws_vectors.json); tests/ws_e2e.rs (ws_e2e.json) | codex | Not ported by name. |
| M5-0302 | sdk/api/handlers/openai/openai_responses_websocket_test.go | partial | crates/cpa-server/src/websocket*.rs; websocket_requests_tests.rs (Go vectors, ws_vectors.json); tests/ws_e2e.rs (ws_e2e.json) | codex | Not ported by name. |
| M5-0303 | sdk/cliproxy/executor/websocket_test.go | partial | crates/cpa-exec/src/codex_ws.rs | codex | Not ported by name. |

## M6

### M6: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M6-0001 | Plugin-defined exact Management routes under `/v0/management/` and unauthenticated browser… | covered | crates/cpa-server/src/plugins.rs, management/plugins.rs (serve_management through NoRoute); tests/plugin_routes.rs plugin_routes_match_go (plugin_routes_go.json); cpa-plugin go_host.rs |  |  |

### M6: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M6-0002 | credentials.concurrency.lifecycle-config-revision | covered | read in crates/cpa-home/src/config.rs, crates/cpa-server/src/management.rs; set in crates/cliproxy/src/home.rs, crates/cpa-home/src/config.rs (+2) |  | heuristic: key name match |
| M6-0003 | credentials.concurrency.observation-barrier-revision | covered | read in crates/cpa-home/src/config.rs, crates/cpa-server/src/management.rs; set in crates/cpa-home/src/config.rs |  | heuristic: key name match |
| M6-0004 | credentials.concurrency.cpa-heartbeat-timeout | covered | read in crates/cpa-home/src/config.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/claude/testdata/go_unit.json (+3) |  | heuristic: key name match |
| M6-0005 | credentials.concurrency.cpa-cancel-bound | covered | read in crates/cpa-home/src/config.rs; set in crates/cliproxy/src/home.rs, crates/cpa-exec/src/claude/testdata/go_unit.json (+2) |  | heuristic: key name match |
| M6-0006 | credentials.concurrency.reclaim-grace | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+1) |  | heuristic: key name match |
| M6-0007 | credentials.concurrency.cleanup-interval | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+1) |  | heuristic: key name match |
| M6-0008 | credentials.concurrency.release-flush-interval | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+1) |  | heuristic: key name match |
| M6-0009 | credentials.concurrency.release-max-backoff | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+1) |  | heuristic: key name match |
| M6-0010 | credentials.concurrency.busy-retry-min | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+2) |  | heuristic: key name match |
| M6-0011 | credentials.concurrency.busy-retry-max | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-exec/src/claude/testdata/go_unit.json, crates/cpa-home/src/config.rs (+1) |  | heuristic: key name match |
| M6-0012 | credentials.concurrency.max-limit | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/src/config.rs, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M6-0013 | credentials.in-flight.snapshot-interval | covered | read in crates/cpa-home/src/config.rs; set in crates/cliproxy/src/home.rs, crates/cpa-home/src/config.rs (+3) |  | heuristic: key name match |
| M6-0014 | credentials.in-flight.stale-after | covered | read in crates/cpa-home/src/config.rs; set in crates/cliproxy/src/home.rs, crates/cpa-home/tests/fixtures/credential_in_flight_contract.json (+2) |  | heuristic: key name match |
| M6-0015 | credentials.in-flight.max-part-bytes | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/src/config.rs, crates/cpa-home/tests/fixtures/credential_in_flight_contract.json (+1) |  | heuristic: key name match |
| M6-0016 | credentials.in-flight.max-part-count | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/tests/fixtures/credential_in_flight_contract.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0017 | credentials.in-flight.max-revision-bytes | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/tests/fixtures/credential_in_flight_contract.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0018 | credentials.in-flight.max-aggregate-groups | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/tests/fixtures/credential_in_flight_contract.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0019 | credentials.in-flight.max-details | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/src/config.rs, crates/cpa-home/tests/fixtures/credential_in_flight_contract.json (+1) |  | heuristic: key name match |
| M6-0020 | credentials.in-flight.max-string-bytes | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/tests/fixtures/credential_in_flight_contract.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0021 | credentials.in-flight.staging-retention | covered | read in crates/cpa-home/src/config.rs; set in crates/cpa-home/tests/fixtures/credential_in_flight_contract.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0022 | plugins.enabled | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/tui/auth.rs (+23); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/i18n.rs (+26) |  | heuristic: key name match |
| M6-0023 | plugins.dir | covered | read in crates/cpa-plugin/src/config.rs, crates/cpa-server/src/management/legacy/view.rs (+1); set in crates/cliproxy/tests/fixtures/plugin_cli_go.json, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+31) |  | heuristic: key name match |
| M6-0024 | plugins.store-sources | covered | read in crates/cpa-plugin/src/config.rs; set in crates/cpa-server/tests/fixtures/plugin_routes_go.json |  | heuristic: key name match |
| M6-0025 | plugins.store-auth[].match | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+8); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-plugin/tests/fixtures/pluginstore_go.json (+10) |  | heuristic: key name match |
| M6-0026 | plugins.store-auth[].apply-to | partial | read in crates/cpa-plugin/src/config.rs; no test sets it | plugins | heuristic: key name match |
| M6-0027 | plugins.store-auth[].type | covered | read in crates/cpa-common/src/codex_catalog.rs, crates/cpa-common/src/codex_client.rs (+124); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/src/home.rs (+215) |  | heuristic: key name match |
| M6-0028 | plugins.store-auth[].token-env | covered | read in crates/cpa-plugin/src/config.rs, crates/cpa-plugin/src/store/auth.rs; set in crates/cpa-server/tests/fixtures/plugin_routes_go.json |  | heuristic: key name match |
| M6-0029 | plugins.store-auth[].username-env | partial | read in crates/cpa-plugin/src/config.rs, crates/cpa-plugin/src/store/auth.rs; no test sets it | plugins | heuristic: key name match |
| M6-0030 | plugins.store-auth[].password-env | partial | read in crates/cpa-plugin/src/config.rs, crates/cpa-plugin/src/store/auth.rs; no test sets it | plugins | heuristic: key name match |
| M6-0031 | plugins.store-auth[].header-name | partial | read in crates/cpa-plugin/src/config.rs; no test sets it | plugins | heuristic: key name match |
| M6-0032 | plugins.store-auth[].header-value-env | partial | read in crates/cpa-plugin/src/config.rs, crates/cpa-plugin/src/store/auth.rs; no test sets it | plugins | heuristic: key name match |
| M6-0033 | plugins.store-auth[].allow-insecure | partial | read in crates/cpa-plugin/src/config.rs; no test sets it | plugins | heuristic: key name match |
| M6-0034 | plugins.auth-revision | covered | read in crates/cpa-server/src/management.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M6-0035 | plugins.configs.{key}.enabled | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/tui/auth.rs (+23); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/i18n.rs (+26) |  | heuristic: key name match |
| M6-0036 | plugins.configs.{key}.priority | covered | read in crates/cliproxy/src/tui/auth.rs, crates/cpa-common/src/codex_catalog.rs (+21); set in crates/cliproxy/src/home.rs, crates/cliproxy/tests/fixtures/plugin_wiring_go.json (+23) |  | heuristic: key name match |
| M6-0037 | server.discovery.enabled | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/tui/auth.rs (+23); set in crates/cliproxy/src/home.rs, crates/cliproxy/src/tui/i18n.rs (+26) |  | heuristic: key name match |
| M6-0038 | server.discovery.service-name | covered | read in crates/cliproxy/src/discovery/mod.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M6-0039 | server.discovery.service-type | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cpa-server/src/management/legacy/view.rs; set in crates/cliproxy/tests/fixtures/discovery_go.json, crates/cpa-server/tests/fixtures/legacy_go.json (+1) |  | heuristic: key name match |
| M6-0040 | server.discovery.subtypes | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/discovery/tests.rs (+1); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/tests/fixtures/discovery_go.json (+2) |  | heuristic: key name match |
| M6-0041 | server.discovery.interfaces.include | covered | read in crates/cliproxy/src/discovery/cli.rs, crates/cliproxy/src/discovery/mod.rs (+6); set in crates/cliproxy/tests/fixtures/discovery_cmd_go.json, crates/cliproxy/tests/fixtures/discovery_go.json (+2) |  | heuristic: key name match |
| M6-0042 | server.discovery.interfaces.exclude | covered | read in crates/cliproxy/src/discovery/cli.rs, crates/cliproxy/src/discovery/mod.rs (+2); set in crates/cliproxy/tests/fixtures/discovery_cmd_go.json, crates/cliproxy/tests/fixtures/discovery_go.json (+2) |  | heuristic: key name match |
| M6-0043 | server.discovery.auth-required | covered | read in crates/cliproxy/src/discovery/mod.rs; set in crates/cpa-server/tests/fixtures/legacy_go.json |  | heuristic: key name match |
| M6-0044 | server.discovery.advertise-management | partial | read in crates/cliproxy/src/discovery/mod.rs; no test sets it | tui | heuristic: key name match |
| M6-0045 | Runtime-only Home schema is not an accepted top-level home config.yaml block because Confi… | partial | crates/cpa-home/src/config.rs (HomeConfig, normalize_home_port test); cpa-core Config has no home field and no deny_unknown_fields, so a YAML home: block is ignored | home | No test parses a home: block (Go TestParseConfigBytesIgnoresHomeConfig). |

### M6: CLI flags

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M6-0046 | --discover-include | covered | crates/cliproxy/src/main.rs (csv_flags, discover_options); every_go_flag_parses_in_single_and_double_dash_form, argv_prescan_matches_go (discovery_main_go.json) |  |  |
| M6-0047 | --discover-exclude | covered | crates/cliproxy/src/main.rs (csv_flags, discover_options); every_go_flag_parses_in_single_and_double_dash_form, argv_prescan_matches_go (discovery_main_go.json) |  |  |
| M6-0048 | --home-jwt | covered | crates/cliproxy/src/main.rs (flag, HOME_JWT/home_jwt env) starts Home mode through cpa-home; cpa-home cert and client tests |  |  |
| M6-0049 | --home-disable-cluster-discovery | covered | crates/cliproxy/src/main.rs passes the flag to cpa-home HomeConfig.disable_cluster_discovery; cpa-home client tests |  |  |
| M6-0050 | --tui | partial | crates/cliproxy/src/main.rs parses --tui and prints "TUI error: the terminal UI is not available in this build yet" | tui |  |
| M6-0051 | discover | partial | crates/cliproxy/src/main.rs (DiscoverArgs, discover subcommand, --discover/--discover-json); discovery::tests::discover_output_matches_go_byte_for_byte (discovery_cmd_go.json runs) | plugins | The discover modes match Go; plugin-owned flags (CommandLinePlugin) are not registered before the main parse because cpa-plugin is not wired. |
| M6-0052 | discover --timeout | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0053 | discover --json | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0054 | discover --service-type | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0055 | discover --config | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0056 | discover --include | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0057 | discover --exclude | covered | crates/cliproxy/src/main.rs (DiscoverArgs); discovery::cli (discovery_cmd_go.json runs and interface_lists) |  |  |
| M6-0058 | TUI Bubble Tea: dashboard, config, auth files, client keys, OAuth and logs; lazy per-tab f… | missing | no terminal UI (--tui prints an error) | tui |  |
| M6-0059 | TUI dashboard fetches config/auth files/client keys on initialization, locale change and m… | missing | no terminal UI (--tui prints an error) | tui |  |
| M6-0060 | LAN discovery mDNS/DNS-SD `_ai-gateway._tcp`, protocol subtypes, stable short ID/service n… | covered | crates/cliproxy/src/discovery (advertise.rs, mdns.rs, dns.rs, iface.rs); tests.rs replays Go outputs (discovery_go.json, discovery_go_packets.json); mdns.rs responder/browser round trips |  | Advertiser update/unregister has no live multicast test. |
| M6-0061 | Home JWT bootstrap parses certificate_id, cluster_id, ca_fingerprint, enrollment_secret, i… | covered | crates/cpa-home/src/cert.rs (cert_tests.rs); wired by crates/cliproxy/src/main.rs Home mode |  |  |
| M6-0062 | Home control plane authoritative config GET/SUBSCRIBE lifetime, reconnect/failover, CLUSTE… | partial | crates/cpa-home/src/client.rs, subscriber.rs (client_tests.rs against fake.rs); Home mode wired in crates/cliproxy/src/main.rs (Home 3) | home | Not every Go client case is ported (M6-0165). |
| M6-0063 | Home dispatch/retry contract keeps retry_round, credential policy, canonical session/paren… | partial | crates/cpa-home/src/dispatch.rs, client.rs rpop_auth and ambiguous-dispatch fencing; dispatch through Home (Home 3) | home | The sdk/cliproxy/auth home_* suites (M6-0221..0237) are mostly unported. |
| M6-0064 | Credential concurrency and in-flight snapshots: lifecycle/observation revisions, heartbeat… | partial | crates/cpa-home/src/release.rs, inflight.rs, config.rs (unit tests) | home | Not every concurrency-release and in-flight contract case is ported (M6-0166, M6-0167). |
| M6-0065 | Plugins discover/load native dynamic libraries, validate ABI and registration JSON schema,… | partial | crates/cpa-server/src/plugins.rs (load and reload on every config); crates/cpa-plugin/src/host.rs, native.rs, platform.rs; go_host.rs, native_abi.rs; cpa-server tests/plugin_routes.rs | plugins | The plugin store (internal/pluginstore: install, store auth, network scope) is not ported. |

### M6: 7a. Plugin ABI capabilities and RPC inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M6-0066 | Native C ABI 1, registration JSON schema 6; schema 2 lifecycle/termination, 3 omit request… | covered | crates/cpa-plugin/src/abi.rs (envelopes_match_go_bytes); executor.rs negotiation_follows_go; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json) |  | Crate level; see M6-0065 for wiring. |
| M6-0067 | Capability `ModelRegistrar` (`ModelRegistrar`): ModelRegistrar contributes development-tim… | partial | crates/cpa-plugin/src/models.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: register_models) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0068 | Capability `ModelProvider` (`ModelProvider`): ModelProvider contributes provider-native st… | partial | crates/cpa-plugin/src/models.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: models_for_auth) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0069 | Capability `AuthProvider` (`AuthProvider`): AuthProvider lets the host parse, login, poll,… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: parse_auths, start_login, poll_login, refresh_auth) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0070 | Capability `FrontendAuthProvider` (`FrontendAuthProvider`): FrontendAuthProvider authentic… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: frontend_auth) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0071 | Capability `FrontendAuthProviderExclusive` (`bool`): FrontendAuthProviderExclusive makes t… | partial | crates/cpa-plugin/src/auth.rs (frontend_auth_provider_exclusive); Go-differential coverage: no exclusive case | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0072 | Capability `Scheduler` (`Scheduler`): Scheduler chooses an auth candidate before the built… | partial | crates/cpa-plugin/src/routing.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: pick_auth) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0073 | Capability `SchedulerAcrossPriorities` (`bool`): SchedulerAcrossPriorities opts into recei… | partial | crates/cpa-plugin/src/routing.rs, rpc.rs (scheduler_across_priorities); crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: pick_auth) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0074 | Capability `ModelRouter` (`ModelRouter`): ModelRouter routes matching requests to a plugin… | partial | crates/cpa-plugin/src/routing.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: route_model) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0075 | Capability `Executor` (`ProviderExecutor`): Executor sends requests to an upstream provide… | partial | crates/cpa-plugin/src/executor.rs, models.rs register_executors; Go-differential coverage: live_checks a_stale_executor_provider_replaced_by_a_builtin_one_is_kept only | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0076 | Capability `ExecutorModelScope` (`ExecutorModelScope`): ExecutorModelScope declares whethe… | partial | crates/cpa-plugin/src/executor.rs; Go-differential coverage: negotiation_follows_go only | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0077 | Capability `ExecutorInputFormats` (`[]string`): ExecutorInputFormats lists request protoco… | partial | crates/cpa-plugin/src/executor.rs; Go-differential coverage: negotiation_follows_go only | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0078 | Capability `ExecutorOutputFormats` (`[]string`): ExecutorOutputFormats lists response prot… | partial | crates/cpa-plugin/src/executor.rs; Go-differential coverage: negotiation_follows_go only | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0079 | Capability `RequestTranslator` (`RequestTranslator`): RequestTranslator converts canonical… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: translate_request) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0080 | Capability `RequestNormalizer` (`RequestNormalizer`): RequestNormalizer converts provider-… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: normalize_request) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0081 | Capability `ResponseTranslator` (`ResponseTranslator`): ResponseTranslator converts canoni… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: translate_response) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0082 | Capability `ResponseBeforeTranslator` (`ResponseNormalizer`): ResponseBeforeTranslator nor… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: normalize_response_before) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0083 | Capability `ResponseAfterTranslator` (`ResponseNormalizer`): ResponseAfterTranslator norma… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: normalize_response_after) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0084 | Capability `RequestInterceptor` (`RequestInterceptor`): RequestInterceptor rewrites execut… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: intercept_before, intercept_after) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0085 | Capability `RequestLifecyclePlugin` (`RequestLifecyclePlugin`): RequestLifecyclePlugin asy… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: complete) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0086 | Capability `ResponseInterceptor` (`ResponseInterceptor`): ResponseInterceptor rewrites suc… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: intercept_response) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0087 | Capability `StreamChunkInterceptor` (`StreamChunkInterceptor`): StreamChunkInterceptor rew… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: intercept_stream_chunk) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0088 | Capability `WebSocketResponseObserver` (`WebSocketResponseObserver`): WebSocketResponseObs… | partial | crates/cpa-plugin/src/interceptors.rs observe_websocket_response_event; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: ws_event) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0089 | Capability `ThinkingApplier` (`ThinkingApplier`): ThinkingApplier applies validated thinki… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: thinking) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0090 | Capability `UsagePlugin` (`UsagePlugin`): UsagePlugin receives completed usage records. sd… | partial | crates/cpa-plugin/src/transform.rs (usage.handle adapter); Go-differential coverage: no test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0091 | Capability `CommandLinePlugin` (`CommandLinePlugin`): CommandLinePlugin declares and handl… | partial | crates/cpa-plugin/src/cli.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: command_line) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0092 | Capability `ManagementAPI` (`ManagementAPI`): ManagementAPI declares plugin-owned diagnost… | covered | crates/cpa-plugin/src/management.rs; crates/cpa-server/src/management/plugins.rs; tests/plugin_routes.rs plugin_routes_match_go |  |  |
| M6-0093 | Capability `QuotaProvider` (`QuotaProvider`): QuotaProvider surfaces credential quota and … | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: describe_quota, fetch_quota, reset_quota_by_plugin) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0094 | RPC `plugin.register` (constant `MethodPluginRegister`); JSON envelope OK/result/error, by… | covered | crates/cpa-plugin/src/host.rs; crates/cpa-server/src/plugins.rs apply_config; go_host.rs apply steps; tests/plugin_routes.rs |  |  |
| M6-0095 | RPC `plugin.quiesce` (constant `MethodPluginQuiesce`); JSON envelope OK/result/error, byte… | covered | crates/cpa-plugin/src/host.rs; crates/cpa-server/src/plugins.rs apply_config; go_host.rs apply steps |  |  |
| M6-0096 | RPC `plugin.reconfigure` (constant `MethodPluginReconfigure`); JSON envelope OK/result/err… | covered | crates/cpa-plugin/src/host.rs; crates/cpa-server/src/plugins.rs apply_config; go_host.rs apply steps |  |  |
| M6-0097 | RPC `plugin.shutdown` (constant `MethodPluginShutdown`); JSON envelope OK/result/error, by… | covered | crates/cpa-plugin/src/abi.rs (constant), native.rs shutdown export; native_abi.rs calls_racing_shutdown_are_safe |  | Go's host never sends plugin.shutdown over RPC either; both call the native shutdown export. |
| M6-0098 | RPC `model.register` (constant `MethodModelRegister`); JSON envelope OK/result/error, byte… | partial | crates/cpa-plugin/src/models.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: model.register responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0099 | RPC `model.static` (constant `MethodModelStatic`); JSON envelope OK/result/error, byte pay… | partial | crates/cpa-plugin/src/models.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: model.static responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0100 | RPC `model.for_auth` (constant `MethodModelForAuth`); JSON envelope OK/result/error, byte … | partial | crates/cpa-plugin/src/models.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: model.for_auth responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0101 | RPC `auth.identifier` (constant `MethodAuthIdentifier`); JSON envelope OK/result/error, by… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: auth.identifier responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0102 | RPC `auth.parse` (constant `MethodAuthParse`); JSON envelope OK/result/error, byte payload… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: auth.parse responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0103 | RPC `auth.login.start` (constant `MethodAuthLoginStart`); JSON envelope OK/result/error, b… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: auth.login.start responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0104 | RPC `auth.login.poll` (constant `MethodAuthLoginPoll`); JSON envelope OK/result/error, byt… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: auth.login.poll responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0105 | RPC `auth.refresh` (constant `MethodAuthRefresh`); JSON envelope OK/result/error, byte pay… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: auth.refresh responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0106 | RPC `frontend_auth.identifier` (constant `MethodFrontendAuthIdentifier`); JSON envelope OK… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: frontend_auth calls) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0107 | RPC `frontend_auth.authenticate` (constant `MethodFrontendAuthAuthenticate`); JSON envelop… | partial | crates/cpa-plugin/src/auth.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: frontend_auth.authenticate responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0108 | RPC `scheduler.pick` (constant `MethodSchedulerPick`); JSON envelope OK/result/error, byte… | partial | crates/cpa-plugin/src/routing.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: scheduler.pick responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0109 | RPC `model.route` (constant `MethodModelRoute`); JSON envelope OK/result/error, byte paylo… | partial | crates/cpa-plugin/src/routing.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: model.route responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0110 | RPC `executor.identifier` (constant `MethodExecutorIdentifier`); JSON envelope OK/result/e… | partial | crates/cpa-plugin/src/executor.rs; tests/live_checks.rs only | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0111 | RPC `executor.execute` (constant `MethodExecutorExecute`); JSON envelope OK/result/error, … | partial | crates/cpa-plugin/src/executor.rs; no Go-differential test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0112 | RPC `executor.execute_stream` (constant `MethodExecutorExecuteStream`); JSON envelope OK/r… | partial | crates/cpa-plugin/src/executor.rs; no Go-differential test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0113 | RPC `executor.count_tokens` (constant `MethodExecutorCountTokens`); JSON envelope OK/resul… | partial | crates/cpa-plugin/src/executor.rs; no Go-differential test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0114 | RPC `executor.http_request` (constant `MethodExecutorHTTPRequest`); JSON envelope OK/resul… | partial | crates/cpa-plugin/src/executor.rs; no Go-differential test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0115 | RPC `request.translate` (constant `MethodRequestTranslate`); JSON envelope OK/result/error… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: request.translate responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0116 | RPC `request.normalize` (constant `MethodRequestNormalize`); JSON envelope OK/result/error… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: request.normalize responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0117 | RPC `request.intercept_before` (constant `MethodRequestInterceptBefore`); JSON envelope OK… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: request.intercept_before responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0118 | RPC `request.intercept_after` (constant `MethodRequestInterceptAfter`); JSON envelope OK/r… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: request.intercept_after responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0119 | RPC `request.complete` (constant `MethodRequestComplete`); JSON envelope OK/result/error, … | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: complete call and records) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0120 | RPC `response.translate` (constant `MethodResponseTranslate`); JSON envelope OK/result/err… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: response.translate responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0121 | RPC `response.normalize_before` (constant `MethodResponseNormalizeBefore`); JSON envelope … | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: response.normalize_before responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0122 | RPC `response.normalize_after` (constant `MethodResponseNormalizeAfter`); JSON envelope OK… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: response.normalize_after responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0123 | RPC `response.intercept_after` (constant `MethodResponseInterceptAfter`); JSON envelope OK… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: response.intercept_after responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0124 | RPC `response.intercept_stream_chunk` (constant `MethodResponseInterceptStreamChunk`); JSO… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: response.intercept_stream_chunk responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0125 | RPC `websocket.response_event` (constant `MethodWebSocketResponseEvent`); JSON envelope OK… | partial | crates/cpa-plugin/src/interceptors.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: ws_event call) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0126 | RPC `thinking.identifier` (constant `MethodThinkingIdentifier`); JSON envelope OK/result/e… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: thinking.identifier responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0127 | RPC `thinking.apply` (constant `MethodThinkingApply`); JSON envelope OK/result/error, byte… | partial | crates/cpa-plugin/src/transform.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: thinking.apply responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0128 | RPC `usage.handle` (constant `MethodUsageHandle`); JSON envelope OK/result/error, byte pay… | partial | crates/cpa-plugin/src/transform.rs; no Go-differential test | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0129 | RPC `command_line.register` (constant `MethodCommandLineRegister`); JSON envelope OK/resul… | partial | crates/cpa-plugin/src/cli.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: command_line.register responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0130 | RPC `command_line.execute` (constant `MethodCommandLineExecute`); JSON envelope OK/result/… | partial | crates/cpa-plugin/src/cli.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: command_line.execute responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0131 | RPC `management.register` (constant `MethodManagementRegister`); JSON envelope OK/result/e… | covered | crates/cpa-plugin/src/management.rs; crates/cpa-server/src/plugins.rs register_management_routes; tests/plugin_routes.rs plugin_routes_match_go |  |  |
| M6-0132 | RPC `management.handle` (constant `MethodManagementHandle`); JSON envelope OK/result/error… | covered | crates/cpa-plugin/src/management.rs; crates/cpa-server/src/management/plugins.rs serve_management; tests/plugin_routes.rs plugin_routes_match_go |  |  |
| M6-0133 | RPC `quota.identifier` (constant `MethodQuotaIdentifier`); JSON envelope OK/result/error, … | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: quota.identifier responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0134 | RPC `quota.describe` (constant `MethodQuotaDescribe`); JSON envelope OK/result/error, byte… | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: quota.describe responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0135 | RPC `quota.fetch` (constant `MethodQuotaFetch`); JSON envelope OK/result/error, byte paylo… | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: quota.fetch responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0136 | RPC `quota.reset` (constant `MethodQuotaReset`); JSON envelope OK/result/error, byte paylo… | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: quota.reset responses) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0137 | RPC `host.http.do` (constant `MethodHostHTTPDo`); JSON envelope OK/result/error, byte payl… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0138 | RPC `host.http.do_stream` (constant `MethodHostHTTPDoStream`); JSON envelope OK/result/err… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0139 | RPC `host.http.operation_open` (constant `MethodHostHTTPOperationOpen`); JSON envelope OK/… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0140 | RPC `host.http.cancel` (constant `MethodHostHTTPCancel`); JSON envelope OK/result/error, b… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0141 | RPC `host.http.stream_read` (constant `MethodHostHTTPStreamRead`); JSON envelope OK/result… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0142 | RPC `host.http.stream_close` (constant `MethodHostHTTPStreamClose`); JSON envelope OK/resu… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0143 | RPC `host.model.execute` (constant `MethodHostModelExecute`); JSON envelope OK/result/erro… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0144 | RPC `host.model.execute_stream` (constant `MethodHostModelExecuteStream`); JSON envelope O… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0145 | RPC `host.model.stream_read` (constant `MethodHostModelStreamRead`); JSON envelope OK/resu… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0146 | RPC `host.model.stream_close` (constant `MethodHostModelStreamClose`); JSON envelope OK/re… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's HTTP, model-execution and auth services behind the host. |
| M6-0147 | RPC `host.stream.emit` (constant `MethodHostStreamEmit`); JSON envelope OK/result/error, b… | partial | crates/cpa-plugin/src/callbacks.rs, streams.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: host.stream.emit in the management calls step) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0148 | RPC `host.stream.close` (constant `MethodHostStreamClose`); JSON envelope OK/result/error,… | partial | crates/cpa-plugin/src/callbacks.rs, streams.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: host.stream.close in the management calls step) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0149 | RPC `host.log` (constant `MethodHostLog`); JSON envelope OK/result/error, byte payload and… | partial | crates/cpa-plugin/src/callbacks.rs host_log; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json: host.log in the management calls step) | plugins | Implemented and Go-golden tested in cpa-plugin; cpa-server loads plugins and serves their management routes (Plugins 3), but the request path never calls this capability. |
| M6-0150 | RPC `host.auth.list` (constant `MethodHostAuthList`); JSON envelope OK/result/error, byte … | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's auth store and session affinity behind the host. |
| M6-0151 | RPC `host.auth.get` (constant `MethodHostAuthGet`); JSON envelope OK/result/error, byte pa… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's auth store and session affinity behind the host. |
| M6-0152 | RPC `host.auth.get_runtime` (constant `MethodHostAuthGetRuntime`); JSON envelope OK/result… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's auth store and session affinity behind the host. |
| M6-0153 | RPC `host.auth.save` (constant `MethodHostAuthSave`); JSON envelope OK/result/error, byte … | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's auth store and session affinity behind the host. |
| M6-0154 | RPC `host.affinity.lookup` (constant `MethodHostAffinityLookup`); JSON envelope OK/result/… | missing | crates/cpa-plugin/src/callbacks.rs call_service answers "unsupported host callback" | plugins | Needs the server's auth store and session affinity behind the host. |

### M6: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Area | Note |
|---|---|---|---|---|---|
| M6-0155 | internal/api/handlers/management/auth_files_plugin_oauth_test.go | missing | 0/8 cases matched; auth_files_plugin_oauth.go not cited | manage |  |
| M6-0156 | internal/api/handlers/management/plugin_quota_test.go | partial | implementation cites plugin_quota.go (crates/cpa-server/src/management.rs, crates/cpa-server/src/management/quota.rs (+1)); no case matched by name | manage |  |
| M6-0157 | internal/api/handlers/management/plugin_store_release_test.go | partial | implementation cites plugin_store_release.go (crates/cpa-server/src/management/plugin_store.rs); no case matched by name | manage |  |
| M6-0158 | internal/api/handlers/management/plugin_store_test.go | partial | implementation cites plugin_store.go (crates/cpa-server/src/management.rs, crates/cpa-server/src/management/plugin_store.rs); no case matched by name | manage |  |
| M6-0159 | internal/api/handlers/management/plugins_test.go | partial | implementation cites plugins.go (crates/cpa-server/src/management/plugins.rs); no case matched by name | manage |  |
| M6-0160 | internal/config/discovery_test.go | partial | crates/cliproxy/src/discovery/cli.rs load_scan_filters (discovery_cmd_go.json config_filters) | tui | Not ported by name. |
| M6-0161 | internal/config/home_test.go | covered | 2/2 cases: crates/cliproxy/src/home.rs, crates/cpa-home/src/config.rs |  |  |
| M6-0162 | internal/config/plugin_config_save_test.go | missing | 0/8 cases matched; plugin_config_save.go not cited | manage |  |
| M6-0163 | internal/config/plugin_config_test.go | partial | crates/cpa-plugin/src/config.rs (items_follow_go, disabled_section_keeps_defaults, dir_resolution_matches_go) | plugins | The store-sources, store-auth and auth-revision cases have no counterpart (no plugin store). |
| M6-0164 | internal/discovery/discovery_test.go | partial | crates/cliproxy/src/discovery/tests.rs (Go-recorded outputs: instance ID, TXT build/parse, validation, interface filters, merge limits) | tui | Advertiser idempotence/shutdown and the live browse integration are not ported. |
| M6-0165 | internal/home/client_test.go | partial | 42/69 cases: crates/cpa-home/src/client_tests.rs; not matched: TestAuthDispatchRequestDefaultsCountToOne, TestAuthDispatchRequestIncludesCredentialPolicy, TestAuthDispatchRequestIncludesExcludedAuthIDs, TestAuthDispatchRequestIncludesEmptyExcludedAuthIDs … | home |  |
| M6-0166 | internal/home/concurrency_release_test.go | covered | 9/9 cases: crates/cpa-home/src/release.rs |  |  |
| M6-0167 | internal/home/in_flight_contract_test.go | covered | 2/2 cases: crates/cpa-home/src/inflight.rs |  |  |
| M6-0168 | internal/home/kv_helpers_test.go | covered | 5/5 cases: crates/cpa-home/src/kv.rs |  |  |
| M6-0169 | internal/home/plugin_status_test.go | missing | 0/4 cases matched; plugin_status.go not cited | plugins |  |
| M6-0170 | internal/homeplugins/network_scope_test.go | missing | 0/1 cases matched; network_scope.go not cited | plugins |  |
| M6-0171 | internal/homeplugins/sync_test.go | missing | 0/21 cases matched; sync.go not cited | plugins |  |
| M6-0172 | internal/logging/home_app_log_forwarder_test.go | covered | 10/10 cases: crates/cliproxy/src/home.rs, crates/cpa-home/src/applog.rs |  |  |
| M6-0173 | internal/logging/request_logger_home_test.go | missing | 0/8 cases matched; request_logger_home.go not cited | home |  |
| M6-0174 | internal/pluginhost/adapters_executors_usage_test.go | missing | 0/16 cases matched; adapters_executors_usage.go not cited | plugins |  |
| M6-0175 | internal/pluginhost/adapters_test.go | partial | implementation cites adapters.go (crates/cpa-plugin/src/models.rs); no case matched by name | plugins |  |
| M6-0176 | internal/pluginhost/affinity_callbacks_test.go | partial | implementation cites affinity_callbacks.go (crates/cpa-plugin/src/hostauth.rs, crates/cpa-server/tests/plugin_host_auth.rs); no case matched by name | plugins |  |
| M6-0177 | internal/pluginhost/auth_callbacks_test.go | partial | implementation cites auth_callbacks.go (crates/cpa-plugin/src/hostauth.rs, crates/cpa-server/tests/plugin_host_auth.rs); no case matched by name | plugins |  |
| M6-0178 | internal/pluginhost/auth_provider_test.go | partial | implementation cites auth_provider.go (crates/cpa-plugin/src/auth.rs); no case matched by name | plugins |  |
| M6-0179 | internal/pluginhost/client_guard_test.go | covered | 1/1 cases: crates/cpa-plugin/src/client.rs |  |  |
| M6-0180 | internal/pluginhost/command_line_test.go | partial | implementation cites command_line.go (crates/cpa-plugin/src/cli.rs); no case matched by name | plugins |  |
| M6-0181 | internal/pluginhost/config_test.go | partial | 1/4 cases: crates/cpa-plugin/src/config.rs; not matched: TestRuntimeConfigYAMLDefaultsEnabledFalse, TestRuntimeConfigFromConfigExtractsStoreVersion, TestRuntimeConfigFromConfigDerivesStoreVersionFromReleaseTag | plugins |  |
| M6-0182 | internal/pluginhost/host_callbacks_test.go | partial | implementation cites host_callbacks.go (crates/cpa-plugin/src/callbacks.rs, crates/cpa-plugin/src/hosthttp.rs); no case matched by name | plugins |  |
| M6-0183 | internal/pluginhost/host_model_stream_callbacks_test.go | missing | 0/1 cases matched; host_model_stream_callbacks.go not cited | plugins |  |
| M6-0184 | internal/pluginhost/host_test.go | partial | implementation cites host.go (crates/cpa-plugin/src/host.rs); no case matched by name | plugins |  |
| M6-0185 | internal/pluginhost/http_bridge_test.go | partial | implementation cites http_bridge.go (crates/cpa-plugin/src/hosthttp.rs, crates/cpa-plugin/tests/host_http.rs); no case matched by name | plugins |  |
| M6-0186 | internal/pluginhost/http_operation_bridge_test.go | partial | implementation cites http_operation_bridge.go (crates/cpa-plugin/src/hosthttp.rs); no case matched by name | plugins |  |
| M6-0187 | internal/pluginhost/loader_windows_test.go | missing | 0/7 cases matched; loader_windows.go not cited | plugins |  |
| M6-0188 | internal/pluginhost/logging_test.go | missing | 0/3 cases matched; logging.go not cited | plugins |  |
| M6-0189 | internal/pluginhost/management_test.go | partial | implementation cites management.go (crates/cpa-plugin/src/management.rs); no case matched by name | plugins |  |
| M6-0190 | internal/pluginhost/model_router_test.go | partial | implementation cites model_router.go (crates/cpa-plugin/src/routing.rs); no case matched by name | plugins |  |
| M6-0191 | internal/pluginhost/platform_test.go | partial | implementation cites platform.go (crates/cpa-plugin/src/platform.rs); no case matched by name | plugins |  |
| M6-0192 | internal/pluginhost/plugin_refresh_compat_executor_test.go | missing | 0/4 cases matched; plugin_refresh_compat_executor.go not cited | plugins |  |
| M6-0193 | internal/pluginhost/quota_provider_test.go | partial | implementation cites quota_provider.go (crates/cpa-plugin/src/quota.rs); no case matched by name | plugins |  |
| M6-0194 | internal/pluginhost/request_lifecycle_test.go | missing | 0/4 cases matched; request_lifecycle.go not cited | plugins |  |
| M6-0195 | internal/pluginhost/rpc_client_error_test.go | missing | 0/6 cases matched; rpc_client_error.go not cited | plugins |  |
| M6-0196 | internal/pluginhost/rpc_client_stream_test.go | partial | implementation cites rpc_client_stream.go (crates/cpa-plugin/src/executor.rs); no case matched by name | plugins |  |
| M6-0197 | internal/pluginhost/rpc_schema_test.go | partial | implementation cites rpc_schema.go (crates/cpa-plugin/src/rpc.rs); no case matched by name | plugins |  |
| M6-0198 | internal/pluginhost/scheduler_test.go | partial | implementation cites scheduler.go (crates/cpa-plugin/src/routing.rs); no case matched by name | plugins |  |
| M6-0199 | internal/pluginhost/stream_bridge_test.go | partial | 1/6 cases: crates/cpa-plugin/src/streams.rs; not matched: TestStreamBridgeEmitUsesAcceptedPumpResultAfterContextCancellation, TestStreamBridgeAbortClosesSaturatedStreamWithoutConsumer, TestStreamBridgeCleanupAbortsPendingGracefulClose, TestStreamBridgeCloseDeliversTerminalError … | plugins |  |
| M6-0200 | internal/pluginhost/websocket_observer_test.go | partial | crates/cpa-plugin/src/interceptors.rs observe_websocket_response_event; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json ws_event step) | plugins | Not ported by name; no WebSocket path calls the observer. |
| M6-0201 | internal/pluginstore/auth_test.go | partial | implementation cites auth.go (crates/cpa-plugin/src/store/auth.rs); no case matched by name | plugins |  |
| M6-0202 | internal/pluginstore/github_rate_limit_test.go | partial | implementation cites github_rate_limit.go (crates/cpa-plugin/src/store/rate_limit.rs); no case matched by name | plugins |  |
| M6-0203 | internal/pluginstore/github_test.go | partial | implementation cites github.go (crates/cpa-plugin/src/store/client.rs); no case matched by name | plugins |  |
| M6-0204 | internal/pluginstore/home_sync_test.go | partial | implementation cites home_sync.go (crates/cpa-plugin/src/store/home_sync.rs); no case matched by name | plugins |  |
| M6-0205 | internal/pluginstore/install_test.go | partial | implementation cites install.go (crates/cpa-plugin/src/store/install.rs); no case matched by name | plugins |  |
| M6-0206 | internal/pluginstore/registry_test.go | partial | implementation cites registry.go (crates/cpa-plugin/src/store/registry.rs); no case matched by name | plugins |  |
| M6-0207 | internal/pluginstore/request_identity_test.go | partial | implementation cites request_identity.go (crates/cpa-plugin/src/store/client.rs); no case matched by name | plugins |  |
| M6-0208 | internal/pluginstore/version_test.go | partial | implementation cites version.go (crates/cpa-plugin/src/store/version.rs); no case matched by name | plugins |  |
| M6-0209 | internal/redisqueue/plugin_test.go | partial | implementation cites plugin.go (crates/cpa-server/src/usage_record.rs); no case matched by name | server |  |
| M6-0210 | internal/runtime/executor/antigravity_home_model_capabilities_test.go | missing | 0/1 cases matched; antigravity_home_model_capabilities.go not cited | google |  |
| M6-0211 | internal/runtime/executor/helps/configuration_update_plugin_test.go | missing | 0/3 cases matched; configuration_update_plugin.go not cited | plugins | Plugin request translators are not wired into the request pair. |
| M6-0212 | internal/runtime/executor/helps/home_refresh_test.go | partial | 9/11 cases: crates/cpa-home/src/refresh.rs; not matched: TestRefreshAuthViaHomePreservesContextErrors, TestAuthAccessTokenSHA256SupportsKnownMetadataShapes | home |  |
| M6-0213 | internal/runtime/executor/helps/request_pair_plugin_host_test.go | missing | 0/1 cases matched; request_pair_plugin_host.go not cited | plugins | Plugin request translators are not wired into the request pair. |
| M6-0214 | internal/runtime/executor/home_codex_terminal_test.go | missing | 0/1 cases matched; home_codex_terminal.go not cited | codex |  |
| M6-0215 | internal/runtime/executor/kimi_home_model_capabilities_test.go | missing | 0/1 cases matched; kimi_home_model_capabilities.go not cited | device-providers |  |
| M6-0216 | internal/runtime/executor/openai_compat_home_options_test.go | missing | 0/4 cases matched; openai_compat_home_options.go not cited | openai-xai |  |
| M6-0217 | internal/runtime/executor/xai_configuration_update_plugin_test.go | missing | 0/1 cases matched; xai_configuration_update_plugin.go not cited | openai-xai |  |
| M6-0218 | internal/tui/client_test.go | missing | no terminal UI client | tui |  |
| M6-0219 | internal/tui/oauth_tab_test.go | partial | 1/8 cases: crates/cliproxy/src/tui/oauth.rs; not matched: TestShouldAcceptOAuthPollFiltersStaleMessages, TestShouldAcceptOAuthStartFiltersStaleMessages, TestOAuthTabUpdateIgnoresStalePollMsg, TestOAuthTabUpdateAcceptsCurrentPollMsg … | tui |  |
| M6-0220 | sdk/api/handlers/handlers_plugin_executor_usage_test.go | missing | 0/14 cases matched; handlers_plugin_executor_usage.go not cited | plugins |  |
| M6-0221 | sdk/cliproxy/auth/home_concurrency_test.go | partial | 19/22 cases: crates/cliproxy/src/home.rs, crates/cpa-home/src/dispatch.rs; not matched: TestPickHomeDispatchSelectionReleasesAccountedScopeAfterPayloadDecodeFailure, TestPickHomeDispatchSelectionReleasesAccountedScopeAfterAuthDecodeFailure, TestRetryAfterFromWrappedHomeBusyError | home |  |
| M6-0222 | sdk/cliproxy/auth/home_configuration_update_test.go | covered | 1/1 cases: crates/cliproxy/src/home.rs |  |  |
| M6-0223 | sdk/cliproxy/auth/home_dispatch_headers_test.go | covered | 4/4 cases: crates/cpa-server/src/remote.rs |  |  |
| M6-0224 | sdk/cliproxy/auth/home_execution_paths_test.go | partial | 9/28 cases: crates/cliproxy/src/home.rs; not matched: TestHomeModeNeverAuthorizesLocalAuthFallback, TestHomeSelectionEndsAfterExecute, TestHomeNonStreamingExecutionLogsSelectedOAuthAuth, TestHomeSelectionClosesAttemptAndWebSocketResources … | home |  |
| M6-0225 | sdk/cliproxy/auth/home_fallback_audit_test.go | missing | 0/2 cases matched; home_fallback_audit.go not cited | home |  |
| M6-0226 | sdk/cliproxy/auth/home_force_mapping_test.go | partial | 3/9 cases: crates/cliproxy/src/home.rs; not matched: TestHomeNonForceAliasSessionReuseAndTargetChangeReleasesAccountedModel, TestHomeAuthSelectionRouteRetainsRequestedResponseAliasAcrossWebsocketReuse, TestHomeForceMappingAliasChangeEndsAndFlushesBeforeRedispatch, TestHomeRetainedRouteRewritesReasoningSuffixAndWaitsForReleaseACK … | home |  |
| M6-0227 | sdk/cliproxy/auth/home_in_flight_publisher_test.go | covered | 18/18 cases: crates/cliproxy/src/home.rs, crates/cpa-home/src/inflight.rs (+2) |  |  |
| M6-0228 | sdk/cliproxy/auth/home_retry_contract_test.go | partial | 33/34 cases: crates/cliproxy/src/home.rs, crates/cpa-server/src/dispatch.rs (+1); not matched: TestHomeExcludedCredentialEndsRetainedWebsocketSelection | home |  |
| M6-0229 | sdk/cliproxy/auth/home_retry_loop_test.go | covered | 1/1 cases: crates/cliproxy/src/home.rs |  |  |
| M6-0230 | sdk/cliproxy/auth/home_selected_auth_callback_test.go | missing | 0/1 cases matched; home_selected_auth_callback.go not cited | home |  |
| M6-0231 | sdk/cliproxy/auth/home_selection_attempt_test.go | missing | 0/3 cases matched; home_selection_attempt.go not cited | home |  |
| M6-0232 | sdk/cliproxy/auth/home_selection_test.go | partial | 2/7 cases: crates/cliproxy/src/home.rs; not matched: TestHomeDispatchSelectionOwnsScopeOutsideAuth, TestHomeDispatchSelectionReplaceAuthPreservesRoutingAttributes, TestHomeDispatchSelectionReplaceAuthConcurrentClone, TestReplaceHomeSelectionAuthUpdatesRetainedRuntimeAuth … | home |  |
| M6-0233 | sdk/cliproxy/auth/home_session_alias_test.go | partial | 15/18 cases: crates/cliproxy/src/home.rs, crates/cpa-home/src/session_alias.rs; not matched: TestHomeDispatchSessionIDsMatchesLCPCompaction, TestHomeDispatchSessionIDsMatchesLCPCompactionViaReportHomeResult, TestHomeDispatchSessionIDsWithoutPresetProviderMetadata | home |  |
| M6-0234 | sdk/cliproxy/auth/home_unauthorized_refresh_test.go | partial | 3/9 cases: crates/cliproxy/src/home.rs; not matched: TestHomeUnauthorizedDoesNotRefreshRetainedSelection, TestRefreshHomeSelectionReusesConcurrentNewerToken, TestHomeNoCandidatePreservesOriginalUpstreamError, TestHomeUnauthorizedStreamDoesNotRefreshOrReplay … | home |  |
| M6-0235 | sdk/cliproxy/auth/home_v8_model_capabilities_test.go | covered | 5/5 cases: crates/cliproxy/src/home.rs, crates/cpa-exec/src/codex_request.rs |  |  |
| M6-0236 | sdk/cliproxy/auth/home_web_search_capability_test.go | covered | 1/1 cases: crates/cliproxy/src/home.rs |  |  |
| M6-0237 | sdk/cliproxy/auth/home_websocket_reuse_test.go | missing | 0/11 cases matched; home_websocket_reuse.go not cited | home |  |
| M6-0238 | sdk/cliproxy/discovery_advertiser_test.go | partial | implementation cites discovery_advertiser.go (crates/cliproxy/src/discovery/mod.rs); no case matched by name | tui |  |
| M6-0239 | sdk/cliproxy/home_plugins_test.go | missing | 0/19 cases matched; home_plugins.go not cited | plugins |  |
| M6-0240 | sdk/cliproxy/service_plugin_executor_test.go | missing | 0/1 cases matched; service_plugin_executor.go not cited | plugins |  |
| M6-0241 | sdk/cliproxy/service_plugin_refresh_executor_test.go | missing | 0/4 cases matched; service_plugin_refresh_executor.go not cited | plugins |  |
| M6-0242 | sdk/cliproxy/service_plugin_scheduler_test.go | missing | 0/3 cases matched; service_plugin_scheduler.go not cited | plugins |  |
| M6-0243 | sdk/pluginabi/types_test.go | partial | crates/cpa-plugin/src/abi.rs (envelopes_match_go_bytes) | plugins | Not ported by name. |
| M6-0244 | sdk/pluginapi/types_test.go | partial | crates/cpa-plugin/src/api.rs, gojson (model_info_round_trips_go_field_names, scheduler and quota decode tests) | plugins | Not ported by name. |
| M6-0245 | sdk/pluginhost/host_test.go | partial | implementation cites host.go (crates/cpa-plugin/src/host.rs); no case matched by name | plugins |  |
| M6-0246 | sdk/pluginhost/quota_test.go | partial | crates/cpa-plugin/src/quota.rs; crates/cpa-plugin/tests/go_host.rs (pluginhost_go.json quota steps) | plugins | Not ported by name. |
| M6-0247 | sdk/pluginstore/network_scope_test.go | missing | 0/2 cases matched; network_scope.go not cited | plugins |  |
| M6-0248 | sdk/pluginstore/pluginstore_test.go | missing | 0/6 cases matched; pluginstore.go not cited | plugins |  |
