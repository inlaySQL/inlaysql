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
page.on("console", m => console.log("CONSOLE:", m.type(), m.text().slice(0, 160)));
page.on("pageerror", e => console.log("PAGEERROR:", e.message.slice(0, 200)));
page.on("requestfailed", r => console.log("REQFAIL:", r.url().slice(0, 90), r.failure()?.errorText));
await page.goto(`${base}/frameworks/jquery.html`, { waitUntil: "domcontentloaded", timeout: 30000 });
await page.waitForTimeout(8000);
console.log("status text:", await page.textContent("#status").catch(() => "?"));
console.log("data:", await page.getAttribute("#status", "data-status").catch(() => "?"));
await page.goto(`${base}/frameworks/vue.html`, { waitUntil: "domcontentloaded", timeout: 30000 });
await page.waitForTimeout(8000);
console.log("vue body:", (await page.textContent("body")).slice(0, 100));
await context.close(); server.close();
