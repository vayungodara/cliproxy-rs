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
/** A random client key: "sk-" and 48 hex characters. */
export const newKey = () => `sk-${Array.from(crypto.getRandomValues(new Uint8Array(24)), (n) => n.toString(16).padStart(2, "0")).join("")}`;
/** Small flags this browser keeps for the first-run card. Never keys. */
export const flag = {
  get: (k: string) => {
    try {
      return localStorage.getItem(`cliproxy-${k}`);
    } catch {
      return null;
    }
  },
  set: (k: string, v: string) => {
    try {
      localStorage.setItem(`cliproxy-${k}`, v);
    } catch {}
  },
};
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
  const all: Data[] = Array.isArray(a.cooldowns) ? a.cooldowns : [];
  // cliproxy-rs: recover_at is the upstream's stated reset when the trust bound cut it.
  const limited = all
    .filter((c) => c.scope !== "model" && Date.parse(c.recover_at) > now)
    .map((c) => [Date.parse(c.recover_at), Date.parse(c.retry_at)])[0];
  if (limited) {
    const next = limited[1] > now ? `in ${span(limited[1] - now)}` : "on next request";
    return {
      lamp: "warn",
      label: `Limited until ${new Date(limited[0]).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" })} · next check ${next}`,
      detail: a.status_message || "",
    };
  }
  const cooldowns = all.filter((c) => Date.parse(c.retry_at) > now);
  const whole = cooldowns.filter((c) => c.scope !== "model");
  const until = Math.min(
    ...whole.map((c) => Date.parse(c.retry_at)),
    a.unavailable && a.next_retry_after ? Date.parse(a.next_retry_after) : Infinity,
  );
  if (until < Infinity && until > now)
    return { lamp: "warn", label: `Cooling ${span(until - now)}`, detail: a.status_message || "" };
  if (a.unavailable || a.status === "error")
    return {
      lamp: "bad",
      // An expired or revoked sign-in is fixed by connecting the account again.
      label: /\b401\b|unauthori[sz]ed|invalid_grant|revoked|refresh token/i.test(a.status_message || "")
        ? "Sign in again"
        : a.unavailable ? "Unavailable" : "Error",
      detail: a.status_message || "",
    };
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

/**
 * One usage window. `used` is percent used, or null when the limit has no ceiling (an
 * uncapped extra-usage budget) and only `detail` applies. `source` is the signal-name
 * prefix a passive window was read from.
 */
export type Window = { label: string; used: number | null; reset: string; source?: string; detail?: string };
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
  if (p === "claude") return claudeWindows(payload);
  if (p === "codex") {
    for (const [k, v] of Object.entries<Data>(payload.rate_limit || payload.rateLimit || {}))
      if (v && typeof v.used_percent === "number")
        out.push({
          label: codexWindow(Number(v.limit_window_seconds), k === "primary_window" ? "Primary limit" : "Secondary limit"),
          used: clamp(v.used_percent),
          reset: v.reset_at
            ? new Date(v.reset_at * 1000).toISOString()
            : Number(v.reset_after_seconds) >= 0 && v.reset_after_seconds !== undefined
              ? new Date(Date.now() + Number(v.reset_after_seconds) * 1000).toISOString()
              : "",
        });
  } else if (p === "kimi" || p === "kimi-ai") {
    for (const [k, v] of Object.entries<Data>(payload.usages || {}))
      if (v?.limit > 0)
        out.push({ label: k.replaceAll("_", " "), used: clamp((Number(v.used) / Number(v.limit)) * 100), reset: String(v.reset_time || "") });
  }
  return out;
}

const percent = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? clamp(v) : null);
/**
 * Claude's legacy buckets, titled as Claude Code's /usage titles them. Anthropic also sends
 * placeholders and buckets Claude Code never shows (iguana_necktie, tangelo,
 * omelette_promotional, seven_day_omelette, seven_day_cowork, seven_day_oauth_apps); they
 * are skipped. cinder_cove has no known title and shows as "Other limit" when set.
 */
