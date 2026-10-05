# Several accounts

This guide covers routing across your own accounts and API keys, for use on your own machines. Do not pool other people's subscriptions. Read [Accounts and provider terms](../README.md#accounts-and-provider-terms). It assumes the proxy is already running ([GETTING-STARTED.md](GETTING-STARTED.md)).

## Add the accounts

Sign in to each account once, from the dashboard (Connect account) or the command line (`--claude-login`, `--codex-login`). Every sign-in becomes one file in `auth-dir`, and every file is one account in the rotation.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/credentials-dark.png">
  <img src="img/credentials-light.png" alt="The Credentials page: three Claude and two Codex accounts with their state and recent requests" width="880">
</picture>

The provider's sign-in page uses whatever account your browser is logged into. To add a second Claude account, log out of claude.ai first, or open the sign-in link in a private window or another browser profile; `--no-browser` prints the link instead of opening it. The dashboard's Credentials page lists every account with its state, and lets you disable one without deleting it.

## Routing strategies

`routing.strategy` decides which ready account serves the next request.

| Strategy | What it does | Use it when |
|---|---|---|
| `round-robin` (default) | Takes the accounts in turn, one request each. | The accounts are alike and you want the load even. |
| `fill-first` | Uses one account until it cools down or reaches a limit, then the next. | You prefer one account and keep another as a fallback. |
| `weighted-round-robin` | Takes the accounts in turn in proportion to each account's `weight`. An account with weight 0 is left out. | The plans differ in size. |
| `soonest-reset` (experimental, a cliproxy-rs addition) | Selects the account whose weekly window resets soonest, until it cools down or reaches a limit. An account without a known reset gets one request to learn it. | You want selection ordered by reset time. CLIProxyAPI reads this value as `round-robin`. |

`soonest-reset` is experimental and only runs if you choose it. Please report anything that looks wrong.

