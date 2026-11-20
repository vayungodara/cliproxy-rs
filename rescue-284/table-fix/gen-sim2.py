import json,sys
cands={n:open(n+'.html').read() for n in sys.argv[1:]}
print('''async (page) => {
  const cands = %s;
  const out = {};
  for (const [key, url] of [["home", "https://github.com/vayungodara/cliproxy-rs/tree/readme-quickstart-dashboard"], ["blob", "https://github.com/vayungodara/cliproxy-rs/blob/readme-quickstart-dashboard/README.md"]]) {
    await page.goto(url, { waitUntil: "load" });
    await page.waitForSelector("article table");
    for (const w of [1280, 390]) {
      await page.setViewportSize({ width: w, height: 900 });
      await page.waitForTimeout(400);
      out[key + w] = await page.evaluate(async (cands) => {
        await document.fonts.ready;
        const a = document.querySelector("article");
        const all = [...a.querySelectorAll("table")].map((t) => `${(t.querySelector("th:nth-child(2)") || t.querySelector("th")).textContent.trim().slice(0, 16)}:${t.clientWidth}/${t.scrollWidth}`);
        const t = [...a.querySelectorAll("table")].find((t) => t.querySelectorAll("thead th").length === 6);
        const res = { article: a.clientWidth, all, mona: document.fonts.check('16px "Mona Sans VF"'), loaded: [...document.fonts].filter((f) => f.status === "loaded").map((f) => f.family).slice(0, 6) };
        const orig = t.innerHTML;
        for (const [name, html] of Object.entries(cands)) {
          const d = document.createElement("div"); d.innerHTML = html;
          t.innerHTML = d.querySelector("table").innerHTML;
          const client = t.clientWidth, scroll = t.scrollWidth;
          const cols = [...t.querySelectorAll("thead th")].map((th) => Math.round(th.getBoundingClientRect().width));
          const height = Math.round(t.getBoundingClientRect().height);
          const save = [t.style.width, t.style.maxWidth];
          t.style.width = "max-content"; t.style.maxWidth = "100px";
          const min = t.scrollWidth;
          const minCols = [...t.querySelectorAll("thead th")].map((th) => Math.round(th.getBoundingClientRect().width));
          [t.style.width, t.style.maxWidth] = save;
          res[name] = { client, scroll, cols, height, min, minCols };
        }
        t.innerHTML = orig;
        return res;
      }, cands);
    }
  }
  return JSON.stringify(out);
}''' % json.dumps(cands))
