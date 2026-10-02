<script lang="ts">
  import { store, every } from "../store.svelte";
  import { buckets, sumBuckets, sum, credState, credName, provider, label, readPath, type Data } from "../core";
  import Grille from "../Grille.svelte";
  import Load from "../Load.svelte";

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
</script>

<div class="head">
  <h1>Overview</h1>
  <a class="key primary" href="#connect"><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect account</a>
</div>

<Load res={store.creds} what="Credentials">
  {#snippet children(list)}
    {#if !list.length}
      <section class="window first">
        <h2>No credentials connected</h2>
        <p class="muted">
          Sign in with a provider, upload a credential file, or add an API key under Providers.
        </p>
        <div class="row">
          <a class="key primary" href="#connect"><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Connect account</a>
          <a class="key" href="#credentials"><svg class="i" aria-hidden="true"><use href="#i-upload" /></svg>Upload a file</a>
          <a class="key quiet" href="#providers">Provider API keys</a>
        </div>
      </section>
    {:else}
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
            >{!traffic ? "Not reported" : total ? `${fmt(failed)} failed` : "No requests yet"}</span
          >
        </div>
        <p class="routing legend">
          Routing <b>{routing.strategy || "server default"}</b>
          {#if routing.retry?.["request-retry"] !== undefined}· {routing.retry["request-retry"]} retries{/if}
          {#if routing["session-affinity"]}· session affinity{/if}
        </p>
      </section>

      <section class="section">
        <div class="section-head">
          <h2>Credentials</h2>
          <a class="key quiet small" href="#credentials">Manage <svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-chevron" /></svg></a>
        </div>
        <ul class="list creds">
          {#each groups as g (g.p)}
            <li class="group">{label(g.p)}<span>{g.rows.length}</span></li>
            {#each g.rows as r (`${r.a.name}\u0000${r.a.auth_index}`)}
              <li>
                <a class="item" href={`#credentials/${encodeURIComponent(r.a.name)}`}>
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
    {/if}
  {/snippet}
</Load>
