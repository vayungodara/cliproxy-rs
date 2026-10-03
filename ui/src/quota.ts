// Live quota checks, shared by Overview and Quotas. A check asks the provider through the
// server (/requests/api-call with the credential's own token, or a quota plugin), so it
// only ever runs when the user presses a button.
import { api } from "./api";
import { store } from "./store.svelte";
import { provider, quotaWindows, signalWindows, type Data, type Window } from "./core";

// Usage endpoints the server calls with the credential's own token ($TOKEN$ is substituted server-side).
const usageURL: Record<string, string> = {
  claude: "https://api.anthropic.com/api/oauth/usage",
  codex: "https://chatgpt.com/backend-api/wham/usage",
  kimi: "https://api.kimi.com/coding/v1/usages",
  "kimi-ai": "https://api.kimi.ai/coding/v1/usages",
};
const plugin = (p: string) =>
  (store.plugins.data || []).find((x) => x.supports_quota && (x.quota_provider || x.id) === p);
export const quotaKey = (a: Data) => a.auth_index || a.name;
/** The provider has a usage endpoint or quota plugin this server can call. */
export const hasSource = (a: Data) =>
  !!plugin(provider(a)) || (!!usageURL[provider(a)] && store.can("POST", "/requests/api-call"));
export const canCheck = (a: Data) => !a.disabled && hasSource(a);

/** What is known about one credential's limits: a live check in this tab, else passive signals. */
export function limits(a: Data): { windows: Window[]; at: number } | { error: string } | null {
  const live = store.quota[quotaKey(a)];
  if (live) return live;
  const windows = signalWindows(provider(a), a.quota);
  return windows.length ? { windows, at: Date.parse(a.quota?.observed_at) || 0 } : null;
}

export async function check(a: Data) {
  const p = provider(a);
  try {
    let raw: Data;
    const pl = plugin(p);
    if (pl) raw = await api(`/plugins/${encodeURIComponent(pl.id)}/quota`, "POST", { name: a.name, auth_index: a.auth_index });
    else {
      const header: Data = { Authorization: "Bearer $TOKEN$", "Content-Type": "application/json" };
      if (p === "claude") Object.assign(header, { "anthropic-beta": "oauth-2025-04-20", "User-Agent": "claude-cli/2.1.280 (external, cli)" });
      if (p === "codex") {
        header["User-Agent"] = "codex-tui/0.149.1";
        const account = a.account_id || a.id_token?.chatgpt_account_id;
        if (account) header["Chatgpt-Account-Id"] = account;
      }
      const r = await api("/requests/api-call", "POST", { authIndex: a.auth_index, method: "GET", url: usageURL[p], header });
      if (r.status_code < 200 || r.status_code >= 300)
        throw new Error(
          r.status_code === 401 || r.status_code === 403
            ? `The provider rejected the account's token (HTTP ${r.status_code}). Refresh the token, or connect the account again.`
            : `The provider answered HTTP ${r.status_code}. Try again later.`,
        );
      raw = typeof r.body === "string" ? JSON.parse(r.body) : r.body;
    }
    store.quota[quotaKey(a)] = { at: Date.now(), windows: quotaWindows(p, raw) };
  } catch (e) {
    store.quota[quotaKey(a)] = { error: e instanceof Error ? e.message : String(e) };
  }
}

/** One check per supported credential, one after another so the provider is not hammered. */
export async function checkAll(list: Data[]) {
  for (const a of list) if (canCheck(a)) await check(a);
}
