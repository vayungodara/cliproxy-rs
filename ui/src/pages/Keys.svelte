<script lang="ts">
  import { store } from "../store.svelte";
  import { readPath, mask } from "../core";
  import Load from "../Load.svelte";

  let draft = $state(""),
    reveal = $state(false);
  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  const generate = () =>
    (draft = `sk-${Array.from(crypto.getRandomValues(new Uint8Array(24)), (n) => n.toString(16).padStart(2, "0")).join("")}`);
  function add(e: SubmitEvent) {
    e.preventDefault();
    const k = draft.trim();
    if (keys.includes(k)) return store.notify("That key is already in the list.", true);
    store
      .act(() => store.replace("access/api-keys", keys, [...keys, k]), "Key added. Copy it now; it is masked from here on.")
      .then((ok) => ok && (draft = ""));
  }
</script>

<div class="head">
  <h1>Client keys{#if keys.length}<span>{keys.length}</span>{/if}</h1>
  {#if keys.length}<button class="key quiet" aria-pressed={reveal} onclick={() => (reveal = !reveal)}
      >{reveal ? "Hide keys" : "Show keys"}</button
    >{/if}
</div>

<Load res={store.config} what="Client keys">
  {#snippet children()}
    <section class="section">
      <p class="muted">Clients send one of these as a Bearer token or <code>x-api-key</code>. They are not your management key.</p>
      {#if keys.length}
        <ul class="list">
          {#each keys as k, i (k)}
            <li class="item">
              <code class="grow ellipsis">{reveal ? k : mask(k)}</code>
              <button
                class="key small"
                onclick={() => store.act(() => navigator.clipboard.writeText(k), "Copied.")}
                ><svg class="i" width="14" height="14" aria-hidden="true"><use href="#i-copy" /></svg>Copy</button
              >
              <button
                class="key small quiet danger"
                disabled={store.busy}
                onclick={() =>
                  confirm("Remove this key? Clients using it lose access immediately.") &&
                  store.act(() => store.replace("access/api-keys", keys, keys.filter((_, n) => n !== i)), "Key removed.")}
                >Remove</button
              >
            </li>
          {/each}
        </ul>
      {:else}
        <div class="state">
          <div class="row"><span class="lamp warn"></span>No client keys</div>
          <p>Without client keys the proxy may accept requests from anyone who can reach it. Add one before exposing it beyond this machine.</p>
        </div>
      {/if}
      <form class="form" onsubmit={add}>
        <label class="field">New key<input type="password" autocomplete="off" required bind:value={draft} /></label>
        <button type="button" class="key" onclick={generate}>Generate</button>
        <button class="key primary" disabled={store.busy}><svg class="i" aria-hidden="true"><use href="#i-plus" /></svg>Add key</button>
      </form>
    </section>
  {/snippet}
</Load>
