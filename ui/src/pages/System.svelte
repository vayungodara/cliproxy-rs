<script lang="ts">
  import { store, Res } from "../store.svelte";
  import { api, endpoint } from "../api";
  import { readPath } from "../core";
  import Load from "../Load.svelte";

  const latest = new Res<string>(async () => String((await api("/server/latest-version"))["latest-version"] || ""));
  const c = $derived(store.config.data || {});
  const facts = $derived(
    [
      ["Management API", new URL(endpoint()).pathname],
      [
        "Server",
        store.kind === "go"
          ? `CLIProxyAPI ${store.meta.version} (Go)`
          : store.kind === "rust"
            ? store.meta.version.replace("/", " ")
            : store.meta.version || "Not reported",
      ],
      ["Commit", store.meta.commit !== "none" && store.meta.commit],
      ["Built", !isNaN(Date.parse(store.meta.built)) && new Date(store.meta.built).toLocaleString()],
      ["Config layout", c["config-version"] ? `v${c["config-version"]}` : "Legacy (migrated on save)"],
      ["Routing strategy", readPath(c, "routing/strategy", "Server default")],
      ["Auth directory", readPath(c, "oauth/auth-dir", "Server default")],
      // The config cannot show the effective policy: MANAGEMENT_PASSWORD allows remote access too.
      ["Remote management", readPath(c, "management/allow-remote", false) ? "Allowed" : "Local only, unless MANAGEMENT_PASSWORD is set"],
      // Where this page's code comes from: it runs with the management key.
      [
        "Dashboard",
        store.kind === "rust"
          ? "Built into the server"
          : readPath(c, "management/disable-auto-update-panel", false)
            ? "Local file, auto-update off"
            : "Auto-updated from GitHub every 3 h",
      ],
    ].filter((f) => f[1]),
  );
  const clean = (v: string) => v.replace(/^v/, "");
</script>

<div class="head"><h1>System</h1></div>

<section class="section">
  <dl class="facts">
    {#each facts as [k, v]}<div><dt>{k}</dt><dd class:mono={k === "Management API" || k === "Commit"}>{v}</dd></div>{/each}
  </dl>
  <p class="note">
    The management API does not expose process uptime, CPU or memory, so they are not shown here.
  </p>
</section>

<section class="section">
  <div class="section-head">
    <h2>Updates</h2>
    <button class="key" disabled={latest.loading} onclick={() => latest.load()}>Check for a new release</button>
  </div>
  {#if latest.data !== undefined || latest.error || latest.loading}
    <Load res={latest} what="Release information">
      {#snippet children(v)}
        <p class="row">
          <span class="lamp {clean(v) === clean(store.meta.version) ? 'ok' : 'warn'}"></span>
          Latest {store.kind === "rust" ? "cliproxy-rs" : "CLIProxyAPI"} release: <strong>{v || "unknown"}</strong>{clean(v) === clean(store.meta.version) ? " · this server is up to date" : ""}
        </p>
      {/snippet}
    </Load>
  {:else}<p class="muted">The server asks GitHub on request; nothing is checked automatically.</p>{/if}
</section>
