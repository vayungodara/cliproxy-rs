# Parity status against docs/PARITY.md

Audit of master `4abce40` against CLIProxyAPI `6fecc6e`, item by item. Milestones audited so far: M1, M2, M3, M4. The rest follow in later deliveries, and the audit is re-run at the end.

Statuses:

- **covered**: implemented, and Rust tests or Go-generated fixtures exercise it.
- **partial**: implemented in part, or implemented without tests that pin Go's behaviour. For Go test suites: the behaviour exists and is exercised, but not every Go case is ported.
- **missing**: not implemented.

Gap owner names the thread that should close a partial or missing item. Threads: ultra/claude, ultra/codex, ultra/google, ultra/device-providers (Kimi, Meta, Devin), ultra/openai-xai, ultra/server, ultra/manage, ultra/dashboard, ultra/translate, ultra/realtime ("Realtime and Live"), ultra/plugins ("Plugins"), ultra/home ("Home control plane, credential concurrency, storage") and ultra/tui ("TUI and LAN discovery", also crates/cliproxy startup).

Method: `docs/parity-audit/audit.py` regenerates this file. Routes come from `probe.py`, which starts the binary and requests every listed method and path without credentials (routed pairs answer from the auth guard or handler, unrouted ones 404/405). A route counts as covered when a test requests it. Go test suites are matched case by case against Go test names cited in Rust code and in Go-generated fixtures (fixture names carry `TestName:line`). Every other item, and every suite the matcher cannot see, was judged by reading the Rust code and tests; those judgments live in `docs/parity-audit/manual.tsv` with their evidence. "Not ported by name" means the area is implemented and tested through other cases (usually Go-generated end-to-end scenarios), but the Go suite's own cases are not reproduced one by one.

## Summary

| Milestone | Items | covered | partial | missing |
|---|---:|---:|---:|---:|
| M1 | 118 | 48 | 70 | 0 |
| M2 | 158 | 77 | 72 | 9 |
| M3 | 350 | 143 | 130 | 77 |
| M4 | 510 | 202 | 259 | 49 |

### Gaps by owner

| Owner | missing | partial | Missing items |
|---|---:|---:|---|
| ultra/google | 66 | 12 | M3-0019, M3-0033, M3-0035, M3-0036, M3-0046, M3-0048, M3-0054, M3-0058, M3-0059, M3-0060, M3-0061, M3-0062, M3-0063, M3-0164, M3-0165, M3-0166, M3-0167, M3-0168, M3-0169, M3-0170, M3-0171, M3-0172, M3-0173, M3-0175, M3-0188, M3-0222, M3-0225, M3-0226, M3-0227, M3-0228, M3-0229, M3-0230, M3-0231, M3-0232, M3-0233, M3-0234, M3-0235, M3-0236, M3-0237, M3-0238, M3-0239, M3-0240, M3-0241, M3-0242, M3-0268, M3-0269, M3-0270, M3-0283, M3-0287, M3-0329, M3-0334, M3-0335, M3-0336, M3-0342, M3-0344, M4-0063, M4-0160, M4-0161, M4-0162, M4-0163, M4-0164, M4-0165, M4-0166, M4-0167, M4-0168, M4-0415 |
| ultra/server | 34 | 217 | M2-0036, M2-0134, M3-0221, M4-0001, M4-0002, M4-0003, M4-0004, M4-0005, M4-0006, M4-0007, M4-0008, M4-0009, M4-0010, M4-0011, M4-0037, M4-0048, M4-0051, M4-0061, M4-0062, M4-0255, M4-0256, M4-0257, M4-0307, M4-0356, M4-0369, M4-0370, M4-0371, M4-0372, M4-0373, M4-0374, M4-0375, M4-0376, M4-0390, M4-0485 |
| ultra/openai-xai | 16 | 23 | M2-0139, M2-0150, M3-0001, M3-0002, M3-0003, M3-0004, M3-0005, M3-0006, M3-0007, M3-0009, M3-0010, M3-0011, M3-0056, M3-0057, M3-0154, M4-0259 |
| ultra/codex | 12 | 56 | M2-0145, M2-0146, M2-0147, M2-0148, M2-0149, M3-0142, M3-0246, M3-0262, M3-0263, M4-0033, M4-0268, M4-0269 |
| ultra/home | 3 | 9 | M4-0245, M4-0361, M4-0362 |
| ultra/realtime | 3 | 4 | M3-0174, M3-0209, M3-0210 |
| ultra/manage | 1 | 43 | M3-0186 |
| ultra/translate | 0 | 81 | — |
| ultra/claude | 0 | 59 | — |
| ultra/device-providers | 0 | 21 | — |
| ultra/tui | 0 | 3 | — |
| ultra/plugins | 0 | 2 | — |
| ultra/codex, ultra/openai-xai | 0 | 1 | — |

## M1

### M1: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0001 | GET /healthz | covered | probe: GET /healthz -> 200; tests: crates/cpa-server/tests/claude_passthrough.rs, crates/cpa-server/tests/routes.rs |  |  |
| M1-0002 | HEAD /healthz | covered | probe: HEAD /healthz -> 200; tests: crates/cpa-server/tests/claude_passthrough.rs, crates/cpa-server/tests/routes.rs |  |  |
| M1-0003 | GET /v1/models | partial | probe GET /v1/models -> 401; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/codex, ultra/openai-xai | Anthropic and OpenAI catalogs only: the Grok Shell and Codex (client_version) catalog formats fall back to the OpenAI list (ponytail in crates/cpa-server/src/models.rs). |
| M1-0004 | POST /v1/messages | covered | probe: POST /v1/messages -> 401; tests: crates/cpa-server/tests/claude_passthrough.rs, crates/cpa-server/tests/routes.rs (+1) |  |  |
| M1-0005 | POST /v1/messages/count_tokens | partial | probe POST /v1/messages/count_tokens -> 401; executor count path: claude scenarios count-oauth-mid-system, claude::tests::custom_origin_counts_locally_without_sending_credentials | ultra/server | No server test drives the route to a Claude credential. |
| M1-0006 | GET / | covered | crates/cpa-server/tests/routes.rs misc_routes_match_go (GET /) |  |  |
| M1-0007 | GET /anthropic/callback | covered | probe: GET /anthropic/callback -> 200; tests: crates/cpa-server/tests/routes.rs |  |  |

