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
//   copied links and unsafe schemes, including under a reverse-proxy path prefix, and a
//   sign-in the server forgot while the tab was hidden (the account list decides the result).
//
// Usage: CHROME_PATH=... node scripts/oauth-check.mjs <page-url> --mode fake|dead|plugin [capture-dir]
import { chromium } from "playwright-core";
import { mkdirSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";
import { assertPrivateAuthDir } from "./private-auth-dir.mjs";

const [url, , mode, dir] = process.argv.slice(2);
assert.ok(url && ["fake", "dead", "plugin"].includes(mode), "usage: oauth-check.mjs <page-url> --mode fake|dead|plugin [capture-dir]");
// fake and dead sign in through a real server; plugin mode answers every request itself.
if (mode !== "plugin") await assertPrivateAuthDir(url, "orb-dashboard-test-only");
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
  await main.getByText("The browser ended on a page that won’t load").click();
  await main.getByLabel("Paste that page’s full address").fill(redirect);
  await button("Send").click();
}

try {
  let loginUrl = `./v0/resource/plugins/commandcode-go-cliproxyapi/oauth#state=${"a".repeat(64)}`;
  const fixture = { name: "fixture.json", type: "claude", disabled: true, email: "operator@example.invalid", updated_at: "2026-11-01T09:00:00Z" };
  let files = [fixture],
    status = { status: "wait" },
    credsFail = false,
    delay = 0,
    polls = [],
    usage = 200,
    reads = 0,
    hold = null,
    credsLeft = Infinity;
  if (mode === "plugin") {
    // A tab the test can hide, and polling slowed thirtyfold, so a status read right after the
    // tab comes back can only be the one its return triggered.
    await page.context().addInitScript(() => {
      window.__hidden = false;
      Object.defineProperty(document, "hidden", { configurable: true, get: () => window.__hidden });
      window.__setHidden = (v) => {
        window.__hidden = v;
        document.dispatchEvent(new Event("visibilitychange"));
      };
      const slow = window.setInterval;
      window.setInterval = (f, ms, ...a) => slow(f, ms * 30, ...a);
    });
    await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
    await page.context().route("**/*", async (route) => {
      const request = new URL(route.request().url());
      if (request.origin !== new URL(url).origin) return route.abort();
      const path = request.pathname.split("/v8/management/")[1];
      if (path !== undefined) {
        let body;
        if (path === "config") body = {};
        else if (path === "credentials") {
          // credsLeft: how many more account-list reads succeed before they start failing.
          if (credsFail || credsLeft-- <= 0) return route.fulfill({ status: 500, json: { error: "fixture: read failed" } });
          body = { files };
        }
        else if (path === "plugins") body = { plugins: [{ id: "commandcode-go-cliproxyapi", supports_oauth: true, oauth_provider: "commandcode" }] };
        else if (path === "requests/api-call") {
          reads++;
          const code = usage;
          // A held read answers, with the status it was sent under, only when released.
          if (hold) await hold.promise;
          const reset = (h) => new Date(Date.now() + h * 3_600_000).toISOString();
          body = { status_code: code, body: code === 200 ? JSON.stringify({ five_hour: { utilization: 37, resets_at: reset(2) }, seven_day: { utilization: 62, resets_at: reset(50) } }) : "{}" };
        }
        else if (path === "oauth/auth-url") body = { status: "ok", state: "a".repeat(64), url: loginUrl };
        else if (path === "oauth/status") {
          (body = status), polls.push(Date.now());
          if (delay) await new Promise((r) => setTimeout(r, delay));
        }
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

    // Go and older servers forget a successful session a minute after it ends and answer
    // "unknown or expired state". A tab that was away longer reads the status as it comes
    // back, and claims success only for exactly one new credential name of exactly this
    // built-in provider between two successful reads; anything else is "Result unavailable".
    loginUrl = "https://provider.example.invalid/authorize?state=expired";
    const unknown = "The proxy no longer has this sign-in’s result. Check Accounts to see whether it was added.";
    const kimi = { name: "kimi-old.json", type: "kimi", email: "old@example.invalid", updated_at: "2026-11-01T09:00:00Z" };
    const fresh = (name, type) => ({ name, type, email: `${name}@example.invalid`, updated_at: "2026-11-03T09:00:00Z" });
    for (const [what, who, before, after, expected, fail] of [
      ["no new credential", "Kimi", [kimi], [kimi], unknown],
      ["one new name", "Kimi", [kimi], [kimi, fresh("kimi-new", "kimi")], "kimi-new@example.invalid appeared during this sign-in."],
      // A refresh, an upload or another sign-in rewrites a file: same name, newer mtime.
      ["an unrelated rewrite", "Kimi", [kimi], [{ ...kimi, updated_at: "2026-11-03T09:00:00Z" }], unknown],
      ["a failed read after", "Kimi", [kimi], [kimi, fresh("kimi-new", "kimi")], unknown, true],
      ["a kimi credential for a kimi-ai sign-in", "Kimi (kimi.ai)", [kimi], [kimi, fresh("kimi-new", "kimi")], unknown],
      ["a plugin provider", "commandcode", [kimi], [kimi, fresh("commandcode-new", "commandcode")], unknown],
    ]) {
      (files = before), (status = { status: "wait" }), (credsFail = false);
      await begin(who);
      await page.evaluate(() => window.__setHidden(true));
      (files = after), (status = { status: "error", error: "unknown or expired state" }), (credsFail = !!fail);
      polls = [];
      await page.waitForTimeout(2500);
      assert.equal(polls.length, 0, `${what}: no status reads while the tab is hidden`);
      const back = Date.now();
      await page.evaluate(() => window.__setHidden(false));
      await main.locator(".flow").getByText(expected, { exact: true }).waitFor({ timeout: 5000 });
      assert.ok(polls.length === 1 && polls[0] - back < 1000, `${what}: one status read as the tab came back (${polls.map((t) => t - back)} ms)`);
      await main.locator(".flow .section-head").getByText(expected === unknown ? "Result unavailable" : "Connected", { exact: true }).waitFor();
      assert.ok(!/in rotation/.test(await main.locator(".flow").textContent()), `${what}: no "in rotation" claim`);
      await page.screenshot({ path: `${out}/oauth-expired-${what.replaceAll(" ", "-")}.png` });
    }
    credsFail = false;
    pass("plugin: returning to the tab reads the status at once; a forgotten sign-in counts only one new name of exactly its built-in provider, read successfully, and is otherwise \"Result unavailable\"");

    // A status reply delayed past a cancel and a new start belongs to the old sign-in only.
    (files = [kimi]), (status = { status: "wait" });
    await begin("Kimi");
    await page.evaluate(() => window.__setHidden(true));
    (status = { status: "error", error: "fixture: the old sign-in failed" }), (delay = 2000);
    await page.evaluate(() => window.__setHidden(false));
    await page.waitForTimeout(300);
    status = { status: "wait" };
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    await button("Kimi").click();
    await main.getByText("Waiting for approval").waitFor();
    await page.waitForTimeout(2500);
    delay = 0;
    assert.equal(await main.getByText("Waiting for approval").count(), 1, "the new sign-in still waits");
    assert.equal(await main.getByText("fixture: the old sign-in failed").count(), 0, "the old reply was dropped");
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    pass("plugin: a status reply delayed past cancel and restart does not change the new sign-in");

    // An ordinary "ok" for an account that was disabled before: a re-login keeps it disabled,
    // so the panel must not claim it serves.
    (files = [{ ...kimi, disabled: true }]), (status = { status: "wait" });
    await begin("Kimi");
    status = { status: "ok" };
    await page.evaluate(() => (window.__setHidden(true), window.__setHidden(false)));
    await main.locator(".flow").getByText("Saved on the server.", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor();
    assert.ok(!/in rotation/.test(await main.locator(".flow").textContent()), 'ok: no "in rotation" claim');
    await page.screenshot({ path: `${out}/oauth-ok-disabled.png` });
    pass("plugin: an ordinary successful sign-in says it was saved, never that it is in rotation");

    // A confirmed sign-in with a new Claude credential: its usage is read once, the bars come
    // from that read, and Connect another starts the next sign-in.
    const claudeNew = { name: "claude-new.json", type: "claude", auth_index: "claude-new", email: "new@example.invalid", status: "active", updated_at: "2026-11-05T09:00:00Z" };
    const connected = async (after, code, end = { status: "ok" }) => {
      (files = [fixture]), (status = { status: "wait" }), (usage = code), (reads = 0);
      await begin("Claude");
      (files = after), (status = end);
      await page.evaluate(() => (window.__setHidden(true), window.__setHidden(false)));
    };
    await connected([fixture, claudeNew], 200);
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow .result").getByText("new@example.invalid").waitFor();
    assert.deepEqual(await main.locator(".flow .result [role=meter]").evaluateAll((m) => m.map((e) => Number(e.getAttribute("aria-valuenow")))), [37, 62]);
    assert.equal(reads, 1, "one usage read");
    assert.match(await main.locator(".flow .result").textContent(), /Claude · ready/);
    assert.ok(!/in rotation/.test(await main.locator(".flow").textContent()), 'verified: no "in rotation" claim');
    await page.screenshot({ path: `${out}/oauth-verified.png` });
    await button("Connect another Claude account").click();
    await main.getByText("Waiting for approval").waitFor();
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    pass("plugin: a verified sign-in reads usage once, shows those bars, and Connect another starts a new sign-in");

    // The same, when the server forgot the sign-in and the new name decides it: the usage
    // read must still find the new account in the reloaded list.
    await connected([fixture, { ...claudeNew, name: "claude-late.json", auth_index: "claude-late", email: "late@example.invalid" }], 200, { status: "error", error: "unknown or expired state" });
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow .result [role=meter]").first().waitFor({ timeout: 5000 });
    assert.equal(reads, 1, "one usage read after a forgotten sign-in");
    await button("Connect another Claude account").click();
    await main.getByText("Waiting for approval").waitFor();
    await button("Cancel sign-in").click();
    await main.getByText("Cancelled", { exact: true }).waitFor();
    pass("plugin: a forgotten sign-in found by its new name still gets its usage read and bars");

    // The provider refuses the new token's usage read: the error, no bars, and Try again
    // within 2 minutes sends nothing.
    const refusedNew = { ...claudeNew, name: "claude-refused.json", auth_index: "claude-refused", email: "refused@example.invalid" };
    await connected([fixture, refusedNew], 401);
    await main.locator(".flow .section-head").getByText("Provider refused", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow").getByText(/rejected the account's token \(HTTP 401\)/).waitFor();
    assert.equal(await main.locator(".flow .result [role=meter]").count(), 0, "no bars for a refused read");
    await page.screenshot({ path: `${out}/oauth-refused.png` });
    await button("Try again").click();
    await page.getByText(/Read moments ago/).waitFor();
    assert.equal(reads, 1, "Try again within 2 minutes sends nothing");
    assert.equal(await button("Sign in again").count(), 1);
    pass("plugin: a refused usage read shows the provider's error without bars, and Try again respects the 2-minute wait");

    // Signing in again within 2 minutes saves a new token but makes no read: the old refusal
    // must not stand in for it.
    (status = { status: "wait" }), (usage = 200);
    await button("Sign in again").click();
    await main.getByText("Waiting for approval").waitFor();
    (files = [fixture, { ...refusedNew, updated_at: "2026-11-05T10:00:00Z" }]), (status = { status: "ok" });
    await page.evaluate(() => (window.__setHidden(true), window.__setHidden(false)));
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow").getByText("Usage not read: read under 2 minutes ago.").waitFor();
    assert.equal(reads, 1, "no read within 2 minutes");
    assert.equal(await main.locator(".flow .result [role=meter]").count(), 0, "no bars from an older read");
    pass("plugin: signing in again within 2 minutes says usage was not read instead of reusing the old result");

    // A usage read that fails for another reason (HTTP 500) is not a refused token.
    await connected([fixture, { ...claudeNew, name: "claude-500.json", auth_index: "claude-500", email: "five@example.invalid" }], 500);
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor({ timeout: 5000 });
    await main.locator(".flow").getByText(/Usage read failed: The provider answered HTTP 500/).waitFor();
    assert.equal(await main.locator(".flow .result [role=meter]").count(), 0, "no bars for a failed read");
    assert.equal(await button("Sign in again").count(), 0);
    pass("plugin: a usage read that fails without rejecting the token says so, not \"Provider refused\"");

    // A read of the old token still in flight (here from Accounts' Check quotas) when the
    // account signs in again: its answer (401) must not be shown as the new token's result.
    let release;
    hold = { promise: new Promise((r) => (release = r)) };
    const held = { ...claudeNew, name: "claude-held.json", auth_index: "claude-held", email: "held@example.invalid" };
    (files = [fixture, held]), (usage = 401), (reads = 0);
    // Signing in again loads the new list (Accounts itself rereads only every 10 s).
    await page.getByRole("button", { name: "Sign out", exact: true }).click();
    await page.getByLabel("Management key").fill("orb-dashboard-test-only");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await page.evaluate(() => (location.hash = "#accounts"));
    await main.getByText("held@example.invalid").first().waitFor({ timeout: 15_000 });
    await button("Check quotas").click();
    for (let i = 0; i < 50 && !reads; i++) await page.waitForTimeout(100);
    assert.equal(reads, 1, "the old read is in flight");
    await page.evaluate(() => (location.hash = "#connect"));
    (status = { status: "wait" }), (usage = 200);
    await begin("Claude");
    (files = [fixture, { ...held, updated_at: "2026-11-05T11:00:00Z" }]), (status = { status: "ok" });
    await page.evaluate(() => (window.__setHidden(true), window.__setHidden(false)));
    await main.locator(".flow").getByText("Usage not read: read under 2 minutes ago.").waitFor({ timeout: 5000 });
    release();
    hold = null;
    await page.waitForTimeout(1000);
    await main.locator(".flow .section-head").getByText("Connected", { exact: true }).waitFor();
    assert.equal(await main.locator(".flow").getByText("Usage not read: read under 2 minutes ago.").count(), 1, "still not read after the old read lands");
    assert.equal(await main.locator(".flow .result [role=meter]").count(), 0);
    assert.equal(reads, 1, "no second read");
    pass("plugin: an old read still in flight across a re-sign-in is never shown as the new token's result");

    // The sign-in is found (forgotten state), then the account list fails to reload: the read
    // cannot be made, and the panel says why instead of claiming a recent read.
    (files = [fixture]), (status = { status: "wait" }), (usage = 200), (reads = 0);
    await begin("Claude");
    (files = [fixture, { ...claudeNew, name: "claude-noreload.json", auth_index: "claude-noreload", email: "noreload@example.invalid" }]),
      (status = { status: "error", error: "unknown or expired state" }),
      (credsLeft = 1);
    await page.evaluate(() => (window.__setHidden(true), window.__setHidden(false)));
    await main.locator(".flow").getByText("Usage read failed: accounts did not reload: fixture: read failed").waitFor({ timeout: 5000 });
    assert.equal(reads, 0, "no read queued");
    assert.equal(await main.locator(".flow").getByText(/read under 2 minutes ago/).count(), 0);
    credsLeft = Infinity;
    pass("plugin: a failed account-list reload after sign-in says the read failed, not that it was recent");
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
    await page.evaluate(() => (location.hash = "#accounts"));
    for (const email of ["claude-login@example.invalid", "codex-login@example.invalid", "meta-login@example.invalid", "xai-login@example.invalid"])
      await main.getByText(email).first().waitFor({ timeout: 15_000 });
    const groups = await main.locator(".group").allTextContents();
    for (const g of ["Kimi", "Devin"]) assert.ok(groups.some((t) => t.startsWith(g)), `${g} group listed`);
    await page.screenshot({ path: `${out}/oauth-credentials-after.png`, fullPage: true });
    pass("all six new credentials are listed on Accounts");

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
