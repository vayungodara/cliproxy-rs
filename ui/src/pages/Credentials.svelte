<script lang="ts">
  import { store, every, Res } from "../store.svelte";
  import { api, download } from "../api";
  import { buckets, sum, credState, credName, provider, label, span, type Data } from "../core";
  import Grille from "../Grille.svelte";
  import Load from "../Load.svelte";
  import Missing from "../Missing.svelte";

  $effect(() => every(10_000, () => store.creds.load(true)));
  let query = $state(""),
    filter = $state("all"),
    open = $state(store.route.arg);
  $effect(() => {
    open = store.route.arg;
  });
  const key = (a: Data) => `${a.name}\u0000${a.auth_index}`;
  // Go allows several credentials from one file; its ID is unique and every per-credential
  // route accepts it in place of the file name. Downloads and deletes still use the file.
  const id = (a: Data) => String(a.id || a.name);
  const rows = $derived(
    (store.creds.data || []).map((a) => ({ a, b: buckets(a), s: credState(a) })),
  );
  const max = $derived(Math.max(1, ...rows.flatMap((r) => r.b?.total || [])));
  const filters = $derived(
    (
      [
        ["all", "All"],
        ["ok", "Ready"],
        ["warn", "Cooling"],
        ["bad", "Failing"],
        ["off", "Disabled"],
      ] as const
    )
      .map(([id, name]) => ({
        id,
        name,
        n: id === "all" ? rows.length : rows.filter((r) => r.s.lamp === id).length,
      }))
      .filter((f) => f.id === "all" || f.n),
  );
  const shown = $derived(
    rows.filter(
      (r) =>
        (filter === "all" || r.s.lamp === filter) &&
        `${r.a.name} ${credName(r.a)} ${provider(r.a)} ${r.a.note || ""}`
          .toLowerCase()
          .includes(query.trim().toLowerCase()),
    ),
  );
  const groups = $derived(
    [...new Set(shown.map((r) => provider(r.a)))].map((p) => ({
      p,
      rows: shown.filter((r) => provider(r.a) === p),
    })),
  );

  // Detail panel state for the open credential.
  const selected = $derived(rows.find((r) => id(r.a) === open)?.a);
  let fields = $state({ note: "", priority: "", weight: "", request_retry: "" });
  let models = $state<Res<Data[]> | null>(null);
  // Fill the form when a credential is opened, not on every poll, so typing is not overwritten.
  let formFor = "";
  $effect(() => {
    const a = selected;
    const k = a ? id(a) : "";
    if (k === formFor) return;
    formFor = k;
    if (!a) return;
    fields = {
      note: a.note || "",
      priority: a.priority === undefined ? "" : String(a.priority),
      weight: a.weight === undefined ? "" : String(a.weight),
      request_retry: a.request_retry === undefined ? "" : String(a.request_retry),
    };
    const name = id(a);
    const res = new Res<Data[]>(async () => (await api(`/credentials/models?name=${encodeURIComponent(name)}`)).models || []);
    models = res;
    res.load();
  });
  function toggle(a: Data) {
    store.go(open === id(a) ? "credentials" : `credentials/${encodeURIComponent(id(a))}`);
  }
  const lookup = (a: Data) => ({ name: a.name, ...(a.auth_index ? { auth_index: a.auth_index } : {}) });
  const fieldNames = [
    ["note", "Note", ""],
    ["priority", "Priority", "0"],
    ["weight", "Weight", "1"],
    ["request_retry", "Retries", "Global"],
  ] as const;
  const facts = (a: Data) =>
    [
      ["File", a.name, true],
      ["Auth index", a.auth_index || "—", true],
      ["Source", a.runtime_only ? "Configuration" : a.source || "file"],
      ["Lifetime", a.success === undefined ? "Not reported" : `${a.success} ok · ${a.failed || 0} failed`],
      ["Token refreshed", a.last_refresh && new Date(a.last_refresh).toLocaleString()],
      ["Status", a.status_message],
    ].filter((f) => f[1]);
  const actions = (a: Data): [string, string, string, Data, string, string, boolean][] => [
    [a.disabled ? "Enable" : "Disable", "PATCH", "/credentials/status", { ...lookup(a), disabled: !a.disabled }, a.disabled ? "Enabled." : "Disabled.", "", false],
    ["Refresh token", "POST", "/credentials/refresh", lookup(a), "Token refreshed.", "", false],
    ["Reset cooldown", "POST", "/routing/cooldown/reset", { auth_index: a.auth_index }, "Cooldown cleared.", "Clear the local cooldown? Provider quota is not restored.", !a.auth_index],
    ["Delete", "DELETE", "/credentials", { names: [a.name] }, "Credential deleted.", `Delete ${a.name}? The file is removed from the server.`, !!a.runtime_only],
  ];
  const reload = () => store.creds.load(true);

  async function upload(input: HTMLInputElement) {
    const files = [...(input.files || [])];
    input.value = "";
    if (!files.length) return;
    await store.act(async () => {
      const form = new FormData();
      files.forEach((f) => form.append("file", f));
      const result = await api("/credentials", "POST", form);
      await reload();
      if (result.failed?.length)
        throw new Error(
          `${result.uploaded || 0} uploaded. Failed: ${result.failed.map((f: Data) => `${f.name} (${f.error})`).join(", ")}`,
        );
    }, files.length > 1 ? `${files.length} files uploaded.` : "Credential uploaded.");
  }
  function refreshAll() {
    store.act(async () => {
      const result = await api("/credentials/refresh", "POST", { all: true });
      await reload();
      const bad = (result.results || []).filter((r: Data) => !r.success);
      if (bad.length)
        throw new Error(`${bad.length} could not refresh: ${bad.map((r: Data) => `${r.id} (${r.error})`).join(", ")}`);
      store.notify(result.results?.length ? `${result.results.length} tokens refreshed.` : "No credentials can refresh.");
    });
  }
  function saveFields(a: Data) {
    const patch: Data = { name: id(a), note: fields.note.trim() };
    for (const k of ["priority", "weight", "request_retry"] as const) {
      const v = fields[k].trim();
      // An emptied field clears the override (Go deletes the key on null).
      if (v === "") {
        if (a[k] !== undefined) patch[k] = null;
        continue;
      }
      if (!/^-?\d+$/.test(v)) return store.notify(`${k.replace("_", " ")} must be a whole number.`, true);
      patch[k] = Number(v);
    }
    store.call("PATCH", "/credentials/fields", patch, "Saved.");
  }
