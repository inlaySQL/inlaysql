// Drive the framework examples in headless Chromium.
//
//   npm run smoke:frameworks     # from crates/inlaysql-wasm/browser
//
// The examples in `www/frameworks/` are what the documentation sends
// framework users to, so each one is opened in a real browser and must run a
// real query: the page itself reports `data-status="ok"` only after the
// engine answered. React and Vue load their frameworks from esm.sh over the
// network — a CDN outage fails these pages, loudly, which is the honest
// failure mode for examples that promise "copy this and it works".
//
// It lives outside `www/` for the same reason the main smoke test does:
// `www/` is published verbatim, and test code is not site content.

import { createServer } from "node:http";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { chromium } from "playwright";

const ROOT = join(import.meta.dirname, "..", "www");

if (!existsSync(join(ROOT, "pkg", "inlaysql_wasm.js"))) {
  console.error("the WASM bundle (www/pkg) is missing — run ../build.sh first");
  process.exit(1);
}

const PAGES = ["vanilla.html", "jquery.html", "react.html", "vue.html"];

const server = createServer(async (req, res) => {
  const path = normalize(decodeURIComponent(new URL(req.url, "http://x").pathname));
  const file = join(ROOT, path === "/" ? "index.html" : path);
  if (!file.startsWith(ROOT) || !existsSync(file)) {
    res.writeHead(404).end("not found");
    return;
  }
  const types = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".mjs": "text/javascript; charset=utf-8",
    ".wasm": "application/wasm",
    ".json": "application/json",
  };
  res.writeHead(200, { "content-type": types[extname(file)] ?? "application/octet-stream" });
  res.end(await readFile(file));
});
await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const base = `http://127.0.0.1:${server.address().port}`;

const context = await chromium.launch({ headless: true });
let failed = 0;

for (const page_name of PAGES) {
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));
  try {
    await page.goto(`${base}/frameworks/${page_name}`, { waitUntil: "domcontentloaded", timeout: 30000 });
    await page.waitForSelector('[data-status="ok"]', { timeout: 60000 });
    const status = await page.getAttribute('[data-status="ok"]', "data-status");
    const text = (await page.textContent("body")).slice(0, 120).replace(/\s+/g, " ");
    console.log(`${page_name}: ok (${text})`);
    if (status !== "ok") throw new Error(`status was ${status}`);
  } catch (error) {
    failed += 1;
    console.error(`${page_name}: FAILED — ${error.message.split("\n")[0]}${errors.length ? ` | page errors: ${errors.join(" / ")}` : ""}`);
  }
  await page.close();
}

await context.close();
server.close();
process.exit(failed ? 1 : 0);