### M1: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0008 | Local OAuth listener `ANY /callback` (browser flow uses GET; ServeMux registration has no … | covered | crates/cpa-exec/src/claude_login_tests.rs callback_server_answers_like_go, success_page_escapes_the_platform_url |  |  |
| M1-0009 | Local OAuth listener `ANY /success` (browser flow uses GET; ServeMux registration has no m… | covered | crates/cpa-exec/src/claude_login_tests.rs callback_server_answers_like_go (302 to /success, success page) |  |  |
| M1-0010 | Client auth input compatibility: Bearer Authorization, `X-Api-Key`, `X-Goog-Api-Key`, and … | covered | crates/cpa-server/src/access.rs (Bearer, X-Goog-Api-Key, X-Api-Key, key/auth_token query; url.ParseQuery tests); gemini_routes.rs; routes.rs 401 envelope |  |  |

### M1: 3. Upstream providers, auth flows, persisted records, and special behavior

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0011 | Claude | covered | crates/cpa-exec/src/oauth_tests.rs pkce_and_authorize_url, code_exchange_login_layout_and_atomic_permissions, refresh_*; claude_login_tests.rs browser_login_writes_go_file_and_migrates_the_legacy_one |  |  |

### M1: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0012 | Claude on-disk typed core | covered | crates/cpa-exec/src/oauth_tests.rs code_exchange_login_layout_and_atomic_permissions, missing_rotated_refresh_and_profile_fields_keep_saved_values; claude_login_tests.rs browser_login_writes_go_file_and_migrates_the_legacy_one |  |  |

### M1: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0013 | Native-vs-cloaked policy must distinguish strong verified CLI signals, entrypoint, exact m… | covered | crates/cpa-exec/src/claude/detect.rs; claude scenarios native-confirmed-stream, apikey-firstparty-default, apikey-firstparty-cli, apikey-cloak-always |  |  |
| M1-0014 | Cloaking builds Claude Code billing and identity system blocks, assigns stable credential … | covered | crates/cpa-exec/src/claude/cloak.rs; claude scenarios oauth-plain, oauth-opus55-complex, oauth-strict-sensitive, oauth-subagent, oauth-legacy-stream |  |  |
| M1-0015 | CCH billing signing is automatic only on native supported origins (Anthropic and Vertex); … | covered | crates/cpa-exec/src/claude/signing.rs; cch values in claude scenarios and crates/cpa-exec/src/claude/testdata/go_captures.json |  |  |
| M1-0016 | Stable header/software baseline: claude-cli/2.1.280 (external, cli), Stainless 0.112.1, ru… | partial | crates/cpa-exec/src/claude/profile.rs (BASELINE, 7-day PROFILE_TTL); claude scenarios | ultra/home | Home KV profile mode (shared profiles, 5 s write lock) is not ported (ponytail in profile.rs). |
| M1-0017 | Do not substitute generic browser TLS for native CLI: Claude Messages/count_tokens uses de… | covered | crates/cpa-exec/src/tls.rs capture_both_clienthellos_against_go_source_profile, transport_lru_bound; harness ClientHello and resumption comparison (integrator run on bb41570: 38 hellos structurally identical) |  |  |
| M1-0018 | OAuth acquisition/profile inspection has its own ordered HTTP/1.1 header profiles and comp… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case; crates/cpa-exec/src/tls.rs ClientHello capture |  |  |
| M1-0019 | MCP tool aliasing rewrites names consistently in declarations, tool_use/tool_result histor… | partial | crates/cpa-exec/src/claude/alias.rs; {{ALIAS}}/{{DRIFT}} replies in claude scenarios | ultra/claude | Go's legacy sjson rewrite for malformed JSON is not ported; such bodies pass through unaliased (ponytail in alias.rs). |
| M1-0020 | Thinking replay and signature caches distinguish validated signed thinking, redacted think… | covered | crates/cpa-exec/src/claude/replay.rs (5 tests); claude::tests::compat_replay_sequence_matches_go; claude scenarios replay-1-store, replay-2-restore, oauth-signature-history; cpa_common::signature recorded replay |  |  |
| M1-0021 | Anthropic quota rejection is not every 429: shared 5h/7d/7d_oi limits, utilization/status,… | covered | crates/cpa-exec/src/quota.rs model_shared_and_fast_entitlement_scopes, overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not, latest_relevant_deadline_fractional_seconds_and_dates; claude scenarios oauth-fast-*, oauth-unified-429, oauth-model-429; crates/cpa-server/tests/scheduler_attempts.rs |  |  |
| M1-0022 | Exact Messages header order: `Accept` → `Authorization` → `Content-Type` → `User-Agent` → … | covered | crates/cpa-exec/src/claude/headers.rs; harness wire captures (upstream requests differ only in x-client-request-id) |  |  |
| M1-0023 | Exact count_tokens header order: `Accept` → `Authorization` → `Content-Type` → `User-Agent… | covered | crates/cpa-exec/src/claude/headers.rs; harness count-tokens capture (crates/cpa-exec/src/claude/testdata/go_captures.json count-tokens) |  |  |
| M1-0024 | Exact OAuth token header order: `Accept` → `Content-Type` → `User-Agent` → `Content-Length… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case |  |  |
| M1-0025 | Exact OAuth inspection header order: `Accept` → `Content-Type` → `Authorization` → `Cache-… | covered | crates/cpa-exec/src/oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case |  |  |
| M1-0026 | Managed beta spelling inventory (conditional, NOT all sent on every request): `token-count… | covered | crates/cpa-exec/src/claude/betas.rs; anthropic-beta headers in every claude scenario |  |  |

### M1: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0027 | oauth.providers.claude.claude-code.disable-cloaking-model-list | partial | read in crates/cpa-server/src/models.rs | ultra/server | No test sets it. |
| M1-0028 | access.api-keys | covered | crates/cpa-core/src/config.rs; crates/cpa-server/tests/routes.rs and claude_passthrough.rs configure access.api-keys |  | heuristic: key name match |
| M1-0029 | api-keys.claude[].keys[].api-key | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios configure api-keys.claude |  | heuristic: key name match |
| M1-0030 | api-keys.claude[].base-url | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios apikey-gateway-cli-stream, apikey-cloak-always (base-url) |  | heuristic: key name match |
| M1-0031 | api-keys.claude[].keys[].models[].name | covered | crates/cpa-core/src/config/credentials.rs; claude scenarios with model aliases (apikey-firstparty-oauth-cli-alias) |  | heuristic: key name match |
| M1-0032 | api-keys.claude[].keys[].models[].display-name | covered | crates/cpa-core/src/config/credentials.rs config_models_reach_the_registry_like_go (display names reach the registry; the test configures codex and vertex keys, Claude keys take the same path); crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M1-0033 | api-keys.claude[].keys[].models[].max-context-length | partial | parsed into crates/cpa-core/src/registry/dynamic.rs max_context_length; cpa_common::codex_client carries the embedded catalog | ultra/codex | The client_version catalog that advertises it is not served on /v1/models. |
| M1-0034 | api-keys.claude[].keys[].models[].is-compat | covered | claude scenarios apikey-compat-openai-reasoning, apikey-plain-openai-reasoning; compat scenarios claude_is_compat_keeps_thinking, claude_not_compat_drops_thinking |  | heuristic: key name match |
| M1-0035 | api-keys.claude[].keys[].models[].thinking.min | covered | claude scenarios apikey-resolved-thinking-in-range, apikey-resolved-thinking-out-of-range |  | heuristic: key name match |
| M1-0036 | api-keys.claude[].keys[].models[].thinking.max | covered | claude scenarios apikey-resolved-thinking-in-range, apikey-resolved-thinking-out-of-range |  | heuristic: key name match |
| M1-0037 | api-keys.claude[].keys[].models[].thinking.zero-allowed | partial | plumbed through crates/cpa-core/src/registry.rs to cpa_common::thinking (recorded replay covers the thinking logic) | ultra/server | No test configures it through config.yaml. |
| M1-0038 | api-keys.claude[].keys[].models[].thinking.dynamic-allowed | partial | plumbed through crates/cpa-core/src/registry.rs to cpa_common::thinking (recorded replay covers the thinking logic) | ultra/server | No test configures it through config.yaml. |
| M1-0039 | api-keys.claude[].keys[].models[].thinking.levels | partial | crates/cpa-core/src/registry/dynamic.rs; crates/cpa-translate/tests/registry_overlay.rs (overlay levels) | ultra/server | No test configures it through config.yaml. |
| M1-0040 | api-keys.claude[].keys[].headers | covered | cpa_common::headers custom_headers; claude scenario with headers config |  | heuristic: key name match |
| M1-0041 | api-keys.claude[].keys[].rebuild-mid-system-message | partial | crates/cpa-exec/src/claude/settings.rs, reconcile.rs | ultra/claude | No test sets rebuild-mid-system-message. |
| M1-0042 | api-keys.claude[].keys[].cloak.mode | covered | claude scenario apikey-cloak-always (cloak.mode always) |  | heuristic: key name match |
| M1-0043 | api-keys.claude[].keys[].cloak.strict-mode | partial | crates/cpa-exec/src/claude/settings.rs; behaviour covered through credential attributes (claude scenario oauth-strict-sensitive, cloak_strict_mode) | ultra/claude | The config.yaml path is not exercised. |
| M1-0044 | api-keys.claude[].keys[].cloak.sensitive-words | partial | crates/cpa-exec/src/claude/settings.rs; behaviour covered through credential attributes (claude scenario oauth-strict-sensitive, cloak_sensitive_words) | ultra/claude | The config.yaml path is not exercised. |
| M1-0045 | api-keys.claude[].keys[].cloak.cache-user-id | covered | claude scenario apikey-cloak-always (cache-user-id: true) |  | heuristic: key name match |
| M1-0046 | api-keys.claude[].keys[].fingerprint-profile | covered | claude scenarios apikey-firstparty-cli, apikey-gateway-cli-stream, apikey-firstparty-oauth-cli-alias (fingerprint-profile) |  | heuristic: key name match |
| M1-0047 | api-keys.claude[].keys[].experimental-cch-signing | covered | accepted by crates/cpa-core/src/config/schema.json; Go gives it no runtime effect |  |  |
| M1-0048 | oauth.providers.claude.header-defaults.user-agent | partial | crates/cpa-exec/src/claude/settings.rs, profile.rs | ultra/claude | No test sets header-defaults. |
| M1-0049 | oauth.providers.claude.header-defaults.package-version | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0050 | oauth.providers.claude.header-defaults.runtime-version | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0051 | oauth.providers.claude.header-defaults.os | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0052 | oauth.providers.claude.header-defaults.arch | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0053 | oauth.providers.claude.header-defaults.timeout | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0054 | oauth.providers.claude.header-defaults.timezone | partial | crates/cpa-exec/src/claude/settings.rs, claude.rs | ultra/claude | No test sets header-defaults; only UTC and the process zone resolve without a tz database (ponytail in claude.rs). |
| M1-0055 | oauth.providers.claude.header-defaults.stabilize-device-profile | partial | crates/cpa-exec/src/claude/settings.rs | ultra/claude | No test sets header-defaults. |
| M1-0056 | oauth.providers.claude.disable-claude-cloak-mode | partial | crates/cpa-exec/src/claude/settings.rs; crates/cpa-server/tests/manage_go.rs (oauth-only scoping) | ultra/claude | No executor scenario disables cloaking globally. |

### M1: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0057 | Fallback/validation source internal/runtime/executor/helps/claude_device_profile.go:1-639:… | covered | crates/cpa-exec/src/claude/profile.rs BASELINE; claude scenarios assert claude-cli/2.1.280, Stainless 0.112.1, v26.3.0, MacOS/arm64 |  |  |

### M1: CLI flags

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0058 | --claude-login | covered | crates/cliproxy/src/main.rs --claude-login -> cpa_exec::claude_login::login (claude_login_tests.rs) |  |  |
| M1-0059 | --no-browser | covered | crates/cliproxy/src/main.rs --no-browser -> LoginOptions.no_browser (claude_login_tests.rs pasted-callback tests) |  |  |
| M1-0060 | --oauth-callback-port | covered | crates/cliproxy/src/main.rs --oauth-callback-port (0 -> provider default); claude_login_tests.rs a_busy_port_is_go_port_in_use |  |  |
| M1-0061 | --config | covered | crates/cliproxy/src/main.rs every_go_flag_parses_in_single_and_double_dash_form, argv_prescan_matches_go; --config resolves to config.yaml in the working directory like Go's empty default |  |  |
| M1-0062 | --local-model | covered | crates/cliproxy/src/main.rs (--local-model -> Runtime::set_local_model); every_go_flag_parses_in_single_and_double_dash_form |  |  |
| M1-0063 | Startup loads .env automatically (do not require a real .env for parity fixtures), config-… | partial | crates/cliproxy/src/main.rs: .env (dotenv.rs parses_like_godotenv), cloud standby (cloud_mode_treats_missing_empty_and_broken_files_as_empty_config), auth-dir expansion, logins | ultra/home | The Postgres/git/object store backends (PGSTORE_*, GITSTORE_*, OBJECTSTORE_*) are absent. |

### M1: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M1-0064 | internal/auth/claude/anthropic_auth_test.go | partial | crates/cpa-exec/src/oauth_tests.rs token_key_singleflight_survives_canceled_waiter, refresh_429_backoff_and_error_redaction, refresh_rotation_preserves_identity_on_optional_profile_failure, missing_rotated_refresh_and_profile_fields_keep_saved_values | ultra/claude | Equivalents cover dedupe, 429 backoff, profile-failure tolerance and account preservation; no Go case is ported by name and the timeout cases are untested. |
| M1-0065 | internal/auth/claude/filename_test.go | covered | crates/cpa-exec/src/claude_login_tests.rs file_names_match_go, legacy_matching_follows_go_identity_rules |  |  |
| M1-0066 | internal/auth/claude/identity_test.go | partial | crates/cpa-exec/src/claude/identity.rs; device pools in claude scenarios | ultra/claude | Pool repair/migration cases are not ported. |
| M1-0067 | internal/auth/claude/oauth_response_test.go | partial | crates/cpa-exec/src/oauth.rs (response decoding) | ultra/claude | Stacked/advertised encoding cases are not ported. |
| M1-0068 | internal/auth/claude/token_test.go | partial | crates/cpa-exec/src/oauth_tests.rs missing_rotated_refresh_and_profile_fields_keep_saved_values | ultra/claude | Custom metadata preservation on save is not tested directly. |
| M1-0069 | internal/auth/claude/utls_transport_test.go | partial | crates/cpa-exec/src/tls.rs capture_both_clienthellos_against_go_source_profile, transport_lru_bound; oauth_tests.rs raw_oauth_token_and_inspection_header_order_and_case | ultra/claude | Handshake bound and resumption wire-safety cases are not ported. |
| M1-0070 | internal/cache/claude_thinking_replay_cache_test.go | partial | crates/cpa-exec/src/claude/replay.rs (5 tests) | ultra/claude | Not ported by name. |
| M1-0071 | internal/client/claude/models/models_test.go | partial | crates/cpa-server/src/claude.rs dd_model_ids_round_trip_like_go (model ID prefix); crates/cpa-server/src/models.rs claude_list | ultra/server | BuildResponse cases (cloaking on/off, empty) are untested. |
| M1-0072 | internal/runtime/executor/claude_cloaked_cache_repro_test.go | partial | claude scenarios oauth-turn-1, oauth-turn-2 (prefix and continuity) | ultra/claude | Fingerprint edge cases and UTF-16 index cases are not ported. |
| M1-0073 | internal/runtime/executor/claude_executor_auth_race_test.go | covered | n/a in Rust: Go data-race invariants on shared credential metadata; Rust credentials are immutable snapshots with MetadataPatch commits |  |  |
| M1-0074 | internal/runtime/executor/claude_executor_auth_test.go | partial | crates/cpa-exec/src/oauth_tests.rs prepare_forces_a_refresh_outside_the_lead_window; crates/cpa-exec/src/claude/identity.rs | ultra/claude | Setup-token, 403 scope fallback and skip-profile cases are not ported. |
| M1-0075 | internal/runtime/executor/claude_executor_beta_passthrough_test.go | partial | crates/cpa-exec/src/claude/betas.rs; claude scenarios | ultra/claude | Not ported by name. |
| M1-0076 | internal/runtime/executor/claude_executor_beta_policy_test.go | partial | crates/cpa-exec/src/claude/betas.rs, quota.rs; claude scenarios | ultra/claude | Not ported by name. |
| M1-0077 | internal/runtime/executor/claude_executor_cloaking_display_test.go | partial | crates/cpa-exec/src/claude/reconcile.rs tests | ultra/claude | Not ported by name. |
| M1-0078 | internal/runtime/executor/claude_executor_diagnostics_test.go | partial | crates/cpa-exec/src/claude/signals.rs, session.rs; claude scenarios oauth-turn-1, oauth-turn-2 | ultra/claude | Not ported by name. |
| M1-0079 | internal/runtime/executor/claude_executor_fable_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs (overage-only, model scope); crates/cpa-server/tests/scheduler_attempts.rs | ultra/claude | Fable-only rejection and model-level cooling cases are not ported. |
| M1-0080 | internal/runtime/executor/claude_executor_fast_error_test.go | partial | claude scenarios decode-fast-bad-gzip-429, decode-fast-truncated-503, oauth-fast-500; crates/cpa-core/src/exec.rs direct error responses | ultra/claude | Not ported by name. |
| M1-0081 | internal/runtime/executor/claude_executor_native_helper_test.go | partial | crates/cpa-exec/src/claude/detect.rs; go_captures.json native-signals | ultra/claude | Not ported by name. |
| M1-0082 | internal/runtime/executor/claude_executor_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs; claude scenarios oauth-unified-429, oauth-model-429; scheduler_attempts.rs | ultra/claude | Not ported by name. |
| M1-0083 | internal/runtime/executor/claude_executor_request_remap_test.go | partial | crates/cpa-exec/src/claude/alias.rs; claude scenarios with alias drift | ultra/claude | Malformed-JSON fallback is not ported; mangled-alias recovery cases are not ported by name. |
| M1-0084 | internal/runtime/executor/claude_executor_stream_terminal_test.go | partial | crates/cpa-exec/src/claude/stream.rs (stops after message_stop) | ultra/claude | Client disconnect after the terminal event is untested. |
| M1-0085 | internal/runtime/executor/claude_executor_subagent_ttl_regression_test.go | partial | claude scenario oauth-subagent | ultra/claude | API-key subagent and stream variants are not ported. |
| M1-0086 | internal/runtime/executor/claude_executor_test.go | partial | crates/cpa-exec/src/claude.rs and claude/*; 46 claude scenarios, go_captures.json, harness | ultra/claude | 221 Go cases; behaviour is exercised through Go-generated scenarios, not ported case by case. |
| M1-0087 | internal/runtime/executor/claude_executor_thinking_signature_test.go | partial | claude scenarios oauth-strict-sensitive, oauth-signature-history | ultra/claude | Not ported by name. |
| M1-0088 | internal/runtime/executor/claude_executor_wire_casing_test.go | partial | crates/cpa-exec/src/claude/headers.rs; harness wire captures | ultra/claude | Not ported by name. |
| M1-0089 | internal/runtime/executor/claude_fingerprint_policy_test.go | partial | crates/cpa-exec/src/claude/detect.rs, claude.rs; claude scenarios apikey-firstparty-*, apikey-gateway-cli-stream | ultra/claude | 24 Go cases; not ported by name. |
| M1-0090 | internal/runtime/executor/claude_issue_6120_test.go | partial | claude scenario oauth-plain (direct Messages OAuth is cloaked) | ultra/claude | Stream variant not ported. |
| M1-0091 | internal/runtime/executor/claude_issue_6193_test.go | partial | crates/cpa-exec/src/claude/betas.rs | ultra/claude | Not ported by name. |
| M1-0092 | internal/runtime/executor/claude_messages_passthrough_test.go | partial | claude scenarios native-confirmed-stream, apikey-firstparty-default | ultra/claude | Not ported by name. |
| M1-0093 | internal/runtime/executor/claude_mid_system_model_test.go | partial | crates/cpa-exec/src/claude/reconcile.rs tests; claude scenario count-oauth-mid-system | ultra/claude | Not ported by name. |
| M1-0094 | internal/runtime/executor/claude_signing_test.go | partial | crates/cpa-exec/src/claude/signing.rs; cch values in claude scenarios | ultra/claude | Known-vector cases are not ported by name. |
| M1-0095 | internal/runtime/executor/claude_thinking_replay_test.go | partial | claude::tests::compat_replay_sequence_matches_go; claude scenarios replay-1-store, replay-2-restore | ultra/claude | Not ported by name. |
| M1-0096 | internal/runtime/executor/helps/claude_builtin_tools_test.go | partial | crates/cpa-exec/src/claude/alias.rs is_server_tool_type | ultra/claude | Not ported by name. |
| M1-0097 | internal/runtime/executor/helps/claude_cli_identity_seed_test.go | partial | crates/cpa-exec/src/claude/identity.rs | ultra/claude | Not ported by name. |
| M1-0098 | internal/runtime/executor/helps/claude_client_detection_test.go | partial | crates/cpa-exec/src/claude/detect.rs; claude scenarios | ultra/claude | 19 Go cases; not ported by name. |
| M1-0099 | internal/runtime/executor/helps/claude_code_session_test.go | partial | cpa_common::session; crates/cpa-exec/src/claude/session.rs | ultra/claude | Not ported by name. |
| M1-0100 | internal/runtime/executor/helps/claude_credential_identity_race_test.go | covered | n/a in Rust: Go data-race invariants on shared credential metadata |  |  |
| M1-0101 | internal/runtime/executor/helps/claude_credential_identity_test.go | partial | crates/cpa-exec/src/claude/identity.rs, session.rs | ultra/claude | Not ported by name; the Home KV case belongs to M6 Home. |
| M1-0102 | internal/runtime/executor/helps/claude_device_profile_test.go | partial | crates/cpa-exec/src/claude/profile.rs | ultra/claude | Local profile cases are not ported by name; the Home cases belong to M6 Home. |
| M1-0103 | internal/runtime/executor/helps/claude_diagnostics_test.go | partial | crates/cpa-exec/src/claude/signals.rs, session.rs | ultra/claude | Not ported by name. |
| M1-0104 | internal/runtime/executor/helps/claude_input_tokens_test.go | partial | claude::tests::custom_origin_counts_locally_without_sending_credentials; local count in crates/cpa-exec/src/claude.rs | ultra/claude | message_start input-token patching cases are not ported. |
| M1-0105 | internal/runtime/executor/helps/claude_ratelimit_test.go | partial | crates/cpa-exec/src/quota.rs latest_relevant_deadline_fractional_seconds_and_dates, overage_only_excludes_retry_after_and_unhealthy_missing_windows_do_not | ultra/claude | Equivalent cases; not ported by name. |
| M1-0106 | internal/runtime/executor/helps/claude_upstream_test.go | partial | crates/cpa-exec/src/claude.rs DEFAULT_BASE_URL handling | ultra/claude | Not ported by name. |
| M1-0107 | internal/signature/claude_messages_sanitize_compat_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M1-0108 | internal/signature/claude_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/22 cases also cited by name) |  |  |
| M1-0109 | internal/thinking/claude_enabled_effort_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (internal/thinking calls) |  |  |
| M1-0110 | internal/util/claude_attribution_test.go | partial | crates/cpa-translate/src/common.rs is_claude_code_attribution_text; translator goldens | ultra/translate | Unit cases not ported. |
| M1-0111 | internal/util/claude_model_test.go | partial | inlined in crates/cpa-translate/src/antigravity_claude.rs; claude-antigravity goldens | ultra/translate | Unit cases not ported. |
| M1-0112 | internal/util/claude_schema_test.go | partial | crates/cpa-translate/src/common.rs; translator goldens | ultra/translate | Unit cases not ported. |
| M1-0113 | internal/util/claude_tool_id_test.go | partial | crates/cpa-translate/src/common.rs; translator goldens | ultra/translate | Unit cases not ported. |
| M1-0114 | internal/util/claude_tool_result_test.go | partial | crates/cpa-translate/src/gemini.rs; translator goldens | ultra/translate | Unit cases not ported. |
| M1-0115 | sdk/api/handlers/claude/code_handlers_error_test.go | partial | crates/cpa-server/src/claude.rs, errors.rs; routes.rs failover_stop_rules_and_cooldown_contracts | ultra/server | Not ported by name. |
| M1-0116 | sdk/api/handlers/claude/code_handlers_model_test.go | partial | crates/cpa-server/src/claude.rs dd_model_ids_round_trip_like_go | ultra/server | Display-name and model-list cloaking cases are untested. |
| M1-0117 | test/claude_code_compatibility_sentinel_test.go | covered | n/a: the Go test checks its own fixture maps, not production code |  |  |
| M1-0118 | test/codex_claude_parallel_function_calls_test.go | partial | claude-codex translator goldens (crates/cpa-translate/tests/fixtures/pairs/claude-codex.json) | ultra/translate | The parallel-call lifecycle test is not ported. |

## M2

### M2: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M2-0001 | POST /v1/chat/completions | covered | probe: POST /v1/chat/completions -> 401; tests: crates/cpa-exec/src/kimi_tests.rs, crates/cpa-server/tests/openai_compat_routes.rs (+1) |  |  |
| M2-0002 | POST /v1/completions | covered | probe: POST /v1/completions -> 401; tests: crates/cpa-server/tests/routes.rs |  |  |
| M2-0003 | POST /v1/responses | covered | probe: POST /v1/responses -> 401; tests: crates/cpa-exec/src/codex_tls_tests.rs, crates/cpa-exec/src/kimi_tests.rs (+3) |  |  |
| M2-0004 | POST /v1/responses/compact | covered | probe: POST /v1/responses/compact -> 401; tests: crates/cpa-server/tests/routes.rs |  |  |
| M2-0005 | POST /backend-api/codex/responses | covered | probe: POST /backend-api/codex/responses -> 401; tests: crates/cpa-exec/src/codex_tls_tests.rs |  |  |
| M2-0006 | POST /backend-api/codex/responses/compact | partial | probe: POST /backend-api/codex/responses/compact -> 401; no test requests this path | ultra/server |  |
| M2-0007 | GET /v1beta/models | covered | probe: GET /v1beta/models -> 401; tests: crates/cpa-server/tests/gemini_routes.rs, crates/cpa-server/tests/routes.rs |  |  |
| M2-0008 | POST /v1beta/interactions | covered | probe: POST /v1beta/interactions -> 401; tests: crates/cpa-server/tests/gemini_routes.rs, crates/cpa-server/tests/routes.rs |  |  |
| M2-0009 | POST /v1beta/models/*action | covered | probe: POST /v1beta/models/*action -> 401; tests: crates/cpa-server/tests/gemini_routes.rs, crates/cpa-server/tests/routes.rs |  |  |
| M2-0010 | GET /v1beta/models/*action | covered | probe: GET /v1beta/models/*action -> 401; tests: crates/cpa-server/tests/gemini_routes.rs, crates/cpa-server/tests/routes.rs |  |  |

### M2: 2. Registered translator matrix and LOC

| ID | Item | Status | Evidence | Gap owner | Note |
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

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M2-0034 | requests.streaming.keepalive-seconds | partial | crates/cpa-server/src/respond.rs, websocket.rs | ultra/server | No test sets keepalive-seconds. |
| M2-0035 | requests.streaming.bootstrap-retries | covered | crates/cpa-server/tests/routes.rs bootstrap_retries_rerun_a_stream_that_broke_before_its_first_payload |  | heuristic: key name match |
| M2-0036 | requests.nonstream-keepalive-interval | missing | only migrated in crates/cpa-core/src/config/document.rs | ultra/server | Non-stream keep-alive blank lines are not emitted. |

### M2: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M2-0037 | internal/config/gemini_keys_normalization_test.go | partial | crates/cpa-core/src/config/credentials.rs sanitized() keeps empty keys with a base URL | ultra/manage | Not ported by name. |
| M2-0038 | internal/runtime/executor/gemini_executor_signature_test.go | partial | cpa_common::signature (recorded replay) used by crates/cpa-exec/src/gemini.rs | ultra/google | Executor-level signature cases (Gemini and Vertex) are not ported. |
| M2-0039 | internal/runtime/executor/gemini_executor_test.go | partial | gemini scenarios (75): gen_cap_*, gen_boundary_user_turns, gen_payload_rules, stream_* | ultra/google | 28 Go cases; equivalents exist for capping, boundary turns and payload rules; not ported by name. |
| M2-0040 | internal/runtime/executor/gemini_interactions_translate_test.go | covered | n/a: Go slice-reuse and plugin-call invariants; Interactions request translation covered by gemini scenarios int_* |  |  |
| M2-0041 | internal/runtime/executor/helps/gemini_content_turns_test.go | covered | gemini scenarios gen_boundary_user_turns, stream_boundary_user_turns, gen_trailing_function_response_kept |  |  |
| M2-0042 | internal/runtime/executor/helps/openai_compat_max_tokens_test.go | covered | compat scenarios chat_max_tokens_to_max_completion_tokens, chat_both_limits_keep_max_completion_tokens, chat_max_completion_tokens_to_max_tokens, chat_requested_alias_selects_limit_mode |  |  |
| M2-0043 | internal/runtime/executor/helps/openai_compat_tool_results_test.go | covered | compat scenarios chat_text_only_tool_results, chat_image_model_keeps_tool_images |  |  |
| M2-0044 | internal/runtime/executor/helps/thinking_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (helps package calls) |  |  |
| M2-0045 | internal/runtime/executor/openai_compat_executor_compact_test.go | partial | compat scenarios compact_*, chat_prompt_cache_key_*, chat_execution_session_* | ultra/openai-xai | 21 Go cases; prompt-cache and compact equivalents exist; config-index scoping cases not ported. |
| M2-0046 | internal/runtime/executor/openai_compat_executor_images_test.go | partial | compat scenarios images_* | ultra/openai-xai | Executor image paths are ported; the /v1/images routes are not (M3-0001, M3-0002). |
| M2-0047 | internal/runtime/executor/openai_compat_executor_max_tokens_test.go | covered | compat scenarios chat_max_tokens_* (non-stream and stream) |  |  |
| M2-0048 | internal/runtime/executor/openai_compat_executor_reasoning_test.go | covered | compat scenarios claude_is_compat_keeps_thinking, claude_not_compat_drops_thinking |  |  |
| M2-0049 | internal/runtime/executor/openai_compat_executor_retry_test.go | covered | compat scenarios error_429_retry_after_* |  |  |
| M2-0050 | internal/runtime/executor/openai_compat_executor_tool_results_test.go | covered | compat scenarios chat_text_only_tool_results, chat_image_model_keeps_tool_images |  |  |
| M2-0051 | internal/runtime/executor/openai_compat_executor_video_test.go | covered | compat scenario chat_video_input_passthrough |  |  |
| M2-0052 | internal/runtime/executor/openai_responses_signature_test.go | partial | crates/cpa-exec/src/codex_request.rs | ultra/codex | Not ported by name. |
| M2-0053 | internal/signature/gemini_sanitize_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (internal/signature calls) |  |  |
| M2-0054 | internal/signature/gemini_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/29 cases also cited by name) |  |  |
| M2-0055 | internal/thinking/apply_configured_api_key_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/15 cases also cited by name) |  |  |
| M2-0056 | internal/thinking/summary_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/12 cases also cited by name) |  |  |
| M2-0057 | internal/translator/claude/gemini/claude_gemini_request_test.go | partial | 12/13 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-claude.json; not matched: TestConvertGeminiRequestToClaude_PreservesCustomToolIDs | ultra/translate |  |
| M2-0058 | internal/translator/claude/gemini/claude_gemini_response_test.go | partial | 3/4 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-claude.json; not matched: TestConvertClaudeResponseToGemini_StreamThinkingSignature | ultra/translate |  |
| M2-0059 | internal/translator/claude/gemini/noop_optimization_test.go | partial | crates/cpa-translate/src/gemini_claude.rs schema normalization; goldens | ultra/translate | Table-driven schema cases are not mined. |
| M2-0060 | internal/translator/claude/interactions/interactions_claude_test.go | covered | 12/12 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-claude.json |  |  |
| M2-0061 | internal/translator/claude/openai/chat-completions/claude_openai_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json |  |  |
| M2-0062 | internal/translator/claude/openai/chat-completions/claude_openai_request_test.go | partial | 32/33 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json; not matched: TestConvertOpenAIRequestToClaudeWithCompat_GroupsAssistantThinkingTextAndTools | ultra/translate |  |
| M2-0063 | internal/translator/claude/openai/chat-completions/claude_openai_response_test.go | partial | 11/12 cases: crates/cpa-translate/tests/fixtures/pairs/openai-claude.json; not matched: TestConvertClaudeResponseToOpenAI_RefusalStopReason | ultra/translate |  |
| M2-0064 | internal/translator/claude/openai/chat-completions/noop_optimization_test.go | partial | crates/cpa-translate/src/claude_chat_response.rs finish_reason mapping; goldens | ultra/translate | Table-driven stop_reason cases are not mined. |
| M2-0065 | internal/translator/claude/openai/responses/claude_openai-responses_citations_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0066 | internal/translator/claude/openai/responses/claude_openai-responses_interleaved_search_test.go | partial | crates/cpa-translate/src/claude_responses_response.rs; goldens | ultra/translate | Interleaved web-search case not mined. |
| M2-0067 | internal/translator/claude/openai/responses/claude_openai-responses_reasoning_order_test.go | partial | 3/4 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestConvertOpenAIResponsesRequestToClaude_ToolCallsSeparateReasoningBlocks | ultra/translate |  |
| M2-0068 | internal/translator/claude/openai/responses/claude_openai-responses_request_test.go | partial | 51/67 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestConvertOpenAIResponsesRequestToClaude_ReasoningItemToThinkingBlock, TestConvertOpenAIResponsesRequestToClaude_SignatureOnlyReasoningFlushesBeforeUser, TestConvertOpenAIResponsesRequestToClaude_RedactedReasoningItemRestoresRedactedThinking, TestConvertOpenAIResponsesRequestToClaude_EmptyRedactedReasoningItemIsDropped … | ultra/translate |  |
| M2-0069 | internal/translator/claude/openai/responses/claude_openai-responses_response_test.go | partial | 33/54 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestConvertClaudeResponseToOpenAIResponses_SuppressesSignatureDeltaPassthrough, TestConvertClaudeResponseToOpenAIResponses_AggregatesTextBlocksUntilMessageStop, TestConvertClaudeResponseToOpenAIResponses_FinalizesMessageBeforeFunctionCall, TestConvertClaudeResponseToOpenAIResponses_UsesContiguousIndicesForReasoningTextAndTool … | ultra/translate |  |
| M2-0070 | internal/translator/claude/openai/responses/claude_openai-responses_server_tool_test.go | partial | 7/17 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestClaudeWebSearchBlocksBecomeWebSearchCallItem, TestClaudeWebSearchBlocksBecomeWebSearchCallItemNonStream, TestRoundTripPreservesReachableClaudeBlocks, TestWebSearchCallIDNormalisedToClaudeServerToolPattern … | ultra/translate |  |
| M2-0071 | internal/translator/claude/openai/responses/claude_openai-responses_tool_names_test.go | partial | 1/5 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json; not matched: TestBuildClaudeToolNames_RoundTripAndStability, TestBuildClaudeToolNames_DeclarationOrderInvariance, TestBuildClaudeToolNames_SingleLongName, TestBuildClaudeToolNames_DeclaredToolsPrecedeHistory | ultra/translate |  |
| M2-0072 | internal/translator/claude/openai/responses/claude_openai_responses_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0073 | internal/translator/claude/openai/responses/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-claude.json |  |  |
| M2-0074 | internal/translator/common/apply_patch_events_test.go | partial | implementation cites apply_patch_events.go (crates/cpa-translate/src/apply_patch.rs); no case matched by name | ultra/translate |  |
| M2-0075 | internal/translator/common/apply_patch_identity_test.go | covered | 3/3 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json |  |  |
| M2-0076 | internal/translator/common/apply_patch_input_test.go | partial | implementation cites apply_patch_input.go (crates/cpa-translate/src/apply_patch.rs); no case matched by name | ultra/translate |  |
| M2-0077 | internal/translator/common/apply_patch_responses_test.go | covered | 17/17 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json |  |  |
| M2-0078 | internal/translator/common/bytes_test.go | partial | crates/cpa-translate/src/common.rs, sse.rs; cpa_common::json | ultra/translate | Unit cases not ported. |
| M2-0079 | internal/translator/common/cache_control_test.go | partial | implementation cites cache_control.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0080 | internal/translator/common/claude_messages_test.go | partial | implementation cites claude_messages.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0081 | internal/translator/common/claude_system_test.go | partial | implementation cites claude_system.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0082 | internal/translator/common/claude_user_id_test.go | partial | implementation cites claude_user_id.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0083 | internal/translator/common/file_data_test.go | partial | implementation cites file_data.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0084 | internal/translator/common/gemini_test.go | partial | implementation cites gemini.go (crates/cpa-translate/src/gemini.rs); no case matched by name | ultra/translate |  |
| M2-0085 | internal/translator/common/openai_tools_test.go | partial | implementation cites openai_tools.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0086 | internal/translator/common/request_test.go | partial | implementation cites request.go (crates/cpa-translate/src/common.rs); no case matched by name | ultra/translate |  |
| M2-0087 | internal/translator/common/responses_test.go | partial | implementation cites responses.go (crates/cpa-translate/src/claude_responses.rs); no case matched by name | ultra/translate |  |
| M2-0088 | internal/translator/gemini/claude/gemini_claude_compat_test.go | partial | 1/2 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json; not matched: TestConvertClaudeRequestToGeminiWithCompat_SignatureCompatibility | ultra/translate |  |
| M2-0089 | internal/translator/gemini/claude/gemini_claude_request_test.go | covered | 15/15 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0090 | internal/translator/gemini/claude/gemini_claude_response_test.go | covered | 6/6 cases: crates/cpa-translate/tests/fixtures/pairs/claude-gemini.json |  |  |
| M2-0091 | internal/translator/gemini/gemini/gemini_gemini_request_test.go | partial | 2/8 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-gemini.json; not matched: TestConvertGeminiRequestToGeminiReusesLargeNormalizedPayload, TestBackfillEmptyFunctionResponseNames_Single, TestBackfillEmptyFunctionResponseNames_Parallel, TestBackfillEmptyFunctionResponseNames_PreservesExisting … | ultra/translate |  |
| M2-0092 | internal/translator/gemini/interactions/interactions_gemini_common_test.go | partial | 41/56 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-interactions.json, crates/cpa-translate/tests/fixtures/pairs/interactions-gemini.json; not matched: TestConvertGeminiResponseToInteractionsNonStream, TestConvertGeminiResponseToInteractionsNonStreamSnakeCaseUsage, TestConvertGeminiResponseToInteractionsNonStreamFunctionCall, TestConvertGeminiResponseToInteractionsNonStreamFunctionCallPreservesCallID … | ultra/translate |  |
| M2-0093 | internal/translator/gemini/interactions/interactions_gemini_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-gemini.json |  |  |
| M2-0094 | internal/translator/gemini/openai/chat-completions/gemini_openai_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0095 | internal/translator/gemini/openai/chat-completions/gemini_openai_request_test.go | covered | 22/22 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0096 | internal/translator/gemini/openai/chat-completions/gemini_openai_response_test.go | covered | 8/8 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0097 | internal/translator/gemini/openai/chat-completions/gemini_openai_signature_test.go | partial | crates/cpa-translate/src/gemini_chat_request.rs; goldens | ultra/translate | Unit case not mined. |
| M2-0098 | internal/translator/gemini/openai/chat-completions/noop_optimization_test.go | covered | 3/3 cases: crates/cpa-translate/tests/fixtures/pairs/openai-gemini.json |  |  |
| M2-0099 | internal/translator/gemini/openai/responses/apply_patch_review_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0100 | internal/translator/gemini/openai/responses/apply_patch_test.go | partial | 8/10 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestGeminiApplyPatchCompleteSnapshotMatrix, TestGeminiApplyPatchDoesNotRebindOrdinaryFunctionCalls | ultra/translate |  |
| M2-0101 | internal/translator/gemini/openai/responses/gemini_openai-responses_request_test.go | partial | 66/76 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestReorderOpenAIResponsesDetachedReasoningDoesNotCrossUserMessage, TestConvertOpenAIResponsesRequestToGemini_PreservesMultipleLeadingToolSignatures, TestConvertOpenAIResponsesRequestToGemini_PreservesReasoningBeforePairedFunctionSignature, TestConvertOpenAIResponsesRequestToGemini_ReattachesDirectionalFunctionCarriersWithoutIDs … | ultra/translate |  |
| M2-0102 | internal/translator/gemini/openai/responses/gemini_openai-responses_response_test.go | partial | 26/46 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestConvertGeminiResponseToOpenAIResponsesNonStream_TrailingCarrierDirectionDoesNotDependOnID, TestConvertGeminiResponseToOpenAIResponses_CachedTrailingCarrierPreservesDirection, TestConvertGeminiResponseToOpenAIResponses_VisibleSignatureDoesNotOverwriteSignedThought, TestConvertGeminiResponseToOpenAIResponses_FlushesVisibleSignatureBeforeLaterThought … | ultra/translate |  |
| M2-0103 | internal/translator/gemini/openai/responses/gemini_openai-responses_web_search_test.go | partial | 33/42 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestBuildResponsesURLCitations_RuneOffsetConversion, TestHasValidWebGrounding, TestModelSupportsWebSearch_StaticVetoTakesPrecedence, TestAllowsResponsesWebSearchToolChoice_AllowedTools … | ultra/translate |  |
| M2-0104 | internal/translator/gemini/openai/responses/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json |  |  |
| M2-0105 | internal/translator/gemini/openai/responses/signature_carrier_test.go | partial | 4/10 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestGeminiResponsesCarrierRoundTrip, TestNormalizeGeminiResponsesCarriersDropsMalformedEnvelope, TestConvertOpenAIResponsesRequestToGemini_DecodesCarrierForAliasModel, TestConvertOpenAIResponsesRequestToGemini_DropsInvalidCarrierPayloads … | ultra/translate |  |
| M2-0106 | internal/translator/gemini/openai/responses/trailing_signature_test.go | partial | 4/7 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-gemini.json; not matched: TestGeminiResponsesCachedSignaturesMergeExplicitPrefixInOrder, TestGeminiResponsesLateThoughtSignatureDoesNotBindEarlierMessage, TestGeminiResponsesCacheRecoveryPreservesFallbackSignatureOrder | ultra/translate |  |
| M2-0107 | internal/translator/interactions/claude/interactions_claude_compat_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/claude-interactions.json |  |  |
| M2-0108 | internal/translator/interactions/claude/interactions_claude_test.go | partial | 13/14 cases: crates/cpa-translate/tests/fixtures/pairs/claude-interactions.json; not matched: TestConvertInteractionsResponseToClaude_ResponseFailed | ultra/translate |  |
| M2-0109 | internal/translator/interactions/import_boundary_test.go | covered | n/a: Go package import boundary check |  |  |
| M2-0110 | internal/translator/openai/claude/openai_claude_compat_test.go | partial | 2/5 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json; not matched: TestConvertClaudeRequestToOpenAIWithCompatPreservesThinkingWithToolCalls, TestConvertClaudeRequestToOpenAIWithCompatDoesNotAddReasoningWithoutThinking, TestConvertClaudeRequestToOpenAIWithCompatPreservesIncompatibleThinking | ultra/translate |  |
| M2-0111 | internal/translator/openai/claude/openai_claude_request_test.go | partial | 27/28 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json; not matched: TestConvertClaudeRequestToOpenAI_SignedThinkingCompatibility | ultra/translate |  |
| M2-0112 | internal/translator/openai/claude/openai_claude_response_test.go | partial | 3/42 cases: crates/cpa-translate/tests/fixtures/pairs/claude-openai.json; not matched: TestStreaming_LateUsageOnlyDoesNotEmitAfterMessageStop, TestStreamingTool_EmptyNameThroughout, TestStreamingTool_NullName, TestStreamingTool_NonStringName … | ultra/translate |  |
| M2-0113 | internal/translator/openai/gemini/openai_gemini_request_test.go | partial | 12/13 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-openai.json; not matched: TestConvertGeminiRequestToOpenAI_PreservesExplicitFunctionCallIDs | ultra/translate |  |
| M2-0114 | internal/translator/openai/gemini/openai_gemini_response_test.go | covered | 5/5 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-openai.json |  |  |
| M2-0115 | internal/translator/openai/interactions/chat-completions/interactions_openai_request_test.go | covered | 12/12 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai.json, crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0116 | internal/translator/openai/interactions/chat-completions/interactions_openai_response_test.go | partial | 16/17 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai.json, crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json; not matched: TestConvertInteractionsResponseToOpenAI_ResponseFailed | ultra/translate |  |
| M2-0117 | internal/translator/openai/interactions/chat-completions/openai_interactions_file_data_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-interactions.json |  |  |
| M2-0118 | internal/translator/openai/interactions/responses/apply_patch_identity_test.go | partial | crates/cpa-translate/src/responses_interactions_response.rs; apply_patch goldens in openai-response-interactions.json | ultra/translate | Unit cases not mined. |
| M2-0119 | internal/translator/openai/interactions/responses/apply_patch_rereview_test.go | partial | crates/cpa-translate/src/responses_interactions_response.rs; apply_patch goldens | ultra/translate | Unit cases not mined. |
| M2-0120 | internal/translator/openai/interactions/responses/apply_patch_review_test.go | partial | crates/cpa-translate/src/responses_interactions_response.rs; apply_patch goldens | ultra/translate | Unit cases not mined. |
| M2-0121 | internal/translator/openai/interactions/responses/apply_patch_source_stop_test.go | partial | crates/cpa-translate/src/responses_interactions_response.rs; apply_patch goldens | ultra/translate | Unit cases not mined. |
| M2-0122 | internal/translator/openai/interactions/responses/apply_patch_test.go | partial | 4/16 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json; not matched: TestInteractionsApplyPatchDeclarationAndHistory, TestInteractionsApplyPatchEverySplitPreviewAndCompletion, TestInteractionsApplyPatchSnapshotMatrix, TestInteractionsApplyPatchDeltaBeforeStartDoesNotLeakArguments … | ultra/translate |  |
| M2-0123 | internal/translator/openai/interactions/responses/interactions_openai_responses_request_test.go | covered | 30/30 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai-response.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json |  |  |
| M2-0124 | internal/translator/openai/interactions/responses/interactions_openai_responses_response_test.go | partial | 32/37 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-openai-response.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-interactions.json; not matched: TestConvertInteractionsResponseToOpenAIResponsesStreamPreservesThoughtSignature, TestConvertOpenAIResponsesResponseToInteractionsStreamCreatedThenDelta, TestConvertOpenAIResponsesResponseToInteractionsStreamCompletesAfterSteps, TestConvertInteractionsResponseToOpenAIResponses_LogReplayTwoToolCalls … | ultra/translate |  |
| M2-0125 | internal/translator/openai/openai/chat-completions/openai_openai_request_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-openai.json |  |  |
| M2-0126 | internal/translator/openai/openai/chat-completions/openai_openai_response_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-openai.json |  |  |
| M2-0127 | internal/translator/openai/openai/responses/custom_tool_namespace_recovery_test.go | partial | 1/3 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json; not matched: TestCustomToolReplayPreservesNamespaceAndResultPair, TestNamespaceRecoveryDoesNotGuessAmbiguousOrOverrideExactNames | ultra/translate |  |
| M2-0128 | internal/translator/openai/openai/responses/openai_openai-responses_request_test.go | partial | 51/64 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json; not matched: TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_UnwrapsStringifiedToolOutputImages, TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_PreservesCustomToolOutputFallbacks, TestConvertOpenAIResponsesRequestToOpenAIChatCompletions_NormalizesInputImageDetail, TestResponsesSingleCustomToolName_CountsDeduplicatedTools … | ultra/translate |  |
| M2-0129 | internal/translator/openai/openai/responses/openai_openai-responses_response_test.go | partial | 31/44 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json; not matched: TestConvertOpenAIChatCompletionsResponseToOpenAIResponses_FinalizesOpenMessageAtStreamEnd, TestApplyPatchChatPreviewBeforeDone, TestApplyPatchChatLateIdentityAndInterleavedCalls, TestApplyPatchChatInvalidArgumentsFailOnce … | ultra/translate |  |
| M2-0130 | internal/translator/openai/openai/responses/openai_openai-responses_video_test.go | covered | 2/2 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0131 | internal/translator/openai/openai/responses/responses_compatibility_digest_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0132 | internal/translator/openai/openai/responses/responses_request_state_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-openai.json |  |  |
| M2-0133 | internal/util/gemini_schema_test.go | covered | crates/cpa-common/tests/fixtures/gemini_schema.json: every JSON literal mined from gemini_schema_test.go run through Go's cleaners (crates/cpa-common/tests/gemini_schema.rs) |  |  |
| M2-0134 | internal/watcher/diff/openai_compat_test.go | missing | no config diff summaries in crates/cpa-server/src/watching.rs | ultra/server | Go logs openai-compatibility diffs on reload; Rust does not. |
| M2-0135 | sdk/api/handlers/gemini/gemini_handlers_stream_error_test.go | partial | crates/cpa-server/src/dispatch.rs (bootstrap); routes.rs mid_stream_failure_ends_with_route_error_frame | ultra/server | Not ported for the Gemini handler. |
| M2-0136 | sdk/api/handlers/gemini/gemini_models_display_name_test.go | partial | crates/cpa-server/src/models.rs gemini_list display_name | ultra/server | Untested. |
| M2-0137 | sdk/api/handlers/gemini/interactions_handlers_test.go | partial | crates/cpa-server/tests/gemini_routes.rs (4 tests), routes.rs misc_routes_match_go (Interactions validation) | ultra/server | 11 Go cases; target parsing and agent selection are not ported by name. |
| M2-0138 | sdk/api/handlers/openai/openai_handlers_stream_error_test.go | partial | crates/cpa-server/src/dispatch.rs; routes.rs bootstrap and mid-stream tests | ultra/server | Not ported by name. |
| M2-0139 | sdk/api/handlers/openai/openai_images_handlers_test.go | missing | POST /v1/images/* not routed (probe 404) | ultra/openai-xai | Images handlers (xAI and compat) are not ported. |
| M2-0140 | sdk/api/handlers/openai/openai_responses_compact_test.go | partial | crates/cpa-server/src/openai.rs compact; routes.rs compact-not-supported case; compat scenarios compact_* | ultra/server | zstd request decoding and fault/cooldown cases are absent. |
| M2-0141 | sdk/api/handlers/openai/openai_responses_handlers_stream_error_test.go | partial | crates/cpa-server/src/openai.rs (response.failed), crates/cpa-translate/src/stream.rs ResponsesFramer | ultra/server | 21 Go cases; not ported by name. |
| M2-0142 | sdk/api/handlers/openai/openai_responses_handlers_stream_test.go | partial | crates/cpa-translate/src/stream.rs responses_framer_matches_recorded_go_frames; crates/cpa-server/src/openai.rs output repair | ultra/server | 23 Go cases; framer recorded frames only. |
| M2-0143 | sdk/api/handlers/openai/openai_responses_multi_agent_test.go | partial | cpa_common::codex_client rewrites; crates/cpa-server/src/openai.rs | ultra/codex | Not ported by name. |
| M2-0144 | sdk/api/handlers/openai/openai_responses_signature_test.go | partial | crates/cpa-server/src/openai.rs (no handler-side validation) | ultra/server | Untested. |
| M2-0145 | sdk/api/handlers/openai/openai_responses_steering_auth_test.go | missing | no Responses steering in Rust | ultra/codex | Responses WebSocket steering is not ported. |
| M2-0146 | sdk/api/handlers/openai/openai_responses_steering_error_test.go | missing | no Responses steering in Rust | ultra/codex | Responses WebSocket steering is not ported. |
| M2-0147 | sdk/api/handlers/openai/openai_responses_steering_integration_test.go | missing | no Responses steering in Rust | ultra/codex | Responses WebSocket steering is not ported. |
| M2-0148 | sdk/api/handlers/openai/openai_responses_steering_test.go | missing | no Responses steering in Rust | ultra/codex | Responses WebSocket steering is not ported. |
| M2-0149 | sdk/api/handlers/openai/openai_responses_steering_validation_test.go | missing | no Responses steering in Rust | ultra/codex | Responses WebSocket steering is not ported. |
| M2-0150 | sdk/api/handlers/openai/openai_videos_handlers_test.go | missing | /v1/videos and /openai/v1/videos not routed (probe 404) | ultra/openai-xai | Videos handlers are not ported. |
| M2-0151 | sdk/api/handlers/openai/permanent_oauth_classification_test.go | partial | crates/cpa-exec/src/codex_oauth.rs (refresh_token_reused classification) | ultra/server | Manager-level permanent-failure handling is untested. |
| M2-0152 | sdk/api/handlers/openai_responses_stream_error_test.go | partial | crates/cpa-server/src/openai.rs response.failed chunks | ultra/server | Not ported by name. |
| M2-0153 | sdk/cliproxy/auth/openai_compat_pool_test.go | partial | crates/cpa-server/src/dispatch.rs alias-pool rotation; routes.rs config_models_alias_and_force_mapping_reach_upstream_and_client | ultra/server | 20 Go cases; not ported by name. |
| M2-0154 | sdk/cliproxy/openai_compat_config_models_test.go | partial | crates/cpa-server/src/models.rs, crates/cpa-core/src/registry/dynamic.rs input modalities | ultra/server | Untested. |
| M2-0155 | sdk/translator/registry_bytes_test.go | covered | Rust transforms return bytes; every translator golden compares bytes |  |  |
| M2-0156 | sdk/translator/registry_summary_test.go | partial | summary cases through sdk.TranslateRequest in translator goldens (matrix summary variants) | ultra/plugins | The 3 plugin-hook cases need plugin hooks (M6). |
| M2-0157 | sdk/translator/registry_test.go | partial | crates/cpa-translate/tests/sdk_translator.rs (registration matrix, fallback vectors); apply_patch nil goldens | ultra/plugins | 8 of 15 cases need plugin hooks or runtime (un)registration (M6). |
| M2-0158 | test/thinking_conversion_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (Thinking\|Summary\|Signature tests in ./test/) |  |  |

## M3

### M3: 1. Public HTTP and WebSocket route inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0001 | POST /v1/images/generations | missing | probe: POST /v1/images/generations -> 404 (not routed) | ultra/openai-xai | The xAI and OpenAI-compatible executors implement image requests, but no /v1/images route reaches them. |
| M3-0002 | POST /v1/images/edits | missing | probe: POST /v1/images/edits -> 404 (not routed) | ultra/openai-xai | The xAI and OpenAI-compatible executors implement image requests, but no /v1/images route reaches them. |
| M3-0003 | POST /v1/videos | missing | probe: POST /v1/videos -> 404 (not routed) | ultra/openai-xai | xAI videos handlers are not routed. |
| M3-0004 | POST /v1/videos/generations | missing | probe: POST /v1/videos/generations -> 404 (not routed) | ultra/openai-xai | xAI videos handlers are not routed. |
| M3-0005 | POST /v1/videos/edits | missing | probe: POST /v1/videos/edits -> 404 (not routed) | ultra/openai-xai | xAI videos handlers are not routed. |
| M3-0006 | POST /v1/videos/extensions | missing | probe: POST /v1/videos/extensions -> 404 (not routed) | ultra/openai-xai | xAI videos handlers are not routed. |
| M3-0007 | GET /v1/videos/:request_id | missing | probe: GET /v1/videos/:request_id -> 404 (not routed) | ultra/openai-xai | xAI videos handlers are not routed. |
| M3-0008 | POST /v1/alpha/search | covered | probe POST /v1/alpha/search -> 401; same handler as /backend-api/codex/alpha/search (crates/cpa-server/src/codex_alpha.rs routes()); crates/cpa-server/tests/codex_alpha.rs alpha_search_uses_policy_eligible_credential_and_passes_upstream_through; codex_go.json alpha_search |  |  |
| M3-0009 | POST /openai/v1/videos | missing | probe: POST /openai/v1/videos -> 404 (not routed) | ultra/openai-xai | OpenAI videos handlers are not routed. |
| M3-0010 | GET /openai/v1/videos/:video_id/content | missing | probe: GET /openai/v1/videos/:video_id/content -> 404 (not routed) | ultra/openai-xai | OpenAI videos handlers are not routed. |
| M3-0011 | GET /openai/v1/videos/:video_id | missing | probe: GET /openai/v1/videos/:video_id -> 404 (not routed) | ultra/openai-xai | OpenAI videos handlers are not routed. |
| M3-0012 | POST /backend-api/codex/alpha/search | covered | probe: POST /backend-api/codex/alpha/search -> 401; tests: crates/cpa-server/tests/codex_alpha.rs |  |  |
| M3-0013 | GET /codex/callback | covered | probe: GET /codex/callback -> 200; tests: crates/cpa-server/src/management/oauth.rs |  |  |
| M3-0014 | GET /antigravity/callback | partial | probe GET /antigravity/callback -> 200 (shared callback page) | ultra/google | Untested; there is no Antigravity login flow behind it. |
| M3-0015 | GET /callback | covered | probe: GET /callback -> 400; tests: crates/cpa-exec/src/claude_login_tests.rs, crates/cpa-exec/src/codex_oauth_tests.rs (+4) |  |  |
| M3-0016 | GET /devin/callback | covered | probe: GET /devin/callback -> 400; tests: crates/cpa-server/tests/routes.rs |  |  |

### M3: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0017 | Local OAuth listener `ANY /auth/callback` (browser flow uses GET; ServeMux registration ha… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener, pasted_callbacks_parse_like_go |  |  |
| M3-0018 | Local OAuth listener `ANY /success` (browser flow uses GET; ServeMux registration has no m… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener |  |  |
| M3-0019 | Local OAuth listener `ANY /oauth-callback` (browser flow uses GET; ServeMux registration h… | missing | no Antigravity login in Rust | ultra/google |  |
| M3-0020 | Local OAuth listener `ANY /callback` (browser flow uses GET; ServeMux registration has no … | partial | crates/cpa-exec/src/devin_auth.rs login listener; devin_tests.rs code_exchange_matches_go_fixtures; devin_auth.rs callback_page_escapes_the_error | ultra/device-providers | Listener routes are not tested case by case. |
| M3-0021 | Local OAuth listener `ANY /` (browser flow uses GET; ServeMux registration has no method r… | partial | crates/cpa-exec/src/devin_auth.rs login listener | ultra/device-providers | The catch-all 404 at / is untested. |

### M3: 2. Registered translator matrix and LOC

| ID | Item | Status | Evidence | Gap owner | Note |
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

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0032 | Codex | covered | crates/cpa-exec/src/codex_oauth.rs; codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go, exchange_and_refresh_wire_and_results_match_go, device_flow_polls_through_pending_and_exchanges_like_go; crates/cpa-server/tests/management.rs codex_login_completes_through_the_main_listener_callback |  |  |
| M3-0033 | Antigravity | missing | no Antigravity login, executor or credential type in Rust (translators only) | ultra/google |  |
| M3-0034 | Gemini API keys / native Interactions keys | covered | crates/cpa-exec/src/gemini.rs; gemini scenarios (gen_*, stream_*, count_*, int_*); crates/cpa-server/tests/gemini_routes.rs |  |  |
| M3-0035 | Vertex | missing | no Vertex executor or service-account import | ultra/google |  |
| M3-0036 | AI Studio | missing | no AI Studio WebSocket relay | ultra/google |  |
| M3-0037 | Kimi (.com and .ai) | covered | crates/cpa-exec/src/kimi_auth.rs (domain_resolution tests), kimi_tests.rs; crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0038 | xAI | covered | crates/cpa-exec/src/xai_auth.rs (xai_auth_tests.rs, xai_auth_go.json); executor crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) |  |  |
| M3-0039 | Meta | covered | crates/cpa-exec/src/meta.rs, meta_auth.rs; meta_tests.rs login_matches_go_requests_and_file, mint_errors_follow_go, execution_matches_go_byte_for_byte |  |  |
| M3-0040 | Devin | covered | crates/cpa-exec/src/devin_auth.rs (PKCE login), crates/cpa-exec/src/devin*.rs; devin_tests.rs (17 Go-fixture tests, tests/device_fixtures/devin: 46 recorded cases) |  |  |
| M3-0041 | OpenAI-compatible | covered | crates/cpa-exec/src/openai_compat.rs; compat scenarios (108); crates/cpa-server/tests/openai_compat_routes.rs |  |  |

### M3: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0042 | Codex on-disk typed core | covered | crates/cpa-exec/src/codex_oauth_tests.rs login_file_bytes_match_go_storage_serializer, existing_file_keeps_user_fields_but_never_old_tokens |  |  |
| M3-0043 | Kimi on-disk typed core | covered | crates/cpa-exec/src/kimi_auth.rs; crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0044 | xAI on-disk typed core | covered | crates/cpa-exec/src/xai_auth_tests.rs login_matches_go_manager_and_file_store |  |  |
| M3-0045 | Meta on-disk typed core | covered | crates/cpa-exec/src/meta_tests.rs file_names_and_writer_match_go, login_matches_go_requests_and_file |  |  |
| M3-0046 | Vertex on-disk typed core | missing | no Vertex import | ultra/google |  |
| M3-0047 | Meta has a custom writer, not ordinary omitempty serialization: expires_in and dca_expires… | covered | crates/cpa-exec/src/meta_tests.rs file_names_and_writer_match_go, remint_patch_matches_go_metadata, login_merge_skips_an_existing_file_go_cannot_decode |  |  |
| M3-0048 | Antigravity on-disk metadata | missing | no Antigravity credentials | ultra/google |  |
| M3-0049 | Devin on-disk metadata | covered | crates/cpa-exec/src/devin_tests.rs auth_record_matches_go_fixtures, login_save_merges_like_go_manager |  |  |
| M3-0050 | AI Studio | partial | YAML-originated records: crates/cpa-core/src/config/credentials.rs (config_models_reach_the_registry_like_go, resolve_api_key_entry_prefers_the_matching_config_index) | ultra/google | AI Studio relay records are absent. |
| M3-0051 | Built-in legacy Gemini CLI OAuth credentials are not an additional upstream to implement: … | covered | crates/cpa-core/src/credential.rs drops type gemini-cli files like the Go file synthesizer |  |  |

### M3: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0052 | Codex WS codex.rate_limits events (and error headers) normalize primary/secondary/code-rev… | partial | crates/cpa-exec/src/codex_response.rs, codex_quota.rs; codex_tests.rs, codex_ws_tests.rs | ultra/codex | Not ported case by case. |

### M3: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0053 | Codex native fidelity: instructions, tools (including apply_patch/spawn_agent), encrypted … | partial | crates/cpa-exec/src/codex*.rs; codex_go.json executor (18), replay; codex_client vectors; codex_tokens_go.json; codex_tls_tests.rs, codex_ws_tests.rs | ultra/codex | The image tool and direct Images API are absent. |
| M3-0054 | Antigravity request sanitization includes project envelope, thinking-signature validation/… | missing | no Antigravity executor (the translators port the request-side sanitizing) | ultra/google |  |

### M3: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0055 | multimedia.disable-image-generation | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0056 | multimedia.gpt-image-2-base-model | missing | accepted by the config schema; no runtime code reads "gpt-image-2-base-model" | ultra/openai-xai | Images handlers are not ported. |
| M3-0057 | multimedia.video-result-auth-cache-ttl | missing | accepted by the config schema; no runtime code reads "video-result-auth-cache-ttl" | ultra/openai-xai | Videos handlers are not ported. |
| M3-0058 | oauth.providers.antigravity.signature-cache-enabled | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0059 | oauth.providers.antigravity.signature-bypass-strict | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0060 | oauth.providers.antigravity.sensitive-words | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0061 | oauth.providers.antigravity.connection-pool.enabled | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0062 | oauth.providers.antigravity.connection-pool.idle-conn-timeout | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0063 | oauth.providers.antigravity.connection-pool.max-idle-conns-per-host | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M3-0064 | oauth.providers.devin.sensitive-words | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/devin.rs; set in crates/cpa-exec/tests/device_fixtures/devin/chat-sensitive-words.json |  | heuristic: key name match |
| M3-0065 | api-keys.gemini[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+6) |  | heuristic: key name match |
| M3-0066 | api-keys.gemini[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+6) |  | heuristic: key name match |
| M3-0067 | api-keys.gemini[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/tests/fixtures/discovery_cmd_go.json, crates/cliproxy/tests/fixtures/discovery_go.json (+49) |  | heuristic: key name match |
| M3-0068 | api-keys.gemini[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs |  | heuristic: key name match |
| M3-0069 | api-keys.gemini[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0070 | api-keys.gemini[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+2) |  | heuristic: key name match |
| M3-0071 | api-keys.gemini[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0072 | api-keys.gemini[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+12) |  | heuristic: key name match |
| M3-0073 | api-keys.gemini[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0074 | api-keys.gemini[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0075 | api-keys.gemini[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+6) |  | heuristic: key name match |
| M3-0076 | api-keys.gemini[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/codex_client_go.json, crates/cpa-common/tests/fixtures/payload_go.json (+18) |  | heuristic: key name match |
| M3-0077 | api-keys.interactions[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+3) |  | heuristic: key name match |
| M3-0078 | api-keys.interactions[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+3) |  | heuristic: key name match |
| M3-0079 | api-keys.interactions[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cliproxy/tests/fixtures/discovery_go.json, crates/cpa-common/src/thinking/mod.rs (+29) |  | heuristic: key name match |
| M3-0080 | api-keys.interactions[].keys[].models[].display-name | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | ultra/google | heuristic: key name match |
| M3-0081 | api-keys.interactions[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0082 | api-keys.interactions[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+1) |  | heuristic: key name match |
| M3-0083 | api-keys.interactions[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0084 | api-keys.interactions[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+7) |  | heuristic: key name match |
| M3-0085 | api-keys.interactions[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0086 | api-keys.interactions[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0087 | api-keys.interactions[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+6) |  | heuristic: key name match |
| M3-0088 | api-keys.interactions[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/devin_tests.rs, crates/cpa-exec/src/gemini_tests.rs (+14) |  | heuristic: key name match |
| M3-0089 | api-keys.codex[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+13) |  | heuristic: key name match |
| M3-0090 | api-keys.codex[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+11) |  | heuristic: key name match |
| M3-0091 | api-keys.codex[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-server/src/websocket_tests.rs (+3) |  | heuristic: key name match |
| M3-0092 | api-keys.codex[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0093 | api-keys.codex[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/src/thinking/mod.rs (+57) |  | heuristic: key name match |
| M3-0094 | api-keys.codex[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs |  | heuristic: key name match |
| M3-0095 | api-keys.codex[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0096 | api-keys.codex[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-exec/tests/fixtures/gemini_go.json (+3) |  | heuristic: key name match |
| M3-0097 | api-keys.codex[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0098 | api-keys.codex[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0099 | api-keys.codex[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+5) |  | heuristic: key name match |
| M3-0100 | api-keys.codex[].keys[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0101 | api-keys.codex[].keys[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0102 | api-keys.codex[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+8) |  | heuristic: key name match |
| M3-0103 | api-keys.codex[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+45) |  | heuristic: key name match |
| M3-0104 | api-keys.codex[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0105 | api-keys.xai[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-exec/tests/fixtures/xai_go.json (+3) |  | heuristic: key name match |
| M3-0106 | api-keys.xai[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-exec/tests/fixtures/xai_go.json (+2) |  | heuristic: key name match |
| M3-0107 | api-keys.xai[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0108 | api-keys.xai[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0109 | api-keys.xai[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+10) |  | heuristic: key name match |
| M3-0110 | api-keys.xai[].keys[].models[].display-name | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0111 | api-keys.xai[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0112 | api-keys.xai[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0113 | api-keys.xai[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0114 | api-keys.xai[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+2) |  | heuristic: key name match |
| M3-0115 | api-keys.xai[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0116 | api-keys.xai[].keys[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0117 | api-keys.xai[].keys[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0118 | api-keys.xai[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+5) |  | heuristic: key name match |
| M3-0119 | api-keys.xai[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai.rs, crates/cpa-exec/src/xai_tests.rs (+5) |  | heuristic: key name match |
| M3-0120 | api-keys.xai[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/manage_go.rs |  | heuristic: key name match |
| M3-0121 | api-keys.meta[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+12) |  | heuristic: key name match |
| M3-0122 | api-keys.meta[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+11) |  | heuristic: key name match |
| M3-0123 | api-keys.meta[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/websocket_tests.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0124 | api-keys.meta[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0125 | api-keys.meta[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/payload_go.json (+104) |  | heuristic: key name match |
| M3-0126 | api-keys.meta[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M3-0127 | api-keys.meta[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0128 | api-keys.meta[].keys[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+4) |  | heuristic: key name match |
| M3-0129 | api-keys.meta[].keys[].models[].support-configuration-update | covered | read in crates/cpa-core/src/registry/dynamic.rs; set in crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M3-0130 | api-keys.meta[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/claude/testdata/go_executor.json (+4) |  | heuristic: key name match |
| M3-0131 | api-keys.meta[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/claude/testdata/go_executor.json (+13) |  | heuristic: key name match |
| M3-0132 | api-keys.meta[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0133 | api-keys.meta[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0134 | api-keys.meta[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/gemini_tests.rs (+4) |  | heuristic: key name match |
| M3-0135 | api-keys.meta[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/codex_client_go.json, crates/cpa-common/tests/fixtures/payload_go.json (+66) |  | heuristic: key name match |
| M3-0136 | api-keys.meta[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0137 | oauth.providers.xai.inject-x-search | covered | read in crates/cpa-exec/src/xai_request.rs; set in crates/cpa-exec/tests/fixtures/xai_go.json |  | heuristic: key name match |
| M3-0138 | oauth.providers.codex.disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0139 | oauth.providers.codex.stream-bootstrap-buffering | covered | read in crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+1) |  | heuristic: key name match |
| M3-0140 | oauth.providers.codex.stream-bootstrap-timeout | covered | read in crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json |  | heuristic: key name match |
| M3-0141 | oauth.providers.codex.orphan-delegation-compatibility | covered | read in crates/cpa-common/src/codex_client.rs; set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-exec/src/codex_client_tests.rs (+3) |  | heuristic: key name match |
| M3-0142 | oauth.providers.codex.response-steering | missing | accepted by the config schema; no runtime code reads "response-steering" | ultra/codex | Responses steering is not ported. |
| M3-0143 | oauth.providers.codex.header-defaults.user-agent | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-exec/src/claude/detect.rs (+9); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-exec/src/codex_oauth_tests.rs (+8) |  | heuristic: key name match |
| M3-0144 | oauth.providers.codex.header-defaults.beta-features | partial | read in crates/cpa-exec/src/codex_request.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0145 | api-keys.openai-compatibility[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-core/src/config/credentials.rs (+7) |  | heuristic: key name match |
| M3-0146 | api-keys.openai-compatibility[].disabled | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-core/src/config/credentials.rs (+3) |  | heuristic: key name match |
| M3-0147 | api-keys.openai-compatibility[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+4) |  | heuristic: key name match |
| M3-0148 | api-keys.openai-compatibility[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+3) |  | heuristic: key name match |
| M3-0149 | api-keys.openai-compatibility[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-core/src/config/credentials.rs (+7) |  | heuristic: key name match |
| M3-0150 | api-keys.openai-compatibility[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M3-0151 | api-keys.openai-compatibility[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0152 | api-keys.openai-compatibility[].models[].image | covered | read in crates/cpa-common/src/session.rs, crates/cpa-common/src/signature/tests.rs (+34); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+2) |  | heuristic: key name match |
| M3-0153 | api-keys.openai-compatibility[].models[].input-modalities | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0154 | api-keys.openai-compatibility[].models[].output-modalities | missing | accepted by the config schema; no runtime code reads "output-modalities" | ultra/openai-xai |  |
| M3-0155 | api-keys.openai-compatibility[].models[].is-compat | covered | read in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+1) |  | heuristic: key name match |
| M3-0156 | api-keys.openai-compatibility[].models[].use-max-completion-tokens | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0157 | api-keys.openai-compatibility[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/devin.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0158 | api-keys.openai-compatibility[].models[].thinking.max | covered | read in crates/cpa-common/src/codex_client.rs, crates/cpa-common/src/thinking/mod.rs (+8); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/src/scheduler.rs (+2) |  | heuristic: key name match |
| M3-0159 | api-keys.openai-compatibility[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0160 | api-keys.openai-compatibility[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0161 | api-keys.openai-compatibility[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/apply.rs, crates/cpa-common/src/thinking/providers.rs (+5); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+2) |  | heuristic: key name match |
| M3-0162 | api-keys.openai-compatibility[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/openai_compat_tests.rs (+4) |  | heuristic: key name match |
| M3-0163 | api-keys.openai-compatibility[].support-prompt-cache-key | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0164 | api-keys.vertex[].keys[].api-key | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0165 | api-keys.vertex[].base-url | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0166 | api-keys.vertex[].keys[].headers | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0167 | api-keys.vertex[].keys[].models[].name | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0168 | api-keys.vertex[].keys[].models[].display-name | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0169 | api-keys.vertex[].keys[].models[].thinking.min | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0170 | api-keys.vertex[].keys[].models[].thinking.max | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0171 | api-keys.vertex[].keys[].models[].thinking.zero-allowed | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0172 | api-keys.vertex[].keys[].models[].thinking.dynamic-allowed | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M3-0173 | api-keys.vertex[].keys[].models[].thinking.levels | missing | no vertex executor in crates/cpa-exec | ultra/google |  |

### M3: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0174 | Fallback/validation source internal/config/codex_live.go:1-106: `DefaultCodexLiveMediaMaxS… | missing | live-media-relay settings are validated (crates/cpa-core/src/config/validate.rs) but the relay is not wired (ponytail in crates/cpa-server/src/realtime/http.rs) | ultra/realtime |  |
| M3-0175 | Fallback/validation source internal/config/vertex_compat.go:1-130: defaults/normalization … | missing | no Vertex compat config handling | ultra/google |  |

### M3: CLI flags

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0176 | --codex-login | covered | crates/cliproxy/src/main.rs --codex-login -> codex_oauth::login (codex_oauth_tests.rs) |  |  |
| M3-0177 | --codex-device-login | covered | crates/cliproxy/src/main.rs --codex-device-login -> codex_oauth::device_login (device_flow_polls_through_pending_and_exchanges_like_go) |  |  |
| M3-0178 | --antigravity-login | partial | crates/cliproxy/src/main.rs parses -antigravity-login, then exits with 'not supported' | ultra/google | The Antigravity login itself is absent. |
| M3-0179 | --kimi-login | covered | crates/cliproxy/src/main.rs --kimi-login -> kimi_auth::login |  |  |
| M3-0180 | --kimi-ai-login | covered | crates/cliproxy/src/main.rs --kimi-ai-login -> kimi_auth::login("kimi-ai") |  |  |
| M3-0181 | --xai-login | covered | crates/cliproxy/src/main.rs --xai-login -> xai_auth::login (login_matches_go_manager_and_file_store) |  |  |
| M3-0182 | --devin-login | covered | crates/cliproxy/src/main.rs --devin-login -> cpa_exec::devin_auth::login (every_go_flag_parses_in_single_and_double_dash_form; devin_tests.rs code_exchange_matches_go_fixtures) |  |  |
| M3-0183 | --meta-login | covered | crates/cliproxy/src/main.rs --meta-login -> meta_auth::login (login_matches_go_requests_and_file) |  |  |
| M3-0184 | --vertex-import | partial | crates/cliproxy/src/main.rs parses -vertex-import, then exits with 'not supported' | ultra/google | Vertex import itself is absent. |
| M3-0185 | --vertex-import-prefix | partial | crates/cliproxy/src/main.rs parses -vertex-import-prefix | ultra/google | Vertex import itself is absent. |

### M3: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0186 | internal/api/server_devin_oauth_test.go | missing | crates/cpa-server/src/management/oauth.rs answers provider_not_found for Devin (ponytail in its header) | ultra/manage |  |
| M3-0187 | internal/api/server_kimi_oauth_test.go | covered | crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0188 | internal/auth/antigravity/auth_test.go | missing | 0/3 cases matched; auth.go not cited | ultra/google |  |
| M3-0189 | internal/auth/codex/filename_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0190 | internal/auth/codex/jwt_parser_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0191 | internal/auth/codex/openai_auth_test.go | partial | crates/cpa-exec/src/codex_oauth_tests.rs exchange_and_refresh_wire_and_results_match_go, refresh_retry_policy_matches_go_attempt_counts, concurrent_refreshes_of_one_token_share_one_exchange | ultra/codex | Not ported by name. |
| M3-0192 | internal/auth/codex/token_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs existing_file_keeps_user_fields_but_never_old_tokens |  |  |
| M3-0193 | internal/auth/devin/devin_auth_test.go | partial | crates/cpa-exec/src/devin*.rs; devin_tests.rs (17 Go-fixture tests, tests/device_fixtures/devin: 46 recorded cases) | ultra/device-providers | Not ported by name. |
| M3-0194 | internal/auth/devin/record_test.go | partial | crates/cpa-exec/src/devin_tests.rs auth_record_matches_go_fixtures | ultra/device-providers | Not ported by name. |
| M3-0195 | internal/auth/devin/user_status_test.go | partial | crates/cpa-exec/src/devin_tests.rs user_status_matches_go | ultra/device-providers | Not ported by name. |
| M3-0196 | internal/auth/kimi/kimi_proxy_test.go | partial | crates/cpa-exec/src/kimi_http.rs, crate::proxy | ultra/device-providers | Proxy cases are not ported. |
| M3-0197 | internal/auth/kimi/kimi_refresh_test.go | covered | 1/1 cases: crates/cpa-exec/src/kimi_tests.rs |  |  |
| M3-0198 | internal/auth/kimi/kimi_test.go | partial | 1/6 cases: crates/cpa-exec/src/kimi_auth.rs; not matched: TestKimiDomainResolution, TestKimiAuthCreationAndEndpoints, TestKimiCreateTokenStorageAndSave, TestRefreshToken_KimiAIEndpoint … | ultra/device-providers |  |
| M3-0199 | internal/auth/meta/meta_auth_test.go | partial | crates/cpa-exec/src/meta_tests.rs login_matches_go_requests_and_file, device_flow_errors_and_slow_down_follow_go, poll_errors_use_go_partial_decoding_and_key_folding | ultra/device-providers | Equivalents; not ported by name. |
| M3-0200 | internal/auth/xai/xai_auth_test.go | partial | crates/cpa-exec/src/xai_auth_tests.rs (9 tests from xai_auth_go.json) | ultra/openai-xai | Equivalents; not ported by name. |
| M3-0201 | internal/cache/antigravity_reasoning_replay_cache_test.go | partial | crates/cpa-translate/src/replay_cache.rs (in-process store, tombstones, purge; 4 tests) | ultra/google | Owner of internal/cache with the Antigravity executor; snapshots, CAS and Home KV are not ported. |
| M3-0202 | internal/cache/codex_reasoning_replay_cache_test.go | partial | crates/cpa-exec/src/codex_replay.rs (cache per model and Claude Code agent/session); codex_replay_tests.rs replay_scenarios_match_go (Go-generated codex_go.json replay scenarios), entries_keep_the_newest_turns | ultra/codex | Not ported by name. |
| M3-0203 | internal/cache/kimi_thinking_replay_cache_test.go | partial | crates/cpa-exec/src/kimi_replay.rs family_shares_k3_variants_only, conditional_writes_keep_newer_content | ultra/device-providers | Not ported by name. |
| M3-0204 | internal/cache/xai_reasoning_replay_cache_test.go | partial | implementation cites xai_reasoning_replay_cache.go (crates/cpa-exec/src/xai_replay.rs); no case matched by name | ultra/openai-xai |  |
| M3-0205 | internal/client/codex/apply-patch/tool_test.go | partial | crates/cpa-translate/src/apply_patch.rs ports tool.go (is_custom_tool, description, parameters, wrap_input, unwrap_input, escape_input_fragment); exercised by apply_patch goldens and tests/fixtures/apply_patch_responses.json | ultra/translate | Unit cases not ported by name. |
| M3-0206 | internal/client/codex/live/capabilities_test.go | partial | implementation cites capabilities.go (crates/cpa-server/src/realtime/http.rs); no case matched by name | ultra/realtime |  |
| M3-0207 | internal/client/codex/live/client_secret_test.go | partial | implementation cites client_secret.go (crates/cpa-exec/src/codex_live.rs, crates/cpa-server/src/realtime/secrets.rs); no case matched by name | ultra/realtime |  |
| M3-0208 | internal/client/codex/live/live_test.go | partial | 1/25 cases: crates/cpa-server/src/realtime/calls.rs; not matched: TestReadLimitedBodyPreservesPayloadOnReadError, TestHandlerRewritesLiveCallAndSchedulesOAuth, TestMediaCredentialNameUsesSafeIdentity, TestProxyURLForAuthPrefersCredentialOverride … | ultra/realtime |  |
| M3-0209 | internal/client/codex/live/media_test.go | missing | the WebRTC media relay is not wired (ponytail in crates/cpa-server/src/realtime/http.rs) | ultra/realtime |  |
| M3-0210 | internal/client/codex/live/tcp_proxy_test.go | missing | 0/11 cases matched; tcp_proxy.go not cited | ultra/realtime |  |
| M3-0211 | internal/client/codex/models/apply_patch_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | Catalog cases not ported; /v1/models does not serve the client_version catalog (ponytail in crates/cpa-server/src/models.rs). |
| M3-0212 | internal/client/codex/models/models_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | 31 Go cases not ported; /v1/models does not serve the client_version catalog; the remote catalog updater is not ported. |
| M3-0213 | internal/client/codex/models/web_search_capability_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | Not ported by name. |
| M3-0214 | internal/client/codex/optimize-multi-agent-v2/optimize_multi_agent_v2_test.go | partial | cpa_common::codex_client multi-agent v2 rewrites; crates/cpa-common/tests/fixtures/codex_client_go.json (52 Go vectors); crates/cpa-exec/src/codex_client_tests.rs replays_go_translation_and_optimization | ultra/codex | 35 Go cases; not ported by name. |
| M3-0215 | internal/client/codex/optimize-multi-agent-v2/orphan_delegation_test.go | partial | cpa_common::codex_client orphan delegation (oauth.providers.codex.orphan-delegation-compatibility); crates/cpa-exec/src/codex_client_tests.rs orphan_delegation_written_oauth_only_skips_api_keys; codex_client_go.json vectors | ultra/codex | Not ported by name. |
| M3-0216 | internal/client/codex/tool-schema/tool_schema_test.go | partial | cpa_common::payload normalize_codex_tool_integer_types | ultra/codex | Not ported by name. |
| M3-0217 | internal/config/codex_live_test.go | partial | crates/cpa-core/src/config/validate.rs (codex live bounds) | ultra/realtime | Validation only; the live relay is absent. |
| M3-0218 | internal/config/config_meta_test.go | partial | crates/cpa-core/src/config.rs | ultra/manage | Not ported by name. |
| M3-0219 | internal/config/xai_alpha_search_test.go | partial | crates/cpa-core/src/config/credentials.rs (xAI alpha-search); xai_go.json | ultra/openai-xai | Not ported by name. |
| M3-0220 | internal/config/xai_api_key_test.go | partial | crates/cpa-core/src/config/credentials.rs (xAI keys); xai_go.json config_key_base_url_and_headers | ultra/openai-xai | Not ported by name. |
| M3-0221 | internal/logging/requestmeta_test.go | missing | no request metadata logging | ultra/server |  |
| M3-0222 | internal/misc/antigravity_version_test.go | missing | 0/9 cases matched; antigravity_version.go not cited | ultra/google |  |
| M3-0223 | internal/registry/codex_client_models_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | Registry catalog cases not ported; no remote updater (StartCodexClientModelsUpdater). |
| M3-0224 | internal/registry/devin_models_test.go | partial | implementation cites devin_models.go (crates/cpa-core/src/registry/devin.rs, crates/cpa-exec/src/devin_models.rs); no case matched by name | ultra/device-providers |  |
| M3-0225 | internal/runtime/executor/aistudio_executor_test.go | missing | 0/12 cases matched; aistudio_executor.go not cited | ultra/google |  |
| M3-0226 | internal/runtime/executor/antigravity_executor_buildrequest_test.go | missing | 0/14 cases matched; antigravity_executor_buildrequest.go not cited | ultra/google |  |
| M3-0227 | internal/runtime/executor/antigravity_executor_compaction_test.go | missing | 0/5 cases matched; antigravity_executor_compaction.go not cited | ultra/google |  |
| M3-0228 | internal/runtime/executor/antigravity_executor_credits_test.go | missing | 0/15 cases matched; antigravity_executor_credits.go not cited | ultra/google |  |
| M3-0229 | internal/runtime/executor/antigravity_executor_disable_cooling_test.go | missing | 0/5 cases matched; antigravity_executor_disable_cooling.go not cited | ultra/google |  |
| M3-0230 | internal/runtime/executor/antigravity_executor_finish_reason_test.go | missing | 0/4 cases matched; antigravity_executor_finish_reason.go not cited | ultra/google |  |
| M3-0231 | internal/runtime/executor/antigravity_executor_interactions_test.go | missing | 0/1 cases matched; antigravity_executor_interactions.go not cited | ultra/google |  |
| M3-0232 | internal/runtime/executor/antigravity_executor_keepalive_test.go | missing | 0/6 cases matched; antigravity_executor_keepalive.go not cited | ultra/google |  |
| M3-0233 | internal/runtime/executor/antigravity_executor_signature_test.go | missing | 0/24 cases matched; antigravity_executor_signature.go not cited | ultra/google |  |
| M3-0234 | internal/runtime/executor/antigravity_executor_split_usage_test.go | missing | 0/2 cases matched; antigravity_executor_split_usage.go not cited | ultra/google |  |
| M3-0235 | internal/runtime/executor/antigravity_executor_transport_test.go | missing | 0/22 cases matched; antigravity_executor_transport.go not cited | ultra/google |  |
| M3-0236 | internal/runtime/executor/antigravity_preupstream_rewrite_differential_test.go | missing | 0/7 cases matched; antigravity_preupstream_rewrite_differential.go not cited | ultra/google |  |
| M3-0237 | internal/runtime/executor/antigravity_reasoning_replay_clear_test.go | missing | 0/1 cases matched; antigravity_reasoning_replay_clear.go not cited | ultra/google |  |
| M3-0238 | internal/runtime/executor/antigravity_reasoning_replay_index_test.go | missing | 0/19 cases matched; antigravity_reasoning_replay_index.go not cited | ultra/google |  |
| M3-0239 | internal/runtime/executor/antigravity_reasoning_replay_test.go | missing | 0/70 cases matched; antigravity_reasoning_replay.go not cited | ultra/google |  |
| M3-0240 | internal/runtime/executor/antigravity_refresh_issue6199_test.go | missing | 0/9 cases matched; antigravity_refresh_issue6199.go not cited | ultra/google |  |
| M3-0241 | internal/runtime/executor/antigravity_refresh_test.go | missing | 0/2 cases matched; antigravity_refresh.go not cited | ultra/google |  |
| M3-0242 | internal/runtime/executor/antigravity_schema_sanitize_test.go | missing | 0/18 cases matched; antigravity_schema_sanitize.go not cited | ultra/google |  |
| M3-0243 | internal/runtime/executor/codex_executor_auth_test.go | partial | crates/cpa-exec/src/codex_oauth_tests.rs refresh_patch_matches_go_executor_refresh | ultra/codex | Not ported by name. |
| M3-0244 | internal/runtime/executor/codex_executor_cache_test.go | partial | codex_go.json executor oauth_execution_session_prompt_cache, apikey_payload_rules_and_session_headers | ultra/codex | Not ported by name. |
| M3-0245 | internal/runtime/executor/codex_executor_compact_test.go | covered | codex_go.json executor oauth_compact |  |  |
| M3-0246 | internal/runtime/executor/codex_executor_grokbuild_keepalive_test.go | missing | no Grok Build keepalive in the Codex executor | ultra/codex |  |
| M3-0247 | internal/runtime/executor/codex_executor_imagegen_test.go | partial | image_generation handling in crates/cpa-exec/src/codex_request.rs | ultra/codex | 16 Go cases; not ported by name. |
| M3-0248 | internal/runtime/executor/codex_executor_input_ids_test.go | partial | crates/cpa-exec/src/codex_request.rs input IDs | ultra/codex | Not ported by name. |
| M3-0249 | internal/runtime/executor/codex_executor_instructions_test.go | partial | codex_go.json executor oauth_nonstream_no_instructions_free_plan | ultra/codex | Not ported by name. |
| M3-0250 | internal/runtime/executor/codex_executor_parallel_tool_calls_test.go | partial | crates/cpa-exec/src/codex_request.rs parallel_tool_calls | ultra/codex | Not ported by name. |
| M3-0251 | internal/runtime/executor/codex_executor_reasoning_replay_cache_test.go | partial | crates/cpa-exec/src/codex_replay.rs; codex_replay_tests.rs replay_scenarios_match_go (Go-generated end-to-end scenarios) | ultra/codex | 24 Go cases; not ported by name. |
| M3-0252 | internal/runtime/executor/codex_executor_retry_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover; crates/cpa-exec/src/codex_tests.rs (invalid_grant) | ultra/codex | Not ported by name. |
| M3-0253 | internal/runtime/executor/codex_executor_routing_hint_test.go | partial | crates/cpa-exec/src/codex_request.rs routing hint | ultra/codex | Not ported by name. |
| M3-0254 | internal/runtime/executor/codex_executor_signature_test.go | partial | crates/cpa-exec/src/codex_request.rs reasoning sanitizing | ultra/codex | Not ported by name. |
| M3-0255 | internal/runtime/executor/codex_executor_spawn_agent_test.go | partial | cpa_common::codex_client spawn_agent/send_message/followup_task rewrites; crates/cpa-common/tests/fixtures/codex_client_go.json (52 Go vectors), crates/cpa-exec/tests/fixtures/codex_client_translate_go.json | ultra/codex | Not ported by name. |
| M3-0256 | internal/runtime/executor/codex_executor_stream_alloc_test.go | covered | n/a: Go allocation count test |  |  |
| M3-0257 | internal/runtime/executor/codex_executor_stream_output_test.go | partial | codex_go.json executor oauth_stream_native, oauth_stream_terminal_failed_in_stream, oauth_stream_truncated, oauth_capacity_in_stream_500 | ultra/codex | 21 Go cases; not ported by name. |
| M3-0258 | internal/runtime/executor/codex_executor_tokens_test.go | covered | crates/cpa-exec/src/codex_tokens.rs; codex_tokens_tests.rs count_tokens_matches_go, encodings_follow_go_model_prefixes (codex_tokens_go.json) |  |  |
| M3-0259 | internal/runtime/executor/codex_executor_tool_schema_test.go | partial | codex_go.json executor oauth_tool_schema_enum_collapse | ultra/codex | Not ported by name. |
| M3-0260 | internal/runtime/executor/codex_executor_translate_test.go | partial | crates/cpa-exec/src/codex.rs via cpa_translate | ultra/codex | Not ported by name. |
| M3-0261 | internal/runtime/executor/codex_native_fidelity_test.go | partial | crates/cpa-exec/src/codex_tls_tests.rs chatgpt_clienthello_matches_go_chrome_profile; codex_go.json executor | ultra/codex | Not ported by name. |
| M3-0262 | internal/runtime/executor/codex_openai_images_extract_test.go | missing | no Codex Images API | ultra/codex |  |
| M3-0263 | internal/runtime/executor/codex_openai_images_test.go | missing | no Codex Images API | ultra/codex |  |
| M3-0264 | internal/runtime/executor/codex_per_credential_cloaking_issue6034_test.go | partial | disable-codex-cloaking per credential (crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs); codex_go.json apikey_stream_cloak_disabled_headers | ultra/codex | Not ported by name. |
| M3-0265 | internal/runtime/executor/codex_response_model_test.go | partial | crates/cpa-server/src/dispatch.rs response model rewrite | ultra/codex | Not ported by name. |
| M3-0266 | internal/runtime/executor/codex_stream_bootstrap_buffering_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover, oauth_bootstrap_time_budget_spent, oauth_bootstrap_holds_then_releases | ultra/codex | 49 Go cases; not ported by name. |
| M3-0267 | internal/runtime/executor/devin_executor_test.go | partial | implementation cites devin_executor.go (crates/cpa-exec/src/devin.rs, crates/cpa-exec/src/devin_request.rs); no case matched by name | ultra/device-providers |  |
| M3-0268 | internal/runtime/executor/gemini_vertex_executor_test.go | missing | 0/1 cases matched; gemini_vertex_executor.go not cited | ultra/google |  |
| M3-0269 | internal/runtime/executor/helps/antigravity_compaction_test.go | missing | 0/7 cases matched; antigravity_compaction.go not cited | ultra/google |  |
| M3-0270 | internal/runtime/executor/helps/antigravity_grounding_urls_test.go | missing | 0/1 cases matched; antigravity_grounding_urls.go not cited | ultra/google |  |
| M3-0271 | internal/runtime/executor/helps/codex_input_ids_test.go | partial | crates/cpa-exec/src/codex_request.rs input IDs | ultra/codex | Not ported by name. |
| M3-0272 | internal/runtime/executor/helps/codex_multi_agent_v2_summary_test.go | partial | cpa_common::codex_client | ultra/codex | Not ported by name. |
| M3-0273 | internal/runtime/executor/helps/codex_multi_agent_v2_test.go | partial | cpa_common::codex_client | ultra/codex | Not ported by name. |
| M3-0274 | internal/runtime/executor/helps/codex_quota_test.go | partial | crates/cpa-exec/src/codex_quota.rs; codex_go.json quota | ultra/codex | 15 Go cases; not ported by name. |
| M3-0275 | internal/runtime/executor/helps/codex_terminal_incomplete_test.go | partial | codex_go.json executor oauth_nonstream_empty_incomplete | ultra/codex | Not ported by name. |
| M3-0276 | internal/runtime/executor/helps/codex_tool_schema_batch_test.go | partial | cpa_common::payload | ultra/codex | Not ported by name. |
| M3-0277 | internal/runtime/executor/helps/codex_tool_schema_test.go | partial | cpa_common::payload normalize_codex_tool_integer_types; crates/cpa-common/tests/payload.rs | ultra/codex | 22 Go cases; not ported by name. |
| M3-0278 | internal/runtime/executor/helps/devin_models_test.go | partial | implementation cites devin_models.go (crates/cpa-core/src/registry/devin.rs, crates/cpa-exec/src/devin_models.rs); no case matched by name | ultra/device-providers |  |
| M3-0279 | internal/runtime/executor/helps/devin_wire_test.go | partial | implementation cites devin_wire.go (crates/cpa-exec/src/devin_wire.rs); no case matched by name | ultra/device-providers |  |
| M3-0280 | internal/runtime/executor/helps/kimi_responses_test.go | partial | 1/4 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestResolveKimiChatURL, TestResolveKimiClaudeBaseURL, TestNormalizeKimiResponsesInput | ultra/device-providers |  |
| M3-0281 | internal/runtime/executor/helps/meta_tools_test.go | partial | crates/cpa-exec/src/meta_wire.rs | ultra/device-providers | Not ported by name. |
| M3-0282 | internal/runtime/executor/helps/payload_helpers_codex_integer_test.go | partial | crates/cpa-common/tests/payload.rs (payload_go.json) | ultra/server | Not ported by name. |
| M3-0283 | internal/runtime/executor/helps/vertex_payload_helpers_test.go | missing | 0/2 cases matched; vertex_payload_helpers.go not cited | ultra/google |  |
| M3-0284 | internal/runtime/executor/kimi_executor_test.go | partial | 1/41 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestNewKimiExecutorInitializesDelegatedClaudeConfig, TestKimiExecutorRequestToFormatMatchesWireProtocol, TestKimiExecutorResponsesPassthrough, TestKimiExecutorResponsesStreamPassthrough … | ultra/device-providers |  |
| M3-0285 | internal/runtime/executor/kimi_thinking_replay_test.go | partial | crates/cpa-exec/src/kimi_replay.rs; device fixtures (crates/cpa-exec/tests/device_fixtures/kimi) | ultra/device-providers | Not ported by name. |
| M3-0286 | internal/runtime/executor/meta_executor_test.go | partial | 3/27 cases: crates/cpa-exec/src/meta_tests.rs; not matched: TestMetaExecutor_Identifier, TestMetaExecutor_ExecuteSuccessAndRateLimit, TestMetaExecutor_Refresh_RequiresManagerAcceptance, TestMetaExecutor_PrepareConcurrentAccounts … | ultra/device-providers |  |
| M3-0287 | internal/runtime/executor/vertex_proxy_token_test.go | missing | 0/1 cases matched; vertex_proxy_token.go not cited | ultra/google |  |
| M3-0288 | internal/runtime/executor/xai_client_version_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | ultra/openai-xai | Not ported by name. |
| M3-0289 | internal/runtime/executor/xai_configuration_update_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | ultra/openai-xai | Not ported by name; the configuration-update intent is a ponytail in xai_request.rs. |
| M3-0290 | internal/runtime/executor/xai_executor_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | ultra/openai-xai | 146 Go cases; not ported by name. The apply_patch path still uses the seam in xai_apply_patch.rs (cpa_translate::apply_patch_responses is now on master). |
| M3-0291 | internal/runtime/executor/xai_status_err_test.go | partial | crates/cpa-exec/src/xai*.rs; xai_tests.rs go_reference_scenarios (81 Go-generated scenarios, tests/fixtures/xai_go.json) | ultra/openai-xai | Not ported by name. |
| M3-0292 | internal/signature/kimi_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/13 cases also cited by name) |  |  |
| M3-0293 | internal/thinking/apply_codex_usage_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/5 cases also cited by name) |  |  |
| M3-0294 | internal/thinking/kimi_max_clamp_repro_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M3-0295 | internal/translator/antigravity/claude/antigravity_claude_request_test.go | partial | 56/110 cases: crates/cpa-translate/tests/fixtures/pairs/claude-antigravity.json; not matched: TestConvertClaudeRequestToAntigravity_ReattachesDetachedGeminiSignature, TestConvertClaudeRequestToAntigravity_ReattachesLeadingDetachedGeminiSignature, TestConvertClaudeRequestToAntigravity_DropsLegacyRawCarrierFromUserMessage, TestConvertClaudeRequestToAntigravity_DistributesConsecutiveTrailingGeminiCarriers … | ultra/translate |  |
| M3-0296 | internal/translator/antigravity/claude/antigravity_claude_response_test.go | partial | 24/39 cases: crates/cpa-translate/tests/fixtures/pairs/claude-antigravity.json; not matched: TestConvertAntigravityResponseToClaudeStream_EmptyCandidateClosesMessage, TestWebSearchResultsFromGrounding_DeduplicatesAndSkipsEmptyURLs, TestBuildWebSearchCitedTextBlocks_TrimsOverlappingGroundingSupports, TestConvertAntigravityResponseToClaude_VisibleGeminiSignatureUsesLeadingCarrier … | ultra/translate |  |
| M3-0297 | internal/translator/antigravity/claude/signature_validation_test.go | partial | crates/cpa-translate/src/antigravity_claude.rs; claude-antigravity goldens (bypass modes, signature carriers) | ultra/translate | Unit cases not ported. |
| M3-0298 | internal/translator/antigravity/gemini/antigravity_gemini_request_test.go | partial | 13/30 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-antigravity.json; not matched: TestConvertGeminiRequestToAntigravity_ClaudeModelNormalizesStrictClaudeThoughtSignature, TestConvertGeminiRequestToAntigravity_ClaudeModelDropsNonStrictEPrefixThoughtSignature, TestConvertGeminiRequestToAntigravity_ClaudeModelDropsEmptyThoughtText, TestConvertGeminiRequestToAntigravity_ClaudeModelStripsUnneededFunctionCallSignature … | ultra/translate |  |
| M3-0299 | internal/translator/antigravity/gemini/antigravity_gemini_response_test.go | partial | 8/10 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-antigravity.json; not matched: TestRestoreUsageMetadata, TestConvertAntigravityResponseToGeminiStream_ResponseFreeEventDoesNotStartStream | ultra/translate |  |
| M3-0300 | internal/translator/antigravity/gemini/noop_optimization_test.go | partial | crates/cpa-translate/src/antigravity_gemini.rs; goldens | ultra/translate | Table-driven cases not mined. |
| M3-0301 | internal/translator/antigravity/interactions/interactions_antigravity_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-antigravity.json |  |  |
| M3-0302 | internal/translator/antigravity/interactions/interactions_antigravity_test.go | covered | 21/21 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-antigravity.json |  |  |
| M3-0303 | internal/translator/antigravity/interactions/noop_optimization_test.go | partial | crates/cpa-translate/src/antigravity_interactions.rs; goldens | ultra/translate | Table-driven cases not mined. |
| M3-0304 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_file_data_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json |  |  |
| M3-0305 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_request_test.go | partial | 23/24 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json; not matched: TestConvertOpenAIRequestToAntigravityMapsToolChoiceModes | ultra/translate |  |
| M3-0306 | internal/translator/antigravity/openai/chat-completions/antigravity_openai_response_test.go | partial | 12/13 cases: crates/cpa-translate/tests/fixtures/pairs/openai-antigravity.json; not matched: TestConvertAntigravityResponseToOpenAI_ResponseFreeEventDoesNotStartStream | ultra/translate |  |
| M3-0307 | internal/translator/antigravity/openai/chat-completions/noop_optimization_test.go | partial | crates/cpa-translate/src/antigravity_chat.rs; goldens | ultra/translate | Table-driven case not mined. |
| M3-0308 | internal/translator/antigravity/openai/responses/antigravity_openai-responses_request_test.go | partial | 33/39 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-antigravity.json; not matched: TestConvertOpenAIResponsesRequestToAntigravity_ClaudeReasoningKeepsClaudeSignature, TestConvertOpenAIResponsesRequestToAntigravity_ClaudeReasoningDropsIncompatibleSignature, TestConvertOpenAIResponsesRequestToAntigravity_ClaudeReasoningDropsEmptyThinkingText, TestConvertOpenAIResponsesRequestToAntigravity_EmptyClaudeReasoningDoesNotShiftLaterSignature … | ultra/translate |  |
| M3-0309 | internal/translator/antigravity/openai/responses/antigravity_openai-responses_response_test.go | covered | 4/4 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-antigravity.json |  |  |
| M3-0310 | internal/translator/codex/claude/codex_claude_compat_test.go | partial | 2/4 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json; not matched: TestConvertClaudeRequestToCodexWithCompat_PreservesMessageFlushingAndEscapedUnknownSignature, TestConvertClaudeRequestToCodexWithCompat_WhitespaceAndNullSignatures | ultra/translate |  |
| M3-0311 | internal/translator/codex/claude/codex_claude_parallel_function_calls_test.go | partial | crates/cpa-translate/src/codex_claude_response.rs; claude-codex goldens | ultra/translate | Parallel function-call cases not mined. |
| M3-0312 | internal/translator/codex/claude/codex_claude_request_test.go | partial | 17/22 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json; not matched: TestConvertClaudeRequestToCodex_ShortenLongToolUseIDs, TestConvertClaudeRequestToCodex_AssistantThinkingSignatureToReasoningItem, TestConvertClaudeRequestToCodex_PreservesContentOrderAcrossToolAndReasoningItems, TestNormalizeToolParameters_StripsNestedSchemaAndId … | ultra/translate |  |
| M3-0313 | internal/translator/codex/claude/codex_claude_response_test.go | partial | 27/35 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json; not matched: TestConvertCodexResponseToClaude_StreamThinkingKeepsSingleBlockAcrossSummaryParts, TestConvertCodexResponseToClaude_StreamThinkingEmitsSingleSignatureAcrossMultipartReasoning, TestConvertCodexResponseToClaude_StreamThinkingNeverEmitsPreContentEncryptedContent, TestConvertCodexResponseToClaude_StreamThinkingEmitsOneBlockPerReasoningItem … | ultra/translate |  |
| M3-0314 | internal/translator/codex/claude/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/claude-codex.json |  |  |
| M3-0315 | internal/translator/codex/gemini/codex_gemini_request_test.go | partial | 4/5 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-codex.json; not matched: TestConvertGeminiRequestToCodex_PreservesCustomCallIDs | ultra/translate |  |
| M3-0316 | internal/translator/codex/gemini/codex_gemini_response_test.go | covered | 7/7 cases: crates/cpa-translate/tests/fixtures/pairs/gemini-codex.json |  |  |
| M3-0317 | internal/translator/codex/gemini/noop_optimization_test.go | partial | crates/cpa-translate/src/codex_gemini.rs; goldens | ultra/translate | Table-driven cases not mined. |
| M3-0318 | internal/translator/codex/interactions/interactions_codex_test.go | covered | 10/10 cases: crates/cpa-translate/tests/fixtures/pairs/interactions-codex.json |  |  |
| M3-0319 | internal/translator/codex/interactions/noop_optimization_test.go | partial | crates/cpa-translate/src/codex_interactions.rs; goldens | ultra/translate | Table-driven cases not mined. |
| M3-0320 | internal/translator/codex/openai/chat-completions/codex_openai_request_test.go | partial | 32/33 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json; not matched: TestToolCallOutputWithStringifiedImageContent | ultra/translate |  |
| M3-0321 | internal/translator/codex/openai/chat-completions/codex_openai_response_test.go | partial | 29/31 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json; not matched: TestConvertCodexResponseToOpenAI_CustomToolCallStreamDeltas, TestConvertCodexResponseToOpenAI_InterleavedToolCallsKeepStateByItem | ultra/translate |  |
| M3-0322 | internal/translator/codex/openai/chat-completions/noop_optimization_test.go | covered | 1/1 cases: crates/cpa-translate/tests/fixtures/pairs/openai-codex.json |  |  |
| M3-0323 | internal/translator/codex/openai/responses/codex_openai-responses_request_test.go | partial | 21/22 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json; not matched: TestConvertOpenAIResponsesRequestToCodex_ServiceTier | ultra/translate |  |
| M3-0324 | internal/translator/codex/openai/responses/codex_openai-responses_response_test.go | partial | 2/3 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json, crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json; not matched: TestConvertCodexResponseToOpenAIResponses_CreatedIncludesOriginalRequestModel | ultra/translate |  |
| M3-0325 | internal/translator/common/antigravity_tools_test.go | partial | crates/cpa-translate/src/antigravity_*.rs tool renames; goldens | ultra/translate | Unit cases not ported. |
| M3-0326 | internal/translator/common/devin_tools_test.go | partial | crates/cpa-translate/src/responses_interactions.rs Devin tool filtering; goldens | ultra/translate | Unit cases not ported. |
| M3-0327 | sdk/api/handlers/handlers_metadata_test.go | partial | crates/cpa-server/src/session.rs, dispatch.rs (execution metadata) | ultra/server | 15 Go cases; not ported by name. |
| M3-0328 | sdk/api/handlers/openai/codex_client_models_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | The handler that serves the client_version catalog is not ported. |
| M3-0329 | sdk/auth/antigravity_headless_test.go | missing | 0/9 cases matched; antigravity_headless.go not cited | ultra/google |  |
| M3-0330 | sdk/auth/codex_auth_record_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs login_file_bytes_match_go_storage_serializer |  |  |
| M3-0331 | sdk/auth/devin_test.go | partial | implementation cites devin.go (crates/cpa-exec/src/devin_auth.rs); no case matched by name | ultra/device-providers |  |
| M3-0332 | sdk/auth/meta_test.go | partial | implementation cites meta.go (crates/cpa-exec/src/meta_auth.rs); no case matched by name | ultra/device-providers |  |
| M3-0333 | sdk/auth/xai_test.go | partial | implementation cites xai.go (crates/cpa-exec/src/xai_auth.rs); no case matched by name | ultra/openai-xai |  |
| M3-0334 | sdk/cliproxy/antigravity_models_dedup_test.go | missing | 0/9 cases matched; antigravity_models_dedup.go not cited | ultra/google |  |
| M3-0335 | sdk/cliproxy/antigravity_models_timeout_test.go | missing | 0/10 cases matched; antigravity_models_timeout.go not cited | ultra/google |  |
| M3-0336 | sdk/cliproxy/auth/antigravity_credits_test.go | missing | 0/6 cases matched; antigravity_credits.go not cited | ultra/google |  |
| M3-0337 | sdk/cliproxy/auth/codex_forcemap_ws_forward_test.go | partial | crates/cpa-server/src/websocket.rs force-mapping forwarding | ultra/codex | Not ported by name. |
| M3-0338 | sdk/cliproxy/auth/codex_model_not_found_cooldown_test.go | partial | crates/cpa-server/src/classify.rs, crates/cpa-exec/src/codex_response.rs (model not found) | ultra/server | Not ported by name. |
| M3-0339 | sdk/cliproxy/auth/meta_refresh_test.go | covered | crates/cpa-exec/src/meta_tests.rs prepare_remints_and_matches_go_refresh |  |  |
| M3-0340 | sdk/cliproxy/auth/metadata_keys_test.go | partial | crates/cpa-core/src/credential.rs | ultra/server | Not ported by name. |
| M3-0341 | sdk/cliproxy/auth/metadata_merge_test.go | partial | crates/cpa-core/src/credential.rs MetadataPatch | ultra/server | Not ported by name. |
| M3-0342 | sdk/cliproxy/auth/response_model_rewriter_antigravity_sim_test.go | missing | 0/3 cases matched; response_model_rewriter_antigravity_sim.go not cited | ultra/google |  |
| M3-0343 | sdk/cliproxy/auth/selected_auth_metadata_test.go | partial | crates/cpa-server/src/dispatch.rs | ultra/server | Not ported by name. |
| M3-0344 | sdk/cliproxy/auth/selector_antigravity_subagent_test.go | missing | 0/8 cases matched; selector_antigravity_subagent.go not cited | ultra/google |  |
| M3-0345 | sdk/cliproxy/auth/session_affinity_metadata_test.go | partial | crates/cpa-server/src/affinity.rs (3 tests), server_go.json affinity | ultra/server | Not ported by name. |
| M3-0346 | sdk/cliproxy/service_codex_executor_binding_test.go | partial | crates/cpa-exec/src/lib.rs Executors binding | ultra/codex | Not ported by name. |
| M3-0347 | sdk/cliproxy/service_codex_models_test.go | partial | cpa_common::codex_client CLIENT_MODELS_JSON (embedded Go catalog) feeds spawn_agent model lists | ultra/codex | Catalog service cases not ported; no remote updater. |
| M3-0348 | test/codex_incomplete_stream_error_type_test.go | partial | crates/cpa-server/src/openai.rs; codex_go.json oauth_stream_truncated | ultra/codex | Not ported by name. |
| M3-0349 | test/codex_quota_failover_test.go | partial | crates/cpa-server/tests/routes.rs failover_stop_rules_and_cooldown_contracts; codex_go.json oauth_usage_limit_429 | ultra/server | Not ported by name. |
| M3-0350 | test/codex_stream_disconnect_failover_test.go | partial | crates/cpa-server/tests/routes.rs empty_stream_fails_over_and_reports_empty_stream, bootstrap retries | ultra/server | Not ported by name. |

## M4

### M4: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0001 | Optional separate pprof listener `ANY /debug/pprof/` (ServeMux imposes no method constrain… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0002 | Optional separate pprof listener `ANY /debug/pprof/cmdline` (ServeMux imposes no method co… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0003 | Optional separate pprof listener `ANY /debug/pprof/profile` (ServeMux imposes no method co… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0004 | Optional separate pprof listener `ANY /debug/pprof/symbol` (ServeMux imposes no method con… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0005 | Optional separate pprof listener `ANY /debug/pprof/trace` (ServeMux imposes no method cons… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0006 | Optional separate pprof listener `ANY /debug/pprof/allocs` (ServeMux imposes no method con… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0007 | Optional separate pprof listener `ANY /debug/pprof/block` (ServeMux imposes no method cons… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0008 | Optional separate pprof listener `ANY /debug/pprof/goroutine` (ServeMux imposes no method … | missing | no pprof listener in Rust | ultra/server |  |
| M4-0009 | Optional separate pprof listener `ANY /debug/pprof/heap` (ServeMux imposes no method const… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0010 | Optional separate pprof listener `ANY /debug/pprof/mutex` (ServeMux imposes no method cons… | missing | no pprof listener in Rust | ultra/server |  |
| M4-0011 | Optional separate pprof listener `ANY /debug/pprof/threadcreate` (ServeMux imposes no meth… | missing | no pprof listener in Rust | ultra/server |  |

### M4: 3a. Exact provider storage fields and open metadata contract

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0012 | Generic auth JSON is open-ended: `type`, `disabled`, `proxy_url`, `prefix`, `email`, `note… | covered | crates/cpa-core/src/credential.rs keeps open metadata; runtime.rs patch_persists_atomically_preserves_unknown_fields_and_rejects_stale; manage_go.json credentials and synth |  |  |
| M4-0013 | Exact shared metadata spelling normalization: api-key → api_key, base-url → base_url, disa… | partial | crates/cpa-core/src/config/credentials.rs, crates/cpa-server/src/management/auth_files.rs | ultra/manage | Spelling normalization is not tested case by case. |
| M4-0014 | Shared auth override value shapes: disabled boolean; proxy_url/prefix/email/note/base_url/… | partial | crates/cpa-core/src/credential.rs; manage_go.json credentials | ultra/manage | Not tested case by case. |
| M4-0015 | Storage merge contract: ordinary typed writers marshal their JSON tags then flatten Metada… | partial | runtime.rs first_use_persists_disabled_in_go_marshal_form, patch_persists_atomically_preserves_unknown_fields_and_rejects_stale; provider file writers (codex_oauth_tests, meta_tests, xai_auth_tests, devin_tests) | ultra/server | FileStore equal-rewrite skipping and creation intent are not tested. |

### M4: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0016 | Passive quota snapshots are provider-scoped, replacement rather than merge: Claude Anthrop… | partial | Codex quota observations (crates/cpa-exec/src/codex_quota.rs, management delivery 7); Claude quota headers in crates/cpa-exec/src/quota.rs | ultra/server | The 64-header/512-byte snapshot bounds and replacement semantics are not tested. |
| M4-0017 | Claude and Codex model-level-cooling defaults false; credential-wide quota propagation mus… | partial | crates/cpa-exec/src/quota.rs model_shared_and_fast_entitlement_scopes; model-level-cooling read in claude/settings.rs and codex_response.rs; scheduler cooldown_deadlines_floor_backoff_and_sibling_success | ultra/server | Antigravity credits fallback is absent (no Antigravity executor). |

### M4: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0018 | Round-robin maintains previous selected identity per provider/model, not monotonically ind… | covered | crates/cpa-server/src/scheduler.rs rotation_tracks_previous_identity_and_provider_model_keys, smooth_weights_keep_signed_credits_across_temporary_exclusions |  |  |
| M4-0019 | Highest available priority tier wins cold selection; priority default 0. Integer weight om… | partial | scheduler.rs normalization_matches_go_runtime_not_example_config, smooth weights; runtime.rs attribute_only_reload_changes_priority_and_rejects_stale_outcome | ultra/server | YAML weight rejection (float, string, overflow) is not tested. |
| M4-0020 | Established session binding outranks recovered higher-priority credentials; fail over only… | covered | scheduler.rs affinity_beats_recovered_priority_until_unusable_or_expired, session_affinity_matches_go_selector; server_go.json affinity |  |  |
| M4-0021 | Session identity precedence: explicit Claude Code/Codex/OpenCode/pi headers; prompt_cache_… | covered | crates/cpa-server/src/session.rs identity_matches_go (server_go.json session, 53 vectors); cpa_common::session |  |  |
| M4-0022 | Retry rounds: initial round 0 then request-retry additional rounds; each credential admitt… | covered | crates/cpa-server/tests/scheduler_attempts.rs cap_is_per_round_and_skipped_credentials_age_by_round; runtime.rs retry_wait_obeys_exact_cap_and_attempted_quota_floor; routes.rs cooling_selection_waits_for_the_next_retry_round |  |  |
| M4-0023 | Do not conflate classification layers: configured status+body substring/regex rules choose… | covered | crates/cpa-server/src/classify.rs request_faults_follow_go_status_and_body_rules; scheduler.rs rules_are_ordered_status_and_body_matches_with_canonical_precedence; scheduler_attempts.rs stop_continue_and_force_cooldown_are_independent |  |  |
| M4-0024 | Default cooldown status policy includes 401/402/403 30m, unsupported 404 12h absent Retry-… | covered | scheduler.rs cooldowns_match_go_mark_result (server_go.json cooldown, 25 sequences), cooldown_deadlines_floor_backoff_and_sibling_success |  |  |
| M4-0025 | Transport/lifecycle failures and request-scoped faults must not poison credential quota; c… | covered | scheduler_attempts.rs transport_and_precommit_faults_fail_over_without_poisoning, postcommit_stream_fault_is_terminal_and_never_replayed |  |  |
| M4-0026 | Cooling override precedence: credential metadata/attributes, provider settings, global; v8… | partial | crates/cpa-server/src/cooldown_store.rs (server_go.json cooldown_files); scheduler.rs forced_cooling_uses_fallback_and_quota_does_not_reuse_transient_state | ultra/server | Override precedence across credential, provider and global is not tested case by case. |
| M4-0027 | OAuth refresh scheduler has one replaceable loop, 5s check interval, default 16 workers, p… | partial | crates/cpa-server/src/refresh.rs pending_failure_ineffective_rotation_and_invalid_grant_backoffs; runtime.rs auth-auto-refresh-workers default 16 | ultra/server | Loop interval and lifecycle (disabled, removal) cases are not tested. |
| M4-0028 | Model matching honors per-key prefixes, force-model-prefix exceptions, OAuth aliases vs AP… | covered | crates/cpa-core/src/registry/dynamic.rs exclusions_aliases_forks_and_prefixes_follow_go, config_models_and_force_mapping; dispatch.rs force_mapping_rewrites_json_and_sse_model_fields; routes.rs config_models_alias_and_force_mapping_reach_upstream_and_client |  |  |
| M4-0029 | Proxy priority: execution/request override, credential proxy, global proxy, injected conte… | covered | crates/cpa-exec/src/proxy.rs effective_proxy_precedence, proxy_parsing_scheme_mapping_and_redaction, environment proxy tests; tests/fixtures/proxy_go.json (source, redirects, lines) |  |  |
| M4-0030 | Custom header map supports literal values and $Header references copied from downstream; m… | partial | cpa_common::headers resolves_literals_references_and_session; crates/cpa-common/tests/payload.rs custom_headers_match_go | ultra/server | requests.passthrough-headers (response header passthrough allowlist) is not read. |
| M4-0031 | Payload rules operate on final provider payload after protocol translation, using default/… | covered | crates/cpa-common/tests/payload.rs payload_rules_match_go (payload_go.json, 2037 config cases) |  |  |

### M4: 5. Configuration keys: canonical v8 paths, legacy paths, types, defaults, meanings

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0032 | client.codex.optimize-multi-agent-v2 | covered | read in crates/cpa-common/src/codex_client.rs; set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-exec/src/codex_client_tests.rs (+1) |  | heuristic: key name match |
| M4-0033 | client.codex.enable-apply-patch | missing | accepted by the config schema; no runtime code reads "enable-apply-patch" | ultra/codex |  |
| M4-0034 | requests.proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/devin_auth.rs (+6); set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_tests.rs (+5) |  | heuristic: key name match |
| M4-0035 | routing.force-model-prefix | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-core/src/registry/dynamic.rs (+2) |  | heuristic: key name match |
| M4-0036 | observability.logs.request-log | covered | read in crates/cpa-home/src/client.rs, crates/cpa-server/src/management/logs.rs; set in crates/cpa-home/tests/fixtures/go_home_golden.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0037 | requests.passthrough-headers | missing | accepted by the config schema; no runtime code reads "passthrough-headers" | ultra/server |  |
| M4-0038 | server.host | covered | read in crates/cliproxy/src/discovery/advertise.rs, crates/cliproxy/src/discovery/mdns.rs (+13); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/tests/fixtures/discovery_cmd_go.json (+21) |  | heuristic: key name match |
| M4-0039 | server.port | covered | read in crates/cliproxy/src/discovery/mod.rs, crates/cliproxy/src/discovery/tests.rs (+2); set in crates/cliproxy/src/discovery/mdns.rs, crates/cliproxy/src/main.rs (+17) |  | heuristic: key name match |
| M4-0040 | server.trusted-proxies | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/manage_go.rs |  | heuristic: key name match |
| M4-0041 | server.tls.enable | covered | read in crates/cpa-server/src/listener.rs, crates/cpa-server/src/management/oauth.rs; set in crates/cpa-home/src/cert_tests.rs, crates/cpa-server/src/listener.rs |  | heuristic: key name match |
| M4-0042 | server.tls.cert | covered | read in crates/cpa-server/src/listener.rs; set in crates/cpa-home/src/cert_tests.rs, crates/cpa-home/src/client_tests.rs (+1) |  | heuristic: key name match |
| M4-0043 | server.tls.key | covered | read in crates/cpa-common/src/json.rs, crates/cpa-exec/src/claude.rs (+7); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/json.json (+48) |  | heuristic: key name match |
| M4-0044 | oauth.auth-dir | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+3) |  | heuristic: key name match |
| M4-0045 | observability.logs.debug | covered | read in crates/cpa-server/src/logging.rs; set in crates/cpa-server/src/logging.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0046 | observability.pprof.enable | covered | read in crates/cpa-server/src/listener.rs, crates/cpa-server/src/management/oauth.rs; set in crates/cpa-home/src/cert_tests.rs, crates/cpa-server/src/listener.rs |  | heuristic: key name match |
| M4-0047 | observability.pprof.addr | covered | read in crates/cpa-home/src/client.rs, crates/cpa-home/src/fake.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/src/openai_compat_tests.rs (+6) |  | heuristic: key name match |
| M4-0048 | server.commercial-mode | missing | accepted by the config schema; no runtime code reads "commercial-mode" | ultra/server |  |
| M4-0049 | observability.logs.logging-to-file | covered | read in crates/cpa-server/src/logging.rs, crates/cpa-server/src/management/logs.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0050 | observability.logs.logs-max-total-size-mb | partial | read in crates/cpa-server/src/logging.rs; no test sets it | ultra/server | heuristic: key name match |
| M4-0051 | observability.logs.error-logs-max-files | missing | accepted by the config schema; no runtime code reads "error-logs-max-files" | ultra/server |  |
| M4-0052 | observability.usage.usage-statistics-enabled | covered | read in crates/cpa-server/src/usage.rs; set in crates/cpa-server/src/usage.rs, crates/cpa-server/tests/management.rs (+1) |  | heuristic: key name match |
| M4-0053 | observability.usage.redis-usage-queue-retention-seconds | covered | read in crates/cpa-server/src/usage.rs; set in crates/cpa-server/src/usage.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0054 | routing.cooldown.disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/meta_auth.rs (+4) |  | heuristic: key name match |
| M4-0055 | routing.cooldown.save-cooldown-status | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/runtime.rs (+1) |  | heuristic: key name match |
| M4-0056 | routing.cooldown.transient-error-cooldown-seconds | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+1) |  | heuristic: key name match |
| M4-0057 | oauth.auth-auto-refresh-workers | partial | read in crates/cpa-server/src/runtime.rs; no test sets it | ultra/server | heuristic: key name match |
| M4-0058 | routing.retry.request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/meta_auth.rs (+7) |  | heuristic: key name match |
| M4-0059 | routing.retry.max-retry-credentials | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs |  | heuristic: key name match |
| M4-0060 | routing.retry.max-retry-interval | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+3) |  | heuristic: key name match |
| M4-0061 | quota-exceeded.switch-project | missing | accepted by the config schema; no runtime code reads "switch-project" | ultra/server |  |
| M4-0062 | quota-exceeded.switch-preview-model | missing | accepted by the config schema; no runtime code reads "switch-preview-model" | ultra/server |  |
| M4-0063 | oauth.providers.antigravity.antigravity-credits | missing | no antigravity executor in crates/cpa-exec | ultra/google |  |
| M4-0064 | routing.strategy | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/runtime.rs (+2) |  | heuristic: key name match |
| M4-0065 | routing.session-affinity | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+1) |  | heuristic: key name match |
| M4-0066 | routing.session-affinity-ttl | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/src/runtime.rs (+2) |  | heuristic: key name match |
| M4-0067 | routing.session-affinity-subagents | covered | read in crates/cpa-core/src/config.rs; set in crates/cpa-core/src/config.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0068 | api-keys.gemini[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/runtime.rs, crates/cpa-server/tests/fixtures/manage_go.json (+2) |  | heuristic: key name match |
| M4-0069 | api-keys.gemini[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0070 | api-keys.gemini[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/gemini_tests.rs (+23) |  | heuristic: key name match |
| M4-0071 | api-keys.gemini[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0072 | api-keys.gemini[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+7) |  | heuristic: key name match |
| M4-0073 | api-keys.gemini[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0074 | api-keys.gemini[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0075 | api-keys.gemini[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0076 | api-keys.gemini[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/src/runtime.rs, crates/cpa-server/tests/fixtures/manage_go.json (+2) |  | heuristic: key name match |
| M4-0077 | api-keys.gemini[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+13) |  | heuristic: key name match |
| M4-0078 | api-keys.gemini[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+2) |  | heuristic: key name match |
| M4-0079 | api-keys.gemini[].keys[].request-scoped-errors[].match-regexr | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); no test sets it | ultra/google | heuristic: key name match |
| M4-0080 | api-keys.gemini[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_signature_matrix.jsonl.gz (+2) |  | heuristic: key name match |
| M4-0081 | api-keys.interactions[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/fixtures/server_go.json |  | heuristic: key name match |
| M4-0082 | api-keys.interactions[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0083 | api-keys.interactions[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+13) |  | heuristic: key name match |
| M4-0084 | api-keys.interactions[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0085 | api-keys.interactions[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+4) |  | heuristic: key name match |
| M4-0086 | api-keys.interactions[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0087 | api-keys.interactions[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0088 | api-keys.interactions[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0089 | api-keys.interactions[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0090 | api-keys.interactions[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/src/kimi_tests.rs (+11) |  | heuristic: key name match |
| M4-0091 | api-keys.interactions[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0092 | api-keys.interactions[].keys[].request-scoped-errors[].match-regexr | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); no test sets it | ultra/google | heuristic: key name match |
| M4-0093 | api-keys.interactions[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0094 | api-keys.codex[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/runtime.rs, crates/cpa-server/tests/fixtures/manage_go.json (+3) |  | heuristic: key name match |
| M4-0095 | api-keys.codex[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0096 | api-keys.codex[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_oauth_tests.rs (+14) |  | heuristic: key name match |
| M4-0097 | api-keys.codex[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_tests.rs (+1) |  | heuristic: key name match |
| M4-0098 | api-keys.codex[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/xai.rs (+9) |  | heuristic: key name match |
| M4-0099 | api-keys.codex[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0100 | api-keys.codex[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0101 | api-keys.codex[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0102 | api-keys.codex[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/src/runtime.rs, crates/cpa-server/tests/fixtures/manage_go.json (+3) |  | heuristic: key name match |
| M4-0103 | api-keys.codex[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/claude_login_tests.rs, crates/cpa-exec/src/codex_replay_tests.rs (+33) |  | heuristic: key name match |
| M4-0104 | api-keys.codex[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+4) |  | heuristic: key name match |
| M4-0105 | api-keys.codex[].keys[].request-scoped-errors[].match-regexr | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); no test sets it | ultra/codex | heuristic: key name match |
| M4-0106 | api-keys.codex[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_signature_matrix.jsonl.gz (+3) |  | heuristic: key name match |
| M4-0107 | api-keys.xai[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/fixtures/server_go.json (+1) |  | heuristic: key name match |
| M4-0108 | api-keys.xai[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-exec/tests/fixtures/xai_auth_go.json (+1) |  | heuristic: key name match |
| M4-0109 | api-keys.xai[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/fixtures/server_go.json (+1) |  | heuristic: key name match |
| M4-0110 | api-keys.xai[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0111 | api-keys.xai[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/src/xai.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0112 | api-keys.xai[].keys[].models[].force-mapping | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M4-0113 | api-keys.xai[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0114 | api-keys.xai[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0115 | api-keys.xai[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-exec/src/xai_auth.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0116 | api-keys.xai[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/xai.rs, crates/cpa-exec/src/xai_auth_tests.rs (+6) |  | heuristic: key name match |
| M4-0117 | api-keys.xai[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/manage_go.rs |  | heuristic: key name match |
| M4-0118 | api-keys.xai[].keys[].request-scoped-errors[].match-regexr | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); no test sets it | ultra/openai-xai | heuristic: key name match |
| M4-0119 | api-keys.xai[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0120 | api-keys.meta[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-plugin/tests/fixtures/pluginhost_go.json, crates/cpa-plugin/tests/go_host.rs (+6) |  | heuristic: key name match |
| M4-0121 | api-keys.meta[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/meta_auth.rs (+5) |  | heuristic: key name match |
| M4-0122 | api-keys.meta[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/claude/replay.rs (+32) |  | heuristic: key name match |
| M4-0123 | api-keys.meta[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/codex_tests.rs, crates/cpa-exec/src/kimi_auth.rs (+4) |  | heuristic: key name match |
| M4-0124 | api-keys.meta[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+8) |  | heuristic: key name match |
| M4-0125 | api-keys.meta[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0126 | api-keys.meta[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-exec/src/xai_auth.rs (+2) |  | heuristic: key name match |
| M4-0127 | api-keys.meta[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-exec/src/xai_auth.rs (+3) |  | heuristic: key name match |
| M4-0128 | api-keys.meta[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-exec/src/xai_auth.rs (+6) |  | heuristic: key name match |
| M4-0129 | api-keys.meta[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/codex_tests.rs (+59) |  | heuristic: key name match |
| M4-0130 | api-keys.meta[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0131 | api-keys.meta[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-server/src/scheduler.rs |  | heuristic: key name match |
| M4-0132 | api-keys.meta[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/src/scheduler.rs (+3) |  | heuristic: key name match |
| M4-0133 | oauth.providers.codex.model-level-cooling | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0134 | oauth.providers.claude.model-level-cooling | covered | read in crates/cpa-exec/src/claude/settings.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0135 | api-keys.claude[].keys[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/runtime.rs, crates/cpa-server/src/scheduler.rs (+4) |  | heuristic: key name match |
| M4-0136 | api-keys.claude[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+3) |  | heuristic: key name match |
| M4-0137 | api-keys.claude[].keys[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+26) |  | heuristic: key name match |
| M4-0138 | api-keys.claude[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/proxy.rs (+1) |  | heuristic: key name match |
| M4-0139 | api-keys.claude[].keys[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+12) |  | heuristic: key name match |
| M4-0140 | api-keys.claude[].keys[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0141 | api-keys.claude[].keys[].excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-server/tests/fixtures/manage_go.json, crates/cpa-server/tests/management.rs |  | heuristic: key name match |
| M4-0142 | api-keys.claude[].keys[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-core/src/config.rs, crates/cpa-server/src/scheduler.rs (+2) |  | heuristic: key name match |
| M4-0143 | api-keys.claude[].keys[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-core/src/config.rs, crates/cpa-server/src/runtime.rs (+5) |  | heuristic: key name match |
| M4-0144 | api-keys.claude[].keys[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+31) |  | heuristic: key name match |
| M4-0145 | api-keys.claude[].keys[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0146 | api-keys.claude[].keys[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-server/src/scheduler.rs |  | heuristic: key name match |
| M4-0147 | api-keys.claude[].keys[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_signature_matrix.jsonl.gz (+4) |  | heuristic: key name match |
| M4-0148 | api-keys.openai-compatibility[].priority | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M4-0149 | api-keys.openai-compatibility[].prefix | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+3) |  | heuristic: key name match |
| M4-0150 | api-keys.openai-compatibility[].keys[].weight | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0151 | api-keys.openai-compatibility[].keys[].proxy-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0152 | api-keys.openai-compatibility[].models[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+4) |  | heuristic: key name match |
| M4-0153 | api-keys.openai-compatibility[].models[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M4-0154 | api-keys.openai-compatibility[].disable-cooling | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0155 | api-keys.openai-compatibility[].request-retry | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/credentials.rs (+2); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0156 | api-keys.openai-compatibility[].request-scoped-errors[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-exec/src/openai_compat_tests.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+3) |  | heuristic: key name match |
| M4-0157 | api-keys.openai-compatibility[].request-scoped-errors[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-server/src/scheduler.rs, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0158 | api-keys.openai-compatibility[].request-scoped-errors[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-server/src/scheduler.rs |  | heuristic: key name match |
| M4-0159 | api-keys.openai-compatibility[].request-scoped-errors[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/src/scheduler.rs (+1) |  | heuristic: key name match |
| M4-0160 | api-keys.vertex[].keys[].priority | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0161 | api-keys.vertex[].keys[].weight | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0162 | api-keys.vertex[].keys[].prefix | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0163 | api-keys.vertex[].keys[].proxy-url | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0164 | api-keys.vertex[].keys[].models[].alias | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0165 | api-keys.vertex[].keys[].models[].force-mapping | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0166 | api-keys.vertex[].keys[].excluded-models | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0167 | api-keys.vertex[].keys[].disable-cooling | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0168 | api-keys.vertex[].keys[].request-retry | missing | no vertex executor in crates/cpa-exec | ultra/google |  |
| M4-0169 | oauth.excluded-models | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+2); set in crates/cpa-exec/src/meta_auth.rs, crates/cpa-exec/src/xai_auth.rs (+2) |  | heuristic: key name match |
| M4-0170 | oauth.model-alias.{key}[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0171 | oauth.model-alias.{key}[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+8); set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+13) |  | heuristic: key name match |
| M4-0172 | oauth.model-alias.{key}[].fork | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/src/scheduler.rs (+1) |  | heuristic: key name match |
| M4-0173 | oauth.model-alias.{key}[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M4-0174 | oauth.model-alias.{key}[].force-mapping | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-server/tests/routes.rs |  | heuristic: key name match |
| M4-0175 | oauth.request-scoped-errors.{key}[].status | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+33); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+141) |  | heuristic: key name match |
| M4-0176 | oauth.request-scoped-errors.{key}[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0177 | oauth.request-scoped-errors.{key}[].match-regexr | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+1); set in crates/cpa-server/src/scheduler.rs |  | heuristic: key name match |
| M4-0178 | oauth.request-scoped-errors.{key}[].action | covered | read in crates/cpa-common/src/signature/tests.rs, crates/cpa-core/src/config/credentials.rs (+7); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_signature_matrix.jsonl.gz (+4) |  | heuristic: key name match |
| M4-0179 | oauth.settings.{key}[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0180 | oauth.settings.{key}[].alias | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs (+8); set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+13) |  | heuristic: key name match |
| M4-0181 | oauth.settings.{key}[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs, crates/cpa-core/src/registry/dynamic.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M4-0182 | requests.payload.default[].models[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0183 | requests.payload.default[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/realtime/socket.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+9) |  | heuristic: key name match |
| M4-0184 | requests.payload.default[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+28); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+155) |  | heuristic: key name match |
| M4-0185 | requests.payload.default[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0186 | requests.payload.default[].models[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0187 | requests.payload.default[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0188 | requests.payload.default[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0189 | requests.payload.default[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0190 | requests.payload.default[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-translate/src/antigravity_chat.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+9) |  | heuristic: key name match |
| M4-0191 | requests.payload.default-raw[].models[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0192 | requests.payload.default-raw[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/realtime/socket.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+9) |  | heuristic: key name match |
| M4-0193 | requests.payload.default-raw[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+28); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+155) |  | heuristic: key name match |
| M4-0194 | requests.payload.default-raw[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0195 | requests.payload.default-raw[].models[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0196 | requests.payload.default-raw[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0197 | requests.payload.default-raw[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0198 | requests.payload.default-raw[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0199 | requests.payload.default-raw[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-translate/src/antigravity_chat.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+9) |  | heuristic: key name match |
| M4-0200 | requests.payload.override[].models[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0201 | requests.payload.override[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/realtime/socket.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+9) |  | heuristic: key name match |
| M4-0202 | requests.payload.override[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+28); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+155) |  | heuristic: key name match |
| M4-0203 | requests.payload.override[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0204 | requests.payload.override[].models[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0205 | requests.payload.override[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0206 | requests.payload.override[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0207 | requests.payload.override[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0208 | requests.payload.override[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-translate/src/antigravity_chat.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+9) |  | heuristic: key name match |
| M4-0209 | requests.payload.override-raw[].models[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0210 | requests.payload.override-raw[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/realtime/socket.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+9) |  | heuristic: key name match |
| M4-0211 | requests.payload.override-raw[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+28); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+155) |  | heuristic: key name match |
| M4-0212 | requests.payload.override-raw[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0213 | requests.payload.override-raw[].models[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0214 | requests.payload.override-raw[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0215 | requests.payload.override-raw[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0216 | requests.payload.override-raw[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0217 | requests.payload.override-raw[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-translate/src/antigravity_chat.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+9) |  | heuristic: key name match |
| M4-0218 | requests.payload.filter[].models[].name | covered | read in crates/cliproxy/src/discovery/iface.rs, crates/cliproxy/src/discovery/mdns.rs (+82); set in crates/cliproxy/src/discovery/dns.rs, crates/cliproxy/src/discovery/mdns.rs (+210) |  | heuristic: key name match |
| M4-0219 | requests.payload.filter[].models[].protocol | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-server/src/realtime/socket.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-common/tests/payload.rs (+9) |  | heuristic: key name match |
| M4-0220 | requests.payload.filter[].models[].headers | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-core/src/config/credentials.rs (+28); set in crates/cpa-common/src/codex_client_tests.rs, crates/cpa-common/tests/fixtures/codex_client_go.json (+155) |  | heuristic: key name match |
| M4-0221 | requests.payload.filter[].models[].from-protocol | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/tests/device_fixtures/kimi/chat-payload-rules.json (+5) |  | heuristic: key name match |
| M4-0222 | requests.payload.filter[].models[].match | covered | read in crates/cpa-common/src/json.rs, crates/cpa-common/src/payload.rs (+3); set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/src/scheduler.rs (+5) |  | heuristic: key name match |
| M4-0223 | requests.payload.filter[].models[].not-match | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0224 | requests.payload.filter[].models[].exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0225 | requests.payload.filter[].models[].not-exist | covered | read in crates/cpa-common/src/payload.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M4-0226 | requests.payload.filter[].params | covered | read in crates/cpa-common/src/payload.rs, crates/cpa-translate/src/antigravity_chat.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+9) |  | heuristic: key name match |
| M4-0227 | config-version | covered | read in crates/cpa-core/src/config.rs, crates/cpa-core/src/config/text.rs (+1); set in crates/cpa-core/src/config.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+5) |  | heuristic: key name match |
| M4-0228 | api-keys.<family>[].name | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | ultra/server | heuristic: key name match |
| M4-0229 | Legacy/v8/mixed YAML precedence is presence-based, including false/zero/null/empty maps/li… | covered | crates/cpa-core/src/config.rs v8_wins_by_presence_even_when_null_or_empty, legacy_spellings_still_work, null_or_scalar_parents_are_rejected; document.rs tests; manage_go.json config_writes |  |  |
| M4-0230 | Codex multi-agent historical paths precedence: client.codex.optimize-multi-agent-v2; then … | covered | crates/cpa-exec/src/codex_client_tests.rs settings_read_go_config_paths |  |  |
| M4-0231 | Normalization/defaults beyond zero values: optional cloud config may be absent/empty/inval… | partial | crates/cliproxy/src/main.rs cloud standby; crates/cpa-server/src/management (bcrypt keys); crates/cpa-core/src/config/trusted.rs | ultra/manage | Clamps (queue retention, log sizes, caps) are not tested case by case. |

### M4: 5a. Source-defined runtime fallbacks and validation

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0232 | Fallback/validation source internal/config/config_defaults.go:1-8: `DefaultPanelGitHubRepo… | covered | crates/cpa-core/src/config.rs defaults_match_go_loader |  |  |
| M4-0233 | Fallback/validation source internal/config/credential_concurrency.go:1-194: `defaultCPAHea… | covered | crates/cpa-home/src/config.rs defaults_match_go_limiter_config, invalid_limiter_values_are_rejected, lifecycle_sum_overflow_is_rejected |  |  |
| M4-0234 | Fallback/validation source internal/config/credential_in_flight.go:1-87: `DefaultInFlightM… | covered | crates/cpa-home/src/config.rs in_flight_reads_defaults_and_overrides |  |  |
| M4-0235 | Fallback/validation source internal/config/claude_fingerprint_profile.go:1-44: defaults/no… | partial | crates/cpa-exec/src/claude/settings.rs fingerprint-profile | ultra/claude | Normalization and validation are not tested by name. |
| M4-0236 | Fallback/validation source internal/config/disable_image_generation_mode.go:1-147: default… | partial | cpa_common::payload disable-image-generation (payload_go.json) | ultra/server | Not tested by name. |
| M4-0237 | Fallback/validation source internal/config/config_validation.go:1-79: defaults/normalizati… | partial | crates/cpa-core/src/config/validate.rs; manage_go.json load_errors (38) | ultra/manage | Not tested by name. |
| M4-0238 | Fallback/validation source internal/config/config_normalization.go:1-494: defaults/normali… | partial | crates/cpa-core/src/config/sanitize.rs, credentials.rs; manage_go.json materialized_defaults | ultra/manage | Not tested by name. |
| M4-0239 | Fallback/validation source internal/runtime/executor/helps/utls_client.go:1-430: defaults/… | partial | crates/cpa-exec/src/tls.rs, crates/cpa-exec/src/claude/headers.rs | ultra/claude | Not tested by name. |

### M4: CLI flags

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0240 | --discover | covered | crates/cliproxy/src/discovery (tests.rs discover_output_matches_go_byte_for_byte); main.rs every_go_flag_parses_in_single_and_double_dash_form |  |  |
| M4-0241 | --discover-timeout | covered | crates/cliproxy/src/discovery/tests.rs go_durations; main.rs flag parsing |  |  |
| M4-0242 | --discover-json | covered | crates/cliproxy/src/discovery/tests.rs discover_output_matches_go_byte_for_byte; main.rs argv_prescan_matches_go |  |  |
| M4-0243 | --discover-service-type | covered | crates/cliproxy/src/discovery/tests.rs txt_parsing_names_subtypes_and_service_types_match_go |  |  |
| M4-0244 | --standalone | partial | crates/cliproxy/src/main.rs parses --tui/--standalone | ultra/tui | The terminal UI is not available yet (main.rs prints 'TUI error'). |
| M4-0245 | Storage choices: file store default; PGSTORE_*, GITSTORE_*, OBJECTSTORE_* environment conf… | missing | no Postgres, git or object store backends (file store only) | ultra/home |  |
| M4-0246 | Watcher fsnotify: config Write/Create/Rename, immediate-child .json auth Create/Write/Remo… | partial | crates/cpa-server/src/watching.rs hash_cache_rereads_only_changed_or_racy_files; crates/cpa-server/tests/management.rs watcher_keeps_last_good_config_and_reconciles_disabled_deleted_and_self_writes | ultra/manage | Metadata polling stands in for fsnotify (ponytail in watching.rs). |
| M4-0247 | Reload updates provider executors, credential synthesis, model registry/aliases/exclusions… | partial | runtime.rs config_routing_drives_policy_at_startup_and_on_publish, reconcile_keeps_unchanged_revisions_and_never_reuses_old_ones; management.rs watcher tests | ultra/manage | Not tested for every reloadable subsystem. |
| M4-0248 | Watcher dispatcher queues/replaces auth updates, protects snapshots from stale concurrent … | partial | runtime.rs reconcile and stale-lease tests; management.rs watcher tests | ultra/manage | Not tested case by case. |
| M4-0249 | Application logging: logrus debug switch, stdout vs rotating file sink, log directory sele… | partial | crates/cpa-server/src/logging.rs lines_follow_go_log_formatter, rotating_file_follows_lumberjack, cleaner_removes_oldest_logs_but_keeps_main_log | ultra/manage | Commercial mode (dropping heavy request logging) is absent. |
| M4-0250 | Request logs capture inbound method/path/headers/body, selected account/provider, upstream… | partial | crates/cpa-server/src/management/logs.rs serves request and error logs | ultra/server | Request logs are never written: no capture of inbound/upstream requests and responses. |
| M4-0251 | Usage records track input/output/reasoning/cache tokens, model/alias/provider, credential/… | partial | crates/cpa-server/src/usage_record.rs queued_records_match_go; usage.rs; routes.rs usage_queue_records_every_attempt; server_go.json usage | ultra/server | No RESP subscribers (ponytail in usage.rs); plugin and Home usage hooks are not wired. |

### M4: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M4-0252 | cmd/server/main_test.go | partial | crates/cliproxy/src/main.rs argv_prescan_matches_go (TestArgvEnablesBoolFlag); model catalog plan in crates/cpa-server/src/model_updater.rs plan_matches_go | ultra/tui | Example-API-key safe mode and management base URL cases are not ported. |
| M4-0253 | internal/api/middleware/request_logging_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0254 | internal/api/middleware/response_writer_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0255 | internal/api/mux_listener_test.go | missing | no RESP protocol on the main listener (ponytail in crates/cpa-server/src/usage.rs) | ultra/server |  |
| M4-0256 | internal/api/protocol_multiplexer_test.go | missing | no RESP protocol on the main listener (ponytail in crates/cpa-server/src/usage.rs) | ultra/server |  |
| M4-0257 | internal/api/redis_queue_protocol_integration_test.go | missing | no RESP protocol on the main listener (ponytail in crates/cpa-server/src/usage.rs) | ultra/server |  |
| M4-0258 | internal/api/server_apply_patch_config_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0259 | internal/api/server_grok_models_test.go | missing | /v1/models has no Grok Shell catalog (ponytail in crates/cpa-server/src/models.rs) | ultra/openai-xai |  |
| M4-0260 | internal/api/server_models_interceptor_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0261 | internal/api/server_multi_agent_config_test.go | partial | cpa_common::codex_client settings | ultra/codex | Not ported by name. |
| M4-0262 | internal/api/server_sdk_config_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0263 | internal/api/server_stop_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0264 | internal/api/server_test.go | partial | crates/cpa-server/src/lib.rs routes; crates/cpa-server/tests/routes.rs, claude_passthrough.rs | ultra/server | Not ported by name. |
| M4-0265 | internal/auth/claude/anthropic_auth_proxy_test.go | partial | Claude OAuth through crate::proxy (crates/cpa-exec/src/oauth.rs) | ultra/claude | Not ported by name. |
| M4-0266 | internal/cache/bounded_lru_test.go | partial | bounded caches (crates/cpa-exec/src/proxy.rs client_cache_is_bounded_lru, affinity eviction) | ultra/server | Not ported by name. |
| M4-0267 | internal/cache/signature_cache_test.go | partial | crates/cpa-translate/src/replay_cache.rs (signature cache adapter, 4 tests) | ultra/google | Owner of internal/cache with the Antigravity executor. |
| M4-0268 | internal/client/grokbuild/grokbuild_test.go | missing | no Grok Build keepalive transform | ultra/codex |  |
| M4-0269 | internal/client/grokbuild/keepalive_test.go | missing | no Grok Build keepalive transform | ultra/codex |  |
| M4-0270 | internal/clienterror/client_error_test.go | partial | implementation cites client_error.go (crates/cpa-server/src/classify.rs); no case matched by name | ultra/server |  |
| M4-0271 | internal/cmd/discover_test.go | partial | crates/cliproxy/src/discovery (tests.rs, dns.rs, mdns.rs Go-derived cases) | ultra/tui | Not ported by name. |
| M4-0272 | internal/config/api_key_is_compat_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0273 | internal/config/claude_code_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0274 | internal/config/claude_fingerprint_profile_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0275 | internal/config/claude_header_defaults_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0276 | internal/config/client_optimize_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0277 | internal/config/client_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0278 | internal/config/cloak_save_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0279 | internal/config/clone_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0280 | internal/config/config_v8_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0281 | internal/config/cooling_override_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0282 | internal/config/credential_concurrency_fixture_test.go | partial | crates/cpa-home/src/config.rs (limiter and in-flight defaults) | ultra/home | Not ported by name. |
| M4-0283 | internal/config/credential_concurrency_test.go | partial | 3/5 cases: crates/cpa-home/src/config.rs; not matched: TestValidateCredentialConcurrencyAcceptsHomeAuthoritativeHeartbeat, TestValidateCredentialConcurrencyLifecycleRejectsSafetyOverflow; crates/cpa-home/src/config.rs (limiter and in-flight defaults) | ultra/home | Not ported by name. |
| M4-0284 | internal/config/credential_in_flight_test.go | partial | crates/cpa-home/src/config.rs (limiter and in-flight defaults) | ultra/home | Not ported by name. |
| M4-0285 | internal/config/disable_image_generation_mode_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0286 | internal/config/is_compat_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0287 | internal/config/max_context_length_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0288 | internal/config/model_display_name_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0289 | internal/config/oauth_model_alias_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0290 | internal/config/oauth_request_scoped_errors_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0291 | internal/config/oauth_scope_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0292 | internal/config/oauth_settings_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0293 | internal/config/request_retry_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0294 | internal/config/request_scoped_errors_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0295 | internal/config/trusted_proxies_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0296 | internal/config/use_max_completion_tokens_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0297 | internal/config/weight_test.go | partial | crates/cpa-core/src/config*; manage_go.json (materialized_defaults, config_writes, load_errors, synth) | ultra/manage | Not ported by name. |
| M4-0298 | internal/credentialweight/weight_test.go | partial | scheduler.rs smooth weights | ultra/server | Not ported by name. |
| M4-0299 | internal/htmlsanitize/htmlsanitize_test.go | covered | 2/2 cases: crates/cpa-plugin/src/management.rs |  |  |
| M4-0300 | internal/httpfetch/httpfetch_test.go | partial | latest-version and model catalog fetches (crates/cpa-server/src/model_updater.rs, management/observability.rs) | ultra/server | Not ported by name. |
| M4-0301 | internal/httpwire/ordered_conn_test.go | partial | ordered HTTP/1.1 writer (crates/cpa-exec/src/claude/headers.rs); harness wire captures | ultra/claude | Not ported by name. |
| M4-0302 | internal/logging/cpa_trace_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | ultra/server | Not ported by name. |
| M4-0303 | internal/logging/diagnostic_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | ultra/server | Not ported by name. |
| M4-0304 | internal/logging/gin_logger_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | ultra/server | Not ported by name. |
| M4-0305 | internal/logging/global_logger_test.go | partial | crates/cpa-server/src/logging.rs (lumberjack rotation, cleaner, formatter tests) | ultra/manage | Not ported by name. |
| M4-0306 | internal/logging/log_dir_cleaner_test.go | partial | crates/cpa-server/src/logging.rs (lumberjack rotation, cleaner, formatter tests) | ultra/manage | Not ported by name. |
| M4-0307 | internal/logging/request_logger_collision_test.go | missing | request logs are never written | ultra/server |  |
| M4-0308 | internal/logging/requestid_test.go | partial | crates/cpa-server/src/logging.rs; request IDs and CPA trace headers in dispatch.rs | ultra/server | Not ported by name. |
| M4-0309 | internal/misc/credentials_test.go | partial | MetadataPatch merges (crates/cpa-core/src/credential.rs) | ultra/server | Not ported by name. |
| M4-0310 | internal/modelconfig/model_info_test.go | partial | crates/cpa-core/src/registry/dynamic.rs | ultra/server | Not ported by name. |
| M4-0311 | internal/redisqueue/queue_test.go | partial | usage queue (crates/cpa-server/src/usage.rs) without RESP | ultra/server | Not ported by name. |
| M4-0312 | internal/registry/model_definitions_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0313 | internal/registry/model_registry_cache_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0314 | internal/registry/model_registry_credential_quota_regression_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0315 | internal/registry/model_registry_grok_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0316 | internal/registry/model_registry_hook_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0317 | internal/registry/model_registry_quota_refresh_regression_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0318 | internal/registry/model_registry_safety_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0319 | internal/registry/model_updater_test.go | covered | 2/2 cases: crates/cpa-core/src/registry.rs |  |  |
| M4-0320 | internal/registry/web_search_capability_test.go | partial | crates/cpa-core/src/registry*; crates/cpa-server/src/registry.rs (server_go.json availability, by_provider) | ultra/server | Not ported by name. |
| M4-0321 | internal/runtime/executor/apply_patch_bridge_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | ultra/openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0322 | internal/runtime/executor/apply_patch_capability_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | ultra/openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0323 | internal/runtime/executor/apply_patch_identity_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | ultra/openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0324 | internal/runtime/executor/apply_patch_integration_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); executor wiring pending | ultra/device-providers | Kimi and Devin executors do not yet run the apply_patch Responses state. |
| M4-0325 | internal/runtime/executor/apply_patch_repair_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | ultra/openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0326 | internal/runtime/executor/apply_patch_source_stop_test.go | partial | cpa_translate::apply_patch_responses (Go-generated scenarios); xAI uses a seam (crates/cpa-exec/src/xai_apply_patch.rs) | ultra/openai-xai | Executor-level cases need the xAI executor on the real bridge API. |
| M4-0327 | internal/runtime/executor/caching_verify_test.go | partial | Claude cache-control placement (crates/cpa-exec/src/claude/cloak.rs); claude scenarios | ultra/claude | Not ported by name. |
| M4-0328 | internal/runtime/executor/custom_magic_headers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0329 | internal/runtime/executor/executor_payload_optimization_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0330 | internal/runtime/executor/helps/apply_patch_responses_test.go | covered | 19/19 cases: crates/cpa-translate/tests/fixtures/apply_patch_responses.json |  |  |
| M4-0331 | internal/runtime/executor/helps/apply_patch_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0332 | internal/runtime/executor/helps/cache_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0333 | internal/runtime/executor/helps/claude_mcp_alias_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0334 | internal/runtime/executor/helps/derived_session_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0335 | internal/runtime/executor/helps/logging_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0336 | internal/runtime/executor/helps/model_capabilities_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0337 | internal/runtime/executor/helps/payload_helpers_disable_image_generation_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0338 | internal/runtime/executor/helps/payload_mutations_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0339 | internal/runtime/executor/helps/proxy_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0340 | internal/runtime/executor/helps/request_pair_compat_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0341 | internal/runtime/executor/helps/response_model_multiprovider_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0342 | internal/runtime/executor/helps/response_model_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0343 | internal/runtime/executor/helps/responses_ttft_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0344 | internal/runtime/executor/helps/responses_usage_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0345 | internal/runtime/executor/helps/session_id_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0346 | internal/runtime/executor/helps/stream_response_model_observer_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0347 | internal/runtime/executor/helps/transport_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0348 | internal/runtime/executor/helps/usage_helpers_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0349 | internal/runtime/executor/helps/user_id_cache_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0350 | internal/runtime/executor/helps/utls_client_alpn_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0351 | internal/runtime/executor/helps/utls_client_resumption_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0352 | internal/runtime/executor/helps/utls_client_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0353 | internal/runtime/executor/oauth_scope_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0354 | internal/runtime/executor/request_proxy_priority_test.go | covered | crates/cpa-exec/src/proxy.rs effective_proxy_precedence; proxy_go.json source |  |  |
| M4-0355 | internal/runtime/executor/response_model_multiprovider_test.go | partial | crates/cpa-exec executors; cpa_common::headers, payload | ultra/server | Not ported by name. |
| M4-0356 | internal/safemode/example_api_keys_test.go | missing | no example-API-key safe mode | ultra/server |  |
| M4-0357 | internal/signature/gpt_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/2 cases also cited by name) |  |  |
| M4-0358 | internal/signature/grok_validation_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/16 cases also cited by name) |  |  |
| M4-0359 | internal/signature/provider_compatibility_test.go | covered | every call Go's suite makes is recorded and replayed: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (0/24 cases also cited by name) |  |  |
| M4-0360 | internal/store/disabled_login_save_test.go | partial | runtime.rs first_use_persists_disabled_in_go_marshal_form | ultra/server | Not ported by name. |
| M4-0361 | internal/store/gitstore_test.go | missing | no git or Postgres store | ultra/home |  |
| M4-0362 | internal/store/postgres_cooldown_store_test.go | missing | no git or Postgres store | ultra/home |  |
| M4-0363 | internal/util/github_test.go | partial | latest-version route (crates/cpa-server/src/management/observability.rs) | ultra/manage | Not ported by name. |
| M4-0364 | internal/util/gjson_test.go | covered | crates/cpa-common/tests/json.rs get_matches_gjson (Go-generated vectors) |  |  |
| M4-0365 | internal/util/header_helpers_test.go | partial | implementation cites header_helpers.go (crates/cpa-common/src/headers.rs, crates/cpa-common/src/lib.rs); no case matched by name | ultra/server |  |
| M4-0366 | internal/util/nocopy_invariant_test.go | covered | n/a: Go slice-aliasing invariants |  |  |
| M4-0367 | internal/util/responses_tools_test.go | partial | implementation cites responses_tools.go (crates/cpa-translate/src/responses_tools.rs); no case matched by name | ultra/server |  |
| M4-0368 | internal/util/sanitize_test.go | partial | crates/cpa-server/src/sanitize.rs sanitize_matches_go | ultra/server | Not ported by name. |
| M4-0369 | internal/watcher/diff/config_diff_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0370 | internal/watcher/diff/cooling_override_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0371 | internal/watcher/diff/model_compat_hash_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0372 | internal/watcher/diff/model_hash_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0373 | internal/watcher/diff/oauth_excluded_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0374 | internal/watcher/diff/oauth_model_alias_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0375 | internal/watcher/diff/oauth_request_scoped_errors_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0376 | internal/watcher/diff/oauth_settings_test.go | missing | no config diff summaries on reload | ultra/server |  |
| M4-0377 | internal/watcher/dispatcher_snapshot_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0378 | internal/watcher/synthesizer/config_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0379 | internal/watcher/synthesizer/cooling_override_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0380 | internal/watcher/synthesizer/file_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0381 | internal/watcher/synthesizer/helpers_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0382 | internal/watcher/watcher_test.go | partial | crates/cpa-server/src/watching.rs; tests/management.rs watcher tests; manage_go.json synth | ultra/manage | Not ported by name. |
| M4-0383 | sdk/access/registry_test.go | partial | crates/cpa-server/src/access.rs | ultra/server | Not ported by name. |
| M4-0384 | sdk/api/handlers/apply_patch_capability_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0385 | sdk/api/handlers/handlers_error_response_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0386 | sdk/api/handlers/handlers_interceptors_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0387 | sdk/api/handlers/handlers_model_router_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0388 | sdk/api/handlers/handlers_request_details_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0389 | sdk/api/handlers/handlers_stream_bootstrap_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0390 | sdk/api/handlers/header_filter_test.go | missing | requests.passthrough-headers is not read | ultra/server |  |
| M4-0391 | sdk/api/handlers/model_execution_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0392 | sdk/api/handlers/retry_deadline_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0393 | sdk/api/handlers/stream_forwarder_test.go | partial | crates/cpa-server/src/dispatch.rs, errors.rs, respond.rs; tests/routes.rs | ultra/server | Not ported by name. |
| M4-0394 | sdk/auth/filestore_disabled_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | ultra/server | Not ported by name. |
| M4-0395 | sdk/auth/filestore_proxy_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | ultra/server | Not ported by name. |
| M4-0396 | sdk/auth/filestore_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | ultra/server | Not ported by name. |
| M4-0397 | sdk/auth/manager_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | ultra/server | Not ported by name. |
| M4-0398 | sdk/auth/refresh_registry_test.go | partial | crates/cpa-server/src/runtime.rs persistence and refresh | ultra/server | Not ported by name. |
| M4-0399 | sdk/cliproxy/auth/api_key_model_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0400 | sdk/cliproxy/auth/api_key_model_capabilities_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0401 | sdk/cliproxy/auth/api_key_model_compat_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0402 | sdk/cliproxy/auth/apply_patch_capability_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0403 | sdk/cliproxy/auth/auto_refresh_issue6199_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0404 | sdk/cliproxy/auth/auto_refresh_loop_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0405 | sdk/cliproxy/auth/catalog_credential_quota_regression_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0406 | sdk/cliproxy/auth/classification_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0407 | sdk/cliproxy/auth/claude_ratelimit_cooldown_test.go | partial | crates/cpa-exec/src/quota.rs; scheduler_attempts.rs | ultra/claude | Not ported by name. |
| M4-0408 | sdk/cliproxy/auth/conductor_alias_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0409 | sdk/cliproxy/auth/conductor_availability_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0410 | sdk/cliproxy/auth/conductor_claude_cancellation_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0411 | sdk/cliproxy/auth/conductor_cloudflare_520_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0412 | sdk/cliproxy/auth/conductor_compact_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0413 | sdk/cliproxy/auth/conductor_cooldown_monotonic_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0414 | sdk/cliproxy/auth/conductor_cooling_precedence_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0415 | sdk/cliproxy/auth/conductor_credits_candidates_test.go | missing | no Antigravity credits fallback | ultra/google |  |
| M4-0416 | sdk/cliproxy/auth/conductor_execution_error_priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0417 | sdk/cliproxy/auth/conductor_execution_quota_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0418 | sdk/cliproxy/auth/conductor_executor_replace_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0419 | sdk/cliproxy/auth/conductor_fast_error_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0420 | sdk/cliproxy/auth/conductor_force_mapping_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0421 | sdk/cliproxy/auth/conductor_oauth_alias_nofork_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0422 | sdk/cliproxy/auth/conductor_oauth_alias_suspension_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0423 | sdk/cliproxy/auth/conductor_oauth_request_scoped_errors_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0424 | sdk/cliproxy/auth/conductor_overrides_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0425 | sdk/cliproxy/auth/conductor_persist_failure_logging_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0426 | sdk/cliproxy/auth/conductor_quota_clock_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0427 | sdk/cliproxy/auth/conductor_recent_requests_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0428 | sdk/cliproxy/auth/conductor_refresh_disabled_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0429 | sdk/cliproxy/auth/conductor_refresh_executor_key_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0430 | sdk/cliproxy/auth/conductor_remove_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0431 | sdk/cliproxy/auth/conductor_request_scoped_errors_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0432 | sdk/cliproxy/auth/conductor_result_policy_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0433 | sdk/cliproxy/auth/conductor_retry_round_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0434 | sdk/cliproxy/auth/conductor_scheduler_cooldown_rebuild_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0435 | sdk/cliproxy/auth/conductor_scheduler_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0436 | sdk/cliproxy/auth/conductor_scheduler_targeted_update_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0437 | sdk/cliproxy/auth/conductor_selection_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0438 | sdk/cliproxy/auth/conductor_session_affinity_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0439 | sdk/cliproxy/auth/conductor_stream_overload_failover_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0440 | sdk/cliproxy/auth/conductor_stream_overload_status_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0441 | sdk/cliproxy/auth/conductor_stream_quota_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0442 | sdk/cliproxy/auth/conductor_subsecond_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0443 | sdk/cliproxy/auth/conductor_transport_retry_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0444 | sdk/cliproxy/auth/conductor_unauthorized_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0445 | sdk/cliproxy/auth/conductor_update_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0446 | sdk/cliproxy/auth/conductor_usage_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0447 | sdk/cliproxy/auth/conductor_warn_logging_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0448 | sdk/cliproxy/auth/conductor_weight_validation_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0449 | sdk/cliproxy/auth/config_apikey_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0450 | sdk/cliproxy/auth/connection_lifecycle_cooldown_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0451 | sdk/cliproxy/auth/cooldown_backoff_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0452 | sdk/cliproxy/auth/cooldown_state_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0453 | sdk/cliproxy/auth/cooldown_view_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0454 | sdk/cliproxy/auth/custom_headers_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0455 | sdk/cliproxy/auth/error_events_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0456 | sdk/cliproxy/auth/errors_compat_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0457 | sdk/cliproxy/auth/force_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0458 | sdk/cliproxy/auth/oauth_model_alias_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0459 | sdk/cliproxy/auth/persist_policy_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0460 | sdk/cliproxy/auth/priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0461 | sdk/cliproxy/auth/quota_signals_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0462 | sdk/cliproxy/auth/request_auth_prepare_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0463 | sdk/cliproxy/auth/request_proxy_refresh_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0464 | sdk/cliproxy/auth/request_termination_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0465 | sdk/cliproxy/auth/response_model_rewriter_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0466 | sdk/cliproxy/auth/retry_deadline_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0467 | sdk/cliproxy/auth/scheduler_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0468 | sdk/cliproxy/auth/selector_lcp_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0469 | sdk/cliproxy/auth/selector_subagent_affinity_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0470 | sdk/cliproxy/auth/selector_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0471 | sdk/cliproxy/auth/session_affinity_lookup_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0472 | sdk/cliproxy/auth/session_affinity_priority_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0473 | sdk/cliproxy/auth/session_cache_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0474 | sdk/cliproxy/auth/types_cooling_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0475 | sdk/cliproxy/auth/types_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0476 | sdk/cliproxy/auth/weight_test.go | partial | crates/cpa-server/src/scheduler.rs tests; crates/cpa-server/tests/scheduler_attempts.rs; server_go.json | ultra/server | Not ported by name. |
| M4-0477 | sdk/cliproxy/builder_weight_validation_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0478 | sdk/cliproxy/config_model_display_name_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0479 | sdk/cliproxy/config_model_max_context_length_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0480 | sdk/cliproxy/executionregistry/concurrency_release_test.go | partial | crates/cpa-home/src/registry.rs | ultra/home | Not ported by name. |
| M4-0481 | sdk/cliproxy/executionregistry/observation_test.go | partial | crates/cpa-home/src/registry.rs | ultra/home | Not ported by name. |
| M4-0482 | sdk/cliproxy/executionregistry/registry_test.go | partial | crates/cpa-home/src/registry.rs | ultra/home | Not ported by name. |
| M4-0483 | sdk/cliproxy/executor/lifecycle_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0484 | sdk/cliproxy/executor/types_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0485 | sdk/cliproxy/pprof_server_test.go | missing | no pprof listener | ultra/server |  |
| M4-0486 | sdk/cliproxy/rtprovider_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0487 | sdk/cliproxy/service_auth_sync_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0488 | sdk/cliproxy/service_config_weight_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0489 | sdk/cliproxy/service_cooldown_store_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0490 | sdk/cliproxy/service_excluded_models_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0491 | sdk/cliproxy/service_executionregistry_test.go | partial | crates/cpa-home/src/registry.rs | ultra/home | Not ported by name. |
| M4-0492 | sdk/cliproxy/service_executor_registration_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0493 | sdk/cliproxy/service_models_config_index_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0494 | sdk/cliproxy/service_oauth_model_alias_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0495 | sdk/cliproxy/service_oauth_settings_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0496 | sdk/cliproxy/service_result_policy_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0497 | sdk/cliproxy/service_stale_state_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0498 | sdk/cliproxy/service_stop_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0499 | sdk/cliproxy/session/identity_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0500 | sdk/cliproxy/session/info_duplicate_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0501 | sdk/cliproxy/session/info_performance_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0502 | sdk/cliproxy/session/info_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0503 | sdk/cliproxy/session/lcp_lookup_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0504 | sdk/cliproxy/session/lcp_test.go | partial | crates/cpa-server/src/session.rs identity_matches_go, affinity.rs; server_go.json session and affinity | ultra/server | Not ported by name. |
| M4-0505 | sdk/cliproxy/usage/accounting_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0506 | sdk/cliproxy/usage/manager_test.go | partial | crates/cpa-server/src/runtime.rs, registry.rs | ultra/server | Not ported by name. |
| M4-0507 | sdk/proxyutil/proxy_test.go | partial | crates/cpa-exec/src/proxy.rs; proxy_go.json | ultra/claude | Not ported by name. |
| M4-0508 | test/builtin_tools_translation_test.go | partial | translator goldens | ultra/translate | Not ported by name. |
| M4-0509 | test/summary_intent_translation_test.go | covered | recorded replay: crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (Summary tests in ./test/) |  |  |
| M4-0510 | test/usage_logging_test.go | partial | crates/cpa-server/tests/routes.rs usage_queue_records_every_attempt | ultra/server | Not ported by name. |
