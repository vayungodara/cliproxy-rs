import { cleanEvent } from "./core";

// This module is imported only when DEV && VITE_DEV_FIXTURES === 'true'.
// Fixtures supplement the live backend; they never replace requests or simulate successful writes.
export const accounts = [
  {
    name: "claude-studio.json",
    email: "studio@example.test",
    provider: "claude",
    auth_index: "demo-claude",
    status: "active",
    success: 1824,
    failed: 3,
    fixture: true,
    note: "Primary workspace",
  },
  {
    name: "codex-build.json",
    email: "build@example.test",
    provider: "codex",
    auth_index: "demo-codex",
    status: "active",
    success: 936,
    failed: 6,
    fixture: true,
    note: "Build agent",
  },
  {
    name: "claude-research.json",
    email: "research@example.test",
    provider: "claude",
    auth_index: "demo-research",
    status: "error",
    unavailable: true,
    success: 312,
    failed: 18,
    fixture: true,
    status_message: "Rate limit reached. Waiting for quota reset.",
  },
  {
    name: "gemini-lab.json",
    email: "lab@example.test",
    provider: "gemini",
    auth_index: "demo-gemini",
    status: "active",
    success: 741,
    failed: 1,
    fixture: true,
    note: "Long-context experiments",
  },
];
export const quotas = {
  "demo-claude": {
    five_hour: {
      utilization: 37,
      resets_at: new Date(Date.now() + 2 * 3600_000).toISOString(),
    },
    seven_day: {
      utilization: 62,
      resets_at: new Date(Date.now() + 3 * 86400_000).toISOString(),
    },
  },
  "demo-codex": {
    plan_type: "plus",
    rate_limit: {
      primary_window: {
        used_percent: 24,
        reset_at: Math.floor(Date.now() / 1000) + 10800,
      },
      secondary_window: {
        used_percent: 48,
        reset_at: Math.floor(Date.now() / 1000) + 259200,
      },
    },
  },
  "demo-research": {
    five_hour: {
      utilization: 100,
      resets_at: new Date(Date.now() + 3600_000).toISOString(),
    },
    seven_day: {
      utilization: 83,
      resets_at: new Date(Date.now() + 2 * 86400_000).toISOString(),
    },
  },
};
export const events = Array.from({ length: 30 }, (_, bucket) => {
  const count = Math.round(8 + bucket / 2 + 7 * Math.sin(bucket / 2.5));
  return Array.from({ length: count }, (_, index) => {
    const i = bucket * 31 + index;
    return cleanEvent({
      timestamp: new Date(
        Date.now() - (30 - bucket) * 30000 + (index / count) * 29000,
      ).toISOString(),
      latency_ms: 380 + ((i * 137) % 920),
      failed: i % 61 === 0,
      provider: accounts[i % 4].provider,
      auth_index: accounts[i % 4].auth_index,
      model: [
        "claude-sonnet-4-5",
        "gpt-5-codex",
        "claude-opus-4-1",
        "gemini-2.5-pro",
      ][i % 4],
      request_id: `sample-${i}`,
      tokens: { total_tokens: 640 + (i % 400) },
    });
  });
}).flat();
