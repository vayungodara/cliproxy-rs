// Dashboard quota regression checks on Overview and Accounts. Management responses are fake;
// no provider is called.
// Usage: CHROME_PATH=... node scripts/quota-check.mjs [dashboard-url] [capture-dir]
import { chromium } from "playwright-core";
import assert from "node:assert/strict";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";

const url = process.argv[2] || "http://127.0.0.1:5173";
const output = process.argv[3] && resolve(process.argv[3]);
if (output) mkdirSync(output, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 1000 }, deviceScaleFactor: 2 })).newPage();
const main = page.locator("main");
const evaluate = (js) => page.evaluate(js);
const settle = () => page.evaluate(() => document.fonts.ready.then(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))));
// A check runs in the background; wait until its button reads as idle again.
const click = async (name) => {
  await page.getByRole("button", { name, exact: true }).click();
  await page.waitForFunction(() => ![...document.querySelectorAll("main button")].some((b) => b.textContent.startsWith("Checking")));
  await settle();
};
// Each account is checked at most once every 2 minutes; move the fixture's clock past that.
const later = () => evaluate("window.quotaFixture.now += 180000");
const shot = (name) => output && page.screenshot({ path: `${output}/${name}.png`, fullPage: true });
// Overview's limits, or the Accounts rows' bars (not the open drawer's full list).
const meters = () => evaluate("[...document.querySelectorAll('main .limits [role=meter], main .acct .item [role=meter]')].map(e=>Number(e.getAttribute('aria-valuenow')))");
const text = () => evaluate("document.querySelector('main').textContent");
const errors = () => evaluate("[...document.querySelectorAll('main .note.error')].map(e=>e.textContent)");
const signIn = async () => {
  await page.getByLabel("Management key").fill("quota-test-only");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await main.getByText("claude-setup.json").first().waitFor();
};
// Accounts: one detail drawer opens at a time; a failed read is explained there.
const rowErrors = async () => {
  const out = [];
  for (const name of ["claude-setup.json", "codex.json"]) {
    const row = main.locator(".acct button.name", { hasText: name });
    if ((await row.getAttribute("aria-expanded")) !== "true") await row.click();
    await settle();
    out.push(...(await errors()));
  }
  return out;
};

