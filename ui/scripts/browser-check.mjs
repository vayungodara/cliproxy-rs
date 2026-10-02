import { execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import assert from "node:assert/strict";

const url = process.argv[2] || "http://127.0.0.1:5174";
const serverUrl = process.argv[3] || url;
const output = resolve(process.argv[4] || "../.amp/in/artifacts/dashboard");
const session = "clip-ui-capture";
mkdirSync(output, { recursive: true });
function browser(...args) {
  return execFileSync("agent-browser", ["--session", session, ...args], {
    encoding: "utf8",
    timeout: 45000,
  }).trim();
}
const pages = [
  "overview",
  "credentials",
  "oauth",
  "providers",
  "keys",
  "models",
  "payload",
  "quotas",
  "configuration",
  "logs",
  "plugins",
  "system",
];
const results = [];
try {
  browser("open", url);
  browser("set", "viewport", "1440", "1000", "2");
  browser(
    "eval",
    `(()=>{window.__apiCalls=[];const fetch=window.fetch;window.fetch=async(...args)=>{const response=await fetch(...args);window.__apiCalls.push({path:new URL(args[0],location.href).pathname,method:args[1]?.method||'GET',status:response.status});return response};window.__cls=0;new PerformanceObserver(list=>list.getEntries().forEach(e=>{if(!e.hadRecentInput)window.__cls+=e.value})).observe({type:'layout-shift',buffered:true});return true})()`,
  );
  for (const theme of ["dark", "light"]) {
    browser("eval", `document.documentElement.dataset.theme='${theme}'`);
    browser("screenshot", `${output}/login-${theme}.png`, "--full");
  }
  browser("find", "label", "Server URL", "fill", serverUrl);
  browser("find", "label", "Management key", "fill", "orb-dashboard-test-only");
  browser("find", "role", "button", "click", "--name", "Connect to server");
  browser("wait", "--text", "Request traffic");
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  for (const theme of ["dark", "light"]) {
    // Theme button updates Svelte state as well as the document attribute.
    browser(
      "eval",
      `(()=>{if(document.documentElement.dataset.theme!=='${theme}')document.querySelector('[aria-label="Toggle theme"]').click();document.documentElement.dataset.theme='${theme}';return true})()`,
    );
    for (const page of pages) {
      const before = JSON.parse(browser("eval", "window.__apiCalls.length"));
      browser("click", `nav a[href="#${page}"]`);
      browser(
        "wait",
        "--fn",
        `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
      );
      browser(
        "eval",
        "document.fonts.ready.then(()=>new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r))))",
      );
      const state = JSON.parse(
        browser(
          "eval",
          `({route:location.hash,heading:document.querySelector('main h1')?.textContent,error:document.querySelector('main [role=alert]')?.textContent||'',overflow:document.documentElement.scrollWidth>innerWidth,calls:window.__apiCalls.slice(${before})})`,
        ),
      );
      assert.equal(state.route, `#${page}`);
      assert.equal(state.error, "", `${page}: ${state.error}`);
      assert.equal(
        state.overflow,
        false,
        `${page}: horizontal viewport overflow`,
      );
      assert.ok(
        state.calls.some((c) => c.path.endsWith("/config") && c.status === 200),
      );
      assert.ok(
        state.calls.every((c) => c.status === 200),
        `${page}: API request failed`,
      );
      browser("screenshot", `${output}/${page}-${theme}.png`, "--full");
      results.push({ theme, ...state });
      console.log(
        `PASS ${theme} ${page}: ${state.calls.length} successful API calls, no viewport overflow`,
      );
    }
  }
  browser("click", 'nav a[href="#payload"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("find", "role", "button", "click", "--name", "Add rule");
  browser("wait", "--text", "Payload rule added.");
  assert.ok(
    JSON.parse(
      browser(
        "eval",
        `window.__apiCalls.some(c=>c.path==='/v8/management/config/requests/payload/default'&&c.method==='PUT'&&c.status===200)`,
      ),
    ),
  );
  assert.equal(
    JSON.parse(
      browser("eval", `document.querySelectorAll('.rule-row').length`),
    ),
    1,
  );
  browser("find", "role", "button", "click", "--name", "Remove", "--exact");
  browser("wait", "--fn", `document.querySelectorAll('.rule-row').length===0`);
  browser("click", 'nav a[href="#configuration"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("find", "role", "button", "click", "--name", "Open YAML editor");
  browser("wait", "--text", "YAML configuration");
  const original = JSON.parse(
    browser("eval", `document.querySelector('textarea').value`),
  );
  browser(
    "fill",
    "textarea",
    original.replace("request-retry: 3", "request-retry: 2"),
  );
  browser("find", "role", "button", "click", "--name", "Review changes");
  browser("wait", "--text", "Change preview");
  assert.ok(
    JSON.parse(
      browser("eval", `document.querySelectorAll('.diff .added').length`),
    ) > 0,
  );
  browser("screenshot", `${output}/config-diff-light.png`, "--full");
  browser("find", "role", "button", "click", "--name", "Back to editing");
  browser("fill", "textarea", original);
  browser("find", "role", "button", "click", "--name", "Close editor");
  browser("find", "role", "button", "click", "--name", "Visual editor");
  browser("wait", "--text", "Visual configuration");
  browser("screenshot", `${output}/config-visual-light.png`, "--full");
  browser("find", "role", "button", "click", "--name", "Close editor");
  browser("click", 'nav a[href="#oauth"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("find", "role", "button", "click", "--name", "Start provider login");
  browser("wait", "--text", "Open authorization page");
  browser("screenshot", `${output}/oauth-pending-light.png`, "--full");
  browser("find", "role", "button", "click", "--name", "Cancel login");
  browser("click", 'nav a[href="#plugins"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("find", "role", "button", "click", "--name", "Browse plugin store");
  browser("wait", "--text", "Plugin store");
  browser("screenshot", `${output}/plugin-store-light.png`, "--full");
  browser("click", 'nav a[href="#system"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("find", "role", "button", "click", "--name", "Check latest version");
  browser("wait", "--fn", `!!document.querySelector('.release-result')`);
  browser("click", 'nav a[href="#overview"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("set", "viewport", "390", "844", "2");
  browser(
    "eval",
    "new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(r)))",
  );
  assert.equal(
    JSON.parse(
      browser("eval", "document.documentElement.scrollWidth>innerWidth"),
    ),
    false,
  );
  browser("screenshot", `${output}/overview-narrow-light.png`, "--full");
  browser("click", 'nav a[href="#quotas"]');
  browser(
    "wait",
    "--fn",
    `document.querySelector('main')?.getAttribute('aria-busy')==='false'`,
  );
  browser("screenshot", `${output}/quotas-narrow-light.png`, "--full");
  const errors = browser("errors");
  assert.ok(!errors, `Browser errors: ${errors}`);
  const storage = JSON.parse(
    browser(
      "eval",
      `({local:JSON.stringify(localStorage),session:JSON.stringify(sessionStorage)})`,
    ),
  );
  assert.ok(
    !JSON.stringify(storage).includes("orb-dashboard-test-only"),
    "Management key leaked to browser storage.",
  );
  writeFileSync(
    `${output}/browser-results.json`,
    JSON.stringify(
      {
        results,
        errors,
        storage,
        cls: JSON.parse(browser("eval", "window.__cls")),
      },
      null,
      2,
    ),
  );
  console.log(
    "PASS: 12 pages in both themes, YAML diff, visual editor, OAuth pending/cancel, plugin store, release check, narrow layouts, no console errors, no management key in storage.",
  );
} finally {
  browser("close");
}
