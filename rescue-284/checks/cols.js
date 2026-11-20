async (page) => {
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.waitForTimeout(300);
  return JSON.stringify(await page.evaluate(() => {
    const t = [...document.querySelectorAll("article table")].find((t) => t.querySelectorAll("thead th").length === 6);
    const cols = [...t.querySelectorAll("thead th")].map((th) => `${th.textContent.trim()}=${Math.round(th.getBoundingClientRect().width)}`);
    // The widest unbreakable run in each column.
    const widest = [...t.querySelectorAll("thead th")].map((_, i) => {
      let best = ["", 0];
      for (const td of t.querySelectorAll(`tr > :nth-child(${i + 1})`)) for (const el of td.querySelectorAll("code, a")) {
        const w = el.getBoundingClientRect().width; if (getComputedStyle(el).whiteSpace !== "normal" || el.tagName === "CODE") if (w > best[1]) best = [el.textContent.slice(0, 40), Math.round(w)];
      }
      return best.join(":");
    });
    return { cols, widest, article: document.querySelector("article").clientWidth };
  }));
}
