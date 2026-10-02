<script lang="ts">
  import { store, every } from "../store.svelte";
  import { api } from "../api";
  import { credState, credName, provider, label, quotaWindows, ago, span, type Data, type Window } from "../core";
  import Load from "../Load.svelte";
  import Missing from "../Missing.svelte";

  // Usage endpoints the server calls with the credential's own token ($TOKEN$ is substituted server-side).
  const usageURL: Record<string, string> = {
    claude: "https://api.anthropic.com/api/oauth/usage",
    codex: "https://chatgpt.com/backend-api/wham/usage",
    kimi: "https://api.kimi.com/coding/v1/usages",
    "kimi-ai": "https://api.kimi.ai/coding/v1/usages",
  };
  $effect(() => every(15_000, () => store.creds.load(true)));
  if (!store.plugins.data) store.plugins.load();
  let live = $state<Record<string, { at: number; windows: Window[] } | { error: string }>>({});
  const plugin = (p: string) => (store.plugins.data || []).find((x) => x.supports_quota && (x.quota_provider || x.id) === p);
  const id = (a: Data) => a.auth_index || a.name;

  async function check(a: Data) {
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
        if (r.status_code < 200 || r.status_code >= 300) throw new Error(`The provider answered HTTP ${r.status_code}. Try refreshing the token.`);
        raw = typeof r.body === "string" ? JSON.parse(r.body) : r.body;
      }
      live[id(a)] = { at: Date.now(), windows: quotaWindows(p, raw) };
    } catch (e) {
      live[id(a)] = { error: e instanceof Error ? e.message : String(e) };
    }
  }
  const supported = (a: Data) => !!plugin(provider(a)) || (!!usageURL[provider(a)] && store.can("POST", "/requests/api-call"));
</script>

{#snippet meter(w: Window)}
  <div class="quota">
    <span class="legend grow">{w.label}</span>
    <span role="meter" aria-valuenow={w.used} aria-valuemin={0} aria-valuemax={100} aria-label={`${w.label} used`}>
      <svg class="grille" width="216" height="7" aria-hidden="true"
        >{#each { length: 20 } as _, i}<circle cx={i * 11 + 3.5} cy="3.5" r="3.5" class={i < Math.round(w.used / 5) ? (w.used >= 90 ? "f" : "l4") : ""} />{/each}</svg
      >
    </span>
    <span class="num count">{w.used}%</span>
    <span class="legend reset">{w.reset ? `resets in ${span(Date.parse(w.reset) - Date.now())}` : ""}</span>
  </div>
{/snippet}

<div class="head"><h1>Quotas</h1></div>
<Missing actions={[["POST", "/requests/api-call", "live quota checks"], ["POST", "/routing/cooldown/reset", "cooldown reset"]]} />
<p class="muted">
  Signals come from recent provider responses. Checking asks the provider with the credential’s token.
  Resetting a cooldown clears local routing state only.
</p>

<Load res={store.creds} what="Credentials">
  {#snippet children(list)}
    <ul class="list">
      {#each list as a (`${a.name}\u0000${a.auth_index}`)}
        {@const s = credState(a)}
        {@const q = live[id(a)]}
        {@const signals = Object.entries(a.quota?.signals || {})}
        <li class="stack quota-item">
          <div class="item">
            <span class="lamp {s.lamp}"></span>
            <span class="name grow"><strong>{credName(a)}</strong><small>{label(provider(a))} · {s.label}</small></span>
            <button class="key small" disabled={!supported(a) || a.disabled} onclick={() => check(a)}>Check quota</button>
            <button
              class="key small quiet"
              disabled={store.busy || !a.auth_index || !a.cooldowns?.length || !store.can("POST", "/routing/cooldown/reset")}
              onclick={() => store.call("POST", "/routing/cooldown/reset", { auth_index: a.auth_index }, "Cooldown cleared.")}
              >Reset cooldown</button
            >
          </div>
          {#if q && "windows" in q}
            {#each q.windows as w}{@render meter(w)}{:else}<p class="legend">The provider answered without usage windows.</p>{/each}
            <span class="legend">Checked {ago(q.at)}</span>
          {:else if q}<p class="note error"><span class="lamp bad"></span>{q.error}</p>{/if}
          {#if signals.length}<div class="chips">
              {#each signals as [k, v]}<span class="chip">{k}: {v}</span>{/each}
              <span class="legend">observed {ago(a.quota.observed_at)}</span>
            </div>{/if}
          {#if !q && !signals.length && !supported(a)}<p class="legend">
              No quota source for {label(provider(a))}. A plugin can add one.
            </p>{/if}
        </li>
      {:else}
        <li class="state"><p>No credentials. Quotas appear once accounts are connected.</p></li>
      {/each}
    </ul>
  {/snippet}
</Load>