</script>

<div class="head">
  <h1>Credentials{#if rows.length}<span>{rows.length}</span>{/if}</h1>
  <label class="key" aria-disabled={!store.can("POST", "/credentials")}>
    <svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Upload
    <input
      class="file"
      type="file"
      accept=".json,application/json"
      multiple
      disabled={store.busy || !store.can("POST", "/credentials")}
      onchange={(e) => upload(e.currentTarget)}
    />
  </label>
  <button
    class="key"
    disabled={store.busy || !rows.length || !store.can("POST", "/credentials/refresh")}
    onclick={refreshAll}><svg class="i" aria-hidden="true"><use href="#i-refresh" /></svg>Refresh tokens</button
  >
  <a class="key primary" href="#connect"><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect account</a>
</div>

<Missing
  actions={[
    ["POST", "/credentials", "upload"],
    ["POST", "/credentials/refresh", "token refresh"],
    ["PATCH", "/credentials/fields", "editing fields"],
    ["DELETE", "/credentials", "delete"],
    ["POST", "/routing/cooldown/reset", "cooldown reset"],
  ]}
/>

<Load res={store.creds} what="Credentials">
  {#snippet children(list)}
    {#if !list.length}
      <div class="state">
        <div class="row"><span class="lamp off"></span>No credentials yet</div>
        <p>
          Connect an account with the provider’s sign-in, or upload credential JSON files
          exported from another CLIProxyAPI server.
        </p>
      </div>
    {:else}
      <section class="section">
        <div class="row">
          <label class="search grow">
            <svg class="i" aria-hidden="true"><use href="#i-search" /></svg><span class="sr">Filter credentials</span>
            <input bind:value={query} placeholder="Filter by name, provider or note" />
          </label>
          <div class="seg" role="group" aria-label="Show">
            {#each filters as f (f.id)}<button aria-pressed={filter === f.id} onclick={() => (filter = f.id)}
                >{f.name}<b>{f.n}</b></button
              >{/each}
          </div>
        </div>
        <ul class="list creds full">
          <li class="columns legend" aria-hidden="true">
            <span></span><span>Credential</span><span>Last 200 min</span><span
              class="count">Requests</span
            ><span>State</span>
          </li>
          {#each groups as g (g.p)}
            <li class="group">{label(g.p)}<span>{g.rows.length}</span></li>
            {#each g.rows as r (key(r.a))}
              {@const isOpen = open === id(r.a)}
              <li class:open={isOpen}>
                <button class="item" aria-expanded={isOpen} onclick={() => toggle(r.a)}>
                  <span class="lamp {r.s.lamp}"></span>
                  <span class="name"
                    ><strong>{credName(r.a)}</strong><small
                      >{r.a.note ? `${r.a.note} · ${r.a.name}` : r.a.name}</small
                    ></span
                  >
                  {#if r.b}<Grille data={r.b} {max} />{:else}<span class="legend">Not reported</span>{/if}
                  <span class="num count"
                    >{r.b ? sum(r.b.total).toLocaleString() : "—"}{#if r.b && sum(r.b.failed)}<small class="error">{sum(r.b.failed)} failed</small>{/if}</span
                  >
                  <span class="state-label">{r.s.label}{#if r.s.detail}<small title={r.s.detail}>{r.s.detail}</small>{/if}</span>
                </button>
                {#if isOpen}
                  {@const a = r.a}
                  <div class="detail window">
                    <dl class="facts">
                      {#each facts(a) as [k, v, mono]}<div><dt>{k}</dt><dd class:mono>{v}</dd></div>{/each}
                    </dl>
                    {#if a.cooldowns?.length || a.quota?.signals && Object.keys(a.quota.signals).length}
                      <div class="chips">
                        {#each a.cooldowns || [] as c}<span class="chip"
                            ><span class="lamp warn"></span>{c.model_key || "credential"} · {c.reason?.replaceAll("_", " ")}{c.http_status ? ` ${c.http_status}` : ""} · {span(Date.parse(c.retry_at) - Date.now())}</span
                          >{/each}
                        {#each Object.entries(a.quota?.signals || {}) as [k, v]}<span class="chip">{k}: {v}</span>{/each}
                      </div>
                    {/if}
                    {#if !a.runtime_only}
                      <form class="form" onsubmit={(e) => (e.preventDefault(), saveFields(a))} inert={!store.can("PATCH", "/credentials/fields")}>
                        {#each fieldNames as [k, name, hint]}<label class="field" class:note-field={k === "note"}
                            >{name}<input bind:value={fields[k]} placeholder={hint} inputmode={k === "note" ? undefined : "numeric"} /></label
                          >{/each}
                        <button class="key" disabled={store.busy || !store.can("PATCH", "/credentials/fields")}>Save</button>
                      </form>
                    {/if}
                    <div class="stack">
                      <h3>Models</h3>
                      {#if models}<Load res={models} what="Models">
                          {#snippet children(list)}<div class="chips">
                              {#each list as m}<span class="chip">{m.id}</span>{:else}<span class="muted">No models reported.</span>{/each}
                            </div>{/snippet}
                        </Load>{/if}
                    </div>
                    <div class="row">
                      {#each actions(a) as [name, method, path, body, done, ask, off]}<button
                          class="key small"
                          class:danger={method === "DELETE"}
                          class:quiet={method === "DELETE"}
                          disabled={store.busy || off || !store.can(method, path)}
                          onclick={() =>
                            store.call(method, path, body, done, ask).then((ok) => ok && method === "DELETE" && store.go("credentials"))}
                          >{name}</button
                        >{/each}
                      <button
                        class="key small"
                        disabled={store.busy || a.runtime_only || !store.can("GET", "/credentials/download")}
                        onclick={() =>
                          store.act(async () =>
                            download(await api(`/credentials/download?name=${encodeURIComponent(a.name)}`, "GET", undefined, "blob"), a.name),
                          )}><svg class="i" aria-hidden="true"><use href="#i-download" /></svg>Download</button
                      >
                    </div>
                  </div>
                {/if}
              </li>
            {/each}
          {:else}
            <li class="state"><p>No credentials match.</p></li>
          {/each}
        </ul>
      </section>
    {/if}
  {/snippet}
</Load>
