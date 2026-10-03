<script lang="ts">
  import { store, Res, every } from "../store.svelte";
  import { api, missing, text } from "../api";
  import { readPath, label, mask, sum, type Data, type Buckets } from "../core";
  import Grille from "../Grille.svelte";
  import Load from "../Load.svelte";

  const families = ["claude", "codex", "gemini", "vertex", "openai-compatibility", "interactions", "xai", "meta"];
  let family = $state("claude"),
    reveal = $state(false);
  let form = $state({ name: "", url: "", key: "" });
  const usage = new Res<Data>(() => api("/observability/usage/api-keys"));
  usage.load();
  $effect(() => every(15_000, () => usage.load(true)));
  const groups = $derived(readPath(store.config.data || {}, `api-keys/${family}`, []) as Data[]);
  const compat = $derived(family === "openai-compatibility");
  /** Usage is keyed by provider (or compat group name) and "base-url|api-key". */
  function traffic(group: Data, k: Data): Buckets | null {
    const byKey = usage.data?.[compat ? String(group.name || "").toLowerCase() : family];
    const hit = byKey?.[`${group["base-url"] || ""}|${k["api-key"]}`];
    if (!hit) return null;
    const rows: Data[] = hit.recent_requests || [];
    return { total: rows.map((b) => (b.success || 0) + (b.failed || 0)), failed: rows.map((b) => b.failed || 0) };
  }
  const max = $derived(
    Math.max(1, ...groups.flatMap((g) => (g.keys || []).flatMap((k: Data) => traffic(g, k)?.total || []))),
  );
  const write = (next: Data[], done: string) => store.act(() => store.replace(`api-keys/${family}`, groups, next), done);
  function add(e: SubmitEvent) {
    e.preventDefault();
    const group: Data = { name: form.name.trim(), keys: [{ "api-key": form.key.trim() }] };
    if (form.url.trim()) group["base-url"] = form.url.trim();
    write([...groups, group], "Group added.").then((ok) => ok && (form = { name: "", url: "", key: "" }));
  }
</script>

<div class="head">
  <h1>Provider keys</h1>
  {#if groups.length}<button class="key quiet" aria-pressed={reveal} onclick={() => (reveal = !reveal)}
      >{reveal ? "Hide keys" : "Show keys"}</button
    >{/if}
  <a class="key" href="#config"><svg class="i" aria-hidden="true"><use href="#i-edit" /></svg>Edit in Configuration</a>
</div>

<div class="seg" role="group" aria-label="Provider">
  {#each families as f}<button aria-pressed={family === f} onclick={() => (family = f)}
      >{label(f)}<b>{readPath(store.config.data || {}, `api-keys/${f}`, []).length || ""}</b></button
    >{/each}
</div>

  <Load res={store.config} what="Providers">
    {#snippet children()}
      {#if groups.length}
        <ul class="list groups">
          {#each groups as g, i (i)}
            <li class="stack">
              <div class="section-head">
                <h2>{g.name || `Group ${i + 1}`}</h2>
                {#if g.disabled}<span class="row legend"><span class="lamp off"></span>Disabled</span>{/if}
                {#if compat}<button class="key small" disabled={store.busy} onclick={() => write(groups.map((x, n) => (n === i ? { ...x, disabled: !x.disabled } : x)), g.disabled ? "Group enabled." : "Group disabled.")}
                    >{g.disabled ? "Enable" : "Disable"}</button
                  >{/if}
                <button
                  class="key small quiet danger"
                  disabled={store.busy}
                  onclick={() => confirm(`Delete group ${g.name || i + 1} and its keys?`) && write(groups.filter((_, n) => n !== i), "Group deleted.")}
                  >Delete</button
                >
              </div>
              <p class="legend mono">{g["base-url"] || "Provider default endpoint"}</p>
              {#each g.keys || [] as k}
                {@const t = traffic(g, k)}
                <div class="item key-row">
                  <code class="grow ellipsis">{reveal ? k["api-key"] : mask(String(k["api-key"] || ""))}</code>
                  <span class="legend">weight {k.weight ?? 1}</span>
                  {#if t}<Grille data={t} {max} /><span class="num count">{sum(t.total).toLocaleString()}</span>{/if}
                </div>
              {/each}
              {#if g.models?.length}<div class="chips">
                  {#each g.models as m}<span class="chip">{m.name}{m.alias && m.alias !== m.name ? ` → ${m.alias}` : ""}</span>{/each}
                </div>{/if}
            </li>
          {/each}
        </ul>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp off"></span>No {label(family)} API keys</div>
          <p>
            API keys from the provider’s developer platform, billed per request. A group is one endpoint with one or more keys,
            used in turn by weight.
          </p>
        </div>
      {/if}
      {#if usage.error}<p class="note"><span class="lamp {missing(usage.error) ? 'off' : 'bad'}"></span>{missing(usage.error) ? "Per-key traffic is not reported by this server." : text(usage.error)}</p>{/if}
      <form class="form" onsubmit={add}>
        <label class="field">Group name<input required bind:value={form.name} /></label>
        <label class="field"
          >Base URL<input type="url" required={compat} bind:value={form.url} placeholder={compat ? "https://…/v1" : "Provider default"} /></label
        >
        <label class="field">API key<input type="password" autocomplete="off" required bind:value={form.key} /></label>
        <button class="key primary" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add group</button>
      </form>
    {/snippet}
  </Load>