`soonest-reset` ranks accounts by the usage headers of their last responses. An account whose one-hour rest after a stated reset has ended (see [Cooldowns and limits](#cooldowns-and-limits)) still reports its window as used up, so it sorts last and is checked only when the other accounts are used up or cooling.

You can change the strategy on the dashboard's Configuration page, or in `config.yaml`.

Each account can also have a `priority` (default 0). The proxy only uses the accounts with the highest priority among those that are ready, and falls back to lower ones when all of those are cooling down or disabled. Set Priority and Weight per account on the dashboard's Credentials page, or as top-level `"priority"` and `"weight"` fields in the account's file.

## Session affinity

The installer enables affinity for new configs. It leaves an existing config unchanged. To enable it by hand:

```yaml
routing:
  session-affinity: true
```

With affinity on, a conversation stays on the account that served its first request. Providers keep a prompt cache per account, so the next turn of a conversation on the same account reads the long shared beginning from cache: it is cheaper against your limits and the reply starts sooner. Moving a conversation to another account throws that cache away. The proxy recognises a conversation from the session headers that Claude Code, Codex, OpenCode and similar tools send. When a request has none, the proxy matches its messages against the conversations it has seen for the same client key and model: a conversation that grows keeps its account, a branch of an earlier conversation stays on that conversation's account, and a conversation whose early history was summarised (compacted) is recognised from the turns it kept. A request that matches no earlier conversation starts a new one, which its next turns are matched against. Message matching needs a client key (`access.api-keys`) and at least one message that is not a system message; a request without either is identified by its instructions and first user message instead, and when it has no user message, by a hash of its first messages.

When `session-affinity` is omitted, both cliproxy-rs and CLIProxyAPI default to off. Affinity works with the routing strategy: a bound conversation keeps its account; new conversations follow the strategy. When the bound account cools down or is disabled, the conversation moves to another account. `session-affinity-ttl` (default `"1h"`) controls how long an idle conversation keeps its account. `session-affinity-subagents` (default `true`) binds child sessions to their parent's account too.

## Cooldowns and limits

When a provider answers 429 (too many requests, or a used-up limit), the proxy rests that account for that model and sends the request to the next ready account:

- If the provider says when to try again (a `Retry-After` header or a stated reset), the account rests that long, at least 10 seconds, but at most one hour at first ([`max-trusted-cooldown`](CONFIGURATION.md#routing)). Then the next request checks the account; while that check is out, other requests go elsewhere. If it is still limited, the next rest doubles, never past the stated reset. The dashboard shows such an account as "Limited until" the stated reset, with the time of the next check.
- Without that, the rest starts at one second and doubles on each further 429, up to 30 minutes.
- An error that says the whole account is out of quota rests the account for every model.
- A rejected token (401 or 403) rests the account for 30 minutes, and the dashboard marks it "Sign in again" when the sign-in has expired or was revoked.
- A successful request on the account clears its cooldown for that model.

`routing.retry.request-retry` (default 0) adds rounds over the accounts when every attempt in a round failed, and `routing.retry.max-retry-interval` (seconds, default 0) lets the proxy wait for an account to come out of cooldown instead of failing at once. The Reset cooldown button on the Credentials and Quotas pages clears the proxy's own record only; it does not give back any provider limit.

The optional [fail over only on 429 preset](CONFIGURATION.md#optional-preset-fail-over-only-on-429) keeps 429 failover for OAuth accounts. It stops ordinary failover after a failed attempt for the listed non-429 statuses. Credential-preparation failures and an enabled `requests.streaming.bootstrap-retries` can still cause another attempt.

## Read the quota view

The Quotas page asks each account's provider for its usage when you press Check quota (or Check limits on the Overview). For a Claude account it shows the rows Claude Code's own `/usage` shows:

- Current session: the 5-hour window.
- Current week (all models): the weekly limit.
- Current week (Fable only), (Opus only) and so on: a separate weekly cap for one model, when the plan has one.
- Extra usage: paid usage beyond the plan, in dollars, when it is turned on.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="img/quotas-dark.png">
  <img src="img/quotas-light.png" alt="The Quotas page: Claude session and weekly limits including a Fable-only weekly cap, and Codex 5-hour and weekly limits" width="880">
</picture>

Each row shows how much is used and when it resets. Codex accounts show their 5-hour and weekly windows. Between checks, the proxy also reads the limit headers that Codex sends with every reply, so the Overview can show Codex limits without a check.

## Per-account proxies and request shaping

These settings go in the account's file in `auth-dir` (the server reloads it when the file changes):

```json
{
  "proxy_url": "socks5://user:pass@proxy.example.com:1080"
}
```

`proxy_url` sends that account's requests through its own proxy (`http://`, `https://`, `socks5://` or `socks5h://`); `"direct"` skips every proxy. Without it, the account uses `requests.proxy-url` from `config.yaml`. When neither is set, most providers follow the `HTTPS_PROXY` environment variables, while Claude accounts connect directly. For an API key in `config.yaml`, the same setting is `proxy-url` on the key's entry.

Claude request shaping follows CLIProxyAPI's rules. Its default `auto` mode can rewrite system prompts for non-native clients. To turn it off, set `"cloak_mode": "never"` in the account's file, or `oauth.providers.claude.disable-claude-cloak-mode: true` in `config.yaml`. The rewrite does not make using a Claude subscription in another tool permitted; use an API key or a supported cloud provider there.

## Codex over WebSocket

Codex accounts can talk to OpenAI over a WebSocket, the transport the Codex CLI itself uses, instead of one HTTPS request per turn. Turn it on per account with a top-level field in the account's file:

```json
{
  "websockets": true
}
```

For a Codex API key, set `websockets: true` on the key's entry under `api-keys.codex`.

## Reach the proxy from other machines

The simplest safe way is [Tailscale](https://tailscale.com/), which puts your machines on a private network:

1. Install Tailscale on the machine running the proxy and on the machines that use it.
2. In `config.yaml`, set `server.host` to the proxy machine's Tailscale address (the `100.x.y.z` address from `tailscale ip -4`), so only your tailnet can connect, and restart. Keep `access.api-keys` set.
3. On the other machines, use `http://<machine-name>:8317` (or the `100.x.y.z` address) as the base URL.

If you use `tailscale serve` to add HTTPS, it forwards from `127.0.0.1`, so also set `server.trusted-proxies: [127.0.0.1, "::1"]`. To open the dashboard from another machine, set `management.allow-remote: true`. Never expose the proxy to the internet, with Tailscale Funnel or any other tunnel, unless `access.api-keys` is set: without client keys anyone who finds the address can use your accounts.

## A worked example: three Claude and two Codex accounts

This example is for one person using their own accounts on their own machines. The Claude Max account takes requests first. Two Claude Pro accounts take over while it cools down. Conversations stay on one account while it is ready, and both Codex accounts use WebSocket. Use Claude subscriptions only in native Anthropic applications, as described in [Accounts and provider terms](../README.md#accounts-and-provider-terms).

`config.yaml`:

```yaml
config-version: 8
server:
  host: "127.0.0.1"
  port: 8317
access:
  api-keys:
    - "your-client-key"
management:
  secret-key: "your-management-key"
oauth:
  auth-dir: "~/.cliproxy-rs/auth"
routing:
  strategy: fill-first
  session-affinity: true
  session-affinity-ttl: "1h"
  retry:
    request-retry: 1
    max-retry-interval: 30
```

Sign in five times (three Claude accounts, two Codex accounts), then set these fields on the Credentials page, or add them to the files in `~/.cliproxy-rs/auth`:

| Account file | Fields | Why |
|---|---|---|
| Claude Max | `"priority": 10` | Used first. |
| Claude Pro, first | `"priority": 0` | Used when the Max account is cooling down. |
| Claude Pro, second | `"priority": 0` | Used when the Max account is cooling down. |
| Codex, first | `"websockets": true` | WebSocket transport. |
| Codex, second | `"websockets": true` | WebSocket transport. |

With `fill-first`, each provider's top-priority account serves requests until it cools down, then the next ready account takes over. A bound conversation stays on its account while that account is ready. Swap `fill-first` for `soonest-reset` to order accounts by reset time, or for `weighted-round-robin` with `weight` fields to split requests by plan size.
