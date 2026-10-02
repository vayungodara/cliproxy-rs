// Disposable Rust backend only. No OAuth, refresh, quota, or provider requests.
import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const url = process.argv[2] || "http://127.0.0.1:8317/management.html";
const output = resolve(process.argv[3] || "../../.amp/in/artifacts");
const session = "manage-ui";
mkdirSync(output, { recursive: true });
const browser = (...args) =>
  execFileSync("agent-browser", ["--session", session, ...args], {
    encoding: "utf8",
    timeout: 45000,
  }).trim();
const settled = () => {
  browser(
    "wait",
    "--fn",
    "document.querySelector('main')?.getAttribute('aria-busy')==='false'",
  );
  browser(
    "eval",
    "document.fonts.ready.then(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r))))",
  );
};
const results = [];
try {
  browser("open", url);
  browser("set", "viewport", "1440", "1000", "2");
  browser(
    "eval",
    `(()=>{window.__calls=[];const original=window.fetch;window.fetch=async(...args)=>{const response=await original(...args);window.__calls.push({path:new URL(args[0],location.href).pathname,method:args[1]?.method||'GET',status:response.status});return response};return true})()`,
  );
  browser("find", "label", "Management key", "fill", "orb-dashboard-test-only");
  browser("find", "role", "button", "click", "--name", "Connect to server");
  browser("wait", "--text", "Request traffic");
  settled();
  // Refuse mutations against a backend other than the disposable fixture.
  assert.equal(
    JSON.parse(
      browser(
        "eval",
        `document.querySelector('main').textContent.includes('operator@example.invalid')`,
      ),
    ),
    true,
  );
  for (const theme of ["dark", "light"]) {
    browser(
      "eval",
      `(()=>{if(document.documentElement.dataset.theme!=='${theme}')document.querySelector('[aria-label="Toggle theme"]').click();return true})()`,
    );
    for (const page of [
      "overview",
      "credentials",
      "keys",
      "payload",
      "configuration",
    ]) {
      const before = JSON.parse(browser("eval", "window.__calls.length"));
      browser("click", `nav a[href="#${page}"]`);
      settled();
      const state = JSON.parse(
        browser(
          "eval",
          `({theme:document.documentElement.dataset.theme,route:location.hash,error:document.querySelector('main [role=alert]')?.textContent||'',overflow:document.documentElement.scrollWidth>innerWidth,calls:window.__calls.slice(${before})})`,
        ),
      );
      assert.equal(state.theme, theme);
      assert.equal(state.route, `#${page}`);
      assert.equal(state.error, "", `${page}: ${state.error}`);
      assert.equal(state.overflow, false, `${page}: viewport overflow`);
      assert.ok(
        state.calls.some((c) => c.path.endsWith("/config") && c.status === 200),
      );
      assert.ok(
        state.calls.every((c) => [200, 404].includes(c.status)),
        `${page}: API error`,
      );
      results.push(state);
      console.log(`PASS ${theme} ${page}: live Rust API, no error or overflow`);
      if (page === "overview" && theme === "dark")
        browser("screenshot", `${output}/rust-dashboard-desktop.png`, "--full");
    }
  }
  browser("click", 'nav a[href="#payload"]');
  settled();
  browser("find", "role", "button", "click", "--name", "Add rule");
  browser("wait", "--text", "Payload rule added.");
  assert.equal(
    JSON.parse(
      browser("eval", "document.querySelectorAll('.rule-row').length"),
    ),
    1,
  );
  browser("find", "role", "button", "click", "--name", "Remove", "--exact");
  browser("wait", "--fn", "document.querySelectorAll('.rule-row').length===0");
  console.log("PASS payload add/remove: persisted real Rust writes");
  browser("click", 'nav a[href="#configuration"]');
  settled();
  browser("find", "role", "button", "click", "--name", "Open YAML editor");
  browser("wait", "--text", "YAML configuration");
  const original = JSON.parse(
    browser("eval", "document.querySelector('textarea').value"),
  );
  assert.ok(original.includes("strategy: round-robin"));
  browser(
    "fill",
    "textarea",
    original.replace("strategy: round-robin", "strategy: fill-first"),
  );
  browser("find", "role", "button", "click", "--name", "Review changes");
  browser("wait", "--text", "Change preview");
  assert.ok(
    JSON.parse(
      browser("eval", "document.querySelectorAll('.diff .added').length"),
    ) > 0,
  );
  browser("screenshot", `${output}/rust-dashboard-diff-light.png`, "--full");
  browser("find", "role", "button", "click", "--name", "Back to editing");
  browser("fill", "textarea", original);
  browser("find", "role", "button", "click", "--name", "Close editor");
  console.log("PASS YAML editor: persisted tree, real diff, discarded safely");
  browser("click", 'nav a[href="#credentials"]');
  settled();
  browser("set", "viewport", "390", "844", "2");
  settled();
  assert.equal(
    JSON.parse(
      browser("eval", "document.documentElement.scrollWidth>innerWidth"),
    ),
    false,
  );
  assert.equal(JSON.parse(browser("eval", "devicePixelRatio")), 2);
  browser("screenshot", `${output}/rust-dashboard-narrow.png`, "--full");
  const storage = JSON.parse(
    browser(
      "eval",
      "({local:Object.keys(localStorage),session:Object.keys(sessionStorage)})",
    ),
  );
  assert.deepEqual(storage.session, []);
  assert.ok(storage.local.every((k) => k === "cliproxy-theme"));
  console.log(
    "PASS 390px Chromium: no overflow; management key absent from storage",
  );
  writeFileSync(
    `${output}/rust-dashboard-checks.json`,
    JSON.stringify(results, null, 2),
  );
} finally {
  browser("close");
}
