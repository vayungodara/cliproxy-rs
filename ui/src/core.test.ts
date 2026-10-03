import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  ago,
  buckets,
  cleanEvent,
  credState,
  equal,
  fieldPath,
  level,
  lineDiff,
  quotaWindows,
  reconcile,
  signalWindows,
  snippet,
  span,
  sumBuckets,
  usageStats,
} from "./core.ts";

const now = Date.parse("2026-10-02T12:00:00Z");
const at = (ms: number) => new Date(now + ms).toISOString();

test("v8 paths, URL boundaries, and structural equality", () => {
  assert.throws(() => fieldPath("oauth/../key"));
  assert.equal(fieldPath("plugins/configs/my plugin"), "/config/plugins/configs/my%20plugin");
  assert.equal(equal({ b: null, a: [1, false] }, { a: [1, false], b: null }), true);
  assert.equal(equal({ a: [1, 2] }, { a: [2, 1] }), false);
  assert.equal(equal({ a: false }, { a: null }), false);
});

test("diff keeps both sides of a change", () => {
  const diff = lineDiff("same\nold\nend", "same\nnew\nextra\nend");
  assert.deepEqual(diff.filter((l) => l.kind !== "added").map((l) => l.text), ["same", "old", "end"]);
  assert.deepEqual(diff.filter((l) => l.kind !== "removed").map((l) => l.text), ["same", "new", "extra", "end"]);
});

test("credential state: disabled wins, only live cooldowns count, model cooldowns do not block", () => {
  const cooling = [{ scope: "credential", reason: "quota", retry_at: at(4 * 60_000) }];
  assert.equal(credState({ disabled: true, status: "disabled", cooldowns: cooling }, now).label, "Disabled");
  assert.deepEqual(credState({ status: "active", cooldowns: cooling }, now), { lamp: "warn", label: "Cooling 4m", detail: "" });
  // An expired retry time is history, not state.
  assert.equal(credState({ status: "active", cooldowns: [{ scope: "credential", retry_at: at(-1000) }] }, now).label, "Ready");
  const model = credState({ status: "active", cooldowns: [{ scope: "model", model_key: "x", retry_at: at(60_000) }] }, now);
  assert.deepEqual([model.lamp, model.label, model.detail], ["ok", "Ready", "1 model cooling"]);
  // Unavailable with a retry time is a cooldown; without one it is a failure.
  assert.equal(credState({ status: "error", unavailable: true, next_retry_after: at(90_000) }, now).label, "Cooling 2m");
  assert.equal(credState({ status: "error", unavailable: true }, now).lamp, "bad");
  assert.equal(credState({ status: "error", status_message: "bad token" }, now).detail, "bad token");
  // Rust reports cooldowns: null today.
  assert.equal(credState({ status: "active", cooldowns: null }, now).label, "Ready");
  assert.deepEqual(credState({ status: "pending" }, now), { lamp: "off", label: "pending", detail: "" });
});

test("traffic buckets: absent history stays absent; sums align to the newest bucket", () => {
  assert.equal(buckets({ name: "rust.json" }), null);
  assert.equal(sumBuckets([null, null]), null);
  const a = buckets({ recent_requests: [{ success: 1 }, { success: 2, failed: 1 }, { success: 3 }] });
  const b = buckets({ recent_requests: [{ success: 10 }, { failed: 20 }] });
  assert.deepEqual(a, { total: [1, 3, 3], failed: [0, 1, 0] });
  assert.deepEqual(sumBuckets([a, null, b]), { total: [1, 13, 23], failed: [0, 1, 20] });
});

test("grille levels: zero is dark, any traffic is visible, the peak is full", () => {
  assert.deepEqual([0, 1, 50, 51, 100].map((n) => level(n, 100)), [0, 1, 2, 3, 4]);
  assert.equal(level(5, 0), 0);
});

test("usage events keep display fields only and summarise by model", () => {
  const raw = {
    timestamp: "2026-10-02T11:59:59Z",
    latency_ms: 100,
    ttft_ms: 40,
    failed: true,
    fail: { status_code: 529, body: "upstream said secret things" },
    api_key: "sk-client-secret",
    client_ip: "203.0.113.9",
    user_agent: "tool/1.0",
    model: "claude-sonnet-4-5",
    alias: "daily",
    tokens: { input_tokens: 10, output_tokens: 5 },
  };
  const e = cleanEvent(raw);
  assert.deepEqual(Object.keys(e).sort(), ["at", "auth", "failed", "id", "input", "latency", "model", "output", "provider", "status", "ttft"]);
  assert.equal(JSON.stringify(e).includes("secret"), false);
  assert.deepEqual([e.model, e.status, e.at], ["daily", 529, Date.parse(raw.timestamp)]);
  const s = usageStats([e, cleanEvent({ model: "m", latency_ms: 0 }), cleanEvent({ model: "m", latency_ms: 300, ttft_ms: 0 })]);
  assert.deepEqual([s.count, s.failed, s.latency, s.ttft], [3, 1, 100, 40]);
  assert.deepEqual(s.models.map((m) => [m.model, m.count]), [["m", 2], ["daily", 1]]);
  assert.equal(usageStats([]).latency, null);
});

