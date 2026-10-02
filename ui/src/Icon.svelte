<script lang="ts">
  let { name, size = 20 }: { name: string; size?: number } = $props();
  const paths: Record<string, string> = {
    overview: "M3 3h7v7H3z M14 3h7v7h-7z M3 14h7v7H3z M14 14h7v7h-7z",
    credentials: "M12 3 3 7v5c0 5 9 9 9 9s9-4 9-9V7z M8 12l3 3 5-6",
    oauth:
      "M14 3h7v7 M21 3l-9 9 M10 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-5",
    providers:
      "M12 4v6 M5 20v-5h14v5 M12 10v5 M8 2h8v4H8z M2 20h6v2H2z M16 20h6v2h-6z",
    keys: "M8 14a5 5 0 1 1 4-4l9 0v4h-3v3h-3v-3z",
    models: "M12 3 2 8l10 5 10-5z M2 12l10 5 10-5 M2 16l10 5 10-5",
    payload: "M8 3 3 8l5 5 M16 11l5 5-5 5 M13 3l-2 18",
    quotas: "M3 20h18 M5 16V9 M12 16V3 M19 16v-5",
    configuration: "M3 6h18 M3 12h18 M3 18h18 M8 3v6 M16 9v6 M8 15v6",
    logs: "M4 3h16v18H4z M8 7h8 M8 11h8 M8 15h5",
    plugins: "M8 3h8v5h5v8h-5v5H8v-5H3V8h5z",
    system: "M3 4h18v12H3z M8 21h8 M12 16v5",
    refresh: "M20 8a8 8 0 1 0 0 8 M20 3v5h-5",
    sun: "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8 M12 2v2 M12 20v2 M2 12h2 M20 12h2 M5 5l1 1 M18 18l1 1 M5 19l1-1 M18 6l1-1",
    moon: "M20 14a8 8 0 0 1-10-10 9 9 0 1 0 10 10",
    arrow: "M4 12h16 M14 6l6 6-6 6",
    terminal: "M5 6l6 6-6 6 M13 18h6",
    logout: "M10 3H4v18h6 M10 12h11 M16 7l5 5-5 5",
    plus: "M12 4v16 M4 12h16",
    close: "M6 6l12 12 M6 18 18 6",
    search: "M10 3a7 7 0 1 0 0 14 7 7 0 0 0 0-14 M15 15l6 6",
  };
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width="1.6"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"><path d={paths[name] || paths.system} /></svg
>