try {
  await page.goto(url);
  await evaluate(`(()=>{
    window.quotaFixture = {
      status: 200, used: 37, calls: 0, hold: false, deleted: [], patched: [], refreshed: [], now: Date.parse('2026-10-31T12:01:00Z'),
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
      const json = (b, status = 200) => new Response(JSON.stringify(b), {status, headers:{'Content-Type':'application/json','X-CPA-COMMIT':'fixture'}});
      const sent = init?.body && typeof init.body === 'string' ? JSON.parse(init.body) : {};
      if (path.endsWith('/config')) body = {};
      else if (path.endsWith('/credentials') && init?.method === 'DELETE') {
        f.deleted.push(...sent.names);
        if (sent.names.includes('codex.json')) return json({error:'file is busy'}, 409);
        f.files = f.files.filter((x) => !sent.names.includes(x.name));
        body = {status:'ok'};
      } else if (path.endsWith('/credentials/status')) {
        f.patched.push(sent.auth_index);
        // multi.json behaves like a plugin source: one request switches the whole file. Its
        // first request fails without changing anything.
        if (sent.auth_index === 'multi-a' && !f.failedOnce) return (f.failedOnce = true), json({error:'source busy'}, 500);
        f.files.filter((x) => (sent.name === 'multi.json' ? x.name === sent.name : x.auth_index === sent.auth_index)).forEach((x) => (x.disabled = sent.disabled));
        // The change lands, then the account list stops answering.
        if (f.listDownAfterPatch) f.listDown = true;
        body = {status:'ok'};
      } else if (path.endsWith('/credentials/refresh')) {
        f.refreshed.push(sent.auth_index);
        if (sent.auth_index === 'codex-test') return json({error:'refresh refused'}, 409);
        body = {status:'ok'};
      } else if (path.endsWith('/credentials')) {
        if (f.listDown) return json({error:'list unavailable'}, 500);
        body = {files:f.files};
      }
      else if (path.endsWith('/plugins')) body = {plugins:[]};
      else if (path.endsWith('/requests/api-call')) {
        f.calls++;
        // A held request answers only once the test releases it.
        if (f.hold) await new Promise((r) => (f.release = r));
        const request = JSON.parse(init.body);
        body = {status_code:f.status, body:JSON.stringify(request.authIndex === 'claude-test' ? {five_hour:{utilization:f.used}} : {rate_limit:{primary_window:{used_percent:f.used}}})};
      } else throw new Error('Unexpected management request: '+path);
      return new Response(JSON.stringify(body), {headers:{'Content-Type':'application/json','X-CPA-COMMIT':'fixture'}});
    };
    return true;
  })()`);
  await signIn();
  assert.equal(await evaluate("window.quotaFixture.calls"), 0, "no automatic quota checks");
  assert.deepEqual(await meters(), [24]);
  await click("Check limits");
  assert.deepEqual(await meters(), [37, 37]);
  await shot("quota-success");

  await evaluate("window.quotaFixture.status = 403; window.quotaFixture.now += 600000");
  await click("Check limits");
  assert.deepEqual(await meters(), [37, 37], "failed checks retain the last successful windows");
  assert.match(await evaluate("document.querySelector('.limits .name small').textContent"), /10m ago/, "a failed check does not update the observation time");
  assert.match(await text(), /setup tokens/i);
  assert.match(await text(), /OAuth/);
  assert.doesNotMatch((await errors())[0], /Refresh the token/);
  await shot("quota-overview-failed");
  console.log("PASS Overview: failed checks keep last live limits and their timestamp, alongside scope-aware advice");

  // A first failed check must fall back to passive data, not just retained live data.
  await page.getByRole("button", { name: "Sign out", exact: true }).click();
  await signIn();
  await page.evaluate(() => (location.hash = "#accounts"));
  await main.getByRole("heading", { name: /^Accounts/ }).waitFor();
  await later();
  await click("Check quotas");
  assert.deepEqual(await meters(), [24], "passive limits survive a failed first check");
  assert.match(await text(), /seen 1\dm ago/);
  assert.doesNotMatch(await text(), /(?<!never )checked/);
  assert.equal((await rowErrors()).length, 2);
  await shot("quota-passive-failed");
  await evaluate("window.quotaFixture.files[1].quota.signals['x-codex-primary-used-percent']='41'");
  await page.waitForFunction(() => document.querySelector("main [role=meter]")?.getAttribute("aria-valuenow") === "41", null, { timeout: 15_000 });
  await settle();
  assert.deepEqual(await meters(), [41], "new passive observations are still shown after a failed check");
  console.log("PASS Accounts: first failure preserves passive limits, newer observations and both errors; never labels them checked");

  await evaluate("window.quotaFixture.status = 200; window.quotaFixture.used = 62");
  await later();
  await click("Check quotas");
  assert.deepEqual(await meters(), [62, 62]);
  assert.deepEqual(await rowErrors(), []);
  assert.match(await text(), /checked just now/);
  console.log("PASS successful retry replaces old limits and clears the errors");

  // Within 2 minutes the same accounts are not asked again.
  const calls = await evaluate("window.quotaFixture.calls");
  await click("Check quotas");
  assert.equal(await evaluate("window.quotaFixture.calls"), calls, "no second check within 2 minutes");
  console.log("PASS a second check within 2 minutes sends nothing");

  for (const status of [401, 500]) {
    await evaluate(`window.quotaFixture.status = ${status}`);
    await later();
    await click("Check quotas");
    assert.deepEqual(await meters(), [62, 62]);
    assert.match((await rowErrors())[0], status === 401 ? /Refresh the token/ : /Try again later/);
  }
  console.log("PASS non-scope failures keep their existing advice without erasing limits");

  // The drawer's Check quota waits 2 minutes, then comes back by itself.
  await evaluate("window.quotaFixture.status = 200");
  await later();
  const drawerCheck = main.locator(".detail").getByRole("button", { name: /^Check quota|^Checked/ });
  await page.waitForFunction(() => [...document.querySelectorAll(".detail button")].some((b) => !b.disabled && b.textContent === "Check quota"), null, { timeout: 15_000 });
  const before = await evaluate("window.quotaFixture.calls");
  await drawerCheck.click();
  assert.equal(await evaluate("window.quotaFixture.calls"), before + 1);
  // The fixture's clock stands still between steps; move it so the page's 10 s tick sees time pass.
  await evaluate("window.quotaFixture.now += 1000");
  await page.waitForFunction(() => [...document.querySelectorAll(".detail button")].some((b) => b.disabled && b.textContent.startsWith("Checked; again in")), null, { timeout: 15_000 });
  await later();
  await page.waitForFunction(() => [...document.querySelectorAll(".detail button")].some((b) => !b.disabled && b.textContent === "Check quota"), null, { timeout: 15_000 });
  console.log("PASS the drawer's Check quota is disabled for 2 minutes and re-enables by itself");

  // The 2-minute wait counts from when a request is sent, not when it was queued, and a
  // queued account cannot be queued again from its drawer.
  await later();
  const start = await evaluate("window.quotaFixture.calls");
  await evaluate("window.quotaFixture.hold = true");
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.waitForFunction((n) => window.quotaFixture.calls === n + 1 && window.quotaFixture.release, start);
  // The open drawer is codex.json's, waiting behind claude-setup.json.
  await page.waitForFunction(() => [...document.querySelectorAll(".detail button")].some((b) => b.disabled && b.textContent === "Queued"), null, { timeout: 15_000 });
  await evaluate("window.quotaFixture.now += 90000; window.quotaFixture.release()");
  await page.waitForFunction((n) => window.quotaFixture.calls === n + 2, start);
  await page.waitForFunction(() => [...document.querySelectorAll(".detail button")].some((b) => b.disabled && b.textContent === "Checking…"), null, { timeout: 15_000 });
  await evaluate("window.quotaFixture.hold = false; window.quotaFixture.release()");
  await page.waitForFunction(() => ![...document.querySelectorAll("main button")].some((b) => b.textContent.startsWith("Checking")));
  assert.equal(await evaluate("window.quotaFixture.calls"), start + 2, "the drawer adds nothing to a queued check");
  await evaluate("window.quotaFixture.now += 40000");
  await click("Check quotas");
  assert.equal(await evaluate("window.quotaFixture.calls"), start + 3, "only the account sent 130 s ago is due; the one sent 40 s ago waits");
  console.log("PASS the 2-minute wait starts when the request is sent, a queued account is not queued twice, and the drawer says Queued, then Checking");

  // An account disabled while it waits in line is not read.
  await later();
  const start2 = await evaluate("window.quotaFixture.calls");
  await evaluate("window.quotaFixture.hold = true");
  await page.getByRole("button", { name: "Check quotas", exact: true }).click();
  await page.waitForFunction((n) => window.quotaFixture.calls === n + 1 && window.quotaFixture.release, start2);
  await evaluate("window.quotaFixture.files[1].disabled = true");
  await page.waitForFunction(() => document.querySelector("main .acct .off-row"), null, { timeout: 15_000 });
  await evaluate("window.quotaFixture.hold = false; window.quotaFixture.release()");
  await page.waitForFunction(() => ![...document.querySelectorAll("main button")].some((b) => b.textContent.startsWith("Checking")));
  assert.equal(await evaluate("window.quotaFixture.calls"), start2 + 1, "the account disabled while queued is not read");
  await evaluate("window.quotaFixture.files[1].disabled = false");
  await page.waitForFunction(() => !document.querySelector("main .acct .off-row"), null, { timeout: 15_000 });
  console.log("PASS an account disabled while queued is skipped when its turn comes");

  // Batches: a shared file is one unit and every account it holds is named; failures do not
  // stop the rest.
  page.on("dialog", (d) => d.accept());
  await evaluate(`window.quotaFixture.files.push(
    {name:'multi.json', auth_index:'multi-a', type:'gemini-cli', email:'one@example.com', status:'active'},
    {name:'multi.json', auth_index:'multi-b', type:'gemini-cli', email:'two@example.com', status:'active'},
    {name:'kimi.json', auth_index:'kimi-test', type:'kimi', status:'active'})`);
  await main.getByLabel("Select one@example.com").waitFor({ timeout: 15_000 });
  await main.getByLabel("Select one@example.com").check();
  assert.match(await main.locator(".batch").textContent(), /1 selected\..*Actions also reach two@example\.com, in the same files/);
  assert.equal(await evaluate("document.querySelector('[role=status].sr').textContent"), "1 selected.");
  assert.equal(await evaluate("document.querySelector('input[aria-label=\"Select all shown\"]').indeterminate"), true);
  // The first request fails, the second switches the whole file: both accounts changed, and
  // the toast says so.
  await main.locator(".batch").getByRole("button", { name: "Disable 2" }).click();
  await page.getByText("2 accounts disabled.").waitFor();
  assert.deepEqual(await evaluate("window.quotaFixture.patched"), ["multi-a", "multi-b"]);
  // One request switches the file back; the sibling is not asked again.
  await main.locator(".batch").getByRole("button", { name: "Enable 2" }).click();
  await page.getByText("2 accounts enabled.").waitFor();
  assert.deepEqual(await evaluate("window.quotaFixture.patched"), ["multi-a", "multi-b", "multi-a"]);
  console.log("PASS a status change on a shared file reaches, names and counts every account by what the server reports");

  // The PATCH succeeds but the reread fails: no second PATCH from stale state, and no guessed counts.
  await evaluate("window.quotaFixture.listDownAfterPatch = true");
  await main.locator(".batch").getByRole("button", { name: "Disable 2" }).click();
  await page.getByText("The result could not be confirmed: the account list did not reload. Check it before trying again.").waitFor();
  assert.deepEqual(await evaluate("window.quotaFixture.patched"), ["multi-a", "multi-b", "multi-a", "multi-a"]);
  assert.doesNotMatch(await text(), /failed: one@example|0 accounts disabled/);
  await evaluate("window.quotaFixture.listDownAfterPatch = window.quotaFixture.listDown = false; window.quotaFixture.files.filter((x) => x.name === 'multi.json').forEach((x) => (x.disabled = false))");
  await page.waitForFunction(() => !document.querySelector("main .acct .off-row"), null, { timeout: 15_000 });
  console.log("PASS a status batch whose reread fails stops and says the result is unconfirmed");

  await main.getByLabel("Select all shown").check();
  assert.equal(await evaluate("document.querySelector('input[aria-label=\"Select all shown\"]').indeterminate"), false);
  await main.locator(".batch").getByRole("button", { name: "Refresh tokens for 5" }).click();
  await page.getByText("Tokens refreshed for 3 files. 1 failed: codex.json (refresh refused)").waitFor();
  assert.deepEqual((await evaluate("window.quotaFixture.refreshed")).sort(), ["claude-test", "codex-test", "kimi-test", "multi-a"]);
  console.log("PASS a batch refresh goes on past a failure, once per file, and names what failed");

  await main.locator(".batch").getByRole("button", { name: "Delete 5…" }).click();
  await page.getByText("3 files deleted. 1 failed: codex.json (file is busy)").waitFor();
  assert.deepEqual((await evaluate("window.quotaFixture.deleted")).sort(), ["claude-setup.json", "codex.json", "kimi.json", "multi.json"]);
  assert.match(await main.locator(".batch").textContent(), /^\s*1 selected\./);
  assert.equal(await main.getByLabel("Select codex.json").isChecked(), true);
  console.log("PASS a batch delete removes each file once, goes on past a failure and keeps the failed account selected");
  await page.setViewportSize({ width: 390, height: 844 });
  await settle();
  assert.equal(await evaluate("document.documentElement.scrollWidth > innerWidth"), false);
  await shot("quota-narrow-failed");
  console.log("PASS 390px Chromium: errors and retained limits have no horizontal overflow");
} finally {
  await browser.close();
}
