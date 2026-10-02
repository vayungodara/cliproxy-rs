<script lang="ts">
  import Tree from "./Tree.svelte";
  let {
    value,
    onchange,
    path = "",
  }: { value: any; onchange: (value: any) => void; path?: string } = $props();
  function update(key: string, next: any) {
    onchange({ ...value, [key]: next });
  }
</script>

{#if value && typeof value === "object" && !Array.isArray(value)}
  {#each Object.entries(value) as [key, val] (key)}
    {#if val && typeof val === "object" && !Array.isArray(val)}
      <details class="tree-group" open={path === ""}>
        <summary>{key}<span>{Object.keys(val).length} fields</span></summary>
        <div class="tree-children">
          <Tree
            value={val}
            path={`${path}/${key}`}
            onchange={(next) => update(key, next)}
          />
        </div>
      </details>
    {:else}
      <label class="tree-field"
        ><span>{key}</span>
        {#if typeof val === "boolean"}<input
            type="checkbox"
            checked={val}
            onchange={(e) => update(key, e.currentTarget.checked)}
          />
        {:else if typeof val === "number"}<input
            type="number"
            value={val}
            onchange={(e) => {
              if (e.currentTarget.value !== "")
                update(key, Number(e.currentTarget.value));
            }}
          />
        {:else if Array.isArray(val)}<textarea
            class="code compact"
            rows="4"
            value={JSON.stringify(val, null, 2)}
            onchange={(e) => {
              try {
                update(key, JSON.parse(e.currentTarget.value));
                e.currentTarget.setCustomValidity("");
              } catch {
                e.currentTarget.setCustomValidity("Enter valid JSON.");
                e.currentTarget.reportValidity();
              }
            }}
            aria-label={`${key} JSON list`}></textarea>
        {:else}<input
            type={/secret|api-key|token|password|credential/.test(key)
              ? "password"
              : "text"}
            value={val ?? ""}
            onchange={(e) => update(key, e.currentTarget.value)}
          />{/if}
      </label>
    {/if}
  {/each}
  {#if Object.keys(value).length === 0}<p class="muted">
      No persisted fields. Use JSON mode to add settings.
    </p>{/if}
{/if}
