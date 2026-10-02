import assert from "node:assert/strict";

// Intentionally restricted to a disposable local backend. Never point this at production.
const base = "http://127.0.0.1:8317";
const key = "orb-dashboard-test-only";
let checks = 0;
async function call(path, method = "GET", body, expected = 200, text = false) {
  const yaml = path === "/config.yaml";
  const headers = { Authorization: `Bearer ${key}` };
  if (body !== undefined && !(body instanceof FormData))
    headers["Content-Type"] = yaml ? "application/yaml" : "application/json";
  const response = await fetch(base + "/v8/management" + path, {
    method,
    headers,
    body:
      body === undefined
        ? undefined
        : body instanceof FormData || yaml
          ? body
          : JSON.stringify(body),
    signal: AbortSignal.timeout(45000),
  });
  const content = await response.text();
  assert.equal(
    response.status,
    expected,
    `${method} ${path}: ${content.slice(0, 200)}`,
  );
  console.log(`${response.status} ${method} ${path.split("?")[0]}`);
  checks++;
  return text ? content : content ? JSON.parse(content) : {};
}

const config = await call("/config");
assert.equal(
  config.oauth?.["auth-dir"],
  "/tmp/cliproxy-backend/auth",
  "Not the disposable backend. Refusing mutations.",
);
const yaml = await call("/config.yaml", "GET", undefined, 200, true);
let uploaded = false;
try {
  await call("/credentials");
  await call("/observability/usage/api-keys");
  await call("/observability/usage/queue?count=50");
  await call("/plugins");
  await call("/plugins/store");
  await call("/server/latest-version");
  const upstream = await call("/requests/api-call", "POST", {
    method: "GET",
    url: base + "/healthz",
  });
  assert.equal(upstream.status_code, 200);
  assert.equal(JSON.parse(upstream.body).status, "ok");

  await call("/config/routing/retry/request-retry", "PUT", 0);
  assert.equal(await call("/config/routing/retry/request-retry"), 0);
  await call("/config/routing/strategy", "PUT", "fill-first");
  assert.equal(await call("/config/routing/strategy"), "fill-first");
  await call("/config", "PATCH", { routing: { "session-affinity": true } });
  assert.equal((await call("/config")).routing["session-affinity"], true);
  await call("/config/routing/session-affinity", "DELETE");
  await call("/config/routing/retry/request-retry", "PUT", { value: 4 }, 422);
  assert.equal(
    await call("/config/routing/retry/request-retry"),
    0,
    "Rejected write changed state.",
  );

  await call("/config/access/api-keys", "PUT", [
    "orb-client-test-only",
    "second-disposable-client",
  ]);
  assert.equal((await call("/config/access/api-keys")).length, 2);
  for (const family of [
    "claude",
    "codex",
    "gemini",
    "vertex",
    "openai-compatibility",
    "interactions",
    "xai",
    "meta",
  ]) {
    const group = {
      name: "disposable",
      "base-url": base,
      "excluded-models": ["*"],
      keys: [{ "api-key": "not-a-provider-credential" }],
    };
    if (family === "openai-compatibility") {
      delete group["excluded-models"];
      group.disabled = true;
      group.models = [{ name: "test-model", alias: "test-model" }];
    }
    await call(`/config/api-keys/${family}`, "PUT", [group]);
    assert.equal(
      (await call(`/config/api-keys/${family}`))[0].name,
      "disposable",
    );
    await call(`/config/api-keys/${family}`, "PUT", []);
  }
  await call("/config/oauth/model-alias/claude", "PUT", [
    { name: "claude-sonnet-4-5", alias: "daily-driver", fork: true },
  ]);
  assert.equal(
    (await call("/config/oauth/model-alias/claude"))[0].alias,
    "daily-driver",
  );
  await call("/config/oauth/excluded-models/claude", "PUT", ["*-preview"]);
  await call("/routing/model-definitions/claude");
  for (const kind of [
    "default",
    "default-raw",
    "override",
    "override-raw",
    "filter",
  ]) {
    await call(`/config/requests/payload/${kind}`, "PUT", [
      {
        models: [{ name: "*", protocol: "openai" }],
        params:
          kind === "filter"
            ? ["metadata"]
            : { temperature: kind.endsWith("-raw") ? "0.7" : 0.7 },
      },
    ]);
    assert.equal((await call(`/config/requests/payload/${kind}`)).length, 1);
  }

  const form = new FormData();
  form.append(
    "file",
    new Blob(
      [
        JSON.stringify({
          type: "dashboard-test",
          email: "test@example.test",
          disabled: true,
          refresh_token: "synthetic-not-a-real-token",
        }),
      ],
      { type: "application/json" },
    ),
    "dashboard-test.json",
  );
  await call("/credentials", "POST", form);
  uploaded = true;
  let credential = (
    await call("/credentials?name=dashboard-test.json")
  ).files.find((a) => a.name === "dashboard-test.json");
  assert.ok(credential);
  await call("/credentials/download?name=dashboard-test.json");
  await call("/credentials/status", "PATCH", {
    name: credential.name,
    auth_index: credential.auth_index,
    disabled: false,
  });
  assert.equal(
    (await call("/credentials?name=dashboard-test.json")).files[0].disabled,
    false,
  );
  await call("/credentials/fields", "PATCH", {
    name: credential.name,
    note: "Disposable test",
    priority: 7,
  });
  assert.equal(
    (await call("/credentials?name=dashboard-test.json")).files[0].note,
    "Disposable test",
  );
  await call("/credentials/models?name=dashboard-test.json");
  await call("/routing/cooldown/reset", "POST", {
    auth_index: credential.auth_index,
  });
  const refresh = await call("/credentials/refresh", "POST", { all: true });
  assert.ok(refresh.ok);
  assert.ok(
    refresh.results.some((r) => r.success === false),
    "Synthetic provider must not claim token refresh success.",
  );
  await call("/credentials", "DELETE", { names: ["dashboard-test.json"] });
  uploaded = false;

  for (const provider of ["codex", "claude"]) {
    const login = await call(
      `/oauth/auth-url?provider=${provider}&is_webui=true`,
    );
    assert.ok(new URL(login.url).protocol === "https:");
    assert.equal(
      (await call(`/oauth/status?state=${login.state}`)).status,
      "wait",
    );
    await call("/oauth/callback", "POST", {
      provider,
      state: login.state,
      error: "access_denied",
    });
    await call(`/oauth/session?state=${login.state}`, "DELETE");
  }
  const invalidImport = new FormData();
  invalidImport.append(
    "file",
    new Blob(["{}"], { type: "application/json" }),
    "invalid.json",
  );
  await call("/oauth/import?provider=vertex", "POST", invalidImport, 400);
  const log = await call("/observability/logs?limit=20");
  assert.ok(Array.isArray(log.lines));
  const incremental = await call(
    `/observability/logs?limit=20&cursor=${encodeURIComponent(log["next-cursor"] || "")}`,
  );
  assert.ok(Array.isArray(incremental.lines));
  await call("/observability/logs/errors");
  await call(
    "/observability/logs/requests/no-such-test-request",
    "GET",
    undefined,
    404,
  );
  await call(
    "/plugins/dashboard-test/quota?auth_index=no-such-account",
    "GET",
    undefined,
    404,
  );
  await call("/plugins/dashboard-test", "DELETE", undefined, 404);
  console.log(
    `PASS: ${checks} endpoint checks, including write/readback, rejection, OAuth start/poll/callback/cancel, credential CRUD, and cursor logs.`,
  );
} finally {
  if (uploaded)
    await call("/credentials", "DELETE", { names: ["dashboard-test.json"] });
  await call("/config.yaml", "PUT", yaml);
  // YAML PUT normalizes and materializes defaults. Verify the actual original settings.
  const restored = await call("/config");
  for (const section of [
    "server",
    "management",
    "access",
    "routing",
    "observability",
  ]) {
    const check = (before, after) => {
      for (const [name, value] of Object.entries(before)) {
        if (value && typeof value === "object" && !Array.isArray(value))
          check(value, after[name]);
        else
          assert.deepEqual(
            after[name],
            value,
            `Restore failed for ${section}.${name}`,
          );
      }
    };
    check(config[section], restored[section]);
  }
  assert.equal(Object.values(restored["api-keys"] || {}).flat().length, 0);
  console.log(
    "PASS: original disposable settings restored (YAML normalization allowed).",
  );
}
