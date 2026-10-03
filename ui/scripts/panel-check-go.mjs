// End-to-end check of the single-file panel served by an unmodified Go CLIProxyAPI.
// Disposable servers only: it edits fake credentials and config, starts and cancels a Codex
// OAuth session (no provider request), and drains the usage queue. It never checks quota.
// Usage: CHROME_PATH=/path/to/chrome node scripts/panel-check-go.mjs [page-url] [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const url = process.argv[2] || "http://127.0.0.1:8317/management.html";
const out = resolve(process.argv[3] || "panel-check");
const KEY = "orb-dashboard-test-only";
mkdirSync(out, { recursive: true });
const origin = new URL(url).origin;
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const context = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 });
const page = await context.newPage();
const requests = [], problems = [], log = [], failures = [];
page.on("request", (r) => requests.push({ url: r.url(), method: r.method() }));
page.on("console", (m) => m.type() === "error" && !m.text().startsWith("Failed to load resource") && problems.push(m.text()));
page.on("response", (r) => r.status() >= 400 && failures.push(`${r.request().method()} ${new URL(r.url()).pathname} ${r.status()}`));
page.on("pageerror", (e) => problems.push(String(e)));
const pass = (text) => (log.push(text), console.log(`PASS ${text}`));
const main = page.locator("main");
const shot = (name, full = false) => page.screenshot({ path: `${out}/${name}.png`, fullPage: full });
const settle = async () => {
  await page.waitForFunction(() => !document.querySelector("main [aria-busy=true]"));
  await page.evaluate(() => document.fonts.ready);
  await page.waitForTimeout(300);
};
const go = async (hash) => {
  await page.evaluate((h) => (location.hash = h), hash);
  await settle();
};
const button = (name, scope = main) => scope.getByRole("button", { name, exact: true });
const toast = (text) => page.locator(".toast", { hasText: text }).waitFor();
// One proxy request with the fixture's fake client key; the fixture routes it to a local
// mock upstream (or a dead proxy port), so it reaches no provider. It gives the log tail a
// request line with an ID and the usage queue a record.
const traffic = () =>
  fetch(`${origin}/v1/messages`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-api-key": "sk-fake-client-0001-not-a-real-key", "anthropic-version": "2023-06-01" },
    body: JSON.stringify({ model: "claude-sonnet-4-5-20250929", max_tokens: 16, messages: [{ role: "user", content: "ping" }] }),
  }).then((r) => r.text(), () => "");

