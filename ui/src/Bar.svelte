<script lang="ts">
  import type { Bar } from "./core";
  // One quota window: percent used, a thin bar and when it resets; unknown says why.
  let { name, w, stale = false }: { name: string; w: Bar; stale?: boolean } = $props();
</script>

{#if "used" in w}
  <div class="win" class:stale role="meter" aria-label={`${name} ${w.used}% used`} aria-valuenow={w.used} aria-valuemin={0} aria-valuemax={100}>
    <span><b>{w.used}%</b> {name.toLowerCase()}</span><span class="track"><i class:hi={w.used >= 85} style:width="{w.used}%"></i></span><span>{w.reset}</span>
  </div>
{:else}
  <div class="win"><span><b>unknown</b><span class="sr"> {name}</span></span><span class="track none"></span><span>{w.why}</span></div>
{/if}
