<script lang="ts">
  import { store, every } from "../store.svelte";
  import { api, ApiError } from "../api";
  import { label, provider, credName, accountRow, span, type Data } from "../core";
  import { check, canCheck, hasSource, limits, quotaKey, wait } from "../quota";
  import Missing from "../Missing.svelte";
  import Bar from "../Bar.svelte";

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
    acct = $state<Data | null>(null),
    // The usage read this sign-in made; null when none was made (the 2-minute wait, or no source).
    got = $state<Data | null>(null),
    now = $state(Date.now()),
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
      session = { ...r, url: url.href, provider: p, seen, started: Date.now() };
      status = "wait";
      problem = found = callback = "";
      acct = got = null;
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
      now = Date.now();
      if (r.error === "unknown or expired state") {
        const fresh = (await mine(s)).filter((c) => !s.seen.has(c.name));
        r = fresh.length === 1 ? { status: "ok", acct: fresh[0], found: `${credName(fresh[0])} appeared during this sign-in.` } : { status: "unknown" };
      }
      // A reply for a session that was cancelled or replaced meanwhile is ignored.
      if (r.status === "wait" || s !== session || status !== "wait") return;
      problem = r.error || "";
      found = r.found || "";
      if (r.status !== "ok") return finish(r.status);
      // A queued usage check reads the account from this list, so it must hold the new one.
      await store.creds.load(true);
      await verify(s, r.acct);
    }, true),
  );
  /** This provider's credentials now, or none when they cannot be read or matched. */
  async function mine(s: Data): Promise<Data[]> {
    const list = s.seen && builtIn.includes(s.provider) ? await names() : null;
    return list ? [...list.values()].filter((c) => provider(c) === s.provider) : [];
  }
  /**
   * After a confirmed success: name the credential when exactly one of this provider's is new
   * or was rewritten since the start, then read its usage once, as Accounts does.
   */
  async function verify(s: Data, known?: Data) {
    let a = known;
    if (!a) {
      const list = await mine(s);
      const fresh = list.filter((c) => !s.seen.has(c.name));
      const changed = list.filter((c) => s.seen.get(c.name)?.updated_at !== c.updated_at);
      a = fresh.length === 1 ? fresh[0] : !fresh.length && changed.length === 1 ? changed[0] : undefined;
    }
    if (s !== session) return;
    acct = a || null;
    got = null;
    // A failed reload leaves the new account out of the list the queue reads from, so no read
    // would be made. Within the 2-minute wait check() would hand back a read already on its way
    // for the old token, so none is made and the panel says usage was not read.
    if (a && canCheck(a))
      if (store.creds.error) got = { error: `accounts did not reload: ${(store.creds.error as Error).message}` };
      else if (!wait(a)) {
        finish("verifying");
        // Every read stores a new object; an unchanged one means this check made no read.
        const before = store.quota[quotaKey(a)];
        await check(a);
        if (s !== session) return;
        if (store.quota[quotaKey(a)] !== before) got = store.quota[quotaKey(a)];
      }
    // Only a token the provider rejects is "refused"; other failed reads say so plainly.
    const refused = [401, 403].includes(got?.code);
    finish(refused ? "refused" : "ok");
    if (!refused) store.notify(`${label(s.provider)} account connected.`);
  }
  // Whose page the user approves on.
  const owner = (p: string) => (p === "codex" ? "OpenAI" : label(p));
  const v = $derived(acct ? accountRow(acct, limits(acct), true, true) : null);
  // The bars come from this tab's own usage read, never from older passive numbers.
  const live = $derived(status === "ok" && !!got?.windows && !got.error);
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
  <p class="muted">You approve access on the provider’s own page. The proxy keeps the token and renews it; it never sees your password.</p>
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
  {@const P = label(session.provider)}
  <section class="window flow" aria-live="polite">
    <div class="section-head">
      <h2>{P}</h2>
      {#if status === "wait" && session.expires_in}<span class="legend"
          >{session.started + session.expires_in * 1000 > now ? `Code expires in ${span(session.started + session.expires_in * 1000 - now)}` : "Code expired"}</span
        >{/if}
      <span class="row">
        <span class="lamp {status === 'ok' ? 'ok' : status === 'wait' || status === 'verifying' ? 'warn live' : status === 'error' || status === 'refused' ? 'bad' : 'off'}"></span>
        {({ wait: "Waiting for approval", verifying: "Checking the account", ok: "Connected", refused: "Provider refused", error: "Failed", unknown: "Result unavailable" } as Record<string, string>)[status] || "Cancelled"}
      </span>
    </div>
    {#if status === "wait"}
      {#if session.user_code}
        <p>Enter this code on {owner(session.provider)}’s sign-in page.</p>
        <div class="plate">
          <code>{session.user_code}</code>
          <button class="key" onclick={() => store.act(() => navigator.clipboard.writeText(session!.user_code), "Code copied.")}><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy code</button>
        </div>
      {:else}
        <p>Approve access on {owner(session.provider)}’s page. This panel updates by itself when you’re done.</p>
      {/if}
      <div class="row">
        <a class="key primary" href={session.url} target="_blank" rel="noopener noreferrer"><svg class="i" aria-hidden="true"><use href="#i-open" /></svg>Open sign-in page</a>
        <button class="key" onclick={() => store.act(() => navigator.clipboard.writeText(session!.url), "Link copied.")}><svg class="i" aria-hidden="true"><use href="#i-copy" /></svg>Copy link</button>
        {#if session.user_code}<span class="link">{session.url}</span>{/if}
      </div>
      {#if session.user_code}<p class="muted small">
          No browser on this server? Copy the link and open it on any device. This panel checks every 2 seconds and again as soon as you come back to the tab.
        </p>{/if}
      {#if session.flow !== "device"}
        <details class="fallback">
          <summary>The browser ended on a page that won’t load</summary>
          <form
            class="form"
            onsubmit={(e) => {
              e.preventDefault();
              store.act(() => api("/oauth/callback", "POST", { provider: session!.provider, redirect_url: callback.trim() }), "Callback sent. Finishing sign-in…");
            }}
          >
            <label class="field">Paste that page’s full address<input type="url" required bind:value={callback} placeholder="http://localhost:…/callback?code=…" /></label>
            <button class="key" disabled={store.busy}>Send</button>
          </form>
        </details>
      {/if}
      <div class="row"><button class="key quiet" onclick={cancel}>Cancel sign-in</button></div>
    {:else if status === "verifying"}
      <ul class="checks">
        <li><svg class="i" aria-hidden="true"><use href="#i-check" /></svg>Approved on {owner(session.provider)}’s page</li>
        <li><svg class="i" aria-hidden="true"><use href="#i-check" /></svg>Token saved on the server</li>
        <li><span class="lamp warn live"></span>Reading usage. This is one free read, not a model request.</li>
      </ul>
    {:else if status === "ok" || status === "refused"}
      {#if acct && v}
        <div class="result">
          <span class="name"
            ><strong>{credName(acct)}</strong><small
              >{[P, acct.id_token?.plan_type].filter(Boolean).join(" ")} · {status === "refused" ? "token saved" : v.word.toLowerCase()}</small
            ></span
          >
          {#if status === "refused"}
            <Bar name="Session" w={{ why: "not read" }} /><Bar name="Weekly" w={{ why: "not read" }} />
          {:else if live}
            <Bar name="Session" w={v.session} /><Bar name="Weekly" w={v.weekly} />
          {/if}
        </div>
      {/if}
      {#if found}<p>{found}</p>{:else if !acct}<p>Saved on the server.</p>{/if}
      {#if status === "refused"}
        <p class="error">{got?.error}</p>
        <p class="muted small">
          The token is saved, but requests with it will probably fail the same way. Try the read again, or sign in again.
        </p>
        <div class="row">
          <button
            class="key"
            onclick={() => (wait(acct!) ? store.notify(`Read moments ago. Try again in ${span(wait(acct!))}.`) : verify(session!, acct!))}
            ><svg class="i" aria-hidden="true"><use href="#i-refresh" /></svg>Try again</button
          >
          <button class="key primary" onclick={() => start(session!.provider)}>Sign in again</button>
          <a class="key quiet" href="#accounts">Open Accounts</a>
        </div>
      {:else}
        {#if live}<p class="muted small">Usage checked just now.</p>
        {:else if got?.error}<p class="error">Usage read failed: {got.error}</p>
        {:else if acct && canCheck(acct)}<p class="muted small">Usage not read: read under 2 minutes ago.</p>{/if}
        <div class="row">
          <button class="key primary" onclick={() => start(session!.provider)}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect another {P} account</button>
          <a class="key" href="#accounts">Open Accounts</a>
        </div>
      {/if}
    {:else if status === "unknown"}
      <p>The proxy no longer has this sign-in’s result. Check Accounts to see whether it was added.</p>
      <div class="row"><a class="key primary" href="#accounts">Open Accounts</a><button class="key" onclick={() => start(session!.provider)}>Start again</button></div>
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
