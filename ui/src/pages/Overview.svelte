<script lang="ts">
  import { tick } from "svelte";
  import { store, every } from "../store.svelte";
  import { buckets, sumBuckets, sum, credState, credName, provider, label, readPath, newKey, ago, strategy, type Data } from "../core";
  import { checkAll, canCheck, limits } from "../quota";
  import Grille from "../Grille.svelte";
  import Load from "../Load.svelte";
  import Meter from "../Meter.svelte";

  $effect(() => every(10_000, () => store.creds.load(true)));
  const rows = $derived(
    (store.creds.data || []).map((a) => ({ a, b: buckets(a), s: credState(a) })),
  );
  const traffic = $derived(sumBuckets(rows.map((r) => r.b)));
  const max = $derived(Math.max(1, ...rows.flatMap((r) => r.b?.total || [])));
  const total = $derived(traffic ? sum(traffic.total) : 0);
  const failed = $derived(traffic ? sum(traffic.failed) : 0);
  const count = (lamp: string) => rows.filter((r) => r.s.lamp === lamp).length;
  const groups = $derived(
    [...new Set(rows.map((r) => provider(r.a)))].map((p) => ({
      p,
      rows: rows.filter((r) => provider(r.a) === p),
    })),
  );
  const routing = $derived(readPath(store.config.data || {}, "routing", {}) as Data);
  const fmt = (n: number) => n.toLocaleString();
  if (!store.plugins.data) store.plugins.load();

  // First run: the three things a new proxy needs before a tool can use it. Gone once done.
  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  const steps = $derived([
    {
      done: rows.length > 0,
      title: "Connect an account",
      text: "Sign in to Claude, ChatGPT, Kimi and others on the provider’s own page, or add a provider API key. The proxy keeps the token, never your password.",
      href: "#connect",
      action: "Connect account",
    },
    {
      done: keys.length > 0,
      title: "Create a client key",
      text: "A password your tools send to this proxy. Without one, anyone who can reach this address can use your accounts.",
      action: "Create a client key",
    },
    {
      done: rows.some((r) => r.a.success || r.a.failed || (r.b && sum(r.b.total))),
      title: "Point a tool at the proxy",
      text: "Copy ready-made settings for Claude Code, Codex CLI, Cursor or an SDK, with this address and your key filled in.",
      href: "#use",
      action: "Use with tools",
    },
  ]);
  // Shown only while an account or a client key is missing; with both in place the overview
  // looks as it always did, and the third step lives in the "No requests yet" line.
  const setup = $derived(store.config.data && store.creds.data && !(steps[0].done && steps[1].done));
  const next = $derived(steps.findIndex((s) => !s.done));
  async function createKey() {
    if (!(await store.act(() => store.replace("access/api-keys", keys, [...keys, newKey()]), "Client key created."))) return;
    // The button is gone now; continue at the next step unless the user already moved on.
    await tick();
    if (document.activeElement !== document.body) return;
    const next = [...document.querySelectorAll<HTMLElement>(".checklist .key.primary, .reading .hint a")].find((e) => e.offsetParent);
    (next ?? document.getElementById("main"))?.focus();
  }

  const limited = $derived(rows.map((r) => ({ a: r.a, l: limits(r.a) })).filter((x) => x.l));
  const checkable = $derived(rows.some((r) => canCheck(r.a)));
  let checking = $state(false);
  async function checkLimits() {
    checking = true;
    await checkAll(store.creds.data || []);
    checking = false;
  }
</script>

<div class="head">
  <h1>Overview</h1>
  <a class="key primary" href="#connect"><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect account</a>
</div>

