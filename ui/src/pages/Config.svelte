<script lang="ts">
  import { store } from "../store.svelte";
  import { api, configValue } from "../api";
  import { fieldPath, readPath, strategies, strategy } from "../core";
  import Editor from "../Editor.svelte";
  import Load from "../Load.svelte";

  const about: Record<string, string> = {
    server: "Listener and TLS. Some changes need a restart.",
    management: "Remote access and the management key.",
    access: "Client keys.",
    routing: "Strategy, retries and cooldowns.",
    requests: "Outbound proxy, streaming and payload rules.",
    "api-keys": "Provider API keys.",
    oauth: "Auth directory, aliases and exclusions.",
    client: "Per-client tweaks.",
    multimedia: "Image and video generation.",
    observability: "Logs and usage statistics.",
    plugins: "Native plugins.",
  };
  let edit = $state<{ path: string; value: unknown; title: string; yaml: boolean } | null>(null);
  // Strategies this server understands; "soonest-reset" is a cliproxy-rs addition.
  const offered = $derived(strategies.filter((s) => !s.rust || store.kind === "rust"));
  const saved = $derived(readPath(store.config.data || {}, "routing/strategy", "") as string);
  const current = $derived(strategy(saved));
  const choose = (value: string) =>
    store.act(() => store.replace("routing/strategy", saved, value, ""), `Routing strategy set to ${strategy(value).name.toLowerCase()}.`);
  function open(section: string) {
    store.act(async () => {
      edit = section
        ? { path: fieldPath(section), value: await configValue(fieldPath(section), {}), title: section, yaml: false }
        : { path: "/config.yaml", value: await api("/config.yaml", "GET", undefined, "text"), title: "config.yaml", yaml: true };
    });
  }
</script>

<div class="head">
  <h1>Configuration</h1>
  {#if !edit}<button class="key" disabled={store.busy} onclick={() => open("")}><svg class="i" aria-hidden="true"><use href="#i-edit" /></svg>Edit YAML</button>{/if}
</div>

{#if edit}
  <Editor {...edit} onclose={() => (edit = null)} />
{:else}
  <Load res={store.config} what="Configuration">
    {#snippet children(config)}
      <section class="section">
        <h2>Routing strategy</h2>
        <div class="seg" role="group" aria-label="Routing strategy">
          {#each offered as s}<button aria-pressed={current.value === s.value} disabled={store.busy} onclick={() => current.value !== s.value && choose(s.value)}
              >{s.name}</button
            >{/each}
        </div>
        <p class="muted">{current.help}</p>
      </section>
      {@const sections = [...new Set([...Object.keys(about), ...Object.keys(config)])].filter((k) => k !== "config-version")}
      <ul class="list">
        {#each sections as k}
          {@const v = config[k]}
          <li>
            <button class="item config-row" onclick={() => open(k)} disabled={store.busy}>
              <span class="name grow"><strong class="mono">{k}</strong><small>{about[k] || "Persisted section."}</small></span>
              <span class="legend"
                >{v === undefined ? "Server defaults" : v && typeof v === "object" ? `${Object.keys(v).length} set` : String(v)}</span
              >
              <svg class="i" aria-hidden="true"><use href="#i-chevron" /></svg>
            </button>
          </li>
        {/each}
      </ul>
      <p class="note">
        Reads show what is saved in the file, not runtime defaults. Each edit is previewed as a diff, and
        the file is reread before writing so a change made elsewhere is caught.
      </p>
    {/snippet}
  </Load>
{/if}
