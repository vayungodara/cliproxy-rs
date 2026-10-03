// Disposable Rust backend only. No OAuth, refresh, quota, or provider requests.
// Usage: node scripts/browser-check-rust.mjs [page-url] [capture-dir]
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const url = process.argv[2] || "http://127.0.0.1:8317/management.html";
const output = resolve(process.argv[3] || "../../.amp/in/artifacts");
const session = "manage-ui";
mkdirSync(output, { recursive: true });
const browser = (...args) =>
  execFileSync("agent-browser", ["--session", session, ...args], { encoding: "utf8", timeout: 45000 }).trim();
const evaluate = (js) => JSON.parse(browser("eval", js));
const settled = () => {
  browser("wait", "--fn", "!document.querySelector('main [aria-busy=true]')");
  browser("eval", "document.fonts.ready.then(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r))))");
};
const results = [];
// Capability probes Go rejects with 400 before any side effect (see store.probe).
const probes = new Set([
  "POST /v8/management/credentials", "POST /v8/management/credentials/refresh", "PATCH /v8/management/credentials/fields",
  "DELETE /v8/management/credentials", "POST /v8/management/routing/cooldown/reset", "POST /v8/management/requests/api-call",
  "POST /v8/management/oauth/import", "GET /v8/management/oauth/auth-url", "GET /v8/management/observability/usage/queue",
]);
try {
  browser("open", url);
  browser("set", "viewport", "1440", "1000", "2");
  browser(
    "eval",
    `(()=>{window.__calls=[];const original=window.fetch;window.fetch=async(...args)=>{const response=await original(...args);window.__calls.push({path:new URL(args[0],location.href).pathname,method:args[1]?.method||'GET',status:response.status});return response};return true})()`,
  );
  browser("find", "label", "Management key", "fill", "orb-dashboard-test-only");
  browser("find", "role", "button", "click", "--name", "Connect");
  browser("wait", "--text", "Overview");
  browser("wait", "--text", "operator@example.invalid");
  settled();
  // Refuse mutations against a backend other than the disposable fixture.
  assert.equal(evaluate(`document.querySelector('main').textContent.includes('operator@example.invalid')`), true);
  for (const theme of ["dark", "light"]) {
    evaluate(`(()=>{if(document.documentElement.dataset.theme!=='${theme}')document.querySelector('[aria-label^="Use "]').click();return true})()`);
    for (const page of ["overview", "use", "credentials", "providers", "keys", "models", "payload", "config", "logs", "system"]) {
      const before = evaluate("window.__calls.length");
      browser("click", `nav a[href="#${page}"]`);
      browser("wait", "800");
      settled();
      const state = evaluate(
        `({theme:document.documentElement.dataset.theme,route:location.hash,error:document.querySelector('main [role=alert]')?.textContent||'',overflow:document.documentElement.scrollWidth>innerWidth,missing:[...document.querySelectorAll('main .state')].map(e=>e.textContent).filter(t=>t.includes('Not available')).length,calls:window.__calls.slice(${before})})`,
      );
      assert.equal(state.theme, theme);
      assert.equal(state.route, `#${page}`);
      assert.equal(state.error, "", `${page}: ${state.error}`);
      assert.equal(state.overflow, false, `${page}: viewport overflow`);
      // Reads succeed, or the route is honestly reported as not implemented (501/405).
      for (const c of state.calls)
        assert.ok(
          c.status < 400 || [404, 405, 501].includes(c.status) || (c.status === 400 && probes.has(`${c.method} ${c.path}`)),
          `${page}: ${c.method} ${c.path} → ${c.status}`,
        );
      results.push(state);
      console.log(`PASS ${theme} ${page}: no error banner or overflow; ${state.missing} not-available state(s)`);
      if (page === "overview" && theme === "dark") browser("screenshot", `${output}/rust-dashboard-desktop.png`, "--full");
    }
  }
  // Each credential action is offered exactly when the server implements it. The expected
  // answer comes from sending the same side-effect-free probe directly (Go rejects it with
  // 400; a server without the route answers an empty 404), not from the UI under test.
  const base = new URL(url).origin + "/v8/management";
  const implemented = async (method, path) => {
    const r = await fetch(base + path, {
      method,
      headers: { Authorization: "Bearer orb-dashboard-test-only", "Content-Type": "application/json" },
      body: "{}",
    });
    const text = await r.text();
    return !([404, 405, 501].includes(r.status) && (r.status !== 404 || !text));
  };
  const actions = [
    ["POST", "/credentials", "upload", `document.querySelector('main label.key[aria-disabled]')?.getAttribute('aria-disabled')==='true'`],
    ["POST", "/credentials/refresh", "token refresh", `[...document.querySelectorAll('main .head button')].find(b=>b.textContent.includes('Refresh tokens')).disabled`],
    ["PATCH", "/credentials/fields", "editing fields", `document.querySelector('.detail form').inert`],
    ["POST", "/routing/cooldown/reset", "cooldown reset", `[...document.querySelectorAll('.detail button')].find(b=>b.textContent==='Reset cooldown').disabled`],
    ["DELETE", "/credentials", "delete", `[...document.querySelectorAll('.detail button')].find(b=>b.textContent==='Delete').disabled`],
  ];
  browser("click", 'nav a[href="#credentials"]');
  settled();
  browser("eval", "location.hash='credentials/claude-research.json'");
  browser("wait", ".detail");
  browser("wait", "2500");
  const missingLine = evaluate("[...document.querySelectorAll('main .note')].map(n=>n.textContent).find(t=>t.includes('Not available'))||''");
  const summary = [];
  for (const [method, path, name, disabledJs] of actions) {
    const has = await implemented(method, path);
    assert.equal(evaluate(disabledJs), !has, `${method} ${path}: UI ${has ? "disables an implemented" : "offers a missing"} action`);
    assert.equal(missingLine.includes(name), !has, `${method} ${path}: "not available" line ${has ? "names an implemented" : "omits a missing"} action`);
    summary.push(`${name} ${has ? "on" : "off"}`);
  }
  console.log(`PASS credential actions match server capabilities (${summary.join(", ")})`);

  // A route the server lacks is read once, not retried in a loop.
  const before = evaluate("window.__calls.length");
  browser("click", 'nav a[href="#quotas"]');
  browser("wait", "4000");
  const pluginReads = evaluate(`window.__calls.slice(${before}).filter(c=>c.path.endsWith('/plugins')&&c.method==='GET').length`);
  assert.ok(pluginReads <= 1, `GET /plugins read ${pluginReads} times`);
  console.log(`PASS unsupported reads are not retried in a loop (${pluginReads} read of GET /plugins)`);

  browser("click", 'nav a[href="#payload"]');
  settled();
  browser("find", "role", "button", "click", "--name", "Add rule");
  browser("wait", "--text", "Rule added.");
  assert.equal(evaluate("document.querySelectorAll('main .rule').length"), 1);
  browser("find", "role", "button", "click", "--name", "Remove", "--exact");
  browser("wait", "--fn", "document.querySelectorAll('main .rule').length===0");
  console.log("PASS payload add/remove: persisted real Rust writes");

  browser("click", 'nav a[href="#config"]');
  settled();
  browser("find", "role", "button", "click", "--name", "Edit YAML");
  browser("wait", "textarea");
  const original = evaluate("document.querySelector('textarea').value");
  assert.ok(original.includes("strategy: round-robin"));
  browser("fill", "textarea", original.replace("strategy: round-robin", "strategy: fill-first"));
  browser("find", "role", "button", "click", "--name", "Review changes");
  browser("wait", ".diff .added");
  assert.ok(evaluate("document.querySelectorAll('.diff .added').length") > 0);
  browser("screenshot", `${output}/rust-dashboard-diff.png`, "--full");
  browser("find", "role", "button", "click", "--name", "Keep editing");
  browser("fill", "textarea", original);
  browser("find", "role", "button", "click", "--name", "Close");
  console.log("PASS YAML editor: persisted tree, real diff, discarded without writing");

  browser("click", 'nav a[href="#credentials"]');
  settled();
  browser("set", "viewport", "390", "844", "2");
  settled();
  assert.equal(evaluate("document.documentElement.scrollWidth>innerWidth"), false);
  browser("screenshot", `${output}/rust-dashboard-narrow.png`, "--full");
  const storage = evaluate("({local:Object.keys(localStorage),session:Object.keys(sessionStorage)})");
  assert.deepEqual(storage.session, []);
  assert.ok(storage.local.every((k) => k === "cliproxy-theme"));
  console.log("PASS 390px Chromium: no overflow; management key absent from storage");
  writeFileSync(`${output}/rust-dashboard-checks.json`, JSON.stringify(results, null, 2));
} finally {
  browser("close");
}
