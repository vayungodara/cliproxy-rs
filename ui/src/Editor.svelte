<script lang="ts">
  import { untrack } from "svelte";
  import { api, configValue } from "./api";
  import { equal, lineDiff } from "./core";
  import { store } from "./store.svelte";
  // Edit one config value. Changes are previewed as a diff and the target is reread before
  // writing, so an edit made elsewhere is refused instead of overwritten.
  let {
    path,
    value,
    title,
    yaml = false,
    onclose,
  }: { path: string; value: any; title: string; yaml?: boolean; onclose: (saved: boolean) => void } =
    $props();
  const baseline = untrack(() => structuredClone($state.snapshot(value)));
  const before = untrack(() => (yaml ? String(baseline) : JSON.stringify(baseline, null, 2)));
  let text = $state(before);
  let review = $state(false),
    saving = $state(false),
    error = $state("");
  const after = $derived(text);
  const dirty = $derived(after !== before);
  const diff = $derived(review ? lineDiff(before, after) : []);
  $effect(() => {
    store.dirty = dirty;
  });

  function check() {
    try {
      if (!yaml) JSON.parse(after);
      review = true;
      error = "";
    } catch {
      error = "The JSON is not valid. Fix it before reviewing.";
    }
  }
  async function save() {
    saving = true;
    error = "";
    try {
      const latest = yaml ? await api(path, "GET", undefined, "text") : await configValue(path, Array.isArray(baseline) ? [] : {});
      if (!equal(latest, baseline))
        throw new Error("The server copy changed since you opened it. Nothing was written. Close, reopen and reapply your edit.");
      await api(path, "PUT", yaml ? after : JSON.parse(after));
      store.dirty = false;
      await store.config.load(true);
      store.notify("Saved.");
      onclose(true);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      saving = false;
    }
  }
  function close() {
    if (dirty && !confirm("Discard unsaved changes?")) return;
    store.dirty = false;
    onclose(false);
  }
</script>

<section class="section editor" aria-label={title}>
  <div class="section-head">
    <h2>{title}</h2>
    <button class="key quiet" onclick={close}>Close</button>
  </div>
  <p class="legend mono">{path}</p>
  {#if error}<p class="note error" role="alert"><span class="lamp bad"></span>{error}</p>{/if}
  {#if review}
    <div class="code-window diff" aria-label="Changes">
      {#each diff as line}<div class={line.kind}>
          <span>{line.kind === "added" ? "+" : line.kind === "removed" ? "−" : ""}</span>{line.text}
        </div>{/each}
    </div>
    <div class="row">
      <button class="key primary" disabled={saving || !dirty} onclick={save}>{saving ? "Saving…" : "Apply"}</button>
      <button class="key" onclick={() => (review = false)}>Keep editing</button>
      <span class="legend">{yaml ? "The server validates YAML and may normalise it." : ""}</span>
    </div>
  {:else}
    <label
        ><span class="sr">{yaml ? "YAML" : "JSON"}</span><textarea
          class="code"
          rows="22"
          spellcheck="false"
          bind:value={text}></textarea></label
    >
    <div class="row"><button class="key primary" disabled={!dirty} onclick={check}>Review changes</button></div>
  {/if}
</section>
