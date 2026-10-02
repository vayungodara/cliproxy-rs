<script lang="ts">
  import { store } from "./store.svelte";
  // One honest line naming the actions this server does not implement.
  // Each path is the probe request: see store.probe for why it is side-effect free.
  let { actions }: { actions: [method: string, path: string, name: string][] } = $props();
  $effect(() => store.probe(actions));
  const off = $derived(actions.filter(([m, p]) => !store.can(m, p)).map((a) => a[2]));
</script>

{#if off.length}<p class="note">
    <span class="lamp off"></span>Not available on this server: {off.join(", ")}.
  </p>{/if}
