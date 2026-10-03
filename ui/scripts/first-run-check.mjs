// First-time user walkthrough on fresh servers. Disposable servers only; run inside
// scripts/isolated.sh so nothing leaves the machine.
//
//   <off-url>   a server with no management key (management API off)
//   <fresh-url> the login-harness build on an empty config with a management key, no
//               accounts and no client keys; scripts/fake-logins.mjs answers its Meta
//               sign-in and mints a key whose base URL is the mock upstream on 9101.
//
// The walk: the sign-in page explains a server with management off; on the fresh server
// the overview's Get started list starts at 0 of 3; creating a client key, signing in to
// Meta and sending one request through the proxy tick off the three steps, after which
// the list disappears and the overview shows the request. Screenshots at desktop and
// phone width in both themes.
//
// Usage: CHROME_PATH=... node scripts/first-run-check.mjs <off-url> <fresh-url> <management-key> [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const [offUrl, freshUrl, secret, dir] = process.argv.slice(2);
assert.ok(offUrl && freshUrl && secret, "usage: first-run-check.mjs <off-url> <fresh-url> <management-key> [capture-dir]");
const out = resolve(dir || "first-run");
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const desktop = { viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 };
const phone = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true };
const pass = (t) => console.log(`PASS ${t}`);
const errors = [];

async function open(url, device, theme) {
  const context = await browser.newContext({ ...device, colorScheme: theme });
  await context.addInitScript((t) => localStorage.setItem("cliproxy-theme", t), theme);
  const page = await context.newPage();
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(url);
  return page;
}
async function signIn(page) {
  await page.getByLabel("Management key").fill(secret);
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.locator("main h1", { hasText: "Overview" }).waitFor();
}
const shot = (page, name) => page.screenshot({ path: `${out}/${name}.png`, fullPage: true });
const overflow = (page) => page.evaluate(() => document.documentElement.scrollWidth > innerWidth);