try {
  await page.goto(url);
  await page.getByLabel("Management key").fill(KEY);
  await button("Connect", page).click();
  await main.getByText("operator@example.invalid").first().waitFor();
  await settle();
  // Go is recognised from its headers, so no capability probes are sent (they would answer 400).
  await go("#system");
  await main.getByText("(Go)").waitFor();
  pass("server detected as Go from X-CPA headers");

  const pages = ["overview", "use", "credentials", "connect", "providers", "keys", "models", "payload", "quotas", "usage", "logs", "config", "plugins", "system"];
  for (const theme of ["light", "dark"]) {
    if ((await page.evaluate(() => document.documentElement.dataset.theme)) !== theme)
      await page.locator('[aria-label^="Use "]').first().click();
    for (const p of pages) {
      await go(`#${p}`);
      const state = await page.evaluate(() => ({
        alert: document.querySelector("main [role=alert]")?.textContent || "",
        missing: document.querySelector("main")?.textContent.includes("Not available on this server"),
        overflow: document.documentElement.scrollWidth > innerWidth,
        title: document.querySelector("main h1")?.textContent || "",
      }));
      assert.equal(state.alert, "", `${theme} ${p}: ${state.alert}`);
      assert.equal(state.missing, false, `${theme} ${p}: Go implements this route`);
      assert.equal(state.overflow, false, `${theme} ${p}: horizontal overflow`);
      assert.ok(state.title, `${theme} ${p}: no heading`);
      await shot(`go-${theme}-${p}`, true);
    }
    pass(`${theme}: ${pages.length} screens render on Go with no error, gap or overflow`);
  }

  // Credentials: details, models, fields, status.
  await go("#credentials/claude-research.json");
  await main.locator(".detail .chip").first().waitFor();
  await main.locator(".detail").getByLabel("Note").fill("Research account");
  await button("Save", main.locator(".detail")).click();
  await toast("Saved.");
  await main.getByText("Research account").first().waitFor();
  await go("#credentials/claude-operator.json");
  await button("Enable", main.locator(".detail")).click();
  await toast("Enabled.");
  await button("Disable", main.locator(".detail")).click();
  await toast("Disabled.");
  await shot("go-dark-credentials-detail", true);
  pass("credentials: models load, note saves, enable/disable round-trips");

  // OAuth: start a Codex session (URL is generated locally), then cancel it.
  await go("#connect");
  await button("Codex").click();
  await main.getByText("Waiting for approval").waitFor();
  const href = await main.getByRole("link", { name: "Open sign-in page" }).getAttribute("href");
  assert.ok(href.startsWith("https://"), href);
  await shot("go-dark-connect-waiting", true);
  await button("Cancel sign-in").click();
  await main.getByText("Cancelled").waitFor();
  pass("connect: Codex sign-in starts, shows the provider link, cancels");

  // Config lists: client key, alias, exclusion, payload rule. Each is added then removed.
  await go("#keys");
  await button("Generate").click();
  await button("Add key").click();
  await toast("Key added.");
  assert.equal(await main.locator(".list li").count(), 2);
  page.once("dialog", (d) => d.accept());
  await main.locator(".list li").nth(1).getByRole("button", { name: "Remove" }).click();
  await toast("Key removed.");
  await go("#models");
  await main.getByLabel("Upstream model").fill("claude-sonnet-4-5-20250929");
  await main.getByLabel("Alias").fill("daily");
  await button("Add alias").click();
  await toast("Alias added.");
  await button("Remove").click();
  await toast("Alias removed.");
  assert.ok((await main.locator(".catalog").count()) > 5);
  await go("#payload");
  await button("Add rule").click();
  await toast("Rule added.");
  await button("Remove").click();
  await toast("Rule removed.");
  pass("config lists: client key, alias and payload rule add and remove with stale-write checks");

  // Configuration editor: diff only, nothing written.
  await go("#config");
  await main.getByRole("button", { name: /^routing/ }).click();
  const area = main.locator("textarea");
  await area.fill((await area.inputValue()).replace('"round-robin"', '"fill-first"'));
  await button("Review changes").click();
  await main.locator(".diff .added").first().waitFor();
  await shot("go-light-config-diff", true);
  await button("Keep editing").click();
  page.once("dialog", (d) => d.accept());
  await button("Close").click();
  pass("configuration: section editor shows a diff and closes without writing");

  // Logs and usage.
  await traffic();
  await go("#logs");
  await main.locator(".log > div").first().waitFor();
  const id = await main.locator(".log a.t").first().textContent();
  await main.locator(".log a.t").first().click();
  await main.locator("pre.request").waitFor();
  assert.ok((await main.locator("pre.request").textContent()).length > 0, `request log ${id}`);
  pass(`logs: tail renders; request log ${id} opens from its line`);
  await go("#usage");
  page.once("dialog", (d) => d.accept());
  await button("Start live view").click();
  await traffic();
  await main.locator(".live-readings").waitFor({ timeout: 90_000 });
  await shot("go-dark-usage-live", true);
  await button("Stop").click();
  pass("usage: live view receives queue records from Go and stops");

  // Phone.
  await page.setViewportSize({ width: 390, height: 844 });
  for (const theme of ["dark", "light"]) {
    if ((await page.evaluate(() => document.documentElement.dataset.theme)) !== theme) {
      await page.locator(".menu").click();
      await page.locator('[aria-label^="Use "]').first().click();
      await page.keyboard.press("Escape");
    }
    for (const p of ["overview", "credentials"]) {
      await go(`#${p}`);
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth > innerWidth), false, `phone ${p}`);
      await shot(`go-phone-${theme}-${p}`);
    }
  }
  // Phone menu: focus moves into the page list, the covered page is inert, Escape returns focus.
  await page.locator(".menu").click();
  await page.waitForFunction(() => document.activeElement?.closest(".nav"));
  assert.equal(await page.evaluate(() => document.querySelector("main").inert), true);
  await page.keyboard.press("Escape");
  assert.equal(await page.evaluate(() => document.activeElement?.classList.contains("menu")), true);
  await page.locator(".menu").click();
  await page.getByRole("link", { name: "Logs" }).click();
  await settle();
  assert.equal(await page.evaluate(() => location.hash), "#logs");
  assert.equal(await page.evaluate(() => document.activeElement?.id), "main");
  pass("phone: both themes fit 390 px; menu takes focus, makes the page inert, Escape returns focus, links navigate");

  // Skip link focuses the content instead of routing to a page called "main".
  await page.setViewportSize({ width: 1440, height: 900 });
  await go("#keys");
  await page.locator(".skip").focus();
  await page.keyboard.press("Enter");
  assert.deepEqual(await page.evaluate(() => [location.hash, document.activeElement?.id]), ["#keys", "main"]);
  pass("skip link moves focus to the content and keeps the page");

  const foreign = requests.filter((r) => !r.url.startsWith(origin) && !r.url.startsWith("data:"));
  assert.deepEqual(foreign, [], "requests outside the serving origin");

  const storage = await page.evaluate(() => JSON.stringify({ ...localStorage, ...sessionStorage }));
  assert.ok(!storage.includes(KEY), "management key in storage");
  assert.deepEqual(problems, [], "console errors");
  // The only HTTP errors are reads of config paths that are not set yet (Go: 404 not_found).
  assert.deepEqual(failures.filter((f) => !/^GET \/v8\/management\/config\/\S+ 404$/.test(f)), [], "unexpected HTTP errors");
  pass(`${requests.length} requests, all to ${origin}; no capability probes; no console errors; key not stored`);
} finally {
  writeFileSync(`${out}/panel-check-go.json`, JSON.stringify({ url, log, problems, failures }, null, 2));
  await browser.close();
}
