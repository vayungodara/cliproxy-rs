// Pure helpers. No DOM, no network: everything here is covered by core.test.ts.
export type Data = Record<string, any>;
export type Lamp = "ok" | "warn" | "bad" | "off";

export function fieldPath(path: string): string {
  return (
    "/config/" +
    path
      .split("/")
      .map((part) => {
        if (!part || part === "." || part === ".." || /[\\\x00-\x1f\x7f]/.test(part))
          throw new Error("Invalid configuration path.");
        return encodeURIComponent(part);
      })
      .join("/")
  );
}

export function readPath(value: Data, path: string, fallback: any = undefined): any {
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

/** Reuse previous objects that did not change, so keyed lists skip unchanged rows. */
export function reconcile<T extends Data>(prev: T[], next: T[], key: (v: T) => string): T[] {
  const old = new Map(prev.map((v) => [key(v), v]));
  return next.map((v) => {
    const p = old.get(key(v));
    return p && equal(p, v) ? p : v;
  });
}

export function lineDiff(before: string, after: string): { kind: string; text: string }[] {
  const a = before.split("\n"),
    b = after.split("\n");
  let start = 0,
    end = 0;
  while (start < Math.min(a.length, b.length) && a[start] === b[start]) start++;
  while (end < Math.min(a.length, b.length) - start && a[a.length - 1 - end] === b[b.length - 1 - end])
    end++;
  return [
    ...a.slice(0, start).map((text) => ({ kind: "same", text })),
    ...a.slice(start, a.length - end).map((text) => ({ kind: "removed", text })),
    ...b.slice(start, b.length - end).map((text) => ({ kind: "added", text })),
    ...a.slice(a.length - end).map((text) => ({ kind: "same", text })),
  ];
}

const names: Record<string, string> = {
  claude: "Claude",
  anthropic: "Claude",
  codex: "Codex",
  gemini: "Gemini",
  vertex: "Vertex",
  aistudio: "AI Studio",
  antigravity: "Antigravity",
  kimi: "Kimi",
  "kimi-ai": "Kimi (kimi.ai)",
  xai: "xAI",
  meta: "Meta",
  devin: "Devin",
  "openai-compatibility": "OpenAI-compatible",
  interactions: "Interactions",
};
export const label = (id: string) => names[id] || id;
export const provider = (a: Data) => String(a.provider || a.type || "unknown").replace("anthropic", "claude");
export const credName = (a: Data) => String(a.email || a.label || a.account || a.name);
export const mask = (k: string) => (k.length > 10 ? `${k.slice(0, 3)}…${k.slice(-4)}` : "••••");

/** Durations like 45s, 4m, 2h 5m, 3d. */
export function span(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 48) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  return `${Math.round(h / 24)}d`;
}
export function ago(value: unknown, now = Date.now()): string {
  const t = typeof value === "number" ? value : Date.parse(String(value ?? ""));
  if (Number.isNaN(t) || t <= 0) return "";
  return now - t < 45_000 ? "just now" : `${span(now - t)} ago`;
}

export type CredState = { lamp: Lamp; label: string; detail: string };
/** One state per credential, derived only from fields Go's /credentials reports. */
export function credState(a: Data, now = Date.now()): CredState {
  if (a.disabled) return { lamp: "off", label: "Disabled", detail: "" };
  const cooldowns: Data[] = (Array.isArray(a.cooldowns) ? a.cooldowns : []).filter(
    (c) => Date.parse(c.retry_at) > now,
  );
  const whole = cooldowns.filter((c) => c.scope !== "model");
  const until = Math.min(
    ...whole.map((c) => Date.parse(c.retry_at)),
    a.unavailable && a.next_retry_after ? Date.parse(a.next_retry_after) : Infinity,
  );
  if (until < Infinity && until > now)
    return { lamp: "warn", label: `Cooling ${span(until - now)}`, detail: a.status_message || "" };
  if (a.unavailable || a.status === "error")
    return { lamp: "bad", label: a.unavailable ? "Unavailable" : "Error", detail: a.status_message || "" };
  const models = cooldowns.length - whole.length;
  const detail = models ? `${models} model${models > 1 ? "s" : ""} cooling` : "";
  if (["active", "ok", ""].includes(String(a.status ?? "")))
    return { lamp: "ok", label: "Ready", detail };
  return { lamp: "off", label: String(a.status), detail };
}

