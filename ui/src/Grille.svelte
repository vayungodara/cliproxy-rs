<script lang="ts">
  import { level, sum, type Buckets } from "./core";
  // A perforated grille of ten-minute buckets, oldest on the left.
  // Single row: hole darkness is the bucket's share of `max`.
  // Stacked: each column lights holes bottom-up, like a level meter.
  let {
    data,
    max,
    d = 7,
    gap = 4,
    stack = 1,
  }: { data: Buckets; max: number; d?: number; gap?: number; stack?: number } = $props();
  const step = $derived(d + gap);
  const r = $derived(d / 2);
  const total = $derived(sum(data.total));
  const failed = $derived(sum(data.failed));
</script>

<svg
  class="grille"
  width={data.total.length * step - gap}
  height={stack * step - gap}
  role="img"
  aria-label={`${total.toLocaleString()} requests in ${data.total.length * 10} minutes${failed ? `, ${failed} failed` : ""}`}
  >{#each data.total as n, i}{#if stack === 1}<circle
        cx={i * step + r}
        cy={r}
        {r}
        class={data.failed[i] ? "f" : `l${level(n, max)}`}
      />{:else}{@const lit = n > 0 ? Math.max(1, Math.round((n / max) * stack)) : 0}{#each { length: stack } as _, j}<circle
          cx={i * step + r}
          cy={j * step + r}
          {r}
          class={stack - j > lit ? "" : stack - j === lit && data.failed[i] ? "f" : "l4"}
        />{/each}{/if}{/each}</svg
>
