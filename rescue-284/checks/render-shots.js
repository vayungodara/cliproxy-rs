// Captures the rendered README on GitHub's blob page in light, dark and mobile.
// For each mode: the top (article start to "Other ways to install") with the details
// closed, the same with them open, and the tables from "Supported providers" to
// "API keys". Returns the measurements it took.
async (page) => {
  const out = [];
  const modes = [
    ["light", 1280, 900, "light"],
    ["dark", 1280, 900, "dark"],
    ["mobile-light", 390, 844, "light"],
    ["mobile-dark", 390, 844, "dark"],
  ];
  const dir = "/home/vayun/projects/.amp/in/artifacts/readme-final";
  const y = async (sel) =>
    page.evaluate((s) => {
      const el = [...document.querySelectorAll("article h1, article h2")].find((h) => h.textContent.trim() === s);
      return el ? Math.round(el.getBoundingClientRect().top + window.scrollY) : -1;
    }, sel);
  for (const [name, w, h, scheme] of modes) {
    await page.setViewportSize({ width: w, height: h });
    await page.emulateMedia({ colorScheme: scheme });
    await page.reload({ waitUntil: "load" });
    await page.waitForSelector("article");
    // Load lazy images, then wait until every image in the article has decoded.
    await page.evaluate(async () => {
      for (let p = 0; p < document.body.scrollHeight; p += 600) {
        window.scrollTo(0, p);
        await new Promise((r) => setTimeout(r, 60));
      }
      window.scrollTo(0, 0);
      await Promise.all([...document.querySelectorAll("article img")].map((i) => (i.complete ? null : new Promise((r) => { i.onload = i.onerror = r; }))));
    });
    await page.waitForTimeout(800);
    const info = await page.evaluate(() => {
      const a = document.querySelector("article");
      const r = a.getBoundingClientRect();
      const imgs = [...a.querySelectorAll("img")].map((i) => ({
        src: (i.currentSrc || i.src).split("/").slice(-1)[0].slice(0, 40),
        w: Math.round(i.getBoundingClientRect().width),
        natural: i.naturalWidth,
        ok: i.complete && i.naturalWidth > 0,
      }));
      const wide = [...a.querySelectorAll("table, pre")].filter((t) => t.scrollWidth > t.clientWidth + 1 && getComputedStyle(t).overflowX === "visible").length;
      return {
        mode: document.documentElement.getAttribute("data-color-mode"),
        bg: getComputedStyle(document.body).backgroundColor,
        article: { x: Math.round(r.left), w: Math.round(r.width) },
        pageOverflow: document.documentElement.scrollWidth - window.innerWidth,
        captionLines: (() => { const p = [...a.querySelectorAll("p")].find((e) => e.textContent.includes("How it was recorded")); return Math.round(p.getBoundingClientRect().height / parseFloat(getComputedStyle(p).lineHeight)); })(),
        badgeRows: new Set([...a.querySelectorAll("p img")].filter((i) => /badge|shields/.test(i.src)).map((i) => Math.round(i.getBoundingClientRect().top))).size,
        details: a.querySelectorAll("details").length,
        unscrollableWide: wide,
        broken: imgs.filter((i) => !i.ok).map((i) => i.src),
        imgs: imgs.filter((i) => /gif|overview/.test(i.src) && i.w > 0).map((i) => `${i.src}:${i.w}`).join(" "),
      };
    });
    await page.evaluate(() => document.querySelectorAll("article details").forEach((d) => (d.open = true)));
    const tall = await page.evaluate(() => document.documentElement.scrollHeight);
    await page.evaluate(() => document.querySelectorAll("article details").forEach((d) => (d.open = false)));
    await page.setViewportSize({ width: w, height: tall + 200 });
    await page.evaluate(() => window.scrollTo(0, 0));
    await page.waitForTimeout(500);
    const top = await page.evaluate(() => Math.round(document.querySelector("article").getBoundingClientRect().top + window.scrollY));
    const clip = async (file, from, to) => {
      const a = await page.evaluate(() => {
        const r = document.querySelector("article").getBoundingClientRect();
        return { x: Math.max(0, Math.round(r.left) - 8), w: Math.round(r.width) + 16 };
      });
      await page.screenshot({ path: `${dir}/${file}`, clip: { x: a.x, y: from - 8, width: Math.min(a.w, w - a.x), height: to - from + 8 } });
    };
    await clip(`${name}-top.png`, top, await y("Other ways to install"));
    await page.evaluate(() => document.querySelectorAll("article details").forEach((d) => (d.open = true)));
    await page.waitForTimeout(300);
    await clip(`${name}-quickstart-open.png`, await y("Quick start"), await y("Other ways to install"));
    await page.evaluate(() => document.querySelectorAll("article details").forEach((d) => (d.open = false)));
    await clip(`${name}-tables.png`, await y("Supported providers"), await y("API keys: Codex CLI and mixed providers"));
    out.push({ name, ...info });
  }
  return JSON.stringify(out, null, 1);
}
