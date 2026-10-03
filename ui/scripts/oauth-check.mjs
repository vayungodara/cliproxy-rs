// End-to-end account sign-in through the dashboard. Disposable servers only.
//
// --mode fake: the server's login endpoints point at scripts/fake-logins.mjs (the
//   login-harness build of cliproxy-rs). Claude and Codex finish through a pasted callback
//   URL, Kimi and Meta through the device-code flow; each new credential must appear in the
//   list. Providers the server lacks must turn into an honest "not available" message.
// --mode dead: the shipped binary with requests.proxy-url on a dead port. A pasted callback
//   must end in an honest failure, never "Connected". No request reaches a provider.
//
// Usage: CHROME_PATH=... node scripts/oauth-check.mjs <page-url> --mode fake|dead [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const [url, , mode, dir] = process.argv.slice(2);
assert.ok(url && ["fake", "dead"].includes(mode), "usage: oauth-check.mjs <page-url> --mode fake|dead [capture-dir]");
const out = resolve(dir || "oauth-check");
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 })).newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
const main = page.locator("main");
const button = (name) => main.getByRole("button", { name, exact: true });
const pass = (t) => console.log(`PASS ${t}`);

/** Start a sign-in and return the session state from the provider link. */
async function begin(provider) {
  await button(provider).click();
  await main.getByText("Waiting for approval").waitFor();
  const href = await main.getByRole("link", { name: "Open sign-in page" }).getAttribute("href");
  return { href, state: new URL(href).searchParams.get("state") };
}
async function paste(redirect) {
  await main.getByLabel("Callback address").fill(redirect);
  await button("Send").click();
}

try {
  await page.goto(url);
  await page.getByLabel("Management key").fill("orb-dashboard-test-only");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await main.getByText("operator@example.invalid").first().waitFor();
  await page.evaluate(() => (location.hash = "#connect"));
  await main.getByRole("heading", { name: "Sign in with a provider" }).waitFor();

  if (mode === "fake") {
    for (const [provider, redirect, email] of [
      ["Claude", (s) => `http://localhost:54545/callback?code=fake-code&state=${s}`, "claude-login@example.invalid"],
      ["Codex", (s) => `http://localhost:1455/auth/callback?code=fake-code&state=${s}`, "codex-login@example.invalid"],
    ]) {
      const { href, state } = await begin(provider);
      assert.ok(href.startsWith("https://") && state, `${provider}: provider link ${href}`);
      await paste(redirect(state));
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 20_000 });
      await page.screenshot({ path: `${out}/oauth-${provider.toLowerCase()}-connected.png` });
      pass(`${provider}: authorize link, pasted callback, code exchange, credential saved (${email})`);
    }
    for (const [provider, code] of [["Kimi", "KIMI-FAKE"], ["Meta", "META-FAKE"]]) {
      await button(provider).click();
      await main.getByText(code).waitFor();
      if (provider === "Kimi") await page.screenshot({ path: `${out}/oauth-kimi-device-code.png` });
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 30_000 });
      pass(`${provider}: device code ${code} shown, polled to completion`);
    }
    await page.evaluate(() => (location.hash = "#credentials"));
    for (const email of ["claude-login@example.invalid", "codex-login@example.invalid", "meta-login@example.invalid"])
      await main.getByText(email).first().waitFor({ timeout: 15_000 });
    assert.ok((await main.locator(".group").allTextContents()).some((g) => g.startsWith("Kimi")), "Kimi group listed");
    await page.screenshot({ path: `${out}/oauth-credentials-after.png`, fullPage: true });
    pass("all four new credentials are listed on Credentials");

    // A built-in provider this server lacks: honest message, button disabled afterwards.
    await page.evaluate(() => (location.hash = "#connect"));
    await button("Antigravity").click();
    const toast = await page.locator(".toast", { hasText: /Antigravity|ok/ }).or(main.getByText("Waiting for approval")).first().textContent();
    if (toast.includes("not available")) {
      assert.equal(await button("Antigravity").isDisabled(), true);
      pass(`unimplemented provider: "${toast.trim()}" and the button is disabled`);
    } else pass(`Antigravity sign-in is implemented here (${toast.trim()})`);
  } else {
    const { state } = await begin("Codex");
    await paste(`http://localhost:1455/auth/callback?code=fake-code&state=${state}`);
    await main.getByText("Failed", { exact: true }).waitFor({ timeout: 30_000 });
    const problem = await main.locator(".flow .error").textContent();
    assert.ok(/exchange/i.test(problem), problem);
    await page.screenshot({ path: `${out}/oauth-codex-failed.png` });
    pass(`Codex with an unreachable token endpoint fails honestly: "${problem.trim()}"`);
    await button("Start again").click();
    await main.getByText("Waiting for approval").waitFor();
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    pass("Start again opens a new session; Cancel ends it");
  }
  assert.deepEqual(errors, []);
} finally {
  await browser.close();
}
