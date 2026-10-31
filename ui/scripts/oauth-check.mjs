// End-to-end account sign-in through the dashboard. Disposable servers only.
//
// --mode fake: the server's login endpoints point at scripts/fake-logins.mjs (the
//   login-harness build of cliproxy-rs). Claude and Codex finish through a pasted callback
//   URL, Devin through a pasted callback to the server's /callback, Kimi, Meta and xAI
//   through the device-code flow; each new credential must appear in the
//   list. Providers the server lacks must turn into an honest "not available" message.
// --mode dead: the shipped binary with requests.proxy-url on a dead port. A pasted callback
//   must end in an honest failure, never "Connected". No request reaches a provider.
// --mode plugin: fake management responses check relative and absolute login URLs,
//   copied links and unsafe schemes, including under a reverse-proxy path prefix.
//
// Usage: CHROME_PATH=... node scripts/oauth-check.mjs <page-url> --mode fake|dead|plugin [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const [url, , mode, dir] = process.argv.slice(2);
assert.ok(url && ["fake", "dead", "plugin"].includes(mode), "usage: oauth-check.mjs <page-url> --mode fake|dead|plugin [capture-dir]");
const out = resolve(dir || "oauth-check");
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME_PATH });
const page = await (await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 2 })).newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
const main = page.locator("main");
const button = (name) => main.getByRole("button", { name, exact: true });
const pass = (t) => console.log(`PASS ${t}`);
/** The toast, if any, must not still say a sign-in is in progress once it has finished. */
async function noProgressToast(when) {
  const toast = await page.locator(".toast").textContent({ timeout: 500 }).catch(() => "");
  assert.ok(!/sent|finishing|waiting/i.test(toast), `${when}: stale toast "${toast.trim()}"`);
}

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
  let loginUrl = `./v0/resource/plugins/commandcode-go-cliproxyapi/oauth#state=${"a".repeat(64)}`;
  if (mode === "plugin") {
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.context().route("**/*", async (route) => {
      const request = new URL(route.request().url());
      if (request.origin !== new URL(url).origin) return route.abort();
      const path = request.pathname.split("/v8/management/")[1];
      if (path !== undefined) {
        let body;
        if (path === "config") body = {};
        else if (path === "credentials") body = { files: [{ name: "fixture.json", type: "claude", disabled: true, email: "operator@example.invalid" }] };
        else if (path === "plugins") body = { plugins: [{ id: "commandcode-go-cliproxyapi", supports_oauth: true, oauth_provider: "commandcode" }] };
        else if (path === "oauth/auth-url") body = { status: "ok", state: "a".repeat(64), url: loginUrl };
        else if (path === "oauth/status") body = { status: "wait" };
        else if (path === "oauth/session" && route.request().method() === "DELETE") {
          assert.equal(request.pathname, new URL("v8/management/oauth/session", url).pathname);
          assert.equal(request.searchParams.get("state"), "a".repeat(64));
          body = { cancelled: true };
        }
        else throw new Error(`Unexpected management request: ${path}`);
        return route.fulfill({ json: body, headers: { "X-CPA-COMMIT": "fixture" } });
      }
      if (request.pathname.endsWith("/v0/resource/plugins/commandcode-go-cliproxyapi/oauth"))
        return route.fulfill({ contentType: "text/html", body: "<h1>Plugin sign-in guide</h1>" });
      return route.continue();
    });
  }
  await page.goto(url);
  await page.getByLabel("Management key").fill("orb-dashboard-test-only");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await main.getByText("operator@example.invalid").first().waitFor();
  await page.evaluate(() => (location.hash = "#connect"));
  await main.getByRole("heading", { name: "Sign in with a provider" }).waitFor();

  if (mode === "plugin") {
    const prefix = new URL(".", url).href;
    for (const [input, expected] of [
      [loginUrl, `${prefix}v0/resource/plugins/commandcode-go-cliproxyapi/oauth#state=${"a".repeat(64)}`],
      ["/v0/resource/plugins/commandcode-go-cliproxyapi/oauth#state=root", `${new URL(url).origin}/v0/resource/plugins/commandcode-go-cliproxyapi/oauth#state=root`],
      ["https://provider.example.invalid/authorize?state=https", "https://provider.example.invalid/authorize?state=https"],
      ["http://provider.example.invalid/authorize?state=http", "http://provider.example.invalid/authorize?state=http"],
    ]) {
      loginUrl = input;
      const { href } = await begin("commandcode");
      assert.equal(href, expected, "sign-in link is absolute and preserves the document's path prefix");
      await button("Copy link").click();
      await page.getByText("Link copied.", { exact: true }).waitFor();
      assert.equal(await page.evaluate(() => navigator.clipboard.readText()), expected);
      if (input.startsWith("./")) {
        const popup = page.waitForEvent("popup");
        await main.getByRole("link", { name: "Open sign-in page" }).click();
        const guide = await popup;
        await guide.getByRole("heading", { name: "Plugin sign-in guide" }).waitFor();
        assert.equal(guide.url(), expected);
        await guide.close();
        await page.screenshot({ path: `${out}/oauth-plugin-waiting.png` });
      }
      await button("Cancel sign-in").click();
      await main.getByText("Cancelled", { exact: true }).waitFor();
    }
    pass("plugin: relative URLs preserve path prefixes; absolute HTTP(S), copied links, guide navigation and cancellation work");
    for (const input of ["javascript:alert(1)", "data:text/html,unsafe", "file:///tmp/unsafe", "mailto:unsafe@example.invalid"]) {
      loginUrl = input;
      await button("commandcode").click();
      await page.getByRole("alert").filter({ hasText: "The server returned an unsafe sign-in URL." }).waitFor();
      assert.equal(await main.getByRole("link", { name: "Open sign-in page" }).count(), 0);
      await page.getByRole("button", { name: "Dismiss", exact: true }).click();
    }
    pass("plugin: javascript, data, file and mailto URLs cannot start a session");
  } else if (mode === "fake") {
    for (const [provider, redirect, email] of [
      ["Claude", (s) => `http://localhost:54545/callback?code=fake-code&state=${s}`, "claude-login@example.invalid"],
      ["Codex", (s) => `http://localhost:1455/auth/callback?code=fake-code&state=${s}`, "codex-login@example.invalid"],
    ]) {
      const { href, state } = await begin(provider);
      assert.ok(href.startsWith("https://") && state, `${provider}: provider link ${href}`);
      await paste(redirect(state));
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 20_000 });
      await noProgressToast(`${provider} connected`);
      await page.screenshot({ path: `${out}/oauth-${provider.toLowerCase()}-connected.png` });
      pass(`${provider}: authorize link, pasted callback, code exchange, credential saved (${email})`);
    }
    // Devin redirects to the server's own /callback route; the pasted URL carries it there.
    {
      const { state } = await begin("Devin");
      assert.ok(state, "Devin: provider link carries a state");
      await paste(`${new URL(url).origin}/callback?code=fake-devin-code&state=${state}`);
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 20_000 });
      await noProgressToast("Devin connected");
      pass("Devin: authorize link, pasted callback to the server's /callback, code exchange, credential saved");
    }
    for (const [provider, code] of [["Kimi", "KIMI-FAKE"], ["Meta", "META-FAKE"], ["xAI", "XAI-FAKE"]]) {
      await button(provider).click();
      await main.getByText(code).waitFor();
      if (provider === "Kimi") await page.screenshot({ path: `${out}/oauth-kimi-device-code.png` });
      await main.getByText("Connected", { exact: true }).waitFor({ timeout: 30_000 });
      pass(`${provider}: device code ${code} shown, polled to completion`);
    }
    await page.evaluate(() => (location.hash = "#credentials"));
    for (const email of ["claude-login@example.invalid", "codex-login@example.invalid", "meta-login@example.invalid", "xai-login@example.invalid"])
      await main.getByText(email).first().waitFor({ timeout: 15_000 });
    const groups = await main.locator(".group").allTextContents();
    for (const g of ["Kimi", "Devin"]) assert.ok(groups.some((t) => t.startsWith(g)), `${g} group listed`);
    await page.screenshot({ path: `${out}/oauth-credentials-after.png`, fullPage: true });
    pass("all six new credentials are listed on Credentials");

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
    await noProgressToast("Codex failed");
    await page.screenshot({ path: `${out}/oauth-codex-failed.png` });
    pass(`Codex with an unreachable token endpoint fails honestly: "${problem.trim()}"`);
    await button("Start again").click();
    await main.getByText("Waiting for approval").waitFor();
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    await noProgressToast("Codex cancelled");
    pass("Start again opens a new session; Cancel ends it; no in-progress toast remains after any end state");
  }
  assert.deepEqual(errors, []);
} finally {
  await browser.close();
}