{#if setup}
  <section class="window first" aria-labelledby="start">
    <h2 id="start">Get started<small>{steps.filter((s) => s.done).length} of 3 done</small></h2>
    <ol class="steps checklist">
      {#each steps as s, i}
        <li class:done={s.done}>
          <span class="row"
            ><span class="lamp {s.done ? 'ok' : 'off'}"></span><strong>{s.title}</strong>{#if s.done}<span class="sr">, done</span>{/if}</span
          >
          {#if !s.done}
            <p class="muted">{s.text}</p>
            {#if s.href}<a class="key {i === next ? 'primary' : ''}" href={s.href}>{s.action}</a
              >{:else}<button class="key {i === next ? 'primary' : ''}" disabled={store.busy} onclick={createKey}>{s.action}</button>{/if}
          {/if}
        </li>
      {/each}
    </ol>
  </section>
{/if}

<Load res={store.creds} what="Credentials">
  {#snippet children(list)}
    {#if list.length}
      <section class="window display" aria-label="Last 200 minutes">
        <div class="reading traffic">
          <span class="legend">Requests · last 200 min</span>
          <strong>{traffic ? fmt(total) : "—"}</strong>
          {#if traffic}
            <Grille data={traffic} max={Math.max(1, ...traffic.total)} d={10} gap={6} stack={5} />
            <span class="axis legend" aria-hidden="true"><span>−200 min</span><span>now</span></span>
          {:else}<span class="legend">Not reported by this server</span>{/if}
        </div>
        <div class="reading">
          <span class="legend">Ready</span>
          <strong>{count("ok")}<small>&nbsp;of {list.length}</small></strong>
          <span class="legend"
            >{[
              count("warn") && `${count("warn")} cooling`,
              count("bad") && `${count("bad")} failing`,
              count("off") && `${count("off")} disabled`,
            ]
              .filter(Boolean)
              .join(" · ") || "All credentials ready"}</span
          >
        </div>
        <div class="reading">
          <span class="legend">Success</span>
          <strong
            >{traffic && total ? `${(((total - failed) / total) * 100).toFixed(total - failed === total ? 0 : 1)}` : "—"}<small
              >{traffic && total ? "%" : ""}</small
            ></strong
          >
          <span class="legend"
            >{!traffic ? "Not reported" : total ? `${fmt(failed)} failed` : "No requests yet"}{#if traffic && !total}<span class="hint">{" · "}<a href="#use">Set up a tool</a></span>{/if}</span
          >
        </div>
        <p class="routing legend">
          Routing <b>{strategy(routing.strategy).name.toLowerCase()}</b>
          {#if routing.retry?.["request-retry"] !== undefined}· {routing.retry["request-retry"]} retries{/if}
          {#if routing["session-affinity"]}· session affinity{/if}
        </p>
      </section>

      <section class="section">
        <div class="section-head">
          <h2>Credentials</h2>
          {#if checkable}<button class="key quiet small" disabled={checking} onclick={checkLimits}
              >{checking ? "Checking limits…" : "Check limits"}</button
            >{/if}
          <a class="key quiet small" href="#credentials">Manage <svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-chevron" /></svg></a>
        </div>
        <ul class="list creds">
          {#each groups as g (g.p)}
            <li class="group">{label(g.p)}<span>{g.rows.length}</span></li>
            {#each g.rows as r (`${r.a.name}\u0000${r.a.auth_index}`)}
              <li>
                <a class="item" href={`#credentials/${encodeURIComponent(r.a.id || r.a.name)}`}>
                  <span class="lamp {r.s.lamp}"></span>
                  <span class="name grow"
                    ><strong>{credName(r.a)}</strong><small>{r.a.note || r.a.name}</small></span
                  >
                  {#if r.b}<Grille data={r.b} {max} />{:else}<span class="legend">No history reported</span>{/if}
                  <span class="num count">{r.b ? fmt(sum(r.b.total)) : "—"}</span>
                  <span class="state-label">{r.s.label}</span>
                </a>
              </li>
            {/each}
          {/each}
          <li>
            <a class="item add" href="#connect"><span class="lamp off"></span>Connect another account</a>
          </li>
        </ul>
      </section>

      {#if limited.length}
        <section class="section">
          <div class="section-head"><h2>Limits</h2></div>
            <ul class="list limits">
              {#each limited as x (`${x.a.name}\u0000${x.a.auth_index}`)}
                <li class="item">
                  <span class="name"><strong>{credName(x.a)}</strong><small>{label(provider(x.a))}{x.l && "at" in x.l && x.l.at ? ` · ${ago(x.l.at)}` : ""}</small></span>
                  {#if x.l && "windows" in x.l}
                    <div class="windows">{#each x.l.windows as w}<Meter {w} />{:else}<span class="legend">No usage windows reported.</span>{/each}</div>
                  {:else if x.l}<p class="note error grow"><span class="lamp bad"></span>{x.l.error}</p>{/if}
                </li>
              {/each}
            </ul>
        </section>
      {/if}
    {/if}
  {/snippet}
</Load>
