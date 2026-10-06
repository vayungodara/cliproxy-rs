import { strict as assert } from "node:assert";
import { spawnSync } from "node:child_process";
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
  otherSignals,
  quotaWindows,
  reconcile,
  signalWindows,
  snippet,
  span,
  strategy,
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
  // A cut stated reset: limited until it, next check at retry_at (or now, when due).
  const cut = credState(
    { status: "active", cooldowns: [{ scope: "credential", retry_at: at(30 * 60_000), recover_at: at(6 * 86_400_000) }] },
    now,
  );
  assert.match(cut.label, /^Limited until .+ · next check in 30m$/);
  const due = credState({ status: "active", cooldowns: [{ scope: "credential", retry_at: at(-1000), recover_at: at(86_400_000) }] }, now);
  assert.match(due.label, /· next check on next request$/);
  assert.equal(due.lamp, "warn");
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
    { label: "Current session", used: 37, reset: "later" },
  ]);
  assert.equal(quotaWindows("codex", { rate_limit: { primary_window: { used_percent: 24, reset_at: 1000 } } })[0].used, 24);
  // Codex windows are named by their length; without one, by their slot.
  assert.deepEqual(
    quotaWindows("codex", {
      rate_limit: {
        primary_window: { used_percent: 61, limit_window_seconds: 18000, reset_at: 1790000000 },
        secondary_window: { used_percent: 18, limit_window_seconds: 604800, reset_at: 1790500000 },
      },
    }).map((w) => [w.label, w.used, w.reset]),
    [
      ["5-hour limit", 61, "2026-09-21T14:13:20.000Z"],
      ["Weekly limit", 18, "2026-09-27T09:06:40.000Z"],
    ],
  );
  assert.equal(quotaWindows("codex", { rate_limit: { secondary_window: { used_percent: 3 } } })[0].label, "Secondary limit");
  assert.equal(quotaWindows("x", { groups: [{ displayName: "Pro", buckets: [{ remainingFraction: 0.25 }] }] })[0].used, 75);
  assert.deepEqual(quotaWindows("gemini", {}), []);
});

test("Claude usage: the limits list, as Claude Code titles it", () => {
  // limits[] wins outright: legacy fields alongside it are ignored, rows keep the server's
  // order within each group, a null percentage is dropped, and kinds decide the title.
  const withLimits = {
    limits: [
      { kind: "weekly_all", group: "weekly", percent: 31, resets_at: "2026-07-28T10:00:00+00:00", scope: null, is_active: false },
      { kind: "session", group: "session", percent: 44, resets_at: "2026-07-27T10:00:00+00:00", scope: null, is_active: true },
      { kind: "weekly_scoped", group: "weekly", percent: 64, resets_at: "2026-07-28T10:00:00+00:00", scope: { model: { id: null, display_name: "Fable" } }, is_active: true },
      { kind: "weekly_scoped", group: "weekly", percent: null, resets_at: null, scope: { model: { id: null, display_name: "Opus" } }, is_active: false },
      { kind: "session", group: "session", percent: 7, resets_at: null, scope: null, label: "Current week (all models)" },
    ],
    five_hour: { utilization: 99, resets_at: null },
    seven_day_opus: { utilization: 88, resets_at: null },
    iguana_necktie: { utilization: 64, resets_at: null },
    extra_usage: { is_enabled: true, monthly_limit: 10000, used_credits: 1250, utilization: 12.5, currency: "USD" },
  };
  assert.deepEqual(quotaWindows("claude", withLimits), [
    { label: "Current week (all models)", used: 31, reset: "2026-07-28T10:00:00+00:00" },
    { label: "Current week (Fable only)", used: 64, reset: "2026-07-28T10:00:00+00:00" },
    { label: "Current session", used: 44, reset: "2026-07-27T10:00:00+00:00" },
    { label: "Current session", used: 7, reset: "" },
    { label: "Extra usage", used: 12.5, reset: "", detail: "$12.50 of $100.00" },
  ]);
});

