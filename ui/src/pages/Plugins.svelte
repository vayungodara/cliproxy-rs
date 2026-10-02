<script lang="ts">
  import { store, Res } from "../store.svelte";
  import { api, configValue } from "../api";
  import { readPath, fieldPath, type Data } from "../core";
  import Editor from "../Editor.svelte";
  import Load from "../Load.svelte";

  store.plugins.load();
  const market = new Res<Data>(() => api("/plugins/store"));
  let edit = $state<{ path: string; value: unknown; title: string } | null>(null);
  const on = $derived(readPath(store.config.data || {}, "plugins/enabled", false) === true);
  // Plugin descriptions come from remote stores: decode entities as text, never insert HTML.
  const text = (v: unknown) => new DOMParser().parseFromString(String(v || ""), "text/html").body.textContent || "";
  const after = async () => {
    await store.plugins.load(true);
    await store.config.load(true);
  };
  function configure(id: string) {
    store.act(async () => {
      const path = fieldPath(`plugins/configs/${id}`);
      edit = { path, value: await configValue(path, {}), title: id };
    });
  }
  function install(p: Data) {
    if (!confirm(`Install ${text(p.name || p.id)}? Plugins are native code that runs inside the server with its full access. Install only from publishers you trust.`)) return;
    store.act(async () => {
      const r = await api(`/plugins/store/${encodeURIComponent(p.id)}/install${p.source_id ? `?source=${encodeURIComponent(p.source_id)}` : ""}`, "POST", {});
      await after();
      await market.load(true);
      store.notify(r.restart_required ? "Installed. Restart the server to load it." : "Installed.");
    });
  }
</script>

<div class="head">
  <h1>Plugins{#if store.plugins.data?.length}<span>{store.plugins.data.length}</span>{/if}</h1>
  {#if !edit}<button
      class="key"
      disabled={store.busy}
      onclick={() => store.call("PUT", fieldPath("plugins/enabled"), !on, on ? "Plugins off." : "Plugins on. A restart may be needed.", "", after)}
      ><span class="lamp {on ? 'ok' : 'off'}"></span>{on ? "Plugins on" : "Plugins off"}</button
    ><button class="key" disabled={market.loading || !store.can("GET", "/plugins") || !store.can("GET", "/plugins/store")} onclick={() => market.load()}>Browse store</button>{/if}
</div>

{#if edit}
  <Editor {...edit} onclose={() => (edit = null)} />
{:else}
  <Load res={store.plugins} what="Plugins">
    {#snippet children(list)}
      {#if list.length}
        <ul class="list">
          {#each list as p (p.id)}
            <li class="item">
              <span class="lamp {p.registered && p.effective_enabled ? 'ok' : p.enabled ? 'warn' : 'off'}"></span>
              <span class="name grow"
                ><strong>{p.metadata?.name || p.id}</strong><small
                  >{[p.metadata?.version, p.supports_oauth && "sign-in", p.supports_quota && "quota", !p.registered && "not loaded, restart needed"].filter(Boolean).join(" · ")}</small
                ></span
              >
              <button
                class="key small"
                disabled={store.busy}
                onclick={() => store.call("PUT", fieldPath(`plugins/configs/${p.id}/enabled`), !p.enabled, "Saved. A restart may be needed.", "", after)}
                >{p.enabled ? "Disable" : "Enable"}</button
              >
              <button class="key small" disabled={store.busy} onclick={() => configure(p.id)}>Configure</button>
              <button
                class="key small quiet danger"
                disabled={store.busy}
                onclick={() => store.call("DELETE", `/plugins/${encodeURIComponent(p.id)}`, undefined, "Plugin deleted.", `Delete plugin ${p.id}, its binary and its settings?`, after)}
                >Delete</button
              >
            </li>
          {/each}
        </ul>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp off"></span>No plugins installed</div>
          <p>Plugins add providers, sign-in flows and quota sources.</p>
        </div>
      {/if}
    {/snippet}
  </Load>
  {#if market.data || market.error || market.loading}
    <section class="section">
      <h2>Store</h2>
      <Load res={market} what="The plugin store">
        {#snippet children(m)}
          {#each m.source_errors || [] as e}<p class="note error"><span class="lamp bad"></span>{e.source_name}: {e.message}</p>{/each}
          <ul class="list">
            {#each m.plugins || [] as p (`${p.source_id}/${p.id}`)}
              <li class="item">
                <span class="name grow"><strong>{text(p.name || p.id)} <span class="legend">{p.version}</span></strong><small>{text(p.description)}</small></span>
                <button class="key small" disabled={store.busy} onclick={() => install(p)}
                  >{p.installed ? (p.update_available ? "Update" : "Reinstall") : "Install"}</button
                >
              </li>
            {:else}<li class="state"><p>The store lists no plugins.</p></li>{/each}
          </ul>
        {/snippet}
      </Load>
    </section>
  {/if}
{/if}
