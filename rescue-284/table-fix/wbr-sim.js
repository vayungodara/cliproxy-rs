async (page) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.reload({ waitUntil: "load" });
  await page.waitForSelector("article table");
  return JSON.stringify(await page.evaluate(() => {
    const t = [...document.querySelectorAll("article table")].find((t) => t.querySelectorAll("thead th").length === 6);
    const before = t.scrollWidth;
    // Force the table to its minimum width to read it.
    t.style.width = "max-content"; t.style.maxWidth = "100px";
    const minBefore = t.scrollWidth;
    for (const c of t.querySelectorAll("code")) {
      if (c.textContent === "/chat/completions") c.innerHTML = "/chat/<wbr>completions";
      if (c.textContent === "config.example.yaml") c.innerHTML = "config.<wbr>example.<wbr>yaml";
    }
    const minAfter = t.scrollWidth;
    const cols = [...t.querySelectorAll("thead th")].map((th) => Math.round(th.getBoundingClientRect().width));
    return { article: document.querySelector("article").clientWidth, before, minBefore, minAfter, cols };
  }));
}