test("Claude usage: legacy buckets when there is no limits list", () => {
  assert.deepEqual(
    quotaWindows("claude", {
      five_hour: { utilization: 12.0, resets_at: "2026-05-03T06:50Z" },
      seven_day: { utilization: 70.0, resets_at: "2026-05-04T00:00Z" },
      seven_day_sonnet: { utilization: 38.0, resets_at: "2026-05-04T00:00Z" },
      seven_day_opus: { utilization: 0, resets_at: "2026-05-04T00:00Z" },
      cinder_cove: { utilization: 5, resets_at: null },
      extra_usage: { is_enabled: true, monthly_limit: null, used_credits: 300, utilization: null, currency: "USD" },
    }),
    [
      { label: "Current session", used: 12, reset: "2026-05-03T06:50Z" },
      { label: "Current week (all models)", used: 70, reset: "2026-05-04T00:00Z" },
      { label: "Current week (Sonnet only)", used: 38, reset: "2026-05-04T00:00Z" },
      // A real 0 is a real 0%; only null is hidden.
      { label: "Current week (Opus only)", used: 0, reset: "2026-05-04T00:00Z" },
      { label: "Other limit", used: 5, reset: "" },
      { label: "Extra usage", used: null, reset: "", detail: "$3.00 spent, no limit" },
    ],
  );
});

test("Claude usage: placeholders and skipped buckets never show, null is never 0%", () => {
  const placeholders = {
    five_hour: { utilization: 23.0, resets_at: "2026-05-03T06:50Z" },
    seven_day: { utilization: null, resets_at: null },
    seven_day_sonnet: null,
    seven_day_opus: null,
    seven_day_omelette: { utilization: 100.0, resets_at: "2026-05-04T00:00Z" },
    seven_day_oauth_apps: { utilization: 3, resets_at: null },
    seven_day_cowork: { utilization: 9, resets_at: null },
    tangelo: null,
    iguana_necktie: { utilization: 0, resets_at: null },
    omelette_promotional: null,
    cinder_cove: null,
    extra_usage: { is_enabled: false, monthly_limit: 5000, used_credits: 100, utilization: 2 },
  };
  assert.deepEqual(quotaWindows("claude", placeholders), [{ label: "Current session", used: 23, reset: "2026-05-03T06:50Z" }]);
  assert.deepEqual(quotaWindows("claude", { tangelo: null, iguana_necktie: null, omelette_promotional: null, limits: [] }), []);
});

test("routing strategy names follow the server's parsing", () => {
  assert.equal(strategy("fill-first").name, "Fill first");
  assert.equal(strategy(" FF ").value, "fill-first");
  assert.equal(strategy("wrr").value, "weighted-round-robin");
  assert.equal(strategy("reset-first").value, "soonest-reset");
  assert.equal(strategy("soonest-reset").rust, true);
  // Unset or unknown values are round robin, as both servers read them.
  assert.equal(strategy(undefined).value, "round-robin");
  assert.equal(strategy("random").value, "round-robin");
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
    { label: "5-hour limit", used: 42.5, reset: "2026-10-02T14:00:00.000Z", source: "x-codex-primary-" },
    // Relative resets count from when the server observed them, not from now.
    { label: "Weekly limit", used: 7, reset: "2026-10-02T13:00:00.000Z", source: "x-codex-secondary-" },
  ]);
  assert.deepEqual(signalWindows("codex", { signals: { "x-codex-primary-window-minutes": "300" } }), []);
  assert.deepEqual(
    signalWindows("devin", { signals: { daily_quota_remaining_percent: "87%", weekly_quota_remaining_percent: "100%", daily_quota_reset_at: "2026-10-03T00:00:00Z" } }),
    [
      { label: "Daily", used: 13, reset: "2026-10-03T00:00:00Z", source: "daily_quota_" },
      { label: "Weekly", used: 0, reset: "", source: "weekly_quota_" },
    ],
  );
  assert.deepEqual(signalWindows("claude", { signals: { "x-codex-primary-used-percent": "5" } }), []);
});

