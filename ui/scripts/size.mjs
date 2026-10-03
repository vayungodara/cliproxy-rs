// Enforces the bundle budget after `vite build`; the build fails if either grows past it.
// The ceilings started as the sizes of the dashboard this one replaced (2026-10-02: JS
// 42,642 B, CSS 6,853 B). JS was raised on 2026-10-03 to 47,200 B for the owner-requested
// beginner onboarding (first-run checklist, Use with tools page, limits on the overview,
// sign-in help), and again the same day to 49,500 B for the owner-requested Claude usage
// parser (plain names, money, the newer limits list) and plan limits on the overview.
import { readdirSync, readFileSync, copyFileSync } from "node:fs";
import { gzipSync } from "node:zlib";

const BUDGET = { js: 49_500, css: 6_853 };

copyFileSync("dist/index.html", "dist/management.html");
const html = readFileSync("dist/index.html", "utf8");
const assets = readdirSync("dist/assets");
const gz = (text) => gzipSync(text).length;
const inline = [...html.matchAll(/<script>([\s\S]*?)<\/script>/g)].map((m) => m[1]).join("\n");
const size = (ext) =>
  assets.filter((f) => f.endsWith(ext)).reduce((n, f) => n + gz(readFileSync(`dist/assets/${f}`)), 0);
const total = { js: size(".js") + (inline ? gz(inline) : 0), css: size(".css") };

let failed = false;
for (const kind of ["js", "css"]) {
  const over = total[kind] > BUDGET[kind];
  failed ||= over;
  console.log(
    `${kind.toUpperCase().padEnd(3)} ${String(total[kind]).padStart(6)} B gzip  budget ${BUDGET[kind]} B  ${over ? "OVER" : `${BUDGET[kind] - total[kind]} B spare`}`,
  );
}
console.log(`HTML ${gz(html)} B gzip (icon sprite and theme script)`);
const panel = readFileSync("dist-panel/management.html");
console.log(`Panel ${gz(panel)} B gzip, ${panel.length} B raw (dist-panel/management.html, font inlined)`);
if (failed) {
  console.error("Bundle budget exceeded.");
  process.exitCode = 1;
}
