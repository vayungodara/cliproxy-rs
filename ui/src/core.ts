export type Data = Record<string, any>;
export type Event = {
  timestamp: string;
  latency_ms: number;
  failed: boolean;
  model: string;
  provider: string;
  auth_index: string;
  request_id: string;
  tokens: { total_tokens: number };
};

export function serverBase(input: string): string {
  const url = new URL(input || location.origin);
  if (
    !["http:", "https:"].includes(url.protocol) ||
    url.username ||
    url.password ||
    url.search ||
    url.hash
  ) {
    throw new Error(
      "Use an HTTP(S) server URL without credentials, query parameters, or a fragment.",
    );
  }
  return url.href.replace(/\/$/, "").replace(/\/v8\/management$/, "");
}

export function fieldPath(path: string): string {
  return (
    "/config/" +
    path
      .split("/")
      .map((part) => {
        if (
          !part ||
          part === "." ||
          part === ".." ||
          /[\\\x00-\x1f\x7f]/.test(part)
        )
          throw new Error("Invalid configuration path.");
        return encodeURIComponent(part);
      })
      .join("/")
  );
}

export function readPath(
  value: Data,
  path: string,
  fallback: any = undefined,
): any {
  return path.split("/").reduce((v, k) => v?.[k], value) ?? fallback;
}

export function equal(a: any, b: any): boolean {
  if (a === b) return true;
  if (!a || !b || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const keys = Object.keys(a);
  return (
    keys.length === Object.keys(b).length &&
    keys.every((k) => Object.hasOwn(b, k) && equal(a[k], b[k]))
  );
}

export function lineDiff(
  before: string,
  after: string,
): { kind: string; text: string }[] {
  const a = before.split("\n"),
    b = after.split("\n");
  let start = 0,
    end = 0;
  while (start < Math.min(a.length, b.length) && a[start] === b[start]) start++;
  while (
    end < Math.min(a.length, b.length) - start &&
    a[a.length - 1 - end] === b[b.length - 1 - end]
  )
    end++;
  return [
    ...a.slice(0, start).map((text) => ({ kind: "same", text })),
    ...a
      .slice(start, a.length - end)
      .map((text) => ({ kind: "removed", text })),
    ...b.slice(start, b.length - end).map((text) => ({ kind: "added", text })),
    ...a.slice(a.length - end).map((text) => ({ kind: "same", text })),
  ];
}

// Keep only display fields. Usage records can also contain client keys and response bodies.
export function cleanEvent(raw: Data): Event {
  return {
    timestamp: String(raw.timestamp || new Date().toISOString()),
    latency_ms: Number(raw.latency_ms) || 0,
    failed: raw.failed === true,
    model: String(raw.model || "unknown"),
    provider: String(raw.provider || "unknown"),
    auth_index: String(raw.auth_index || ""),
    request_id: String(raw.request_id || ""),
    tokens: { total_tokens: Number(raw.tokens?.total_tokens) || 0 },
  };
}

export function metrics(events: Event[], now = Date.now()) {
  const recent = events.filter(
    (e) =>
      now - Date.parse(e.timestamp) < 15 * 60_000 &&
      Date.parse(e.timestamp) <= now,
  );
  const minute = recent.filter((e) => now - Date.parse(e.timestamp) < 60_000);
  const latencies = recent
    .map((e) => e.latency_ms)
    .filter((n) => n > 0)
    .sort((a, b) => a - b);
  const bins = Array.from({ length: 30 }, (_, i) => {
    const start = now - (30 - i) * 30_000;
    return (
      recent.filter(
        (e) =>
          Date.parse(e.timestamp) >= start &&
          Date.parse(e.timestamp) < start + 30_000,
      ).length * 2
    );
  });
  return {
    rpm: minute.length,
    p50: latencies.length
      ? latencies[Math.floor((latencies.length - 1) / 2)]
      : null,
    error: recent.length
      ? (recent.filter((e) => e.failed).length / recent.length) * 100
      : null,
    tokens: recent.reduce((n, e) => n + e.tokens.total_tokens, 0),
    bins,
  };
}

export function quotaWindows(
  provider: string,
  payload: Data,
): { label: string; used: number; reset: string }[] {
  const windows: { label: string; used: number; reset: string }[] = [];
  if (Array.isArray(payload.groups)) {
    for (const group of payload.groups) {
      for (const bucket of group.buckets || []) {
        const remaining = bucket.remainingFraction ?? bucket.remaining_fraction;
        if (typeof remaining === "number")
          windows.push({
            label: [
              group.displayName || group.display_name,
              bucket.window || bucket.description,
            ]
              .filter(Boolean)
              .join(" · "),
            used:
              Math.round(
                Math.max(0, Math.min(100, (1 - remaining) * 100)) * 10,
              ) / 10,
            reset: bucket.resetTime || bucket.reset_time || "",
          });
      }
    }
    return windows;
  }
  if (provider === "claude" || provider === "anthropic") {
    for (const [key, val] of Object.entries(payload)) {
      if (val && typeof val.utilization === "number")
        windows.push({
          label: key.replaceAll("_", " "),
          used: Math.max(0, Math.min(100, val.utilization)),
          reset: val.resets_at || "",
        });
    }
  } else if (provider === "codex") {
    for (const [key, val] of Object.entries<Data>(
      payload.rate_limit || payload.rateLimit || {},
    )) {
      if (val && typeof val.used_percent === "number")
        windows.push({
          label: key === "primary_window" ? "Primary window" : "Weekly window",
          used: Math.max(0, Math.min(100, val.used_percent)),
          reset: val.reset_at
            ? new Date(val.reset_at * 1000).toISOString()
            : "",
        });
    }
  } else if (provider === "kimi" || provider === "kimi-ai") {
    for (const [key, val] of Object.entries<Data>(payload.usages || {})) {
      if (val?.limit > 0)
        windows.push({
          label: key.replaceAll("_", " "),
          used: Math.max(
            0,
            Math.min(100, (Number(val.used) / Number(val.limit)) * 100),
          ),
          reset: String(val.reset_time || ""),
        });
    }
  }
  return windows;
}
