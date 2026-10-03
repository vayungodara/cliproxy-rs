<script lang="ts">
  import { store } from "../store.svelte";
  import { readPath, mask, newKey } from "../core";
  import Load from "../Load.svelte";

  let draft = $state(""),
    reveal = $state(false);
  const keys = $derived(readPath(store.config.data || {}, "access/api-keys", []) as string[]);
  const generate = () => (draft = newKey());
  function add(e: SubmitEvent) {
    e.preventDefault();
    const k = draft.trim();
    if (keys.includes(k)) return store.notify("That key is already in the list.", true);
    store
      .act(() => store.replace("access/api-keys", keys, [...keys, k]), "Key added.")
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
      <p class="muted">
        Client keys are passwords your tools send to this proxy, as a Bearer token or <code>x-api-key</code>. They are not your
        management key.
      </p>
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
          <p>Without one, anyone who can reach the proxy may be able to use it.</p>
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
