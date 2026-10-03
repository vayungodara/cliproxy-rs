// POST /requests/api-call as the Quotas screen sends it, against fake endpoints only.
// For each server: pick the fake credential claude-research.json, ask for Claude usage from
// scripts/fake-logins.mjs (port 9102) with "Authorization: Bearer $TOKEN$", and check that
// the server substituted the credential's token, returned Go's {status_code, header, body}
// shape, and that the dashboard's quota parser reads the windows. Servers are compared.
// Usage: node scripts/api-call-check.mjs <server-base> [<server-base> ...]
import assert from "node:assert/strict";
import { quotaWindows } from "../src/core.ts";

const KEY = "orb-dashboard-test-only";
const shapes = [];
for (const base of process.argv.slice(2)) {
  const api = async (path, init = {}) => {
    const r = await fetch(`${base}/v8/management${path}`, {
      ...init,
      headers: { Authorization: `Bearer ${KEY}`, "Content-Type": "application/json" },
    });
    return { status: r.status, server: r.headers.get("x-cpa-version"), body: await r.json() };
  };
  const files = (await api("/credentials")).body.files;
  const cred = files.find((f) => f.name === "claude-research.json");
  assert.ok(cred?.auth_index, `${base}: fixture credential claude-research.json`);
  const r = await api("/requests/api-call", {
    method: "POST",
    body: JSON.stringify({
      authIndex: cred.auth_index,
      method: "GET",
      url: "http://127.0.0.1:9102/api/oauth/usage",
      header: { Authorization: "Bearer $TOKEN$", "anthropic-beta": "oauth-2025-04-20" },
      // The fixture's global proxy is a dead port on purpose; reach the local fake directly.
      proxy_url: "direct",
    }),
  });
  assert.equal(r.status, 200, `${base}: ${JSON.stringify(r.body)}`);
  assert.equal(r.body.status_code, 200);
  const usage = typeof r.body.body === "string" ? JSON.parse(r.body.body) : r.body.body;
  assert.equal(usage.echo_authorization, "Bearer fake-access-not-real", "$TOKEN$ substituted with the credential token");
  const windows = quotaWindows("claude", usage);
  assert.deepEqual(windows.map((w) => [w.label, w.used]), [["Current session", 37], ["Current week (all models)", 12.5]]);
  shapes.push([r.server, Object.keys(r.body).sort().join(","), typeof r.body.body]);
  console.log(`PASS ${r.server} api-call: token substituted, ${windows.length} quota windows parsed`);
}
if (shapes.length > 1) {
  assert.ok(shapes.every((s) => s[1] === shapes[0][1] && s[2] === shapes[0][2]), JSON.stringify(shapes));
  console.log(`PASS response shape identical across servers: {${shapes[0][1]}}, body as ${shapes[0][2]}`);
}
