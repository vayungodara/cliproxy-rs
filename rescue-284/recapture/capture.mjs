// Captures the dashboard's Provider keys page, OpenAI-compatible tab, in light and dark,
// framed like docs/img/overview-*.png: a 1280 px wide viewport at 2x, once per height.
// With RELABEL (a JSON object of text replacements) the management API's answers are
// rewritten on their way to the page, which is how the mock's base URLs are shown as the
// providers' real ones. Nothing else in the answers changes.
// Usage: PW=<playwright-core dir> node capture.mjs <dashboard-url> <management-key> <out-dir> <height>[,<height>]
import { mkdirSync } from "node:fs";
const { chromium } = await import(`${process.env.PW}/index.mjs`);
const [url, secret, out, heights = "1000"] = process.argv.slice(2);
const relabel = Object.entries(JSON.parse(process.env.RELABEL || "{}"));
mkdirSync(out, { recursive: true });
const origin = new URL(url).origin;
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH || "/usr/bin/chromium", args: ["--no-sandbox"] });
for (const theme of ["light", "dark"]) for (const height of heights.split(",").map(Number)) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height }, deviceScaleFactor: 2, colorScheme: theme });
  await ctx.addInitScript((t) => localStorage.setItem("cliproxy-theme", t), theme);
  const outside = [];
  await ctx.route("**/*", async (route) => {
    const u = route.request().url();
    if (!u.startsWith(origin)) {
      if (!u.startsWith("data:")) outside.push(u);
      return u.startsWith("data:") ? route.continue() : route.abort();
    }
    if (!relabel.length || !u.includes("/v8/management/") || route.request().method() !== "GET") return route.continue();
    const response = await route.fetch();
    let body = await response.text();
    for (const [from, to] of relabel) body = body.split(from).join(to);
    return route.fulfill({ response, body });
  });
  const page = await ctx.newPage();
  await page.goto(`${url}#providers`);
  await page.fill('input[type="password"]', secret);
  await page.click("button.primary");
  await page.waitForSelector(".shell");
  await page.getByRole("button", { name: /^OpenAI-compatible/ }).click();
  await page.waitForSelector("form.form");
  await page.waitForSelector(".groups .count");
  await page.evaluate(() => document.fonts.ready);
  await page.waitForTimeout(1500);
  const facts = await page.evaluate(() => ({
    theme: document.documentElement.dataset.theme,
    fields: [...document.querySelectorAll("form.form label")].map((l) => l.childNodes[0].textContent.trim()),
    urls: [...document.querySelectorAll(".groups p.mono")].map((p) => p.textContent.trim()),
    counts: [...document.querySelectorAll(".groups .key-row")].map((r) => `${r.querySelector("code").textContent} ${r.querySelector(".count")?.textContent ?? "no traffic"}`),
    header: document.querySelector("header, .top, .bar")?.innerText.replace(/\s+/g, " ").slice(0, 120),
    pageHeight: document.documentElement.scrollHeight,
    formBottom: Math.round(document.querySelector("form.form").getBoundingClientRect().bottom),
  }));
  await page.screenshot({ path: `${out}/overview-${theme}-${height}.png` });
  console.log(JSON.stringify({ height, ...facts, outside }));
  await ctx.close();
}
await browser.close();