test("quota is percent used, not remaining; unknown payloads stay empty", () => {
  assert.deepEqual(quotaWindows("claude", { five_hour: { utilization: 37, resets_at: "later" }, seven_day: null }), [
    { label: "five hour", used: 37, reset: "later" },
  ]);
  assert.equal(quotaWindows("codex", { rate_limit: { primary_window: { used_percent: 24, reset_at: 1000 } } })[0].used, 24);
  assert.equal(quotaWindows("x", { groups: [{ displayName: "Pro", buckets: [{ remainingFraction: 0.25 }] }] })[0].used, 75);
  assert.deepEqual(quotaWindows("gemini", {}), []);
});

test("reconcile keeps unchanged rows by identity", () => {
  const prev = [{ name: "a", n: 1 }, { name: "b", n: 1 }];
  const next = reconcile(prev, [{ name: "a", n: 1 }, { name: "b", n: 2 }, { name: "c", n: 0 }], (v) => v.name);
  assert.equal(next[0], prev[0]);
  assert.notEqual(next[1], prev[1]);
  assert.equal(next.length, 3);
});

test("durations round to the unit a reader needs", () => {
  assert.deepEqual([59_000, 90_000, 125 * 60_000, 3 * 86_400_000].map(span), ["59s", "2m", "2h 5m", "3d"]);
  assert.equal(ago("not a date", now), "");
  assert.equal(ago(at(-10_000), now), "just now");
  assert.equal(ago(at(-3_600_000), now), "1h ago");
  // Epoch milliseconds (as stored for quota checks) are accepted, not parsed as strings.
  assert.equal(ago(now - 120_000, now), "2m ago");
});

test("passive limits from Codex headers and Devin quota", () => {
  const observed = "2026-10-02T12:00:00Z";
  const codex = signalWindows("codex", {
    observed_at: observed,
    signals: {
      "X-Codex-Primary-Used-Percent": "42.5",
      "X-Codex-Primary-Window-Minutes": "300",
      "X-Codex-Primary-Reset-At": String(Date.parse("2026-10-02T14:00:00Z") / 1000),
      "x-codex-secondary-used-percent": "7",
      "x-codex-secondary-window-minutes": "10080",
      "x-codex-secondary-reset-after-seconds": "3600",
      "x-codex-code-review-primary-used-percent": "99",
    },
  });
  assert.deepEqual(codex, [
    { label: "5-hour", used: 42.5, reset: "2026-10-02T14:00:00.000Z" },
    // Relative resets count from when the server observed them, not from now.
    { label: "Weekly", used: 7, reset: "2026-10-02T13:00:00.000Z" },
  ]);
  assert.deepEqual(signalWindows("codex", { signals: { "x-codex-primary-window-minutes": "300" } }), []);
  assert.deepEqual(
    signalWindows("devin", { signals: { daily_quota_remaining_percent: "87%", weekly_quota_remaining_percent: "100%", daily_quota_reset_at: "2026-10-03T00:00:00Z" } }),
    [
      { label: "Daily", used: 13, reset: "2026-10-03T00:00:00Z" },
      { label: "Weekly", used: 0, reset: "" },
    ],
  );
  assert.deepEqual(signalWindows("claude", { signals: { "x-codex-primary-used-percent": "5" } }), []);
});

test("expired sign-ins ask for a new sign-in; other errors stay errors", () => {
  assert.equal(credState({ status: "error", status_message: "refresh token was revoked" }, now).label, "Sign in again");
  assert.equal(credState({ status: "error", status_message: "upstream returned 401" }, now).label, "Sign in again");
  assert.equal(credState({ status: "error", status_message: "context window 4010 tokens over" }, now).label, "Error");
  assert.equal(credState({ unavailable: true, status_message: "upstream 503" }, now).label, "Unavailable");
});

test("tool setup points at this server with the chosen key", () => {
  const base = "https://proxy.example.test/cpa";
  assert.match(snippet("Claude Code", base, "sk-k", "m"), /^export ANTHROPIC_BASE_URL=https:\/\/proxy\.example\.test\/cpa\n/);
  assert.match(snippet("Anthropic SDK", base, "sk-k", "m"), /base_url="https:\/\/proxy\.example\.test\/cpa"/);
  for (const tool of ["Codex CLI", "Cursor", "OpenAI SDK", "curl"] as const)
    assert.ok(snippet(tool, base, "sk-k", "m").includes(`${base}/v1`), tool);
  assert.ok(snippet("OpenAI SDK", base, "sk-k", "claude-x").includes('model="claude-x"'));
  for (const tool of ["Claude Code", "Codex CLI", "Cursor", "OpenAI SDK", "Anthropic SDK", "curl"] as const)
    assert.ok(snippet(tool, base, "sk-secret-key", "m").includes("sk-secret-key"), tool);
});