const claudeBuckets: [string, string][] = [
  ["five_hour", "Current session"],
  ["seven_day", "Current week (all models)"],
  ["seven_day_sonnet", "Current week (Sonnet only)"],
  ["seven_day_opus", "Current week (Opus only)"],
  ["cinder_cove", "Other limit"],
];
function money(minor: number, places: number, currency: string): string {
  const amount = minor / 10 ** places;
  try {
    return new Intl.NumberFormat("en-US", { style: "currency", currency: currency || "USD" }).format(amount);
  } catch {
    return `${amount.toFixed(places)} ${currency}`;
  }
}

/**
 * Anthropic's /api/oauth/usage, rendered the way Claude Code's /usage renders it. When the
 * `limits` list is present it is the only source: rows are classified by `kind` (never by
 * label) and grouped by `group` in the server's order. Without it the legacy buckets apply.
 * Percentages are percent used, 0 to 100; null means the limit does not apply and is left
 * out, never shown as 0%. Extra usage is in cents and shows only when enabled.
 */
export function claudeWindows(payload: Data): Window[] {
  const out: Window[] = [];
  const limits: Data[] = Array.isArray(payload.limits) ? payload.limits : [];
  if (limits.length) {
    const groups: string[] = [];
    for (const l of limits) if (!groups.includes(String(l?.group))) groups.push(String(l?.group));
    for (const g of groups)
      for (const l of limits) {
        const used = percent(l?.percent);
        if (String(l?.group) !== g || used === null) continue;
        const model = l.scope?.model?.display_name;
        const label =
          l.kind === "session" ? "Current session"
          : l.kind === "weekly_all" ? "Current week (all models)"
          : l.kind === "weekly_scoped" && model ? `Current week (${model} only)`
          : "";
        if (label) out.push({ label, used, reset: l.resets_at || "" });
      }
  } else
    for (const [key, label] of claudeBuckets) {
      const used = percent(payload[key]?.utilization);
      if (used !== null) out.push({ label, used, reset: payload[key].resets_at || "" });
    }
  const extra = payload.extra_usage;
  if (extra?.is_enabled === true && typeof extra.used_credits === "number") {
    const places = extra.decimal_places ?? 2;
    const cur = extra.currency || "USD";
    const cap = typeof extra.monthly_limit === "number" ? extra.monthly_limit : null;
    const spent = money(extra.used_credits, places, cur);
    const used = cap === null ? null : (percent(extra.utilization) ?? (cap > 0 ? clamp((extra.used_credits / cap) * 100) : 0));
    out.push({ label: "Extra usage", used, reset: "", detail: cap === null ? `${spent} spent, no limit` : `${spent} of ${money(cap, places, cur)}` });
  }
  return out;
}

/** A Codex rate-limit window by its length, as the Codex CLI names them. */
const codexWindow = (seconds: number, fallback: string) =>
  seconds === 18_000 ? "5-hour limit" : seconds === 604_800 ? "Weekly limit" : seconds > 0 ? `${span(seconds * 1000)} limit` : fallback;

/**
 * Limits the server observed passively, without asking the provider: Codex rate-limit
 * headers (x-codex-primary-*, x-codex-secondary-*) and Devin's daily and weekly quota.
 */
export function signalWindows(p: string, quota: Data | undefined): Window[] {
  const sig: Data = {};
  for (const [k, v] of Object.entries<string>(quota?.signals || {})) sig[k.toLowerCase()] = v;
  const at = Date.parse(quota?.observed_at) || 0;
  const out: Window[] = [];
  if (p === "codex")
    for (const w of ["primary", "secondary"]) {
      const g = (k: string) => sig[`x-codex-${w}-${k}`];
      const used = parseFloat(g("used-percent")),
        minutes = Number(g("window-minutes"));
      if (Number.isNaN(used)) continue;
      const resetAt = Number(g("reset-at")),
        after = Number(g("reset-after-seconds"));
      out.push({
        label: codexWindow(minutes * 60, w === "primary" ? "Primary limit" : "Secondary limit"),
        used: clamp(used),
        reset: resetAt > 0 ? new Date(resetAt * 1000).toISOString() : after >= 0 && at ? new Date(at + after * 1000).toISOString() : "",
        source: `x-codex-${w}-`,
      });
    }
  else if (p === "devin")
    for (const w of ["daily", "weekly"]) {
      const left = parseFloat(sig[`${w}_quota_remaining_percent`]);
      if (!Number.isNaN(left))
        out.push({ label: w === "daily" ? "Daily" : "Weekly", used: clamp(100 - left), reset: sig[`${w}_quota_reset_at`] || "", source: `${w}_quota_` });
    }
  return out;
}