try {
  // 1. A server started without a management key.
  {
    const page = await open(offUrl, desktop, "light");
    await page.getByLabel("Management key").fill("anything");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    const alert = page.locator('.login [role="alert"]');
    await alert.waitFor();
    const message = await alert.textContent();
    assert.match(message, /management API is off/i);
    assert.match(message, /secret-key/);
    assert.equal(await page.locator(".toast").count(), 0, "no second message as a toast");
    await shot(page, "01-login-management-off-desktop-light");
    pass(`management off: "${message.trim()}"`);
    await page.context().close();
  }

  // 2. A fresh server: nothing connected yet.
  const page = await open(freshUrl, desktop, "light");
  const main = page.locator("main");
  await signIn(page);
  const start = main.locator("section", { has: page.getByRole("heading", { name: /Get started/ }) });
  await start.waitFor();
  assert.match(await start.textContent(), /0 of 3 done/);
  assert.equal(await main.getByRole("link", { name: "Connect account" }).first().isVisible(), true);
  await shot(page, "02-overview-fresh-desktop-light");
  for (const [theme, device, name] of [
    ["dark", desktop, "03-overview-fresh-desktop-dark"],
    ["light", phone, "04-overview-fresh-phone-light"],
    ["dark", phone, "05-overview-fresh-phone-dark"],
  ]) {
    const p = await open(freshUrl, device, theme);
    await signIn(p);
    await p.getByRole("heading", { name: /Get started/ }).waitFor();
    assert.equal(await overflow(p), false, `${name}: no horizontal overflow`);
    await shot(p, name);
    await p.context().close();
  }
  pass("fresh server: Get started shows 0 of 3 in both themes, desktop and phone, without overflow");

  // 3. Step 2 first, by keyboard. Both places that create a key remove the pressed button,
  //    so focus must move on to the next action. Use with tools first, then reset the key
  //    list and do it again from Get started.
  const focused = (selector) => page.waitForFunction((s) => document.activeElement?.matches(s), selector, { timeout: 10_000 });
  await page.evaluate(() => (location.hash = "#use"));
  await main.getByRole("button", { name: "Create a client key" }).focus();
  await page.keyboard.press("Enter");
  await focused('.seg button[aria-pressed="true"]');
  pass("Use with tools: Enter on Create a client key adds a key and focus moves to the tool choice");
  await page.evaluate(async (s) => {
    const r = await fetch("./v8/management/config/access/api-keys", {
      method: "PUT",
      headers: { Authorization: `Bearer ${s}`, "Content-Type": "application/json" },
      body: "[]",
    });
    if (!r.ok) throw new Error(`reset api-keys: HTTP ${r.status}`);
  }, secret);
  await page.evaluate(() => (location.hash = "#overview"));
  await page.reload();
  await signIn(page);
  await start.getByText("0 of 3 done").waitFor();
  await start.getByRole("button", { name: "Create a client key" }).focus();
  await page.keyboard.press("Enter");
  await start.getByText("1 of 3 done").waitFor();
  await focused(".checklist a.key.primary");
  assert.equal(await page.evaluate(() => document.activeElement.textContent.trim()), "Connect account");
  pass("Get started: Enter on Create a client key adds it (1 of 3) and focus moves to Connect account");

  // 4. Use with tools before any account: the key works, no models yet.
  await page.evaluate(() => (location.hash = "#use"));
  const status = main.locator('[role="status"]');
  await status.filter({ hasText: /no models are available yet/ }).waitFor();
  await shot(page, "06-use-no-account-desktop-light");
  pass("Use with tools tests the new key: works, no models until an account is connected");

  // 5. Connect an account (Meta device code against the fake login server).
  await page.evaluate(() => (location.hash = "#connect"));
  await main.getByRole("button", { name: "Meta", exact: true }).click();
  await main.getByText("META-FAKE").waitFor();
  await shot(page, "07-connect-meta-device-code-desktop-light");
  await main.getByText("Connected", { exact: true }).waitFor({ timeout: 30_000 });
  pass("Meta sign-in: device code shown, connected");

  // 6. Use with tools again: real models, ready-to-copy setup with this address.
  await page.evaluate(() => (location.hash = "#use"));
  await status.filter({ hasText: /Working\. This key reaches \d+ model/ }).waitFor();
  const origin = new URL(freshUrl).origin;
  const config = await page.evaluate(
    async (s) => (await fetch("./v8/management/config", { headers: { Authorization: `Bearer ${s}` } })).json(),
    secret,
  );
  const clientKey = config.access["api-keys"][0];
  for (const tool of ["Claude Code", "Codex CLI", "Cursor", "OpenAI SDK", "Anthropic SDK", "curl"]) {
    await main.getByRole("button", { name: tool, exact: true }).click();
    const code = await main.locator("pre.code-window").textContent();
    if (tool === "Cursor") assert.ok(code.includes("https://your-public-address/v1"), "Cursor gets a public-address placeholder on localhost");
    else assert.ok(code.includes(origin), `${tool}: uses ${origin}`);
    assert.ok(!code.includes(clientKey) && code.includes(`${clientKey.slice(0, 3)}…${clientKey.slice(-4)}`), `${tool}: the key is shown shortened`);
  }
  await main.getByRole("button", { name: "Claude Code", exact: true }).click();
  await shot(page, "08-use-ready-desktop-light");
  const models = await main.getByLabel("Model").locator("option").allTextContents();
  assert.ok(models.length > 0);
  pass(`Use with tools: key reaches ${models.length} models; all six setups point at ${origin} with the key shortened`);

  // 7. The tool's first request (what Claude Code or an SDK would send).
  const reply = await page.evaluate(
    async ([k, m]) => {
      const r = await fetch("./v1/chat/completions", {
        method: "POST",
        headers: { Authorization: `Bearer ${k}`, "Content-Type": "application/json" },
        body: JSON.stringify({ model: m, messages: [{ role: "user", content: "Hello" }] }),
      });
      return { status: r.status, body: await r.text() };
    },
    [clientKey, models[0]],
  );
  assert.equal(reply.status, 200, reply.body);
  pass(`first request through the proxy: ${models[0]} answered 200`);

  // 8. Overview after setup: the list is gone, the request shows.
  await page.evaluate(() => (location.hash = "#overview"));
  await page.waitForFunction(() => !document.querySelector("main h2#start") && document.querySelector(".display"), null, { timeout: 30_000 });
  await shot(page, "09-overview-ready-desktop-light");
  for (const [theme, device, name] of [
    ["dark", desktop, "10-overview-ready-desktop-dark"],
    ["light", phone, "11-overview-ready-phone-light"],
    ["dark", phone, "12-credentials-ready-phone-dark"],
  ]) {
    const p = await open(freshUrl, device, theme);
    await signIn(p);
    if (name.includes("credentials")) {
      await p.evaluate(() => (location.hash = "#credentials"));
      await p.locator("main h1", { hasText: "Credentials" }).waitFor();
    }
    await p.waitForTimeout(500);
    assert.equal(await overflow(p), false, `${name}: no horizontal overflow`);
    await shot(p, name);
    await p.context().close();
  }
  pass("after setup: Get started is gone and the overview shows traffic, in both themes and on a phone");
  assert.deepEqual(errors, []);
  pass("no page errors");
} finally {
  await browser.close();
}
