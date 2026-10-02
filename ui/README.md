# cliproxy-rs dashboard

A static Svelte 5 dashboard for servers implementing CLIProxyAPI's v8 Management API. No component library, runtime chart library, or server-side rendering. Twelve hash-routed pages, native controls, local typography, and light/dark themes.

## Run and embed

Requires Node 22.12 or newer; Node 26 was used for verification.

```sh
npm ci
npm run dev
npm run check
npm test
npm run build
```

The development server proxies `/v8` and `/healthz` to `127.0.0.1:8317`. Change `vite.config.ts` if your development backend uses another port. A separate server URL can also be entered at login; cross-origin servers must allow the dashboard's origin and expose their version headers.

Embed the complete contents of `dist/`, not only `management.html`. Both `dist/index.html` and `dist/management.html` are supplied. Asset, favicon, and font references are relative. Serve the files unchanged at `/management.html`, `/`, or `/some-prefix/management.html`; hash navigation requires no routing fallback. The login defaults to the page origin. When the Management API itself lives under a prefix, enter its server base explicitly, such as `https://proxy.example/prefix`.

Configuration writes use direct v8 values, not a `{value: ...}` envelope. Editors preview changes and reread their target before saving. This catches observed stale edits, but is not an atomic compare-and-swap: the API has no revision precondition. YAML validation and normalization happen on the server; a successful write may reorder settings or materialize defaults.

The management key stays in application memory and is cleared on logout. Reloading requires login again. Do not run the dashboard over untrusted HTTP. Downloaded credential files and configuration previews contain sensitive values; treat them accordingly. Browser autofill behavior is controlled by the browser, not the application.

## Development fixtures

```sh
VITE_DEV_FIXTURES=true npm run dev -- --port 5174
```

This requires a real management connection. Fixtures supplement real requests only for account health, traffic, and quota views. Their labels and banners are explicit, and sample account actions are disabled. Fixture data is dynamically imported only when both `import.meta.env.DEV` and the explicit flag are true. The production build contains no fixture accounts or traffic, even if the flag is set during build. Empty production telemetry is not represented as zero latency or zero error rate.

Live telemetry is opt-in because `GET /observability/usage/queue` consumes events shared with other consumers. Metrics retain only display fields, never client keys or response bodies. They cover the current browser session, capped at 15 minutes and 10,000 events. Logs use cursor polling every three seconds, retaining 1,500 lines; polling pauses when the document is hidden. A scrolled-up reader is not pulled back to the bottom.

## Endpoint map

All paths below are relative to `/v8/management`. Login uses `GET /config`. Every connected page loads `GET /config` and `GET /credentials`; these common calls are omitted from the table.

| Page | Additional endpoints and operations |
| --- | --- |
| Overview | Opt-in `GET /observability/usage/queue?count=500`; periodic `GET /credentials`. RPM, p50 latency, and 15-minute error rate are calculated locally. |
| Credentials | `POST /credentials` multipart upload; `DELETE /credentials` with names; `GET /credentials/download?name=...`; `GET /credentials/models?name=...`; `PATCH /credentials/status`; `PATCH /credentials/fields`; `POST /credentials/refresh`; `POST /routing/cooldown/reset`. |
| Connect an account | `GET /plugins`; `GET /oauth/auth-url?provider=...&is_webui=true` with optional provider parameters; `GET /oauth/status?state=...`; `POST /oauth/callback` with full `redirect_url`; `DELETE /oauth/session?state=...`; `POST /oauth/import?provider=vertex` multipart upload. |
| Providers | `GET /observability/usage/api-keys`; `GET, PUT /config/api-keys/<family>`. Families: claude, codex, gemini, vertex, openai-compatibility, interactions, xai, meta. Unknown group/key fields are retained by the JSON editor. Native per-key enablement uses Credentials; OpenAI-compatible group enablement writes the group list. |
| Client keys | `GET, PUT /config/access/api-keys`. Generate, copy, reveal, add, remove, and edit the list. |
| Models | `GET /routing/model-definitions/<channel>`; `GET, PUT /config/oauth/model-alias/<channel>` and `/config/oauth/excluded-models/<channel>`; `GET, PUT /config/oauth` in the full editor. |
| Payload rules | `GET, PUT /config/requests/payload` and `/config/requests/payload/<kind>`; kinds: default, default-raw, override, override-raw, filter. |
| Quotas | `GET /plugins`; `POST /requests/api-call` using `authIndex` and server-substituted `$TOKEN$`; `POST /plugins/<id>/quota`; `DELETE /plugins/<id>/quota?auth_index=...`; `POST /routing/cooldown/reset`. Built-in upstream adapters: Claude usage, Codex wham/usage, Kimi usage. Plugin normalized groups/buckets are supported. |
| Configuration | `GET, PUT /config`; `GET, PUT /config.yaml`; `GET, PUT /config/<section>` for routing, requests, observability, server, client, oauth, and plugins. JSON mode also exposes the entire persisted tree. |
| Logs | Cursor/limit `GET /observability/logs`; `DELETE /observability/logs`; `GET /observability/logs/errors`; `GET /observability/logs/errors/<name>` download; `GET /observability/logs/requests/<id>` text/download. |
| Plugins | `GET /plugins`; `GET /plugins/store`; `POST /plugins/store/<id>/install` with optional source query; `DELETE /plugins/<id>`; `GET, PUT /config/plugins` and `/config/plugins/configs/<id>`; `PUT /config/plugins/configs/<id>/enabled`. |
| System | `GET /server/latest-version`; server version from response headers; persisted routing/observability configuration and real credential counts. It does not invent Rust process uptime or memory statistics. |

