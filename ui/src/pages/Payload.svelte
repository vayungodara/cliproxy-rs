<script lang="ts">
  import { store } from "../store.svelte";
  import { readPath, type Data } from "../core";
  import Load from "../Load.svelte";

  const kinds: [string, string][] = [
    ["default", "Set a value the request lacks."],
    ["default-raw", "Default, with raw JSON values."],
    ["override", "Always set the value."],
    ["override-raw", "Override, with raw JSON values."],
    ["filter", "Remove paths from the request."],
  ];
  let kind = $state("default");
  let form = $state({ model: "*", protocol: "", params: '{\n  "temperature": 0.7\n}' });
  const payload = $derived(readPath(store.config.data || {}, "requests/payload", {}) as Data);
  const rules = $derived((payload[kind] || []) as Data[]);
  function add(e: SubmitEvent) {
    e.preventDefault();
    let params: unknown;
    try {
      params = JSON.parse(form.params);
    } catch {
      return store.notify("Parameters must be valid JSON.", true);
    }
    const list = Array.isArray(params);
    if (kind === "filter" ? !list : list || !params || typeof params !== "object")
      return store.notify(kind === "filter" ? "Filter parameters are a JSON list of paths." : "Parameters are a JSON object of path → value.", true);
    const model: Data = { name: form.model.trim() };
    if (form.protocol) model.protocol = form.protocol;
    store.act(() => store.replace(`requests/payload/${kind}`, rules, [...rules, { models: [model], params }]), "Rule added.");
  }
</script>

<div class="head">
  <h1>Payload rules</h1>
  <a class="key" href="#config"><svg class="i" aria-hidden="true"><use href="#i-edit" /></svg>Edit in Configuration</a>
</div>

  <div class="seg" role="group" aria-label="Rule type">
    {#each kinds as [k]}<button aria-pressed={kind === k} onclick={() => (kind = k)}>{k}<b>{(payload[k] || []).length || ""}</b></button>{/each}
  </div>
  <Load res={store.config} what="Payload rules">
    {#snippet children()}
      <section class="section">
        <p class="muted">{kinds.find((k) => k[0] === kind)?.[1]} Rules match on model name and, optionally, protocol.</p>
        {#if rules.length}
          <ul class="list">
            {#each rules as rule, i}
              <li class="item rule">
                <div class="stack grow">
                  <div class="chips">
                    {#each rule.models || [] as m}<span class="chip">{m.name}{m.protocol ? ` · ${m.protocol}` : ""}</span>{/each}
                  </div>
                  <pre class="legend">{JSON.stringify(rule.params, null, 2)}</pre>
                </div>
                <button
                  class="key small quiet danger"
                  disabled={store.busy}
                  onclick={() => store.act(() => store.replace(`requests/payload/${kind}`, rules, rules.filter((_, n) => n !== i)), "Rule removed.")}
                  >Remove</button
                >
              </li>
            {/each}
          </ul>
        {:else}<p class="note"><span class="lamp off"></span>No {kind} rules.</p>{/if}
        <form class="stack" onsubmit={add}>
          <div class="form">
            <label class="field">Model pattern<input required bind:value={form.model} /></label>
            <label class="field"
              >Protocol<select value={form.protocol} onchange={(e) => (form.protocol = e.currentTarget.value)}
                ><option value="">Any</option>{#each ["openai", "claude", "gemini", "codex", "antigravity"] as p}<option>{p}</option>{/each}</select
              ></label
            >
          </div>
          <label class="field"
            >{kind === "filter" ? "Paths, as a JSON list" : "Values, as a JSON object of path → value"}<textarea
              class="code"
              rows="5"
              spellcheck="false"
              bind:value={form.params}></textarea></label
          >
          <div class="row"><button class="key primary" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add rule</button></div>
        </form>
      </section>
    {/snippet}
  </Load>
