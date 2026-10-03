# Security policy

## Reporting a vulnerability

Please report security problems privately through GitHub's [private vulnerability reporting](https://github.com/vayungodara/cliproxy-rs/security/advisories/new), not in a public issue. Include the version (`cliproxy --version`), what an attacker can do, and the steps to reproduce. Leave real keys, tokens and account files out of the report.

You should get an answer within a week. Fixes go into a new release, and the advisory is published once users can update.

## Supported versions

Only the latest release gets security fixes.

## Scope

In scope: the cliproxy-rs server, its Management API and the built-in dashboard, for example a way around client-key or management-key checks, a credential or key leaking into logs or responses, or a request that reaches a provider with the wrong account.

Out of scope: a proxy deliberately exposed to the internet without `access.api-keys`, behaviour of the upstream providers, and problems in CLIProxyAPI itself (report those to [router-for-me/CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI)). [Running it safely](docs/GETTING-STARTED.md#running-it-safely) lists the settings a safe deployment needs.
