<script lang="ts" generics="T">
  import type { Snippet } from "svelte";
  import { ApiError } from "./api";
  import type { Res } from "./store.svelte";
  // Honest states for one server read: loading, not available, error, then the data.
  let {
    res,
    what,
    children,
  }: { res: Res<T>; what: string; children: Snippet<[T]> } = $props();
  const error = $derived(res.error as ApiError | Error | null);
  const missing = $derived(error instanceof ApiError && error.missing);
</script>

{#if res.data !== undefined && !missing}
  {#if error}<p class="note error" role="alert">
      <span class="lamp bad"></span>Showing the last good read. {error.message}
      <button class="key small quiet" onclick={() => res.load()}>Retry</button>
    </p>{/if}
  {@render children(res.data)}
{:else if missing && error instanceof ApiError}
  <div class="state">
    <div class="row"><span class="lamp off"></span>Not available on this server</div>
    <p>
      <code>{error.method} {error.path}</code> answered {error.status}. It is part of the v8 API, but this
      server does not implement it yet.
    </p>
  </div>
{:else if error}
  <div class="state" role="alert">
    <div class="row"><span class="lamp bad"></span>Could not load {what.toLowerCase()}</div>
    <p>{error.message}</p>
    <div class="actions">
      <button class="key small" onclick={() => res.load()}>Try again</button>
    </div>
  </div>
{:else}
  <div aria-busy="true" aria-label={`Loading ${what.toLowerCase()}`}>
    <div class="skel"></div>
  </div>
{/if}
