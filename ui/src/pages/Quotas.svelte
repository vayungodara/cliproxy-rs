<script lang="ts">
  import { store, every } from "../store.svelte";
  import { credState, credName, provider, label, ago, otherSignals } from "../core";
  import { check, checkAll, canCheck, hasSource, limits, quotaKey } from "../quota";
  import Load from "../Load.svelte";
  import Missing from "../Missing.svelte";
  import Meter from "../Meter.svelte";

  $effect(() => every(15_000, () => store.creds.load(true)));
  if (!store.plugins.data) store.plugins.load();
  let checking = $state(false);
  async function all() {
    checking = true;
    await checkAll(store.creds.data || []);
    checking = false;
  }
</script>

<div class="head">
  <h1>Quotas</h1>
  <button class="key" disabled={checking || !(store.creds.data || []).some(canCheck)} onclick={all}
    >{checking ? "Checking…" : "Check all"}</button
  >
</div>
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
        {@const q = limits(a)}
        {@const signals = otherSignals(a.quota, q && "windows" in q ? q.windows : [])}
        <li class="stack quota-item">
          <div class="item">
            <span class="lamp {s.lamp}"></span>
            <span class="name grow"><strong>{credName(a)}</strong><small>{label(provider(a))} · {s.label}</small></span>
            <button class="key small" disabled={!canCheck(a)} onclick={() => check(a)}>Check quota</button>
            <button
              class="key small quiet"
              disabled={store.busy || !a.auth_index || !a.cooldowns?.length || !store.can("POST", "/routing/cooldown/reset")}
              onclick={() => store.call("POST", "/routing/cooldown/reset", { auth_index: a.auth_index }, "Cooldown cleared.")}
              >Reset cooldown</button
            >
          </div>
          {#if q && "windows" in q}
            {#each q.windows as w}<Meter {w} />{:else}<p class="legend">The provider answered without usage windows.</p>{/each}
            <!-- Passive windows share the signals' "observed" line; only a live check was "checked". -->
            <span class="legend"
              >{!q.at ? "" : store.quota[quotaKey(a)] ? `Checked ${ago(q.at)}` : signals.length ? "" : `Observed ${ago(q.at)}`}</span
            >
          {:else if q}<p class="note error"><span class="lamp bad"></span>{q.error}</p>{/if}
          {#if signals.length}<div class="chips">
              {#each signals as [k, v]}<span class="chip">{k}: {v}</span>{/each}
              <span class="legend">observed {ago(a.quota.observed_at)}</span>
            </div>{/if}
          {#if !q && !signals.length && !hasSource(a)}<p class="legend">
              No quota source for {label(provider(a))}. A plugin can add one.
            </p>{/if}
        </li>
      {:else}
        <li class="state"><p>No credentials. Quotas appear once accounts are connected.</p></li>
      {/each}
    </ul>
  {/snippet}
</Load>
