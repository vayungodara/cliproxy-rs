import { readdirSync, readFileSync, copyFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
copyFileSync("dist/index.html", "dist/management.html");
const files = readdirSync("dist/assets").filter((f) => f.endsWith(".js"));
const inline = [
  ...readFileSync("dist/index.html", "utf8").matchAll(
    /<script>([\s\S]*?)<\/script>/g,
  ),
]
  .map((match) => match[1])
  .join("\n");
const bytes = files.reduce(
  (n, file) => n + gzipSync(readFileSync(`dist/assets/${file}`)).length,
  gzipSync(inline).length,
);
console.log(
  `Total JavaScript gzip: ${bytes} bytes (${(bytes / 1024).toFixed(2)} KiB), ${files.length} file(s)`,
);
if (bytes >= 100_000) {
  console.error("JavaScript exceeds the 100 KB budget.");
  process.exitCode = 1;
}
