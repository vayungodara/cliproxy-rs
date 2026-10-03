<script lang="ts">
  import { tick } from "svelte";
  import { store, every } from "../store.svelte";
  import { buckets, sumBuckets, sum, credState, credName, provider, label, readPath, newKey, ago, strategy, flag, type Data, type Lamp } from "../core";
  import { clientModels } from "../api";
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

  // First run. The card opens when this server has no accounts and stays, in this browser, until
  // its three steps are done or it is dismissed. A server that already had accounts never shows it,
  // so an established setup looks as it always did.
  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  const served = $derived(rows.some((r) => r.a.success || r.a.failed || (r.b && sum(r.b.total))));
  let start = $state(flag.get("start"));
  let tested = $state(flag.get("start-test") === "1");
  let probe = $state<{ lamp: Lamp; text: string } | null>(null);
  const steps = $derived([
    {
      done: rows.length > 0,
      title: "Connect an account",
      text: "Sign in on the provider’s own page, or add a provider API key. The proxy keeps the token, never your password.",
      href: "#connect",
      action: "Connect account",
    },
    keys.length
      ? {
          done: served || flag.get("start-tool") === "1",
          title: "Point a tool at the proxy",
          text: "Copy the settings for Claude Code, Codex CLI, Cursor or an SDK, with this address and your key filled in.",
          href: "#use",
          action: "Use with tools",
        }
      : {
          done: false,
          title: "Point a tool at the proxy",
          text: "Tools send a client key, a password for this proxy. Without one, anyone who can reach this address can use your accounts.",
          run: createKey,
          action: "Create a client key",
        },
    {
      done: served || tested,
      title: "Send a test request",
      text: "Asks this proxy for its models with your client key, as a tool would. It uses none of your plan’s limits.",
      run: test,
      action: "Send test request",
      off: !keys.length,
    },
  ]);
  const left = $derived(steps.filter((s) => !s.done).length);
  const next = $derived(steps.findIndex((s) => !s.done));
  $effect(() => {
    if (start || !store.creds.data || store.creds.data.length) return;
    start = "open";
    flag.set("start", start);
  });
  $effect(() => {
    if (start !== "open" || left || !store.creds.data) return;
    start = "done";
    flag.set("start", start);
  });
  const setup = $derived(start === "open" && store.config.data && store.creds.data && left > 0);
  function dismiss() {
    start = "dismissed";
    flag.set("start", start);
    document.getElementById("main")?.focus();
  }
  async function createKey() {
    if (!(await store.act(() => store.replace("access/api-keys", keys, [...keys, newKey()]), "Client key created."))) return;
    // The button is gone now; continue at the next step unless the user already moved on.
    await tick();
    if (document.activeElement === document.body) document.querySelector<HTMLElement>(".start .key.primary")?.focus();
  }
  async function test() {
    probe = { lamp: "off", text: "Sending…" };
    try {
      const ids = await clientModels(keys[0]);
      if (!ids.length) {
        probe = { lamp: "warn", text: "The proxy answered, but no models are available yet. Connect an account first." };
        return;
      }
      probe = { lamp: "ok", text: `It works: this proxy serves ${ids.length} model${ids.length > 1 ? "s" : ""} to your tools.` };
      tested = true;
      flag.set("start-test", "1");
      if (!steps.some((s) => !s.done)) store.notify("All set. Your tools can use this proxy now.");
    } catch (e) {
      probe = { lamp: "bad", text: e instanceof Error ? e.message : "Cannot reach the proxy." };
    }
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
  <section class="window start" aria-labelledby="start">
    <div class="section-head">
      <h2 id="start">Get started<small>{3 - left} of 3 done</small></h2>
      <button class="key quiet small" onclick={dismiss}>Dismiss</button>
    </div>
    <ol class="list">
      {#each steps as s, i}
        <li class:done={s.done}>
          {#if s.done}<svg class="i mark" aria-hidden="true"><use href="#i-check" /></svg>{:else}<span class="lamp off"></span>{/if}
          <span class="grow"
            ><strong>{s.title}</strong>{#if s.done}<span class="sr">, done</span>{/if}
            {#if i === 2 && probe}<span class="note" role="status"
                >{#if !s.done}<span class="lamp {probe.lamp}" class:live={probe.text.endsWith("…")}></span>{/if}{probe.text}</span
              >{:else if !s.done}<small>{s.text}</small>{/if}</span
          >
          {#if !s.done}
            {#if s.href}<a class="key small {i === next ? 'primary' : ''}" href={s.href}>{s.action}</a
              >{:else}<button class="key small {i === next ? 'primary' : ''}" disabled={store.busy || s.off} onclick={s.run}>{s.action}</button>{/if}
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
