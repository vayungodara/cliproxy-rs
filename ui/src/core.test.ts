import { strict as assert } from "node:assert";
import { test } from "node:test";
import {
  cleanEvent,
  equal,
  fieldPath,
  lineDiff,
  metrics,
  quotaWindows,
  serverBase,
} from "./core.ts";

test("v8 paths, URL boundaries, and structural equality", () => {
  assert.equal(
    serverBase("https://proxy.example.test/prefix/v8/management/"),
    "https://proxy.example.test/prefix",
  );
  assert.throws(() => serverBase("https://user:pass@example.test"));
  assert.throws(() => serverBase("javascript:alert(1)"));
  assert.throws(() => fieldPath("oauth/../key"));
  assert.equal(
    fieldPath("plugins/configs/my plugin"),
    "/config/plugins/configs/my%20plugin",
  );
  assert.equal(
    equal({ b: null, a: [1, false] }, { a: [1, false], b: null }),
    true,
  );
  assert.equal(equal({ a: [1, 2] }, { a: [2, 1] }), false);
  assert.equal(equal({ a: false }, { a: null }), false);
});

test("diff preserves original and new lines on both sides of a change", () => {
  const diff = lineDiff("same\nold\nend", "same\nnew\nextra\nend");
  assert.deepEqual(
    diff.filter((l) => l.kind !== "added").map((l) => l.text),
    ["same", "old", "end"],
  );
  assert.deepEqual(
    diff.filter((l) => l.kind !== "removed").map((l) => l.text),
    ["same", "new", "extra", "end"],
  );
  assert.deepEqual(lineDiff("unchanged", "unchanged"), [
    { kind: "same", text: "unchanged" },
  ]);
});

test("metrics respect minute and 15-minute boundaries; sanitize secrets", () => {
  const now = Date.parse("2026-10-02T12:00:00Z");
  const events = [
    cleanEvent({
      timestamp: "2026-10-02T11:59:59Z",
      latency_ms: 100,
      failed: true,
      api_key: "never-display",
      tokens: { total_tokens: 4 },
    }),
    cleanEvent({
      timestamp: "2026-10-02T11:59:00Z",
      latency_ms: 700,
      tokens: { total_tokens: 5 },
    }),
    cleanEvent({
      timestamp: "2026-10-02T11:45:00Z",
      latency_ms: 999,
      tokens: { total_tokens: 6 },
    }),
    cleanEvent({ timestamp: "2026-10-02T12:01:00Z", latency_ms: 88 }),
  ];
  assert.equal("api_key" in events[0], false);
  assert.equal(metrics(events, now).rpm, 1);
  assert.equal(metrics(events, now).p50, 100);
  assert.equal(metrics(events, now).error, 50);
  assert.equal(metrics(events, now).tokens, 9);
  assert.equal(metrics([], now).error, null);
});

test("quota is percent USED, not remaining; unknown and null stay absent", () => {
  assert.deepEqual(
    quotaWindows("claude", {
      five_hour: { utilization: 37, resets_at: "later" },
      seven_day: null,
    }),
    [{ label: "five hour", used: 37, reset: "later" }],
  );
  assert.equal(
    quotaWindows("codex", {
      rate_limit: { primary_window: { used_percent: 24, reset_at: 1000 } },
    })[0].used,
    24,
  );
  assert.deepEqual(quotaWindows("gemini", {}), []);
});