## Verification

Tested against the latest Linux amd64 release **CLIProxyAPI v8.0.10**, published October 2, 2026. The backend was downloaded into `/tmp/cliproxy-backend` and started with a disposable local configuration, disabled native plugins, enabled file/request logs, and no real provider credentials.

- `npm run check`: **0 errors, 0 warnings**.
- `npm test`: **4 passing tests**, covering URL/path boundaries, structural comparisons, reversible diff reconstruction, telemetry boundaries/sanitization, and quota utilization direction.
- `node scripts/verify-backend.mjs`: **84 endpoint checks passed**, followed by successful restoration of original disposable settings. Includes mutation/readback, rejected writes, synthetic credential CRUD/status/fields/download/refresh failure, cooldown reset, Claude/Codex OAuth start/poll/error callback/cancel, invalid Vertex import, cursor logs, release/store reads, and upstream-call plumbing.
- `node scripts/browser-check.mjs`: **all 12 pages in both themes**, successful page read calls, no viewport overflow, no console errors, and no management key in browser storage. Also checks an actual payload add/remove, YAML diff, visual editor, pending OAuth/cancel, store, latest release, and 390px responsive views.
- The same browser check passed against the **production static build** served at both `/management.html` and `/nested/management.html`. All static assets, font, and favicon resolved. Production empty states were inspected with no fixture notices or fabricated metrics.
- An independent initial-load measurement at `/management.html` recorded **CLS 0**, with the JS, CSS, font, and favicon all returning HTTP 200. This is a bounded startup measurement, not a guarantee for every future data state.
- Inspected every page in both themes and expanded/editor/mobile states with the media viewer. Fixed OAuth card stretching, narrow account-table clipping, narrow navigation clipping, stale action toasts, and escaped plugin-description entities.
- Impeccable detector: **no findings**. Captures and API evidence are in `design/`.
- Production total JavaScript including inline theme initialization: **42,642 bytes gzip (41.64 KiB)**. One JS chunk. CSS is approximately **6.85 KB gzip**. `npm run build` enforces the 100,000-byte JS ceiling.

The integration scripts are deliberately for the disposable backend only. They use the public test management key `orb-dashboard-test-only`; never adapt them to a production server. The API verifier refuses mutations unless the auth directory is `/tmp/cliproxy-backend/auth`. The browser verifier expects empty provider credentials/payload rules and retry value 3. Its optional arguments are page URL, server base URL, and capture directory.

## Honest parity gaps and unverified paths

- Complex provider fields, mappings, plugin configuration, and nested settings use the generic JSON/visual editors rather than every specialized official form. New omitted settings can be added in JSON/YAML mode; visual mode edits persisted fields only.
- Built-in quota parsing covers Claude, Codex, and Kimi. Other providers need a plugin advertising normalized quota; there are no dedicated Gemini/Antigravity/Vertex/etc. adapters or credit-purchase controls.
- Plugin-defined extension menus are not dynamically rendered. Core discovery, trusted store install/update, deletion, enablement, configuration, and normalized quota actions are implemented.
- No historical telemetry database, persisted login, locale switcher, usage export/import, or separate full historical-usage page. The overview is session-live rather than a replacement for the official historical analytics tools.
- Real OAuth credential exchange, successful token refresh, real provider quotas, request-log retrieval for a successful provider request, and native plugin installation/execution could not be validated without real credentials or executing third-party native code. Expected invalid/missing-resource paths and upstream proxy plumbing were verified; fixture screenshots are not evidence of those live operations.
- Status means the management connection and reported credential health, not an unexposed Rust process-health endpoint. Latest-version check reports the API's release information, not a fabricated Rust release feed.

## Design and screenshots

Three Painter directions are retained in [`design/directions.png`](design/directions.png): Signal (ink/acid), Workshop (cream/vermilion), and Observatory (slate/ice). Observatory was selected for the strongest readable telemetry hierarchy and calm operator-facing density. `DIRECTION.md` records the choice; `DESIGN.md` documents the shipped tokens and responsive behavior.

All page/theme captures are in `design/screenshots/`. The best views are `overview-dark.png`, `quotas-light.png`, `config-diff-light.png`, and the honest `production-empty-dark.png`. Sample-data screenshots are explicitly labeled in the UI. Public Sans is bundled locally under its SIL Open Font License, included in `public/fonts/OFL.txt`.
