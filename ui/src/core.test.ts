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
  serverBase,
  span,
  sumBuckets,
  usageStats,
} from "./core.ts";

const now = Date.parse("2026-10-02T12:00:00Z");
const at = (ms: number) => new Date(now + ms).toISOString();

test("v8 paths, URL boundaries, and structural equality", () => {
  assert.equal(serverBase("https://proxy.example.test/prefix/v8/management/"), "https://proxy.example.test/prefix");
  assert.throws(() => serverBase("https://user:pass@example.test"));
  assert.throws(() => serverBase("javascript:alert(1)"));
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
});
