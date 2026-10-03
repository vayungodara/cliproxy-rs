<script lang="ts">
  import { store, Res } from "../store.svelte";
  import { api } from "../api";
  import { readPath, label, type Data } from "../core";
  import Load from "../Load.svelte";

  const channels = ["claude", "codex", "antigravity", "kimi", "xai", "meta", "vertex", "aistudio"];
  let channel = $state("claude");
  let alias = $state({ name: "", alias: "", fork: false }),
    pattern = $state("");
  const aliases = $derived(readPath(store.config.data || {}, `oauth/model-alias/${channel}`, []) as Data[]);
  const excluded = $derived(readPath(store.config.data || {}, `oauth/excluded-models/${channel}`, []) as string[]);
  let catalog = $state.raw<Res<Data[]>>();
  $effect(() => {
    const c = channel;
    const r = new Res<Data[]>(async () => (await api(`/routing/model-definitions/${encodeURIComponent(c)}`)).models || []);
    catalog = r;
    r.load();
  });
  const k = (n: number) => (n >= 1000 ? `${Math.round(n / 1000)}k` : String(n || "—"));
  function addAlias(e: SubmitEvent) {
    e.preventDefault();
    const next = { name: alias.name.trim(), alias: alias.alias.trim(), ...(alias.fork ? { fork: true } : {}) };
    if (aliases.some((a) => a.alias === next.alias)) return store.notify("That alias already exists.", true);
    store
      .act(() => store.replace(`oauth/model-alias/${channel}`, aliases, [...aliases, next]), "Alias added.")
      .then((ok) => ok && (alias = { name: "", alias: "", fork: false }));
  }
  function exclude(e: SubmitEvent) {
    e.preventDefault();
    store
      .act(() => store.replace(`oauth/excluded-models/${channel}`, excluded, [...new Set([...excluded, pattern.trim()])]), "Model excluded.")
      .then((ok) => ok && (pattern = ""));
  }
</script>

<div class="head"><h1>Models</h1></div>
<div class="seg" role="group" aria-label="Channel">
  {#each channels as c}<button aria-pressed={channel === c} onclick={() => (channel = c)}>{label(c)}</button>{/each}
</div>

<Load res={store.config} what="Model settings">
  {#snippet children()}
    <section class="section">
      <div class="section-head"><h2>Aliases</h2><span class="legend">Clients ask for the alias; the upstream receives the model.</span></div>
      {#if aliases.length}
        <ul class="list">
          {#each aliases as a, i}
            <li class="item">
              <code>{a.name}</code><svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-chevron" /></svg><code class="grow">{a.alias}</code>
              {#if a.fork}<span class="legend">keeps original</span>{/if}
              <button
                class="key small quiet danger"
                disabled={store.busy}
                onclick={() => store.act(() => store.replace(`oauth/model-alias/${channel}`, aliases, aliases.filter((_, n) => n !== i)), "Alias removed.")}
                >Remove</button
              >
            </li>
          {/each}
        </ul>
      {:else}<p class="muted">No aliases. Clients use upstream model names.</p>{/if}
      <form class="form" onsubmit={addAlias}>
        <label class="field">Upstream model<input list="defs" required bind:value={alias.name} /></label>
        <label class="field">Alias<input required bind:value={alias.alias} /></label>
        <label class="check"><input type="checkbox" checked={alias.fork} onchange={(e) => (alias.fork = e.currentTarget.checked)} />Keep original</label>
        <button class="key" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add alias</button>
      </form>
    </section>

    <section class="section">
      <div class="section-head"><h2>Excluded</h2><span class="legend">Hidden from listings and refused. Wildcards allowed.</span></div>
      <div class="chips">
        {#each excluded as m, i}<span class="chip"
            >{m}<button
              aria-label={`Allow ${m}`}
              disabled={store.busy}
              onclick={() => store.act(() => store.replace(`oauth/excluded-models/${channel}`, excluded, excluded.filter((_, n) => n !== i)))}
              ><svg class="i" width="12" height="12" aria-hidden="true"><use href="#i-close" /></svg></button
            ></span
          >{:else}<span class="muted">Every model is available.</span>{/each}
      </div>
      <form class="form" onsubmit={exclude}>
        <label class="field">Model or pattern<input required bind:value={pattern} placeholder="*-preview" /></label>
        <button class="key" disabled={store.busy}>Exclude</button>
      </form>
    </section>
  {/snippet}
</Load>

<section class="section">
  <div class="section-head"><h2>Catalog</h2>{#if catalog?.data}<span class="legend">{catalog.data.length} models</span>{/if}</div>
  {#if catalog}<Load res={catalog} what="The model catalog">
    {#snippet children(list)}
      {#if list.length}
        <ul class="list">
          <li class="item catalog legend" aria-hidden="true"><span>Model</span><span>Name</span><span class="count">Context</span><span class="count">Output</span></li>
          {#each list as m (m.id)}
            <li class="item catalog">
              <code class="ellipsis">{m.id}</code><span class="ellipsis">{m.display_name || ""}</span>
              <span class="num count">{k(m.context_length)}</span><span class="num count">{k(m.max_completion_tokens)}</span>
            </li>
          {/each}
        </ul>
        <datalist id="defs">{#each list as m}<option value={m.id}></option>{/each}</datalist>
      {:else}<p class="muted">No static definitions for {label(channel)}.</p>{/if}
    {/snippet}
  </Load>{/if}
</section>
