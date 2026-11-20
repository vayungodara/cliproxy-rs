async (page) => {
  const res = {};
  for (const w of [1280, 390]) {
    await page.setViewportSize({ width: w, height: 900 });
    await page.waitForTimeout(400);
    res[w] = await page.evaluate(() => [...document.querySelectorAll("article table")].map((t) => {
      const h = t.querySelector("th:nth-child(2)")?.textContent.trim().slice(0, 14) || t.querySelector("th")?.textContent.trim().slice(0, 14);
      return `${h}: cols=${t.querySelectorAll("thead th").length} client=${t.clientWidth} scroll=${t.scrollWidth} overflowX=${getComputedStyle(t).overflowX}`;
    }));
  }
  return JSON.stringify(res, null, 1);
}
