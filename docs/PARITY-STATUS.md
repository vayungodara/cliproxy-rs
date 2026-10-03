# Parity status against docs/PARITY.md

Audit of master `c78bb56` against CLIProxyAPI `6fecc6e`, item by item. Milestones audited so far: M1, M2, M3. The rest follow in later deliveries, and the audit is re-run at the end.

Statuses:

- **covered**: implemented, and Rust tests or Go-generated fixtures exercise it.
- **partial**: implemented in part, or implemented without tests that pin Go's behaviour. For Go test suites: the behaviour exists and is exercised, but not every Go case is ported.
- **missing**: not implemented.

Gap owner names the thread that should close a partial or missing item. Threads: ultra/claude, ultra/codex, ultra/google, ultra/device-providers (Kimi, Meta, Devin), ultra/openai-xai, ultra/server, ultra/manage, ultra/dashboard, ultra/translate, ultra/realtime ("Realtime and Live"), ultra/plugins ("Plugins"), ultra/home ("Home control plane, credential concurrency, storage") and ultra/tui ("TUI and LAN discovery", also crates/cliproxy startup).

Method: `docs/parity-audit/audit.py` regenerates this file. Routes come from `probe.py`, which starts the binary and requests every listed method and path without credentials (routed pairs answer from the auth guard or handler, unrouted ones 404/405). A route counts as covered when a test requests it. Go test suites are matched case by case against Go test names cited in Rust code and in Go-generated fixtures (fixture names carry `TestName:line`). Every other item, and every suite the matcher cannot see, was judged by reading the Rust code and tests; those judgments live in `docs/parity-audit/manual.tsv` with their evidence. "Not ported by name" means the area is implemented and tested through other cases (usually Go-generated end-to-end scenarios), but the Go suite's own cases are not reproduced one by one.

## Summary

| Milestone | Items | covered | partial | missing |
|---|---:|---:|---:|---:|
| M1 | 118 | 47 | 70 | 1 |
| M2 | 158 | 75 | 74 | 9 |
| M3 | 350 | 120 | 94 | 136 |

### Gaps by owner

| Owner | missing | partial | Missing items |
|---|---:|---:|---|
| ultra/google | 58 | 6 | M3-0019, M3-0033, M3-0035, M3-0036, M3-0046, M3-0048, M3-0054, M3-0058, M3-0059, M3-0060, M3-0061, M3-0062, M3-0063, M3-0164, M3-0165, M3-0166, M3-0167, M3-0168, M3-0169, M3-0170, M3-0171, M3-0172, M3-0173, M3-0175, M3-0178, M3-0184, M3-0185, M3-0188, M3-0222, M3-0225, M3-0226, M3-0227, M3-0228, M3-0229, M3-0230, M3-0231, M3-0232, M3-0233, M3-0234, M3-0235, M3-0236, M3-0237, M3-0238, M3-0239, M3-0240, M3-0241, M3-0242, M3-0268, M3-0269, M3-0270, M3-0283, M3-0287, M3-0329, M3-0334, M3-0335, M3-0336, M3-0342, M3-0344 |
| ultra/openai-xai | 40 | 7 | M3-0001, M3-0002, M3-0003, M3-0004, M3-0005, M3-0006, M3-0007, M3-0009, M3-0010, M3-0011, M3-0056, M3-0057, M3-0105, M3-0106, M3-0107, M3-0108, M3-0109, M3-0110, M3-0111, M3-0112, M3-0113, M3-0114, M3-0115, M3-0116, M3-0117, M3-0118, M3-0119, M3-0120, M3-0137, M3-0152, M3-0154, M3-0204, M3-0219, M3-0220, M3-0288, M3-0289, M3-0290, M3-0291, M2-0139, M2-0150 |
| ultra/codex | 22 | 43 | M3-0097, M3-0141, M3-0142, M3-0202, M3-0205, M3-0211, M3-0212, M3-0213, M3-0215, M3-0223, M3-0246, M3-0251, M3-0255, M3-0262, M3-0263, M3-0328, M2-0145, M2-0146, M2-0147, M2-0148, M2-0149, M3-0347 |
| ultra/device-providers | 16 | 10 | M3-0020, M3-0021, M3-0040, M3-0049, M3-0064, M3-0129, M3-0182, M3-0186, M3-0193, M3-0194, M3-0195, M3-0224, M3-0267, M3-0278, M3-0279, M3-0331 |
| ultra/realtime | 6 | 1 | M3-0174, M3-0206, M3-0207, M3-0208, M3-0209, M3-0210 |
| ultra/server | 3 | 31 | M2-0036, M3-0221, M2-0134 |
| ultra/tui | 1 | 1 | M1-0062 |
| ultra/translate | 0 | 81 | — |
| ultra/claude | 0 | 52 | — |
| ultra/manage | 0 | 2 | — |
| ultra/plugins | 0 | 2 | — |
| ultra/codex, ultra/openai-xai | 0 | 1 | — |
| ultra/home | 0 | 1 | — |

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
| M1-0033 | api-keys.claude[].keys[].models[].max-context-length | partial | parsed into crates/cpa-core/src/registry/dynamic.rs max_context_length | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
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
| M1-0061 | --config | covered | crates/cliproxy/src/main.rs --config (default config.yaml in the working directory, as Go resolves its empty default); Go single-dash flags accepted |  |  |
| M1-0062 | --local-model | missing | crates/cliproxy/src/main.rs has no --local-model | ultra/tui | The flag is rejected at startup, so Go command lines that pass it fail. Rust has no remote catalog updater, so accepting it as a no-op matches behaviour. |
| M1-0063 | Startup loads .env automatically (do not require a real .env for parity fixtures), config-… | partial | crates/cliproxy/src/main.rs (config load, auth-dir expansion, logins) | ultra/tui | .env is not loaded from the working directory; cloud standby and the store backends (Postgres/git/object) are absent. |

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
| M2-0075 | internal/translator/common/apply_patch_identity_test.go | partial | crates/cpa-translate/src/apply_patch.rs; apply_patch goldens | ultra/translate | Unit cases not ported. |
| M2-0076 | internal/translator/common/apply_patch_input_test.go | partial | implementation cites apply_patch_input.go (crates/cpa-translate/src/apply_patch.rs); no case matched by name | ultra/translate |  |
| M2-0077 | internal/translator/common/apply_patch_responses_test.go | partial | implementation cites apply_patch_responses.go (crates/cpa-translate/src/codex_responses.rs); no case matched by name | ultra/translate |  |
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
| M3-0001 | POST /v1/images/generations | missing | probe: POST /v1/images/generations -> 404 (not routed) | ultra/openai-xai | Images handlers (xAI, Codex image tool and OpenAI-compatible images) are not routed. |
| M3-0002 | POST /v1/images/edits | missing | probe: POST /v1/images/edits -> 404 (not routed) | ultra/openai-xai | Images handlers are not routed. |
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
| M3-0015 | GET /callback | covered | probe: GET /callback -> 400; tests: crates/cpa-exec/src/claude_login_tests.rs, crates/cpa-exec/src/codex_oauth_tests.rs (+3) |  |  |
| M3-0016 | GET /devin/callback | covered | probe: GET /devin/callback -> 400; tests: crates/cpa-server/tests/routes.rs |  |  |

