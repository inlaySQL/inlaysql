import { chromium } from "playwright";
const context = await chromium.launch({ headless: true });
const page = await context.newPage();
for (const url of ["https://esm.sh/vue@3.4.38", "https://esm.sh/react@18.3.1"]) {
  try {
    const resp = await page.goto(url, { timeout: 20000 });
    const ct = resp.headers()["content-type"] ?? "?";
    const body = (await page.content()).slice(0, 120).replace(/\s+/g, " ");
    console.log(url, "->", ct, "|", body);
  } catch (e) { console.log(url, "FAILED:", e.message.split("\n")[0]); }
}
await context.close();
