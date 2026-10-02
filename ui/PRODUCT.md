# cliproxy-rs dashboard

The management dashboard embedded in cliproxy-rs, a Rust rewrite of CLIProxyAPI. It serves at `/management.html` from the server binary and talks only to the v8 Management API. It is the most visible part of the launch, so it has to look authored and still be quick to use.

## Who and where

The operator is one developer or a small team. The proxy runs on a laptop, a home server or a small VPS, and coding agents send it requests all day. The dashboard sits in a browser tab next to a terminal and an editor, often late at night. On a phone it is used to check status or flip one switch.

They come with these questions, in this order: is traffic flowing, which credentials are ready, cooling down, disabled or failing, how do I connect another account, how do I change a setting without breaking anything, and what do the logs say.

## Contract

- The data contract is Go CLIProxyAPI's v8 Management API at commit 6fecc6e (v8.0.10). The UI is built to Go's request and response shapes.
- The server may be the Rust binary, which does not implement every route yet. A route it lacks answers 501, and a method it lacks answers 405. The UI probes routes with `OPTIONS` (no side effects) and shows unsupported actions disabled, with one line naming them. Reads that fail this way show a "not available on this server" state naming the route.

## Non-negotiables

- Honest data. Every read shows one of loading, empty, error, not available, or data. Nothing is invented. Telemetry comes from server-side `recent_requests` buckets; the destructive usage queue is opt-in and stays in the tab.
- The management key lives in memory only. It is never written to browser storage. Reloading signs out.
- Accessibility: a complete keyboard path, visible focus, WCAG AA contrast in both themes, status never shown by colour alone, reduced motion respected.
- A small, fast bundle. JavaScript and CSS may not grow past the sizes of the dashboard this one replaced: 42,642 B and 6,853 B gzip. `npm run build` fails if they do. No runtime dependencies beyond Svelte, and no chart library.

## Scope

Overview, credentials and connecting accounts (OAuth, device codes, callback paste, Vertex import), provider API keys, client keys, models (aliases, exclusions, catalog), payload rules, quotas, configuration (JSON and YAML with a reviewed diff and a stale-write check), logs, usage, plugins and system.

Out of scope: accounts or billing, a historical telemetry database, invented Rust-only endpoints, and process metrics the API does not expose.
