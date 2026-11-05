<script lang="ts">
  import { store, every, Res } from "../store.svelte";
  import { api, download } from "../api";
  import { buckets, credName, provider, label, span, accountRow, strategy, readPath, type Bar, type Data, type Window } from "../core";
  import { check, checkAll, canCheck, hasSource, limits, quotaKey, wait } from "../quota";
  import Grille from "../Grille.svelte";
  import Meter from "../Meter.svelte";
  import Load from "../Load.svelte";
  import Missing from "../Missing.svelte";

  // `now` also re-enables Check quota once an account's 2-minute wait is over.
  let now = $state(Date.now());
  $effect(() => every(10_000, () => ((now = Date.now()), store.creds.load(true))));
  if (!store.plugins.data) store.plugins.load();
  let tab = $state("all"),
    filter = $state(""),
    checking = $state(false),
    sel = $state<string[]>([]),
    open = $state(store.route.arg);
  $effect(() => {
    open = store.route.arg;
  });
  const key = (a: Data) => `${a.name}\u0000${a.auth_index}`;
  // Go allows several credentials from one file; its ID is unique and every per-credential
  // route accepts it in place of the file name. Downloads and deletes still use the file.
  const id = (a: Data) => String(a.id || a.name);
  const rows = $derived(
    (store.creds.data || [])
      .map((a) => ({ a, k: key(a), p: provider(a), b: buckets(a), v: accountRow(a, limits(a), !!store.quota[quotaKey(a)] && "windows" in store.quota[quotaKey(a)], hasSource(a)) }))
      .sort((x, y) => x.v.next - y.v.next || credName(x.a).localeCompare(credName(y.a))),
  );
  const max = $derived(Math.max(1, ...rows.flatMap((r) => r.b?.total || [])));
  const tabs = $derived([["all", rows.length], ...[...new Set(rows.map((r) => r.p))].map((p) => [p, rows.filter((r) => r.p === p).length])] as [string, number][]);
  const quick = [
    ["ready", "Ready"],
    ["low", "Low"],
    ["attention", "Needs attention"],
  ] as const;
  const inTab = $derived(rows.filter((r) => tab === "all" || r.p === tab));
  const shown = $derived(
    // ponytail: no search box; it was cut to keep the JS under its 52 KB gzip budget. Bring it
    // back (a text filter over name, email and note) when the budget allows.
    inTab.filter((r) => !filter || r.v[filter as "ready" | "low" | "attention"]),
  );
  const groups = $derived([...new Set(shown.map((r) => r.p))].map((p) => ({ p, rows: shown.filter((r) => r.p === p) })));
  const reset = $derived(strategy(readPath(store.config.data || {}, "routing/strategy", ""), store.kind === "rust").value === "soonest-reset");

  // Selection survives tab and filter changes; the batch bar says how many the view hides.
  const picked = $derived(rows.filter((r) => sel.includes(r.k)));
  const hidden = $derived(picked.filter((r) => !shown.includes(r)));
  const where = $derived(
    tab !== "all" && hidden.every((r) => r.p === hidden[0].p && r.p !== tab) ? `on the ${label(hidden[0]?.p)} tab` : "hidden by the filters",
  );
  const pick = (k: string) => (sel = sel.includes(k) ? sel.filter((x) => x !== k) : [...sel, k]);
  const all = $derived(shown.length > 0 && shown.every((r) => sel.includes(r.k)));
  const some = $derived(!all && shown.some((r) => sel.includes(r.k)));
  // Accounts that share one file (multi-project files, plugin sources) change together: a
  // delete removes the file, and a plugin source's status change reaches every account it
  // expands to (Go auth_files_fields.go patchPluginVirtualSourceStatus). Batch actions act on
  // whole files and name every account they reach, selected or not.
  const file = (a: Data) => (a.runtime_only ? `\u0000${key(a)}` : a.name);
  const reach = $derived([...new Set(picked.map((r) => file(r.a)))].map((f) => rows.filter((r) => file(r.a) === f).map((r) => r.a)));
  const extra = $derived(reach.flat().filter((a) => !sel.includes(key(a))));
  const names = (list: Data[]) => list.map(credName).join(", ");
  /**
   * Runs every unit, past ordinary failures, then reports how many finished and which failed.
   * With `ok`, the outcome is read back from the server instead: a unit already done is not
   * run, and a unit failed when it is not done afterwards, whichever requests errored.
   */
  function batch<T>(units: T[], name: (u: T) => string, run: (u: T) => Promise<unknown>, done: (n: number) => string, ask = "", ok?: (u: T) => boolean) {
    if (ask && !confirm(ask)) return;
    store.act(async () => {
      const why = new Map<T, string>();
      for (const u of units)
        if (!ok?.(u)) {
          // A reread after each unit shows progress and lets `ok` see file-wide changes.
          await run(u).catch((e) => why.set(u, e instanceof Error ? e.message : e));
          await reload();
        }
      const failed = units.filter((u) => (ok ? !ok(u) : why.has(u)));
      if (failed.length) throw new Error(`${done(units.length - failed.length)}. ${failed.length} failed: ${failed.map((u) => name(u) + (why.has(u) ? ` (${why.get(u)})` : "")).join(", ")}`);
    }, `${done(units.length)}.`);
  }
  // Every account of a file that holds a selected account to switch, so a shared source never
  // ends half switched.
  const toSwitch = (disabled: boolean) =>
    reach.filter((g) => g.some((a) => sel.includes(key(a)) && !!a.disabled !== disabled)).flat().filter((a) => !!a.disabled !== disabled);
  // A plugin source switches its whole file on one request, which the list does not mark, so
  // the outcome is read back: an account a file-wide change already switched is not asked
  // again, and the counts are what the server reports afterwards.
  const setStatus = (disabled: boolean) => {
    const list = toSwitch(disabled);
    const also = list.filter((a) => !sel.includes(key(a)));
    batch(
      list,
      credName,
      (a) => api("/credentials/status", "PATCH", { ...lookup(a), disabled }),
      (n) => `${plural(n, "account")} ${disabled ? "disabled" : "enabled"}`,
      also.length ? `${disabled ? "Disable" : "Enable"} ${plural(list.length, "account")}? Same files, so these change too: ${names(also)}` : "",
      (a) => (store.creds.data || []).some((x) => key(x) === key(a) && !!x.disabled === disabled),
    );
  };
  const plural = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

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
    store.go(open === id(a) ? "accounts" : `accounts/${encodeURIComponent(id(a))}`);
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
  async function checkQuotas() {
    checking = true;
    await checkAll(store.creds.data || []);
    checking = false;
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

{#snippet bar(name: string, w: Bar, stale: boolean)}
  {#if "used" in w}
    <div class="win" class:stale role="meter" aria-label={`${name} ${w.used}% used`} aria-valuenow={w.used} aria-valuemin={0} aria-valuemax={100}>
      <span><b>{w.used}%</b> {name.toLowerCase()}</span><span class="track"><i class:hi={w.used >= 85} style:width="{w.used}%"></i></span><span>{w.reset}</span>
    </div>
  {:else}
    <div class="win"><span><b>unknown</b><span class="sr"> {name}</span></span><span class="track none"></span><span>{w.why}</span></div>
  {/if}
{/snippet}

<div class="head">
  <h1>Accounts{#if rows.length}<span>{rows.length}</span>{/if}</h1>
  <label class="key quiet" aria-disabled={!store.can("POST", "/credentials")}>
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
  <button class="key" disabled={checking || !rows.some((r) => canCheck(r.a))} onclick={checkQuotas}
    ><svg class="i" aria-hidden="true"><use href="#i-refresh" /></svg>{checking ? "Checking…" : "Check quotas"}</button
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
    ["POST", "/requests/api-call", "quota checks"],
  ]}
/>

<Load res={store.creds} what="Accounts">
  {#snippet children(list)}
    {#if !list.length}
      <div class="state">
        <div class="row"><span class="lamp off"></span>No accounts yet</div>
        <p>
          Connect an account with the provider’s sign-in, or upload credential JSON files
          exported from another CLIProxyAPI server.
        </p>
      </div>
    {:else}
      <section class="section">
        <p class="muted small">
          Sorted by soonest weekly reset{reset ? ", the order Reset first uses" : ""}. Quota checks go one account at a time, each at most once every 2 minutes.
        </p>
        <div class="toolbar">
          <div class="seg" role="group" aria-label="Provider">
            {#each tabs as [t, n] (t)}<button aria-pressed={tab === t} onclick={() => (tab = t)}>{t === "all" ? "All" : label(t)}<b>{n}</b></button>{/each}
          </div>
          <div class="seg" role="group" aria-label="Show">
            {#each quick as [f, name] (f)}<button aria-pressed={filter === f} onclick={() => (filter = filter === f ? "" : f)}
                >{name}<b>{inTab.filter((r) => r.v[f]).length}</b></button
              >{/each}
          </div>
        </div>
        <ul class="list acct">
          <li class="columns legend">
            <input
              type="checkbox"
              aria-label="Select all shown"
              checked={all}
              indeterminate={some}
              onchange={() => (sel = all ? sel.filter((k) => !shown.some((r) => r.k === k)) : [...new Set([...sel, ...shown.map((r) => r.k)])])}
            /><span aria-hidden="true">Status</span><span aria-hidden="true">Account</span><span aria-hidden="true">Session</span><span aria-hidden="true">Weekly</span><span
              aria-hidden="true">Requests, 200 min</span
            ><span class="count" aria-hidden="true">Last used</span>
          </li>
          {#each groups as g (g.p)}
            <li class="group">{label(g.p)}<span>{g.rows.length} · {g.rows.filter((r) => r.v.ready).length} can serve</span></li>
            {#each g.rows as r (r.k)}
              {@const isOpen = open === id(r.a)}
              {@const v = r.v}
              <li class:open={isOpen}>
                <div class="item" class:sel={sel.includes(r.k)} class:off-row={r.a.disabled}>
                  <input type="checkbox" aria-label={`Select ${credName(r.a)}`} checked={sel.includes(r.k)} onchange={() => pick(r.k)} />
                  <span class="st"><span class="lamp {v.lamp}"></span><span class="state-label">{v.word}<small title={v.sub}>{v.sub}</small></span></span>
                  <button class="name" aria-expanded={isOpen} onclick={() => toggle(r.a)}
                    ><strong>{credName(r.a)}</strong><small>{[r.a.id_token?.plan_type, r.a.note].filter(Boolean).join(" · ") || r.a.name}</small></button
                  >
                  {@render bar("Session", v.session, v.stale)}
                  {@render bar("Weekly", v.weekly, v.stale)}
                  {#if r.b}<Grille data={r.b} {max} d={5} gap={2} />{:else}<span class="legend">Not reported</span>{/if}
                  <span class="last"><b>{v.last}</b><span>{v.age}</span></span>
                </div>
                {#if isOpen}
                  {@const a = r.a}
                  <div class="detail window">
                    <dl class="facts">
                      {#each facts(a) as [k, v, mono]}<div><dt>{k}</dt><dd class:mono>{v}</dd></div>{/each}
                    </dl>
                    {#each (limits(a) as { windows?: Window[] })?.windows || [] as w}<Meter {w} />{/each}
                    {#if limits(a)?.error}<p class="note error" role="status"><span class="lamp bad"></span>Usage read failed: {limits(a)?.error}</p>{/if}
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
                      <button class="key small" disabled={!canCheck(a) || !!wait(a, now)} onclick={() => (check(a), (now = Date.now()))}
                        >{store.pending[quotaKey(a)] || (wait(a, now) ? `Checked; again in ${span(wait(a, now))}` : "Check quota")}</button
                      >
                      {#each actions(a) as [name, method, path, body, done, ask, off]}<button
                          class="key small"
                          class:danger={method === "DELETE"}
                          class:quiet={method === "DELETE"}
                          disabled={store.busy || off || !store.can(method, path)}
                          onclick={() =>
                            store.call(method, path, body, done, ask).then((ok) => ok && method === "DELETE" && store.go("accounts"))}
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
            <li class="state"><p>No accounts match.</p></li>
          {/each}
        </ul>
        <p class="sr" role="status">{picked.length ? `${picked.length} selected.` : ""}</p>
        {#if picked.length}
          {@const off = toSwitch(false)}
          {@const on = toSwitch(true)}
          {@const files = reach.filter((g) => !g[0].runtime_only)}
          {@const gone = files.flat()}
          <div class="batch" role="region" aria-label="Selected accounts">
            <p>
              <b>{picked.length} selected.</b>
              {#if hidden.length}<span class="muted">{hidden.length === 1 ? "1 of them is" : `${hidden.length} of them are`} {where}.</span>{/if}
              {#if extra.length}<span class="muted">Actions also reach {names(extra)}, in the same files.</span>{/if}
            </p>
            {#if off.length}<button class="key small" disabled={store.busy || !store.can("PATCH", "/credentials/status")} onclick={() => setStatus(false)}
                >Enable {off.length}</button
              >{/if}
            {#if on.length}<button class="key small" disabled={store.busy || !store.can("PATCH", "/credentials/status")} onclick={() => setStatus(true)}
                >Disable {on.length}</button
              >{/if}
            <button class="key small" disabled={store.busy || !store.can("POST", "/credentials/refresh")}
              onclick={() =>
                // One refresh per file: accounts in one file share its token.
                batch(reach, names, (g) => api("/credentials/refresh", "POST", lookup(g.find((a) => sel.includes(key(a))) || g[0])), (n) => `Tokens refreshed for ${plural(n, "file")}`)}
              >Refresh tokens for {reach.flat().length}</button
            >
            {#if files.length}<button class="key small danger" disabled={store.busy || !store.can("DELETE", "/credentials")}
                onclick={() =>
                  batch(
                    files,
                    (g) => g[0].name,
                    async (g) => {
                      await api("/credentials", "DELETE", { names: [g[0].name] });
                      sel = sel.filter((k) => !g.some((a) => key(a) === k));
                    },
                    (n) => `${plural(n, "file")} deleted`,
                    `Delete ${plural(gone.length, "account")}? ${files.length === 1 ? "Its file is" : "Their files are"} removed from the server: ${files.map((g) => `${g[0].name} (${names(g)})`).join(", ")}`,
                  )}>Delete {gone.length}…</button
              >{/if}
            <button class="key small quiet" onclick={() => (sel = [])}><svg class="i" aria-hidden="true"><use href="#i-close" /></svg>Clear</button>
          </div>
        {/if}
      </section>
    {/if}
  {/snippet}
</Load>
