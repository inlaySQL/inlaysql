import { createServer } from "node:http";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { chromium } from "playwright";
const ROOT = join(import.meta.dirname, "..", "www");
const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname));
  const file = join(ROOT, path === "/" ? "index.html" : path);
  if (!file.startsWith(ROOT) || !existsSync(file)) { res.writeHead(404).end("nf"); return; }
  const types = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm" };
  res.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
  res.end(await readFile(file));
});
await new Promise(r => server.listen(0, "127.0.0.1", r));
const base = `http://127.0.0.1:${server.address().port}`;
const context = await chromium.launch({ headless: true });
const page = await context.newPage();
page.on("requestfailed", r => console.log("REQFAIL:", r.url().slice(0, 110), r.failure()?.errorText));
page.on("response", r => { if (r.url().includes("esm.sh")) console.log("RESP:", r.status(), r.url().slice(0, 100)); });
page.on("console", m => console.log("CONSOLE:", m.type(), m.text().slice(0, 200)));
page.on("pageerror", e => console.log("PAGEERROR:", e.message.slice(0, 250)));
await page.goto(`${base}/frameworks/vue.html`, { waitUntil: "load", timeout: 30000 });
await page.waitForTimeout(8000);
const el = await page.$("[data-status]");
console.log("status el:", el ? await el.getAttribute("data-status") : "NOT FOUND");
console.log("visible text:", JSON.stringify((await page.textContent("#app"))?.slice(0, 60)));
await context.close(); server.close();
