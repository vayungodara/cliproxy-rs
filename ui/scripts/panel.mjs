// Builds dist-panel/management.html: the same app as dist/, as one self-contained file for
// Go CLIProxyAPI's static/management.html slot. JS, CSS, the font and the favicon are inlined;
// the file makes no requests except to the Management API of the server that serves it.
// Runs after `vite build`. No dependencies: the inlining is plain string work on Vite's output.
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";

const read = (path) => readFileSync(`dist/${path.replace(/^\.\//, "")}`);
let html = readFileSync("dist/index.html", "utf8");

const take = (pattern, what) => {
  const match = html.match(pattern);
  if (!match) throw new Error(`panel: ${what} not found in dist/index.html`);
  return match;
};
const [scriptTag, scriptSrc] = take(/<script type="module" crossorigin src="([^"]+)"><\/script>/, "module script");
const [styleTag, styleHref] = take(/<link rel="stylesheet" crossorigin href="([^"]+)">/, "stylesheet");

const js = read(scriptSrc).toString("utf8");
if (/<\/script/i.test(js)) throw new Error("panel: script contains </script and cannot be inlined safely");
const font = `data:font/woff2;base64,${read("fonts/host-grotesk.woff2").toString("base64")}`;
const css = read(`assets/${styleHref.split("/").pop()}`)
  .toString("utf8")
  .replace(/url\((["']?)\.\.\/fonts\/host-grotesk\.woff2\1\)/g, `url(${font})`);
if (/url\((?!["']?data:)/.test(css)) throw new Error("panel: stylesheet still references an external file");
const icon = `data:image/svg+xml,${encodeURIComponent(read("favicon.svg").toString("utf8").trim())}`;

html = html
  .replace(scriptTag, () => `<script type="module">${js}</script>`)
  .replace(styleTag, () => `<style>${css}</style>`)
  .replace(/<link rel="preload"[^>]*>\s*/, "")
  .replace(/href="\.\/favicon\.svg"/, () => `href="${icon}"`);
// Only data: URLs and in-page fragments (#i-icon sprite references) may remain.
const external = [...html.matchAll(/\b(?:src|href)="(?!data:|#)([^"]*)"/g)].map((m) => m[1]);
if (external.length) throw new Error(`panel: external references remain: ${external.join(", ")}`);

mkdirSync("dist-panel", { recursive: true });
writeFileSync("dist-panel/management.html", html);
const sha = createHash("sha256").update(html).digest("hex");
writeFileSync("dist-panel/management.html.sha256", `${sha}  management.html\n`);

// Keep the published checksum in PANEL.md in step with the file it describes.
const doc = readFileSync("PANEL.md", "utf8");
const marked = doc.replace(/(<!-- sha256 -->)[\s\S]*?(<!-- \/sha256 -->)/, `$1\`${sha}\`$2`);
if (marked !== doc) writeFileSync("PANEL.md", marked);
console.log(`Panel dist-panel/management.html  ${Buffer.byteLength(html)} B  sha256 ${sha}`);
