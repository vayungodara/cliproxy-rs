// Browser contract for the click-only update notice; all API responses are local fakes.
// Usage: node scripts/update-check.mjs <dashboard-url> [capture-directory]
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const [url, directory] = process.argv.slice(2);
assert.ok(url, "dashboard URL required");
const output = directory && resolve(directory);
if (output) mkdirSync(output, { recursive: true });
const browser = (...args) => execFileSync("agent-browser", ["--session", "update-check", ...args], { encoding: "utf8" }).trim();
const evaluate = (code) => JSON.parse(browser("eval", code));
const shot = (name) => output && browser("screenshot", `${output}/${name}.png`, "--full");
try {
  browser("open", url);
  browser("set", "viewport", "1280", "720", "2");
  evaluate(`(() => {
    window.updateCalls = 0;
    window.updateAnswer = {"latest-version":"v0.1.2"};
    window.fetch = async (url, options) => {
      const path = new URL(url, location.href).pathname;
      let value = {}, status = 200;
      if (path.endsWith('/server/latest-version')) {
        window.updateCalls++;
        await new Promise(resolve => { window.finishUpdate = resolve });
        value = window.updateAnswer;
        if (value.error) status = 503;
      } else if (path.endsWith('/config')) value = {"config-version":8,"server":{"host":"127.0.0.1"}};
      else if (path.endsWith('/credentials')) value = {files:[]};
      else if (path.endsWith('/api-keys')) value = {"api-keys":[]};
      else if (options?.method && options.method !== 'GET') status = 400;
      return new Response(JSON.stringify(value), {status, headers:{'Content-Type':'application/json','X-CPA-VERSION':'cliproxy-rs/0.1.2'}});
    };
    return true;
  })()`);
  browser("find", "label", "Management key", "fill", "FAKE-browser-test-key");
  browser("find", "role", "button", "click", "--name", "Connect", "--exact");
  browser("wait", "--text", "Overview");
  browser("click", 'nav a[href="#system"]');
  browser("wait", "--text", "Check for a new release");
  assert.equal(evaluate("window.updateCalls"), 0, "page load must not check");
  shot("updates-before-click");
  const button = "[...document.querySelectorAll('button')].find(b=>b.textContent==='Check for a new release')";
  browser("find", "role", "button", "click", "--name", "Check for a new release");
  browser("wait", "--fn", "window.updateCalls===1");
  assert.equal(evaluate(`${button}.disabled`), true, "disable duplicate clicks while loading");
  evaluate("window.finishUpdate(); true");
  browser("wait", "--text", "this server is up to date");
  shot("updates-current");
  evaluate('window.updateAnswer={"latest-version":"v0.2.0"}; true');
  browser("find", "role", "button", "click", "--name", "Check for a new release");
  browser("wait", "--fn", "window.updateCalls===2");
  evaluate("window.finishUpdate(); true");
  browser("wait", "--text", "v0.2.0");
  assert.equal(evaluate("document.querySelector('main').textContent.includes('this server is up to date')"), false);
  shot("updates-available");
  evaluate(`window.updateAnswer={error:'update_check_disabled',message:'Update checks are disabled by CLIPROXY_NO_UPDATE_CHECK=1.'}; true`);
  browser("find", "role", "button", "click", "--name", "Check for a new release");
  browser("wait", "--fn", "window.updateCalls===3");
  evaluate("window.finishUpdate(); true");
  browser("wait", "--text", "Update checks are disabled");
  assert.equal(evaluate(`${button}.disabled`), true);
  browser("set", "viewport", "390", "844", "2");
  evaluate("new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(()=>r(true))))");
  assert.equal(evaluate("document.documentElement.scrollWidth > innerWidth"), false);
  shot("updates-disabled-narrow");
  console.log("PASS: zero checks on load; one per click; loading blocks duplicate clicks; current, newer and disabled states; no narrow overflow");
} finally {
  browser("close");
}
