<script lang="ts">
  import { store } from "../store.svelte";
  import { base } from "../api";
  import { readPath, mask, newKey, tools, snippet, type Data, type Lamp } from "../core";
  import Load from "../Load.svelte";

  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  let pick = $state(0),
    tool = $state<(typeof tools)[number]>("Claude Code"),
    models = $state<string[]>([]),
    model = $state(""),
    test = $state<{ lamp: Lamp; text: string }>({ lamp: "off", text: "" });
  const key = $derived(keys[Math.min(pick, keys.length - 1)] || "");
  const local = /^(localhost|127\.|\[?::1\]?$)/.test(location.hostname);
  // Cursor calls the proxy from its own servers, so a local address cannot work there.
  const target = $derived(tool === "Cursor" && local ? "https://your-public-address" : base);
  const notes: Record<string, string> = {
    "Claude Code": "Run these in the shell you start Claude Code from. To keep them, put both variables in the env block of ~/.claude/settings.json. /status in Claude Code shows the address in use.",
    "Codex CLI": "Add the provider to ~/.codex/config.toml and set the key in your shell.",
    Cursor: "Cursor sends requests from its own servers, so the proxy needs a public HTTPS address, for example through a Cloudflare tunnel. Enter both values under Cursor Settings, Models.",
    "OpenAI SDK": "Python. The JavaScript SDK takes the same baseURL and apiKey. Chat Completions and Responses both work.",
    "Anthropic SDK": "Python. The address has no /v1 here: the SDK adds it.",
    curl: "Lists the models your connected accounts can serve. A 401 answer means the key is wrong.",
  };

  /** Asks this proxy for its model list with the chosen client key, as a tool would. */
  async function check() {
    const k = key;
    test = { lamp: "off", text: "Testing the key…" };
    try {
      const r = await fetch(`${base}/v1/models`, { headers: { Authorization: `Bearer ${k}` }, cache: "no-store" });
      if (r.status === 401) throw new Error("The proxy rejected this key. Check that it is listed under Client keys.");
      if (!r.ok) throw new Error(`The proxy answered HTTP ${r.status}. Check Logs for the reason.`);
      const ids: string[] = ((await r.json()).data || []).map((m: Data) => String(m.id));
      if (k !== key) return;
      models = ids;
      if (!ids.includes(model)) model = ids[0] || "";
      test = ids.length
        ? { lamp: "ok", text: `Working. This key reaches ${ids.length} model${ids.length > 1 ? "s" : ""}.` }
        : { lamp: "warn", text: "The key works, but no models are available yet. Connect an account first." };
    } catch (e) {
      if (k === key) test = { lamp: "bad", text: e instanceof Error ? e.message : "Cannot reach the proxy." };
    }
  }
  $effect(() => {
    if (key) check();
  });
  const create = () =>
    store.act(() => store.replace("access/api-keys", keys, [...keys, newKey()]), "Client key created.");
</script>

<div class="head"><h1>Use with tools</h1></div>
<p class="muted">
  Point a coding tool or an SDK at this proxy. It speaks the OpenAI, Anthropic and Gemini APIs and answers with the accounts you
  connected.
</p>

<Load res={store.config} what="Client keys">
  {#snippet children()}
    <section class="section">
      <h2>Address and key</h2>
      <dl class="facts">
        <div><dt>Proxy address</dt><dd class="mono">{base}</dd></div>
        <div><dt>OpenAI-style base URL</dt><dd class="mono">{base}/v1</dd></div>
      </dl>
      {#if local}<p class="note">
          <span class="lamp warn"></span>This address works only on this computer. Other machines need its network address, and
          cloud services such as Cursor need a public HTTPS address.
        </p>{/if}
      {#if keys.length}
        {#if keys.length > 1 || models.length}<div class="form">
            {#if keys.length > 1}<label class="field"
                >Client key<select bind:value={pick}>{#each keys as k, i}<option value={i}>{mask(k)}</option>{/each}</select></label
              >{/if}
            {#if models.length}<label class="field"
                >Model<select bind:value={model}>{#each models as m}<option>{m}</option>{/each}</select></label
              >{/if}
          </div>{/if}
        <div class="row">
          <p class="note grow" role="status"><span class="lamp {test.lamp}" class:live={test.text.endsWith("…")}></span>{test.text}</p>
          <button class="key small" onclick={check}>Test again</button>
        </div>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp warn"></span>No client key yet</div>
          <p>Tools need a client key, a password they send to this proxy. Create one here and manage it under Client keys.</p>
          <button class="key primary" disabled={store.busy} onclick={create}
            ><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Create a client key</button
          >
        </div>
      {/if}
    </section>

    {#if keys.length}
      <section class="section">
        <h2>Set up a tool</h2>
        <div class="seg" role="group" aria-label="Tool">
          {#each tools as t}<button aria-pressed={tool === t} onclick={() => (tool = t)}>{t}</button>{/each}
        </div>
        <p class="muted">{notes[tool]}</p>
        <pre class="code-window" aria-label={`${tool} setup`}><div>{snippet(tool, target, mask(key), model || "MODEL_ID")}</div></pre>
        <div class="row">
          <button
            class="key primary"
            onclick={() => store.act(() => navigator.clipboard.writeText(snippet(tool, target, key, model || "MODEL_ID")), "Copied, with the full key.")}
            ><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy</button
          >
          <span class="legend">The copy includes the full key; the page shows it shortened.</span>
        </div>
      </section>
    {/if}
  {/snippet}
</Load>
