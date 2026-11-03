<script lang="ts">
  import { store, every } from "../store.svelte";
  import { api, ApiError } from "../api";
  import { label, provider, credName, type Data } from "../core";
  import Missing from "../Missing.svelte";

  const builtIn = ["claude", "codex", "antigravity", "kimi", "kimi-ai", "xai", "devin", "meta"];
  // Go starts a local callback forwarder for these when asked by a web UI.
  const forwarded = ["claude", "codex", "antigravity", "xai", "devin"];
  if (!store.plugins.data) store.plugins.load();
  const providers = $derived([
    ...builtIn,
    ...(store.plugins.data || []).filter((p) => p.supports_oauth).map((p) => String(p.oauth_provider || p.id)),
  ]);
  let gone = $state<string[]>([]),
    session = $state<Data | null>(null),
    status = $state(""),
    problem = $state(""),
    found = $state(""),
    callback = $state(""),
    region = $state("");
  const can = $derived(store.can("GET", "/oauth/auth-url"));

  async function start(p: string) {
    if (session && status === "wait" && !(await cancel())) return;
    store.act(async () => {
      // The credential names before the sign-in starts, to recognise its credential later.
      const seen = await names();
      const r = await api(`/oauth/auth-url?provider=${encodeURIComponent(p)}${forwarded.includes(p) ? "&is_webui=true" : ""}`).catch((e) => {
        // Go knows every built-in provider; a server that lacks one answers provider_not_found.
        if (e instanceof ApiError && e.code === "provider_not_found") {
          gone = [...gone, p];
          throw new Error(`${label(p)} sign-in is not available on this server.`);
        }
        throw e;
      });
      const url = new URL(r.url, location.href);
      if (!/^https?:$/.test(url.protocol)) throw new Error("The server returned an unsafe sign-in URL.");
      session = { ...r, url: url.href, provider: p, seen };
      status = "wait";
      problem = found = callback = "";
    });
  }
  /** The server's credentials by name, or null when they cannot be read. */
  const names = () =>
    api("/credentials").then(
      (r) => new Map<string, Data>((r.files || []).map((c: Data) => [c.name, c])),
      () => null,
    );
  /** Ends the pending sign-in; false when the server did not take the request. */
  async function cancel() {
    const s = session!;
    // "Cancelled" only when the server says so. A session that already ended answers
    // cancelled: false, and the next status read shows how it really ended.
    try {
      const r = await api(`/oauth/session?state=${encodeURIComponent(s.state)}`, "DELETE");
      if (s === session && r.cancelled !== false) finish("cancelled");
      return true;
    } catch (e) {
      store.notify(`Sign-in not cancelled: ${e instanceof Error ? e.message : e}`, true);
      return false;
    }
  }
  // A finished session replaces any in-progress message ("Callback sent…") so the toast
  // never contradicts the panel. Errors stay: the panel shows them in place.
  function finish(next: string) {
    status = next;
    if (store.toast && !store.toast.bad) store.toast = null;
  }
  $effect(() =>
    every(2000, async () => {
      const s = session;
      if (!s || status !== "wait") return;
      let r = await api(`/oauth/status?state=${encodeURIComponent(s.state)}`).catch((e) => ({ status: "error", error: e.message }));
      // cliproxy-rs keeps a finished sign-in's result for 30 minutes; Go and older servers
      // forget a successful one after a minute. Then only a new credential name for exactly
      // this built-in provider, between two successful reads, says the sign-in worked.
      // Anything else is unknown: a rewritten file can be a refresh or an upload, and a
      // plugin's provider name need not match its credentials'.
      if (r.error === "unknown or expired state") {
        const now = s.seen && builtIn.includes(s.provider) ? await names() : null;
        const fresh = now ? [...now.values()].filter((c) => !s.seen.has(c.name) && provider(c) === s.provider) : [];
        r = fresh.length === 1 ? { status: "ok", found: `${credName(fresh[0])} appeared during this sign-in.` } : { status: "unknown" };
      }
      // A reply for a session that was cancelled or replaced meanwhile is ignored.
      if (r.status === "wait" || s !== session || status !== "wait") return;
      problem = r.error || "";
      found = r.found || "";
      finish(r.status);
      if (r.status === "ok") {
        store.notify(`${label(s.provider)} account connected.`);
        store.creds.load(true);
      }
    }, true),
  );
  function vertex(input: HTMLInputElement) {
    const file = input.files?.[0];
    input.value = "";
    if (!file) return;
    const form = new FormData();
    form.append("file", file);
    // The server defaults an empty region to us-central1.
    if (region.trim()) form.append("location", region.trim());
    store.act(async () => {
      const r = await api("/oauth/import?provider=vertex", "POST", form).catch((e) => {
        if (!(e instanceof ApiError && e.code === "provider_not_found")) throw e;
        gone = [...gone, "vertex"];
        throw new Error("Vertex import is not available on this server.");
      });
      await store.creds.load(true);
      store.notify(`Vertex service account for ${r.project_id} imported (${r.location}).`);
    });
  }
</script>

<div class="head"><h1>Connect an account</h1></div>
<Missing actions={[["GET", "/oauth/auth-url", "provider sign-in"], ["POST", "/oauth/import", "Vertex import"]]} />

<section class="section">
  <h2>Sign in with a provider</h2>
  <p class="muted">You approve access on the provider’s own page (OAuth). The proxy stores the resulting token and renews it; it never sees your password.</p>
  <div class="providers">
    {#each providers as p}
      <button
        class="key"
        aria-pressed={session?.provider === p && status === "wait"}
        disabled={store.busy || !can || gone.includes(p)}
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
        {status === "wait" ? "Waiting for approval" : status === "ok" ? "Connected" : status === "error" ? "Failed" : status === "unknown" ? "Result unavailable" : "Cancelled"}
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
            <span>If the browser ends on a page that fails to load, paste its full address here.</span>
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
      <p>{found || "Saved on the server."}</p>
      <div class="row"><a class="key primary" href="#credentials">View credentials</a></div>
    {:else if status === "unknown"}
      <p>The proxy no longer has this sign-in’s result. Check Credentials to see whether it was added.</p>
      <div class="row"><a class="key primary" href="#credentials">Open Credentials</a><button class="key" onclick={() => start(session!.provider)}>Start again</button></div>
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
      <span class="grow">Use a Google Cloud service-account JSON for Vertex AI.</span>
      <label class="field region"
        ><span class="sr">Vertex region</span><input
          bind:value={region}
          placeholder="us-central1"
          spellcheck="false"
          autocomplete="off"
          disabled={!store.can("POST", "/oauth/import") || gone.includes("vertex")}
        /></label
      >
      <label class="key" aria-disabled={!store.can("POST", "/oauth/import") || gone.includes("vertex")}
        ><svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Import service account<input
          class="file"
          type="file"
          accept=".json,application/json"
          disabled={store.busy || !store.can("POST", "/oauth/import") || gone.includes("vertex")}
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
