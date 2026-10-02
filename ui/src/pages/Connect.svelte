<script lang="ts">
  import { store, every } from "../store.svelte";
  import { api } from "../api";
  import { label, type Data } from "../core";
  import Missing from "../Missing.svelte";

  const builtIn = ["claude", "codex", "antigravity", "kimi", "kimi-ai", "xai", "devin", "meta"];
  // Go starts a local callback forwarder for these when asked by a web UI.
  const forwarded = ["claude", "codex", "antigravity", "xai", "devin"];
  $effect(() => {
    if (!store.plugins.data && !store.plugins.loading) store.plugins.load();
  });
  const providers = $derived([
    ...builtIn,
    ...(store.plugins.data || []).filter((p) => p.supports_oauth).map((p) => String(p.oauth_provider || p.id)),
  ]);
  let session = $state<Data | null>(null),
    status = $state(""),
    problem = $state(""),
    callback = $state("");
  const can = $derived(store.can("GET", "/oauth/auth-url"));

  async function start(p: string) {
    if (session && status === "wait") await cancel();
    store.act(async () => {
      const r = await api(`/oauth/auth-url?provider=${encodeURIComponent(p)}${forwarded.includes(p) ? "&is_webui=true" : ""}`);
      if (!/^https?:/.test(new URL(r.url).protocol)) throw new Error("The server returned an unsafe sign-in URL.");
      session = { ...r, provider: p, started: Date.now() };
      status = "wait";
      problem = callback = "";
    });
  }
  async function cancel() {
    const s = session;
    if (!s) return;
    status = "cancelled";
    await api(`/oauth/session?state=${encodeURIComponent(s.state)}`, "DELETE").catch(() => {});
  }
  $effect(() =>
    every(2000, async () => {
      if (!session || status !== "wait") return;
      const r = await api(`/oauth/status?state=${encodeURIComponent(session.state)}`).catch((e) => ({ status: "error", error: e.message }));
      if (r.status === "wait") return;
      status = r.status;
      problem = r.error || "";
      if (r.status === "ok") {
        store.notify(`${label(session.provider)} account connected.`);
        store.creds.load(true);
      }
    }),
  );
  function vertex(input: HTMLInputElement) {
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    const form = new FormData();
    form.append("file", file);
    store.act(async () => {
      await api("/oauth/import?provider=vertex", "POST", form);
      await store.creds.load(true);
    }, "Vertex service account imported.");
  }
</script>

<div class="head"><h1>Connect an account</h1></div>
<Missing actions={[["GET", "/oauth/auth-url", "provider sign-in"], ["POST", "/oauth/import", "Vertex import"]]} />

<section class="section">
  <h2>Sign in with a provider</h2>
  <div class="providers">
    {#each providers as p}
      <button
        class="key"
        aria-pressed={session?.provider === p && status === "wait"}
        disabled={store.busy || !can}
        onclick={() => start(p)}>{label(p)}</button
      >
    {/each}
  </div>
</section>

{#if session}
  <section class="window flow" aria-live="polite">
    <div class="section-head">
      <h2>{label(session.provider)}</h2>
      <span class="row">
        <span class="lamp {status === 'ok' ? 'ok' : status === 'wait' ? 'warn live' : status === 'error' ? 'bad' : 'off'}"></span>
        {status === "wait" ? "Waiting for approval" : status === "ok" ? "Connected" : status === "error" ? "Failed" : "Cancelled"}
      </span>
    </div>
    {#if status === "wait"}
      <ol class="steps">
        <li>
          <span>Open the provider’s page and approve access{session.user_code ? " with this code" : ""}.</span>
          {#if session.user_code}<code class="code-big">{session.user_code}</code>{/if}
          <div class="row">
            <a class="key primary" href={session.url} target="_blank" rel="noopener noreferrer"><svg class="i" aria-hidden="true"><use href="#i-open" /></svg>Open sign-in page</a>
            <button class="key" onclick={() => store.act(() => navigator.clipboard.writeText(session!.url), "Link copied.")}><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy link</button>
          </div>
        </li>
        {#if session.flow !== "device"}
          <li>
            <span>If the browser ends on a page that does not load, the server is on another machine. Paste that page’s full address here.</span>
            <form
              class="form"
              onsubmit={(e) => {
                e.preventDefault();
                store.act(() => api("/oauth/callback", "POST", { provider: session!.provider, redirect_url: callback.trim() }), "Callback sent. Finishing sign-in…");
              }}
            >
              <label class="field"><span class="sr">Callback address</span><input type="url" required bind:value={callback} placeholder="http://localhost:…/callback?code=…" /></label>
              <button class="key" disabled={store.busy}>Send</button>
            </form>
          </li>
        {/if}
      </ol>
      <div class="row"><button class="key quiet" onclick={cancel}>Cancel sign-in</button></div>
    {:else if status === "ok"}
      <p>The credential is saved on the server and already in rotation.</p>
      <div class="row"><a class="key primary" href="#credentials">View credentials</a></div>
    {:else}
      {#if problem}<p class="error">{problem}</p>{/if}
      <div class="row"><button class="key" onclick={() => start(session!.provider)}>Start again</button></div>
    {/if}
  </section>
{/if}

<section class="section">
  <h2>Other ways</h2>
  <ul class="list">
    <li class="item">
      <span class="grow">Upload credential files exported from another CLIProxyAPI server.</span>
      <a class="key" href="#credentials"><svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Upload on Credentials</a>
    </li>
    <li class="item">
      <span class="grow">Use a Google Cloud service-account JSON for Vertex AI.</span>
      <label class="key" aria-disabled={!store.can("POST", "/oauth/import")}
        ><svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Import service account<input
          class="file"
          type="file"
          accept=".json,application/json"
          disabled={store.busy || !store.can("POST", "/oauth/import")}
          onchange={(e) => vertex(e.currentTarget)}
        /></label
      >
    </li>
    <li class="item">
      <span class="grow">Use an API key instead of an account.</span>
      <a class="key" href="#providers">Providers</a>
    </li>
  </ul>
</section>
