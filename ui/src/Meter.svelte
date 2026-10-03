<script lang="ts">
  import { span, type Window } from "./core";

  let { w }: { w: Window } = $props();
  const resets = $derived(w.reset && Date.parse(w.reset) > Date.now() ? `resets in ${span(Date.parse(w.reset) - Date.now())}` : "");
</script>

<div class="quota">
  <span class="legend grow">{w.label}</span>
  {#if w.used === null}
    <span class="legend detail">{w.detail}</span>
  {:else}
    <span role="meter" aria-valuenow={w.used} aria-valuemin={0} aria-valuemax={100} aria-label={`${w.label} used`}>
      <svg class="grille" width="216" height="7" aria-hidden="true"
        >{#each { length: 20 } as _, i}<circle cx={i * 11 + 3.5} cy="3.5" r="3.5" class={i < Math.round(w.used / 5) ? (w.used >= 90 ? "f" : "l4") : ""} />{/each}</svg
      >
    </span>
    <span class="num count">{w.used}%</span>
    <span class="legend reset">{w.detail || resets}</span>
  {/if}
</div>