export type Buckets = { total: number[]; failed: number[] };
/** Go reports 20 ten-minute buckets per credential. Null means the server did not report history. */
export function buckets(a: Data): Buckets | null {
  if (!Array.isArray(a.recent_requests)) return null;
  const rows = a.recent_requests as Data[];
  return {
    total: rows.map((b) => (Number(b.success) || 0) + (Number(b.failed) || 0)),
    failed: rows.map((b) => Number(b.failed) || 0),
  };
}
export function sumBuckets(list: (Buckets | null)[]): Buckets | null {
  const known = list.filter((b): b is Buckets => !!b);
  if (!known.length) return null;
  const n = Math.max(...known.map((b) => b.total.length));
  const add = (k: keyof Buckets) =>
    Array.from({ length: n }, (_, i) =>
      known.reduce((s, b) => s + (b[k][i - (n - b[k].length)] || 0), 0),
    );
  return { total: add("total"), failed: add("failed") };
}
export const sum = (v: number[]) => v.reduce((a, b) => a + b, 0);
/** Map a count to five steps; any non-zero count is at least step 1. */
export const level = (n: number, max: number) => (n <= 0 || max <= 0 ? 0 : Math.max(1, Math.ceil((n / max) * 4)));

// Usage records also carry client keys, IPs, and response bodies. Keep display fields only.
export type Event = {
  at: number;
  latency: number;
  ttft: number;
  failed: boolean;
  status: number;
  model: string;
  provider: string;
  auth: string;
  id: string;
  input: number;
  output: number;
};
export function cleanEvent(raw: Data): Event {
  const t = Date.parse(String(raw.timestamp));
  return {
    at: Number.isNaN(t) ? Date.now() : t,
    latency: Number(raw.latency_ms) || 0,
    ttft: Number(raw.ttft_ms) || 0,
    failed: raw.failed === true,
    status: Number(raw.fail?.status_code) || (raw.failed === true ? 0 : 200),
    model: String(raw.alias || raw.model || "unknown"),
    provider: String(raw.provider || "unknown"),
    auth: String(raw.auth_index || ""),
    id: String(raw.request_id || ""),
    input: Number(raw.tokens?.input_tokens) || 0,
    output: Number(raw.tokens?.output_tokens) || 0,
  };
}
const median = (v: number[]) => {
  const s = v.filter((n) => n > 0).sort((a, b) => a - b);
  return s.length ? s[Math.floor((s.length - 1) / 2)] : null;
};
export function usageStats(events: Event[]) {
  const models = new Map<string, { model: string; count: number; failed: number; input: number; output: number }>();
  for (const e of events) {
    const m = models.get(e.model) || { model: e.model, count: 0, failed: 0, input: 0, output: 0 };
    m.count++;
    m.failed += +e.failed;
    m.input += e.input;
    m.output += e.output;
    models.set(e.model, m);
  }
  return {
    count: events.length,
    failed: events.filter((e) => e.failed).length,
    latency: median(events.map((e) => e.latency)),
    ttft: median(events.map((e) => e.ttft)),
    input: sum(events.map((e) => e.input)),
    output: sum(events.map((e) => e.output)),
    models: [...models.values()].sort((a, b) => b.count - a.count),
  };
}

export type Window = { label: string; used: number; reset: string };
const clamp = (n: number) => Math.round(Math.max(0, Math.min(100, n)) * 10) / 10;
/** Normalised plugin groups first; then the built-in Claude, Codex and Kimi usage payloads. */
export function quotaWindows(p: string, payload: Data): Window[] {
  const out: Window[] = [];
  if (Array.isArray(payload.groups)) {
    for (const g of payload.groups)
      for (const b of g.buckets || []) {
        const left = b.remainingFraction ?? b.remaining_fraction;
        if (typeof left === "number")
          out.push({
            label: [g.displayName || g.display_name, b.window || b.description].filter(Boolean).join(" · "),
            used: clamp((1 - left) * 100),
            reset: b.resetTime || b.reset_time || "",
          });
      }
    return out;
  }
  if (p === "claude") {
    for (const [k, v] of Object.entries<Data>(payload))
      if (v && typeof v.utilization === "number")
        out.push({ label: k.replaceAll("_", " "), used: clamp(v.utilization), reset: v.resets_at || "" });
  } else if (p === "codex") {
    for (const [k, v] of Object.entries<Data>(payload.rate_limit || payload.rateLimit || {}))
      if (v && typeof v.used_percent === "number")
        out.push({
          label: k === "primary_window" ? "Primary window" : "Weekly window",
          used: clamp(v.used_percent),
          reset: v.reset_at ? new Date(v.reset_at * 1000).toISOString() : "",
        });
  } else if (p === "kimi" || p === "kimi-ai") {
    for (const [k, v] of Object.entries<Data>(payload.usages || {}))
      if (v?.limit > 0)
        out.push({ label: k.replaceAll("_", " "), used: clamp((Number(v.used) / Number(v.limit)) * 100), reset: String(v.reset_time || "") });
  }
  return out;
}
