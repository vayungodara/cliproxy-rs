<script lang="ts">
  import { untrack } from "svelte";
  import { api, configValue } from "./api";
  import { equal, lineDiff } from "./core";
  import Tree from "./Tree.svelte";
  let {
    path,
    value,
    title = "Edit configuration",
    yaml = false,
    onsaved,
    oncancel,
  }: {
    path: string;
    value: any;
    title?: string;
    yaml?: boolean;
    onsaved: () => void;
    oncancel: () => void;
  } = $props();
  // The parent keys the editor by path. Snapshot once; edits must not move the baseline.
  const baseline = untrack(() => structuredClone($state.snapshot(value)));
  const isYaml = untrack(() => yaml);
  let draft = $state(structuredClone(baseline));
  let text = $state(
    isYaml ? String(baseline) : JSON.stringify(baseline, null, 2),
  );
  let mode = $state(
    isYaml ? "yaml" : Array.isArray(baseline) ? "json" : "visual",
  );
  let preview = $state(false),
    busy = $state(false),
    error = $state("");
  let currentText = $derived(
    mode === "visual" ? JSON.stringify(draft, null, 2) : text,
  );
  let beforeText = $derived(
    yaml ? String(baseline) : JSON.stringify(baseline, null, 2),
  );
  let diff = $derived(lineDiff(beforeText, currentText));
  let dirty = $derived(currentText !== beforeText);
  function switchMode(next: string) {
    try {
      if (mode !== "visual") draft = JSON.parse(text);
      else text = JSON.stringify(draft, null, 2);
      mode = next;
      error = "";
      preview = false;
    } catch {
      error = "Enter valid JSON before switching to visual mode.";
    }
  }
  function review() {
    try {
      if (!yaml) JSON.parse(currentText);
      preview = true;
      error = "";
    } catch {
      error = "Invalid JSON. Fix the syntax before reviewing.";
    }
  }
  async function save() {
    busy = true;
    error = "";
    try {
      const latest = yaml
        ? await api(path, "GET", undefined, "text")
        : await configValue(path, Array.isArray(baseline) ? [] : {});
      if (!equal(latest, baseline))
        throw new Error(
          "Configuration changed on the server. Close this editor, refresh, and reapply your changes. Nothing was written.",
        );
      await api(path, "PUT", yaml ? currentText : JSON.parse(currentText));
      onsaved();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
  function close() {
    if (!dirty || confirm("Discard your unsaved changes?")) oncancel();
  }
  $effect(() => {
    const handler = (e: BeforeUnloadEvent) => {
      if (dirty) e.preventDefault();
    };
    window.addEventListener("beforeunload", handler);
    return () => window.removeEventListener("beforeunload", handler);
  });
</script>

<section class="editor panel">
  <div class="section-head">
    <div>
      <h2>{title}</h2>
      <p class="muted">{path} · Changes require review before saving.</p>
    </div>
    <button onclick={close}>Close editor</button>
  </div>
  {#if !yaml && !Array.isArray(baseline)}<div class="tabs">
      <button
        class:active={mode === "visual"}
        onclick={() => switchMode("visual")}>Visual</button
      ><button class:active={mode === "json"} onclick={() => switchMode("json")}
        >JSON</button
      >
    </div>{/if}
  {#if error}<div class="notice danger" role="alert">{error}</div>{/if}
  {#if preview}
    <div class="section-head">
      <h3>Change preview</h3>
      <span class="muted">Removed / added · values are visible</span>
    </div>
    <div class="diff code" aria-label="Configuration diff">
      {#each diff as line}<div class={line.kind}>
          <span
            >{line.kind === "added"
              ? "+"
              : line.kind === "removed"
                ? "−"
                : " "}</span
          >{line.text || " "}
        </div>{/each}
    </div>
    <div class="editor-actions">
      <button onclick={() => (preview = false)}>Back to editing</button><button
        class="primary"
        disabled={busy || !dirty}
        onclick={save}>{busy ? "Saving…" : "Apply changes"}</button
      >
    </div>
  {:else}
    {#if mode === "visual"}<form
        onsubmit={(e) => {
          e.preventDefault();
          review();
        }}
      >
        <Tree value={draft} onchange={(next) => (draft = next)} />
        <div class="editor-actions">
          <button type="submit" class="primary" disabled={!dirty}
            >Review changes</button
          >
        </div>
      </form>
    {:else}<label class="editor-label"
        >{yaml ? "YAML configuration" : "JSON value"}<textarea
          class="code editor-area"
          spellcheck="false"
          bind:value={text}
          oninput={() => (preview = false)}></textarea></label
      >
      <div class="editor-actions">
        <span class="muted"
          >{yaml
            ? "YAML validation happens on the server. Rejected writes preserve the file."
            : "Use v8 field names. Null values inherit group settings."}</span
        ><button class="primary" disabled={!dirty} onclick={review}
          >Review changes</button
        >
      </div>{/if}
  {/if}
</section>