test("signals not drawn as a window stay visible", () => {
  const quota = {
    signals: {
      "X-Codex-Primary-Used-Percent": "40",
      "x-codex-primary-window-minutes": "300",
      // No used percent, so no secondary window: its reset must stay visible.
      "x-codex-secondary-reset-at": "1790000000",
      "x-codex-code-review-primary-used-percent": "100",
      "x-codex-credits-balance": "5",
      "retry-after": "30",
    },
  };
  const keys = (w: ReturnType<typeof signalWindows>) => otherSignals(quota, w).map(([k]) => k);
  assert.deepEqual(keys(signalWindows("codex", quota)), [
    "x-codex-secondary-reset-at",
    "x-codex-code-review-primary-used-percent",
    "x-codex-credits-balance",
    "retry-after",
  ]);
  // Windows from a live check carry no source, so every signal shows beside them.
  assert.equal(keys([{ label: "5-hour", used: 1, reset: "" }]).length, 6);
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

test("tool setup survives keys with shell and string metacharacters", (t) => {
  // Each language reads the value back itself; the expectation is the raw key, not our escaping.
  const key = `sk-'a"$HOME\`id\`\\n$(x)`;
  const model = `m"o'd\\el`;
  const base = "http://127.0.0.1:8317";
  const sh = (script: string) => {
    const r = spawnSync("sh", ["-c", script], { encoding: "utf8", env: { PATH: process.env.PATH, HOME: "/nonexistent" } });
    assert.equal(r.status, 0, r.stderr);
    return r.stdout;
  };
  const claude = snippet("Claude Code", base, key, model).replace(/\nclaude$/, "");
  assert.equal(sh(`${claude}\nprintf '%s|%s' "$ANTHROPIC_BASE_URL" "$ANTHROPIC_AUTH_TOKEN"`), `${base}|${key}`);
  const codex = snippet("Codex CLI", base, key, model);
  const exportLine = codex.split("\n").find((l) => l.startsWith("export "))!;
  assert.equal(sh(`${exportLine}\nprintf '%s' "$CLIPROXY_API_KEY"`), key);
  const curl = snippet("curl", `${base}/a b`, key, model).replace(/^curl /, "printf '%s\\n' ");
  assert.equal(sh(curl), `${base}/a b/v1/models\n-H\nAuthorization: Bearer ${key}\n`);

  const py = spawnSync("python3", ["--version"]);
  if (py.status !== 0) return t.skip("python3 not installed");
  const run = (code: string, input: string) => {
    const r = spawnSync("python3", ["-c", code], { input, encoding: "utf8" });
    assert.equal(r.status, 0, r.stderr);
    return JSON.parse(r.stdout);
  };
  const kwargs = `import ast, json, sys
print(json.dumps({k.arg: k.value.value for n in ast.walk(ast.parse(sys.stdin.read())) if isinstance(n, ast.Call)
  for k in n.keywords if isinstance(k.value, ast.Constant)}))`;
  for (const [tool, url] of [["OpenAI SDK", `${base}/v1`], ["Anthropic SDK", base]] as const) {
    const got = run(kwargs, snippet(tool, base, key, model));
    assert.equal(got.base_url, url, tool);
    assert.equal(got.api_key, key, tool);
    assert.equal(got.model, model, tool);
  }
  // tomllib arrived in Python 3.11; older versions skip only the TOML part.
  if (spawnSync("python3", ["-c", "import tomllib"]).status !== 0) return t.skip("python3 has no tomllib (before 3.11)");
  const toml = codex.slice(0, codex.indexOf("# then"));
  const parsed = run("import json, sys, tomllib; print(json.dumps(tomllib.loads(sys.stdin.read())))", toml);
  assert.equal(parsed.model_providers.cliproxy.base_url, `${base}/v1`);
  const odd = run("import json, sys, tomllib; print(json.dumps(tomllib.loads(sys.stdin.read())))",
    snippet("Codex CLI", `http://h/p"\\q`, key, model).split("# then")[0]);
  assert.equal(odd.model_providers.cliproxy.base_url, `http://h/p"\\q/v1`);
});