### M3: Temporary OAuth callback listeners (separate from the primary listener)

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0017 | Local OAuth listener `ANY /auth/callback` (browser flow uses GET; ServeMux registration ha… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener, pasted_callbacks_parse_like_go |  |  |
| M3-0018 | Local OAuth listener `ANY /success` (browser flow uses GET; ServeMux registration has no m… | covered | crates/cpa-exec/src/codex_oauth_tests.rs callback_routes_match_go_listener |  |  |
| M3-0019 | Local OAuth listener `ANY /oauth-callback` (browser flow uses GET; ServeMux registration h… | missing | no Antigravity login in Rust | ultra/google |  |
| M3-0020 | Local OAuth listener `ANY /callback` (browser flow uses GET; ServeMux registration has no … | missing | no Devin login listener in Rust | ultra/device-providers | The main-listener /callback and /devin/callback pages exist; the login flow does not. |
| M3-0021 | Local OAuth listener `ANY /` (browser flow uses GET; ServeMux registration has no method r… | missing | no Devin login listener in Rust | ultra/device-providers |  |

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
| M3-0038 | xAI | partial | crates/cpa-exec/src/xai_auth.rs; xai_auth_tests.rs (9 tests, xai_auth_go.json) | ultra/openai-xai | Login and refresh are ported; there is no xAI executor, so xAI credentials never serve requests. |
| M3-0039 | Meta | covered | crates/cpa-exec/src/meta.rs, meta_auth.rs; meta_tests.rs login_matches_go_requests_and_file, mint_errors_follow_go, execution_matches_go_byte_for_byte |  |  |
| M3-0040 | Devin | missing | no Devin login, executor or Connect-RPC wire | ultra/device-providers |  |
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
| M3-0049 | Devin on-disk metadata | missing | no Devin credentials | ultra/device-providers |  |
| M3-0050 | AI Studio | partial | YAML-originated records: crates/cpa-core/src/config/credentials.rs (config_models_reach_the_registry_like_go, resolve_api_key_entry_prefers_the_matching_config_index) | ultra/google | AI Studio relay records are absent. |
| M3-0051 | Built-in legacy Gemini CLI OAuth credentials are not an additional upstream to implement: … | covered | crates/cpa-core/src/credential.rs drops type gemini-cli files like the Go file synthesizer |  |  |

### M3: 3b. Claude wire fidelity, quota, and replay requirements

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0052 | Codex WS codex.rate_limits events (and error headers) normalize primary/secondary/code-rev… | partial | crates/cpa-exec/src/codex_response.rs, codex_quota.rs; codex_tests.rs, codex_ws_tests.rs | ultra/codex | Not ported case by case. |

### M3: 4. Scheduler, routing, retry, cooldown, and proxy semantics

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0053 | Codex native fidelity: instructions, tools (including apply_patch/spawn_agent), encrypted … | partial | crates/cpa-exec/src/codex*.rs; codex_go.json executor (18), codex_tls_tests.rs, codex_ws_tests.rs | ultra/codex | The image tool, direct Images API and spawn_agent rewrites are absent; local token counting differs (ponytail in codex.rs). |
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
| M3-0064 | oauth.providers.devin.sensitive-words | missing | no devin executor in crates/cpa-exec | ultra/device-providers |  |
| M3-0065 | api-keys.gemini[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+5) |  | heuristic: key name match |
| M3-0066 | api-keys.gemini[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+5) |  | heuristic: key name match |
| M3-0067 | api-keys.gemini[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/payload_go.json (+38) |  | heuristic: key name match |
| M3-0068 | api-keys.gemini[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs |  | heuristic: key name match |
| M3-0069 | api-keys.gemini[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0070 | api-keys.gemini[].keys[].models[].is-compat | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+1) |  | heuristic: key name match |
| M3-0071 | api-keys.gemini[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0072 | api-keys.gemini[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/src/thinking/tests.rs (+4); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0073 | api-keys.gemini[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0074 | api-keys.gemini[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0075 | api-keys.gemini[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0076 | api-keys.gemini[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-core/src/config/credentials.rs (+7) |  | heuristic: key name match |
| M3-0077 | api-keys.interactions[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+2) |  | heuristic: key name match |
| M3-0078 | api-keys.interactions[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+2) |  | heuristic: key name match |
| M3-0079 | api-keys.interactions[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-exec/src/gemini_tests.rs (+18) |  | heuristic: key name match |
| M3-0080 | api-keys.interactions[].keys[].models[].display-name | partial | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; no test sets it | ultra/google | heuristic: key name match |
| M3-0081 | api-keys.interactions[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0082 | api-keys.interactions[].keys[].models[].is-compat | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json |  | heuristic: key name match |
| M3-0083 | api-keys.interactions[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+2) |  | heuristic: key name match |
| M3-0084 | api-keys.interactions[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/src/thinking/tests.rs (+4); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+3) |  | heuristic: key name match |
| M3-0085 | api-keys.interactions[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0086 | api-keys.interactions[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0087 | api-keys.interactions[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0088 | api-keys.interactions[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/gemini_tests.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+3) |  | heuristic: key name match |
| M3-0089 | api-keys.codex[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+8) |  | heuristic: key name match |
| M3-0090 | api-keys.codex[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/gemini_go.json (+7) |  | heuristic: key name match |
| M3-0091 | api-keys.codex[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-server/src/websocket_tests.rs (+3) |  | heuristic: key name match |
| M3-0092 | api-keys.codex[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0093 | api-keys.codex[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/payload_go.json (+37) |  | heuristic: key name match |
| M3-0094 | api-keys.codex[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs |  | heuristic: key name match |
| M3-0095 | api-keys.codex[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0096 | api-keys.codex[].keys[].models[].is-compat | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/gemini_go.json, crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0097 | api-keys.codex[].keys[].models[].support-configuration-update | missing | accepted by the config schema; no runtime code reads "support-configuration-update" | ultra/codex |  |
| M3-0098 | api-keys.codex[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-common/tests/fixtures/go_thinking_matrix.jsonl.gz (+1) |  | heuristic: key name match |
| M3-0099 | api-keys.codex[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/src/thinking/tests.rs (+4); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+2) |  | heuristic: key name match |
| M3-0100 | api-keys.codex[].keys[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0101 | api-keys.codex[].keys[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0102 | api-keys.codex[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/tests/fixtures/go_calls.jsonl.gz (+4) |  | heuristic: key name match |
| M3-0103 | api-keys.codex[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-core/src/config/credentials.rs (+21) |  | heuristic: key name match |
| M3-0104 | api-keys.codex[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0105 | api-keys.xai[].keys[].api-key | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0106 | api-keys.xai[].base-url | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0107 | api-keys.xai[].keys[].websockets | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0108 | api-keys.xai[].keys[].alpha-search | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0109 | api-keys.xai[].keys[].models[].name | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0110 | api-keys.xai[].keys[].models[].display-name | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0111 | api-keys.xai[].keys[].models[].max-context-length | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0112 | api-keys.xai[].keys[].models[].is-compat | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0113 | api-keys.xai[].keys[].models[].support-configuration-update | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0114 | api-keys.xai[].keys[].models[].thinking.min | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0115 | api-keys.xai[].keys[].models[].thinking.max | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0116 | api-keys.xai[].keys[].models[].thinking.zero-allowed | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0117 | api-keys.xai[].keys[].models[].thinking.dynamic-allowed | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0118 | api-keys.xai[].keys[].models[].thinking.levels | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0119 | api-keys.xai[].keys[].headers | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0120 | api-keys.xai[].keys[].disable-codex-cloaking | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0121 | api-keys.meta[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+8) |  | heuristic: key name match |
| M3-0122 | api-keys.meta[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-exec/src/claude/testdata/go_executor.json, crates/cpa-exec/src/gemini_tests.rs (+8) |  | heuristic: key name match |
| M3-0123 | api-keys.meta[].keys[].websockets | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/src/websocket_tests.rs, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0124 | api-keys.meta[].keys[].alpha-search | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0125 | api-keys.meta[].keys[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-core/src/registry/dynamic.rs (+86) |  | heuristic: key name match |
| M3-0126 | api-keys.meta[].keys[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M3-0127 | api-keys.meta[].keys[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0128 | api-keys.meta[].keys[].models[].is-compat | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/src/claude/testdata/go_executor.json (+2) |  | heuristic: key name match |
| M3-0129 | api-keys.meta[].keys[].models[].support-configuration-update | missing | accepted by the config schema; no runtime code reads "support-configuration-update" | ultra/device-providers |  |
| M3-0130 | api-keys.meta[].keys[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/claude/testdata/go_executor.json (+2) |  | heuristic: key name match |
| M3-0131 | api-keys.meta[].keys[].models[].thinking.max | covered | read in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/src/thinking/tests.rs (+4); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/claude/testdata/go_executor.json (+3) |  | heuristic: key name match |
| M3-0132 | api-keys.meta[].keys[].models[].thinking.zero-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0133 | api-keys.meta[].keys[].models[].thinking.dynamic-allowed | covered | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/src/gemini_tests.rs |  | heuristic: key name match |
| M3-0134 | api-keys.meta[].keys[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-exec/src/gemini_tests.rs (+1) |  | heuristic: key name match |
| M3-0135 | api-keys.meta[].keys[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/payload_go.json, crates/cpa-exec/src/claude/testdata/go_executor.json (+43) |  | heuristic: key name match |
| M3-0136 | api-keys.meta[].keys[].disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0137 | oauth.providers.xai.inject-x-search | missing | no xai executor in crates/cpa-exec | ultra/openai-xai |  |
| M3-0138 | oauth.providers.codex.disable-codex-cloaking | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json, crates/cpa-server/tests/fixtures/manage_go.json (+1) |  | heuristic: key name match |
| M3-0139 | oauth.providers.codex.stream-bootstrap-buffering | covered | read in crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/src/codex_ws_tests.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+1) |  | heuristic: key name match |
| M3-0140 | oauth.providers.codex.stream-bootstrap-timeout | covered | read in crates/cpa-exec/src/codex_request.rs; set in crates/cpa-exec/tests/fixtures/codex_go.json |  | heuristic: key name match |
| M3-0141 | oauth.providers.codex.orphan-delegation-compatibility | missing | accepted by the config schema; no runtime code reads "orphan-delegation-compatibility" | ultra/codex | Orphan delegation compatibility is not configurable. |
| M3-0142 | oauth.providers.codex.response-steering | missing | accepted by the config schema; no runtime code reads "response-steering" | ultra/codex | Responses steering is not ported. |
| M3-0143 | oauth.providers.codex.header-defaults.user-agent | covered | read in crates/cpa-exec/src/claude/detect.rs, crates/cpa-exec/src/claude/headers.rs (+8); set in crates/cpa-exec/src/codex_oauth_tests.rs, crates/cpa-exec/tests/fixtures/codex_go.json (+6) |  | heuristic: key name match |
| M3-0144 | oauth.providers.codex.header-defaults.beta-features | partial | read in crates/cpa-exec/src/codex_request.rs; no test sets it | ultra/codex | heuristic: key name match |
| M3-0145 | api-keys.openai-compatibility[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+5) |  | heuristic: key name match |
| M3-0146 | api-keys.openai-compatibility[].disabled | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-core/src/config/credentials.rs (+3) |  | heuristic: key name match |
| M3-0147 | api-keys.openai-compatibility[].base-url | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+3) |  | heuristic: key name match |
| M3-0148 | api-keys.openai-compatibility[].keys[].api-key | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json (+2) |  | heuristic: key name match |
| M3-0149 | api-keys.openai-compatibility[].models[].name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs (+5) |  | heuristic: key name match |
| M3-0150 | api-keys.openai-compatibility[].models[].display-name | covered | read in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/config/sanitize.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-core/src/registry/dynamic.rs |  | heuristic: key name match |
| M3-0151 | api-keys.openai-compatibility[].models[].max-context-length | partial | parsed in crates/cpa-core/src/config/sanitize.rs | ultra/codex | Its consumer, the Codex client model catalog, is not ported. |
| M3-0152 | api-keys.openai-compatibility[].models[].image | missing | accepted by the config schema; no runtime code reads "image" | ultra/openai-xai |  |
| M3-0153 | api-keys.openai-compatibility[].models[].input-modalities | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0154 | api-keys.openai-compatibility[].models[].output-modalities | missing | accepted by the config schema; no runtime code reads "output-modalities" | ultra/openai-xai |  |
| M3-0155 | api-keys.openai-compatibility[].models[].is-compat | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-core/src/registry/dynamic.rs, crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0156 | api-keys.openai-compatibility[].models[].use-max-completion-tokens | covered | read in crates/cpa-exec/src/openai_compat_payload.rs; set in crates/cpa-exec/tests/fixtures/openai_compat_go.json |  | heuristic: key name match |
| M3-0157 | api-keys.openai-compatibility[].models[].thinking.min | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0158 | api-keys.openai-compatibility[].models[].thinking.max | covered | read in crates/cpa-common/src/thinking/mod.rs, crates/cpa-common/src/thinking/tests.rs (+4); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/src/scheduler.rs (+1) |  | heuristic: key name match |
| M3-0159 | api-keys.openai-compatibility[].models[].thinking.zero-allowed | partial | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0160 | api-keys.openai-compatibility[].models[].thinking.dynamic-allowed | partial | read in crates/cpa-core/src/registry.rs, crates/cpa-exec/src/openai_compat_payload.rs; no test sets it | ultra/openai-xai | heuristic: key name match |
| M3-0161 | api-keys.openai-compatibility[].models[].thinking.levels | covered | read in crates/cpa-common/src/thinking/tests.rs, crates/cpa-core/src/registry/dynamic.rs (+1); set in crates/cpa-common/tests/fixtures/go_calls.jsonl.gz, crates/cpa-server/tests/fixtures/manage_go.json |  | heuristic: key name match |
| M3-0162 | api-keys.openai-compatibility[].headers | covered | read in crates/cpa-core/src/config/credentials.rs; set in crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/openai_compat_tests.rs (+3) |  | heuristic: key name match |
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
| M3-0174 | Fallback/validation source internal/config/codex_live.go:1-106: `DefaultCodexLiveMediaMaxS… | missing | no Codex live media relay | ultra/realtime |  |
| M3-0175 | Fallback/validation source internal/config/vertex_compat.go:1-130: defaults/normalization … | missing | no Vertex compat config handling | ultra/google |  |

### M3: CLI flags

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0176 | --codex-login | covered | crates/cliproxy/src/main.rs --codex-login -> codex_oauth::login (codex_oauth_tests.rs) |  |  |
| M3-0177 | --codex-device-login | covered | crates/cliproxy/src/main.rs --codex-device-login -> codex_oauth::device_login (device_flow_polls_through_pending_and_exchanges_like_go) |  |  |
| M3-0178 | --antigravity-login | missing | no --antigravity-login | ultra/google | clap rejects the flag. |
| M3-0179 | --kimi-login | covered | crates/cliproxy/src/main.rs --kimi-login -> kimi_auth::login |  |  |
| M3-0180 | --kimi-ai-login | covered | crates/cliproxy/src/main.rs --kimi-ai-login -> kimi_auth::login("kimi-ai") |  |  |
| M3-0181 | --xai-login | covered | crates/cliproxy/src/main.rs --xai-login -> xai_auth::login (login_matches_go_manager_and_file_store) |  |  |
| M3-0182 | --devin-login | missing | no --devin-login | ultra/device-providers | clap rejects the flag. |
| M3-0183 | --meta-login | covered | crates/cliproxy/src/main.rs --meta-login -> meta_auth::login (login_matches_go_requests_and_file) |  |  |
| M3-0184 | --vertex-import | missing | no --vertex-import | ultra/google | clap rejects the flag. |
| M3-0185 | --vertex-import-prefix | missing | no --vertex-import-prefix | ultra/google | clap rejects the flag. |

### M3: Complete compatibility test-suite inventory

| ID | Item | Status | Evidence | Gap owner | Note |
|---|---|---|---|---|---|
| M3-0186 | internal/api/server_devin_oauth_test.go | missing | 0/1 cases matched; server_devin_oauth.go not cited | ultra/device-providers |  |
| M3-0187 | internal/api/server_kimi_oauth_test.go | covered | crates/cpa-server/tests/management.rs kimi_device_login_saves_and_a_cancelled_one_does_not |  |  |
| M3-0188 | internal/auth/antigravity/auth_test.go | missing | 0/3 cases matched; auth.go not cited | ultra/google |  |
| M3-0189 | internal/auth/codex/filename_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0190 | internal/auth/codex/jwt_parser_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs jwt_claims_file_names_and_authorize_url_match_go |  |  |
| M3-0191 | internal/auth/codex/openai_auth_test.go | partial | crates/cpa-exec/src/codex_oauth_tests.rs exchange_and_refresh_wire_and_results_match_go, refresh_retry_policy_matches_go_attempt_counts, concurrent_refreshes_of_one_token_share_one_exchange | ultra/codex | Not ported by name. |
| M3-0192 | internal/auth/codex/token_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs existing_file_keeps_user_fields_but_never_old_tokens |  |  |
| M3-0193 | internal/auth/devin/devin_auth_test.go | missing | 0/7 cases matched; devin_auth.go not cited | ultra/device-providers |  |
| M3-0194 | internal/auth/devin/record_test.go | missing | 0/2 cases matched; record.go not cited | ultra/device-providers |  |
| M3-0195 | internal/auth/devin/user_status_test.go | missing | 0/4 cases matched; user_status.go not cited | ultra/device-providers |  |
| M3-0196 | internal/auth/kimi/kimi_proxy_test.go | partial | crates/cpa-exec/src/kimi_http.rs, crate::proxy | ultra/device-providers | Proxy cases are not ported. |
| M3-0197 | internal/auth/kimi/kimi_refresh_test.go | covered | 1/1 cases: crates/cpa-exec/src/kimi_tests.rs |  |  |
| M3-0198 | internal/auth/kimi/kimi_test.go | partial | 1/6 cases: crates/cpa-exec/src/kimi_auth.rs; not matched: TestKimiDomainResolution, TestKimiAuthCreationAndEndpoints, TestKimiCreateTokenStorageAndSave, TestRefreshToken_KimiAIEndpoint … | ultra/device-providers |  |
| M3-0199 | internal/auth/meta/meta_auth_test.go | partial | crates/cpa-exec/src/meta_tests.rs login_matches_go_requests_and_file, device_flow_errors_and_slow_down_follow_go, poll_errors_use_go_partial_decoding_and_key_folding | ultra/device-providers | Equivalents; not ported by name. |
| M3-0200 | internal/auth/xai/xai_auth_test.go | partial | crates/cpa-exec/src/xai_auth_tests.rs (9 tests from xai_auth_go.json) | ultra/openai-xai | Equivalents; not ported by name. |
| M3-0201 | internal/cache/antigravity_reasoning_replay_cache_test.go | partial | crates/cpa-translate/src/replay_cache.rs (in-process store, tombstones, purge; 4 tests) | ultra/google | Owner of internal/cache with the Antigravity executor; snapshots, CAS and Home KV are not ported. |
| M3-0202 | internal/cache/codex_reasoning_replay_cache_test.go | missing | no Codex reasoning replay cache | ultra/codex |  |
| M3-0203 | internal/cache/kimi_thinking_replay_cache_test.go | partial | crates/cpa-exec/src/kimi_replay.rs family_shares_k3_variants_only, conditional_writes_keep_newer_content | ultra/device-providers | Not ported by name. |
| M3-0204 | internal/cache/xai_reasoning_replay_cache_test.go | missing | 0/7 cases matched; xai_reasoning_replay_cache.go not cited | ultra/openai-xai |  |
| M3-0205 | internal/client/codex/apply-patch/tool_test.go | missing | no internal/client/codex/apply-patch tool definitions | ultra/codex |  |
| M3-0206 | internal/client/codex/live/capabilities_test.go | missing | 0/5 cases matched; capabilities.go not cited | ultra/realtime |  |
| M3-0207 | internal/client/codex/live/client_secret_test.go | missing | 0/10 cases matched; client_secret.go not cited | ultra/realtime |  |
| M3-0208 | internal/client/codex/live/live_test.go | missing | 0/25 cases matched; live.go not cited | ultra/realtime |  |
| M3-0209 | internal/client/codex/live/media_test.go | missing | 0/4 cases matched; media.go not cited | ultra/realtime |  |
| M3-0210 | internal/client/codex/live/tcp_proxy_test.go | missing | 0/11 cases matched; tcp_proxy.go not cited | ultra/realtime |  |
| M3-0211 | internal/client/codex/models/apply_patch_test.go | missing | no Codex client model catalog | ultra/codex |  |
| M3-0212 | internal/client/codex/models/models_test.go | missing | no Codex client model catalog | ultra/codex |  |
| M3-0213 | internal/client/codex/models/web_search_capability_test.go | missing | no Codex client model catalog | ultra/codex |  |
| M3-0214 | internal/client/codex/optimize-multi-agent-v2/optimize_multi_agent_v2_test.go | partial | cpa_common::codex_client rewrites (crates/cpa-exec/src/codex_request.rs, crates/cpa-server/src/websocket.rs) | ultra/codex | 35 Go cases; not ported by name; optimize-multi-agent-v2 has a ponytail in websocket.rs. |
| M3-0215 | internal/client/codex/optimize-multi-agent-v2/orphan_delegation_test.go | missing | orphan delegation compatibility not configurable | ultra/codex |  |
| M3-0216 | internal/client/codex/tool-schema/tool_schema_test.go | partial | cpa_common::payload normalize_codex_tool_integer_types | ultra/codex | Not ported by name. |
| M3-0217 | internal/config/codex_live_test.go | partial | crates/cpa-core/src/config/validate.rs (codex live bounds) | ultra/realtime | Validation only; the live relay is absent. |
| M3-0218 | internal/config/config_meta_test.go | partial | crates/cpa-core/src/config.rs | ultra/manage | Not ported by name. |
| M3-0219 | internal/config/xai_alpha_search_test.go | missing | no xAI executor | ultra/openai-xai |  |
| M3-0220 | internal/config/xai_api_key_test.go | missing | no xAI executor | ultra/openai-xai |  |
| M3-0221 | internal/logging/requestmeta_test.go | missing | no request metadata logging | ultra/server |  |
| M3-0222 | internal/misc/antigravity_version_test.go | missing | 0/9 cases matched; antigravity_version.go not cited | ultra/google |  |
| M3-0223 | internal/registry/codex_client_models_test.go | missing | 0/5 cases matched; codex_client_models.go not cited | ultra/codex |  |
| M3-0224 | internal/registry/devin_models_test.go | missing | 0/8 cases matched; devin_models.go not cited | ultra/device-providers |  |
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
| M3-0251 | internal/runtime/executor/codex_executor_reasoning_replay_cache_test.go | missing | no Codex reasoning replay cache | ultra/codex |  |
| M3-0252 | internal/runtime/executor/codex_executor_retry_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover; crates/cpa-exec/src/codex_tests.rs (invalid_grant) | ultra/codex | Not ported by name. |
| M3-0253 | internal/runtime/executor/codex_executor_routing_hint_test.go | partial | crates/cpa-exec/src/codex_request.rs routing hint | ultra/codex | Not ported by name. |
| M3-0254 | internal/runtime/executor/codex_executor_signature_test.go | partial | crates/cpa-exec/src/codex_request.rs reasoning sanitizing | ultra/codex | Not ported by name. |
| M3-0255 | internal/runtime/executor/codex_executor_spawn_agent_test.go | missing | no spawn_agent handling | ultra/codex |  |
| M3-0256 | internal/runtime/executor/codex_executor_stream_alloc_test.go | covered | n/a: Go allocation count test |  |  |
| M3-0257 | internal/runtime/executor/codex_executor_stream_output_test.go | partial | codex_go.json executor oauth_stream_native, oauth_stream_terminal_failed_in_stream, oauth_stream_truncated, oauth_capacity_in_stream_500 | ultra/codex | 21 Go cases; not ported by name. |
| M3-0258 | internal/runtime/executor/codex_executor_tokens_test.go | partial | crates/cpa-exec/src/codex.rs count path | ultra/codex | Go counts input tokens locally with tiktoken; Rust differs (ponytail in codex.rs). |
| M3-0259 | internal/runtime/executor/codex_executor_tool_schema_test.go | partial | codex_go.json executor oauth_tool_schema_enum_collapse | ultra/codex | Not ported by name. |
| M3-0260 | internal/runtime/executor/codex_executor_translate_test.go | partial | crates/cpa-exec/src/codex.rs via cpa_translate | ultra/codex | Not ported by name. |
| M3-0261 | internal/runtime/executor/codex_native_fidelity_test.go | partial | crates/cpa-exec/src/codex_tls_tests.rs chatgpt_clienthello_matches_go_chrome_profile; codex_go.json executor | ultra/codex | Not ported by name. |
| M3-0262 | internal/runtime/executor/codex_openai_images_extract_test.go | missing | no Codex Images API | ultra/codex |  |
| M3-0263 | internal/runtime/executor/codex_openai_images_test.go | missing | no Codex Images API | ultra/codex |  |
| M3-0264 | internal/runtime/executor/codex_per_credential_cloaking_issue6034_test.go | partial | disable-codex-cloaking per credential (crates/cpa-core/src/config/credentials.rs, crates/cpa-exec/src/codex_request.rs); codex_go.json apikey_stream_cloak_disabled_headers | ultra/codex | Not ported by name. |
| M3-0265 | internal/runtime/executor/codex_response_model_test.go | partial | crates/cpa-server/src/dispatch.rs response model rewrite | ultra/codex | Not ported by name. |
| M3-0266 | internal/runtime/executor/codex_stream_bootstrap_buffering_test.go | partial | codex_go.json executor oauth_bootstrap_overload_failover, oauth_bootstrap_time_budget_spent, oauth_bootstrap_holds_then_releases | ultra/codex | 49 Go cases; not ported by name. |
| M3-0267 | internal/runtime/executor/devin_executor_test.go | missing | 0/56 cases matched; devin_executor.go not cited | ultra/device-providers |  |
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
| M3-0278 | internal/runtime/executor/helps/devin_models_test.go | missing | 0/2 cases matched; devin_models.go not cited | ultra/device-providers |  |
| M3-0279 | internal/runtime/executor/helps/devin_wire_test.go | missing | 0/22 cases matched; devin_wire.go not cited | ultra/device-providers |  |
| M3-0280 | internal/runtime/executor/helps/kimi_responses_test.go | partial | 1/4 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestResolveKimiChatURL, TestResolveKimiClaudeBaseURL, TestNormalizeKimiResponsesInput | ultra/device-providers |  |
| M3-0281 | internal/runtime/executor/helps/meta_tools_test.go | partial | crates/cpa-exec/src/meta_wire.rs | ultra/device-providers | Not ported by name. |
| M3-0282 | internal/runtime/executor/helps/payload_helpers_codex_integer_test.go | partial | crates/cpa-common/tests/payload.rs (payload_go.json) | ultra/server | Not ported by name. |
| M3-0283 | internal/runtime/executor/helps/vertex_payload_helpers_test.go | missing | 0/2 cases matched; vertex_payload_helpers.go not cited | ultra/google |  |
| M3-0284 | internal/runtime/executor/kimi_executor_test.go | partial | 1/41 cases: crates/cpa-exec/src/kimi_tests.rs; not matched: TestNewKimiExecutorInitializesDelegatedClaudeConfig, TestKimiExecutorRequestToFormatMatchesWireProtocol, TestKimiExecutorResponsesPassthrough, TestKimiExecutorResponsesStreamPassthrough … | ultra/device-providers |  |
| M3-0285 | internal/runtime/executor/kimi_thinking_replay_test.go | partial | crates/cpa-exec/src/kimi_replay.rs; device fixtures (crates/cpa-exec/tests/device_fixtures/kimi) | ultra/device-providers | Not ported by name. |
| M3-0286 | internal/runtime/executor/meta_executor_test.go | partial | 3/27 cases: crates/cpa-exec/src/meta_tests.rs; not matched: TestMetaExecutor_Identifier, TestMetaExecutor_ExecuteSuccessAndRateLimit, TestMetaExecutor_Refresh_RequiresManagerAcceptance, TestMetaExecutor_PrepareConcurrentAccounts … | ultra/device-providers |  |
| M3-0287 | internal/runtime/executor/vertex_proxy_token_test.go | missing | 0/1 cases matched; vertex_proxy_token.go not cited | ultra/google |  |
| M3-0288 | internal/runtime/executor/xai_client_version_test.go | missing | 0/3 cases matched; xai_client_version.go not cited | ultra/openai-xai |  |
| M3-0289 | internal/runtime/executor/xai_configuration_update_test.go | missing | 0/2 cases matched; xai_configuration_update.go not cited | ultra/openai-xai |  |
| M3-0290 | internal/runtime/executor/xai_executor_test.go | missing | 0/146 cases matched; xai_executor.go not cited | ultra/openai-xai |  |
| M3-0291 | internal/runtime/executor/xai_status_err_test.go | missing | 0/8 cases matched; xai_status_err.go not cited | ultra/openai-xai |  |
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
| M3-0324 | internal/translator/codex/openai/responses/codex_openai-responses_response_test.go | partial | 2/3 cases: crates/cpa-translate/tests/fixtures/pairs/openai-response-codex.json; not matched: TestConvertCodexResponseToOpenAIResponses_CreatedIncludesOriginalRequestModel | ultra/translate |  |
| M3-0325 | internal/translator/common/antigravity_tools_test.go | partial | crates/cpa-translate/src/antigravity_*.rs tool renames; goldens | ultra/translate | Unit cases not ported. |
| M3-0326 | internal/translator/common/devin_tools_test.go | partial | crates/cpa-translate/src/responses_interactions.rs Devin tool filtering; goldens | ultra/translate | Unit cases not ported. |
| M3-0327 | sdk/api/handlers/handlers_metadata_test.go | partial | crates/cpa-server/src/session.rs, dispatch.rs (execution metadata) | ultra/server | 15 Go cases; not ported by name. |
| M3-0328 | sdk/api/handlers/openai/codex_client_models_test.go | missing | no Codex client model catalog | ultra/codex |  |
| M3-0329 | sdk/auth/antigravity_headless_test.go | missing | 0/9 cases matched; antigravity_headless.go not cited | ultra/google |  |
| M3-0330 | sdk/auth/codex_auth_record_test.go | covered | crates/cpa-exec/src/codex_oauth_tests.rs login_file_bytes_match_go_storage_serializer |  |  |
| M3-0331 | sdk/auth/devin_test.go | missing | 0/11 cases matched; devin.go not cited | ultra/device-providers |  |
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
| M3-0347 | sdk/cliproxy/service_codex_models_test.go | missing | no Codex client model catalog | ultra/codex |  |
| M3-0348 | test/codex_incomplete_stream_error_type_test.go | partial | crates/cpa-server/src/openai.rs; codex_go.json oauth_stream_truncated | ultra/codex | Not ported by name. |
| M3-0349 | test/codex_quota_failover_test.go | partial | crates/cpa-server/tests/routes.rs failover_stop_rules_and_cooldown_contracts; codex_go.json oauth_usage_limit_429 | ultra/server | Not ported by name. |
| M3-0350 | test/codex_stream_disconnect_failover_test.go | partial | crates/cpa-server/tests/routes.rs empty_stream_fails_over_and_reports_empty_stream, bootstrap retries | ultra/server | Not ported by name. |
