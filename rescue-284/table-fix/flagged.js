async (page) => {
  await page.goto("https://github.com/vayungodara/cliproxy-rs/tree/readme-quickstart-dashboard", { waitUntil: "load" });
  await page.setViewportSize({ width: 1280, height: 900 });
  await page.waitForSelector("article pre");
  return JSON.stringify(await page.evaluate(() => {
    const a = document.querySelector("article");
    const pres = [...a.querySelectorAll("pre")].filter((p) => p.scrollWidth > p.clientWidth).map((p) => ({ text: p.textContent.trim().slice(0, 70), client: p.clientWidth, scroll: p.scrollWidth, overflowX: getComputedStyle(p).overflowX }));
    const imgs = [...a.querySelectorAll("img")].filter((i) => /\.gif/.test(i.src) || /gif/.test(i.getAttribute("data-canonical-src") || "")).map((i) => ({ src: (i.getAttribute("data-canonical-src") || i.src).slice(-40), w: i.naturalWidth, h: i.naturalHeight, animated: !!i.closest("animated-image") }));
    return { pres, imgs, pageOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth };
  }));
}