/** Signals no window was read from (code-review limits, credits, retry-after), to show as they are. */
export const otherSignals = (quota: Data | undefined, windows: Window[]): [string, string][] =>
  Object.entries<string>(quota?.signals || {}).filter(([k]) => !windows.some((w) => w.source && k.toLowerCase().startsWith(w.source)));

/** routing.strategy values, with a one-line explanation each. `rust`: offered only by cliproxy-rs. */
export const strategies = [
  { value: "round-robin", name: "Round robin", rust: false, help: "Takes the accounts in turn, one request each. This is the default." },
  { value: "fill-first", name: "Fill first", rust: false, help: "Uses one account until it cools down or reaches a limit, then moves to the next." },
  {
    value: "weighted-round-robin",
    name: "Weighted round robin",
    rust: false,
    help: "Takes the accounts in turn in proportion to each account's weight, which you set on Credentials.",
  },
  {
    value: "soonest-reset",
    name: "Soonest reset first",
    rust: true,
    help: "Experimental. Uses the account whose weekly limit resets soonest until it cools down or uses up a window, then moves to the next. Conversations bound to an account stay on it.",
  },
] as const;
/** The strategy a configured value selects, aliases included; anything else is round robin, as on the server. */
export function strategy(value: unknown): (typeof strategies)[number] {
  const v = String(value ?? "").trim().toLowerCase();
  const alias: Record<string, string> = {
    rr: "round-robin", roundrobin: "round-robin", ff: "fill-first", fillfirst: "fill-first",
    wrr: "weighted-round-robin", weightedroundrobin: "weighted-round-robin", "reset-first": "soonest-reset",
  };
  return strategies.find((s) => s.value === (alias[v] || v)) || strategies[0];
}

export const tools = ["Claude Code", "Codex CLI", "Cursor", "OpenAI SDK", "Anthropic SDK", "curl"] as const;
/** A POSIX shell single-quoted literal: nothing inside is expanded. */
const shq = (s: string) => `'${s.replaceAll("'", `'\\''`)}'`;
/** A shell word, quoted only when it holds characters the shell would interpret. */
const shw = (s: string) => (/^[\w@%+=:,./-]+$/.test(s) ? s : shq(s));
/** A double-quoted string that Python and TOML read back unchanged (JSON escapes are valid in both). */
const str = (s: string) => JSON.stringify(s);
/**
 * Setup text for one tool, pointed at this server (base has no trailing slash). Values are
 * quoted for the language they land in, so a key holding $, quotes or backslashes pastes intact.
 */
export function snippet(tool: (typeof tools)[number], base: string, key: string, model: string): string {
  const v1 = `${base}/v1`;
  switch (tool) {
    case "Claude Code":
      return `export ANTHROPIC_BASE_URL=${shw(base)}\nexport ANTHROPIC_AUTH_TOKEN=${shq(key)}\nclaude`;
    case "Codex CLI":
      return `# ~/.codex/config.toml\nmodel_provider = "cliproxy"\n\n[model_providers.cliproxy]\nname = "cliproxy"\nbase_url = ${str(v1)}\nenv_key = "CLIPROXY_API_KEY"\nwire_api = "responses"\n\n# then, in your shell\nexport CLIPROXY_API_KEY=${shq(key)}`;
    case "Cursor":
      return `OpenAI API key:            ${key}\nOverride OpenAI Base URL:  ${v1}`;
    case "OpenAI SDK":
      return `from openai import OpenAI\n\nclient = OpenAI(base_url=${str(v1)}, api_key=${str(key)})\nr = client.chat.completions.create(\n    model=${str(model)}, messages=[{"role": "user", "content": "Hello"}]\n)\nprint(r.choices[0].message.content)`;
    case "Anthropic SDK":
      return `import anthropic\n\nclient = anthropic.Anthropic(base_url=${str(base)}, api_key=${str(key)})\nr = client.messages.create(\n    model=${str(model)}, max_tokens=256, messages=[{"role": "user", "content": "Hello"}]\n)\nprint(r.content[0].text)`;
    default:
      return `curl ${shw(`${v1}/models`)} -H ${shq(`Authorization: Bearer ${key}`)}`;
  }
}
