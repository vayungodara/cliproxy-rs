# cliproxy-rs dashboard

An embedded, static management dashboard for a Rust server implementing CLIProxyAPI's v8 Management API. Operators manage AI provider credentials, routing, client access, configuration, quotas, logs, and plugins. The UI itself is a launch feature: fast, distinctive, and useful.

Use Svelte 5, Vite, TypeScript, native browser controls, and plain CSS. No component library. JavaScript must remain below 100 KB gzip. Build assets are relative and navigation uses hashes. Both light and dark themes serve daytime and late-night operations.

The API is authoritative. Never fabricate production telemetry or quota. Local fixtures require both a Vite development build and an explicit flag. The management key stays in memory. Config edits preserve unknown fields, preview changes, and refuse stale writes. Plugin install and destructive operations require confirmation.

Assumption from the explicit brief: the primary operator is a technical individual or small team, not a multi-tenant enterprise administrator. No accounts, billing, or invented Rust-specific server endpoints.
