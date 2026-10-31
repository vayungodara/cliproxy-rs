// Dashboard quota regression checks. Management responses are fake; no provider is called.
// Usage: node scripts/quota-check.mjs [dashboard-url] [capture-dir]
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const url = process.argv[2] || "http://127.0.0.1:5173";
const output = process.argv[3] && resolve(process.argv[3]);
if (output) mkdirSync(output, { recursive: true });
const browser = (...args) => execFileSync("agent-browser", ["--session", "quota-check", ...args], { encoding: "utf8", timeout: 45000 }).trim();
const evaluate = (js) => JSON.parse(browser("eval", js));
const settle = () => browser("eval", "document.fonts.ready.then(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r))))");
const click = (name) => { browser("find", "role", "button", "click", "--name", name, "--exact"); settle(); };
const shot = (name) => { if (output) browser("screenshot", `${output}/${name}.png`, "--full"); };
const meters = () => evaluate("[...document.querySelectorAll('main [role=meter]')].map(e=>Number(e.getAttribute('aria-valuenow')))");
const text = () => evaluate("document.querySelector('main').textContent");
const errors = () => evaluate("[...document.querySelectorAll('main .note.error')].map(e=>e.textContent)");

try {
  browser("open", url);
  browser("set", "viewport", "1440", "1000", "2");
  evaluate(`(()=>{
    window.quotaFixture = {
      status: 200, used: 37, calls: 0, now: Date.parse('2026-10-31T12:01:00Z'),
      files: [
        {name:'claude-setup.json', auth_index:'claude-test', type:'claude', status:'active', success:1},
        {name:'codex.json', auth_index:'codex-test', type:'codex', status:'active', success:1, quota:{observed_at:'2026-10-31T12:00:00Z', signals:{'x-codex-primary-used-percent':'24','x-codex-primary-window-minutes':'300'}}}
      ]
    };
    Date.now = () => window.quotaFixture.now;
    const original = window.fetch;
    window.fetch = async (input, init) => {
      const path = new URL(input, location.href).pathname;
      if (!path.includes('/v8/management/')) return original(input, init);
      const f = window.quotaFixture;
      let body;
      if (path.endsWith('/config')) body = {};
      else if (path.endsWith('/credentials')) body = {files:f.files};
      else if (path.endsWith('/plugins')) body = {plugins:[]};
      else if (path.endsWith('/requests/api-call')) {
        f.calls++;
        const request = JSON.parse(init.body);
        body = {status_code:f.status, body:JSON.stringify(request.authIndex === 'claude-test' ? {five_hour:{utilization:f.used}} : {rate_limit:{primary_window:{used_percent:f.used}}})};
      } else throw new Error('Unexpected management request: '+path);
      return new Response(JSON.stringify(body), {headers:{'Content-Type':'application/json','X-CPA-COMMIT':'fixture'}});
    };
    return true;
  })()`);
  browser("find", "label", "Management key", "fill", "quota-test-only");
  click("Connect");
  browser("wait", "--text", "claude-setup.json");
  assert.equal(evaluate("window.quotaFixture.calls"), 0, "no automatic quota checks");
  assert.deepEqual(meters(), [24]);
  click("Check limits");
  assert.deepEqual(meters(), [37, 37]);
  shot("quota-success");

  evaluate("window.quotaFixture.status = 403; window.quotaFixture.now += 600000");
  click("Check limits");
  assert.deepEqual(meters(), [37, 37], "failed checks retain the last successful windows");
  assert.match(evaluate("document.querySelector('.limits .name small').textContent"), /10m ago/, "a failed check does not update the observation time");
  assert.match(text(), /setup tokens/i);
  assert.match(text(), /OAuth/);
  assert.doesNotMatch(errors()[0], /Refresh the token/);
  shot("quota-overview-failed");
  console.log("PASS Overview: failed checks keep last live limits and their timestamp, alongside scope-aware advice");

  // A first failed check must fall back to passive data, not just retained live data.
  click("Sign out");
  browser("find", "label", "Management key", "fill", "quota-test-only");
  click("Connect");
  browser("click", 'nav a[href="#quotas"]');
  settle();
  click("Check all");
  assert.deepEqual(meters(), [24], "passive limits survive a failed first check");
  assert.match(text(), /Observed 11m ago/);
  assert.doesNotMatch(text(), /Checked /);
  assert.equal(errors().length, 2);
  shot("quota-passive-failed");
  evaluate("window.quotaFixture.files[1].quota.signals['x-codex-primary-used-percent']='41'");
  browser("wait", "--fn", "document.querySelector('main [role=meter]')?.getAttribute('aria-valuenow') === '41'");
  settle();
  assert.deepEqual(meters(), [41], "new passive observations are still shown after a failed check");
  console.log("PASS Quotas: first failure preserves passive limits, newer observations and both errors; never labels them Checked");

  evaluate("window.quotaFixture.status = 200; window.quotaFixture.used = 62");
  click("Check all");
  assert.deepEqual(meters(), [62, 62]);
  assert.deepEqual(errors(), []);
  assert.match(text(), /Checked /);
  console.log("PASS successful retry replaces old limits and clears the errors");

  for (const status of [401, 500]) {
    evaluate(`window.quotaFixture.status = ${status}`);
    click("Check all");
    assert.deepEqual(meters(), [62, 62]);
    assert.match(errors()[0], status === 401 ? /Refresh the token/ : /Try again later/);
  }
  console.log("PASS non-scope failures keep their existing advice without erasing limits");
  browser("set", "viewport", "390", "844", "2");
  settle();
  assert.equal(evaluate("document.documentElement.scrollWidth > innerWidth"), false);
  shot("quota-narrow-failed");
  console.log("PASS 390px Chromium: errors and retained limits have no horizontal overflow");
} finally {
  browser("close");
}
