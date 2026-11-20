async (page) => {
  const out = {};
  for (const url of ["https://github.com/vayungodara/cliproxy-rs/tree/readme-quickstart-dashboard", "https://github.com/vayungodara/cliproxy-rs/blob/readme-quickstart-dashboard/README.md"]) {
    await page.goto(url, { waitUntil: "load" });
    await page.waitForSelector("article table");
    const key = url.includes("/tree/") ? "home" : "blob";
    out[key] = {};
    for (const w of [1024, 1280, 1366, 1440, 1920, 390]) {
      await page.setViewportSize({ width: w, height: 900 });
      await page.waitForTimeout(400);
      out[key][w] = await page.evaluate(() => {
        const a = document.querySelector("article");
        const t = [...a.querySelectorAll("table")].find((t) => t.querySelectorAll("thead th").length === 6);
        const td = t.querySelector("td");
        const cs = getComputedStyle(td);
        return { article: a.clientWidth, tableClient: t.clientWidth, tableScroll: t.scrollWidth, font: cs.fontFamily.slice(0, 90), size: cs.fontSize, pad: cs.paddingLeft };
      });
    }
  }
  return JSON.stringify(out);
}
