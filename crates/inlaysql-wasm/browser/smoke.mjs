// Drive the browser demo in headless Chromium.
//
//   npm run smoke            # from crates/inlaysql-wasm/browser
//
// The demo is the page a reader lands on from the README, so "it still loads"
// is not a thing to find out from a bug report. This opens it in a real
// browser, searches, and round-trips the database through the origin-private
// file system — the one part of the persistence story that only exists in a
// browser and therefore cannot be covered by the Rust tests.
//
// It deliberately lives outside `www/`: that directory is published to GitHub
// Pages verbatim, and a `node_modules/` has no business being deployed.

import { createServer } from "node:http";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";
import { chromium } from "playwright";

const ROOT = join(import.meta.dirname, "..", "www");

// `www/pkg` is build output, not checked in. Say so plainly: without this the
// failure is a 404 inside the page, reported as "the module did not load".
if (!existsSync(join(ROOT, "pkg", "inlaysql_wasm.js"))) {
  console.error("the WASM bundle (www/pkg) is missing — run ../build.sh first");
  process.exit(1);
}
const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".wasm": "application/wasm",
};

let failures = 0;
function check(what, condition, detail) {
  if (condition) {
    console.log(`  ok    ${what}`);
  } else {
    failures += 1;
    console.log(`  FAIL  ${what}${detail === undefined ? "" : ` — ${detail}`}`);
  }
}

// A static server rather than `file://`: ES modules, `fetch` and OPFS all need
// a real origin, and `application/wasm` has to be the actual content type.
const server = createServer(async (request, response) => {
  const path = new URL(request.url, "http://localhost").pathname;
  // Directory URLs get their index page, as a host would — Pages serves
  // /demo/site-search/ that way and the demo must not depend on a .html link.
  const rel = path.endsWith("/") ? `${path}index.html` : path;
  const file = join(ROOT, normalize(rel));
  if (!file.startsWith(ROOT)) {
    response.writeHead(403).end();
    return;
  }
  try {
    const body = await readFile(file);
    response.writeHead(200, { "content-type": TYPES[extname(file)] ?? "application/octet-stream" });
    response.end(body);
  } catch {
    response.writeHead(404).end("not found");
  }
});

await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}/`;

const browser = await chromium.launch();
const page = await browser.newPage();
const problems = [];
page.on("pageerror", (error) => problems.push(String(error)));
page.on("console", (message) => {
  if (message.type() === "error") problems.push(message.text());
});

try {
  await page.goto(base, { waitUntil: "networkidle" });

  // The page seeds and searches on load, so waiting for results to stop saying
  // "loading" is waiting for the whole engine to have run.
  await page.waitForFunction(
    () => !document.getElementById("results").textContent.startsWith("loading"),
    { timeout: 60_000 },
  );

  const results = await page.locator("#results").textContent();
  console.log(`\n  search "embedded database":\n${results.replace(/^/gm, "    ")}\n`);
  check("the module loaded and ranked rows", /\[\d+\]/.test(results), results);
  check("the ranking is not an error", !/error|Error/.test(results), results);

  // Plain SQL, to prove the demo is a database and not a search box.
  await page.fill("#sql", "SELECT id, body FROM docs WHERE id = 3");
  await page.click("#run");
  await page.waitForFunction(() => document.getElementById("out").textContent.length > 0);
  const out = await page.locator("#out").textContent();
  check("arbitrary SQL runs client-side", out.includes("rust"), out);

  // OPFS: the only persistence path that exists solely in a browser.
  await page.click("#save");
  await page.waitForFunction(() => /saved|unavailable/.test(document.getElementById("storage").textContent));
  const saved = await page.locator("#storage").textContent();
  check("the database saved to OPFS", /saved \d+ bytes/.test(saved), saved);

  await page.click("#load");
  await page.waitForFunction(() => /loaded|nothing/.test(document.getElementById("storage").textContent));
  const loaded = await page.locator("#storage").textContent();
  check("it came back out of OPFS", /loaded \d+ bytes/.test(loaded), loaded);

  // Reopening must not lose the corpus — a database that round-trips its bytes
  // but forgets its rows would pass every check above.
  await page.waitForFunction(
    () => !document.getElementById("results").textContent.startsWith("loading"),
  );
  const afterReload = await page.locator("#results").textContent();
  check("the reopened database still ranks", /\[\d+\]/.test(afterReload), afterReload);

  // ---- the static-site demo ----
  //
  // Same module, second story: a search box on a website with no backend.
  // The Pages URL is /demo/site-search/, and it is driven here through the
  // directory URL for the same reason Pages would serve it that way.
  await page.goto(`${base}demo/site-search/`, { waitUntil: "networkidle" });
  await page.waitForFunction(
    () => !document.getElementById("status").textContent.startsWith("loading"),
    { timeout: 60_000 },
  );
  const indexed = await page.locator("#status").textContent();
  check("the site-search index opened", /pages indexed/.test(indexed), indexed);

  // Hybrid is the default mode: one statement, both retrievers.
  await page.fill("#q", "renew a passport");
  await page.click("#search");
  await page.waitForFunction(() => document.querySelectorAll("#results li").length > 0);
  const hybrid = await page.locator("#results").textContent();
  check("hybrid search ranks the passport page first", /renew a passport/i.test(hybrid), hybrid);
  check("results carry their source paths", /\/services\/passport\/renew\.html/.test(hybrid), hybrid);

  // The mode toggle is the claim that ranking is just SQL.
  await page.selectOption("#mode", "semantic");
  await page.click("#search");
  await page.waitForFunction(
    () => document.getElementById("sql").textContent.includes("vector_score"),
  );
  const semantic = await page.locator("#results").textContent();
  check("semantic-only search also ranks", /passport/i.test(semantic), semantic);

  // A result opens the page itself — there is no server to navigate to, so
  // the page is rendered from the very row the search ranked.
  await page.locator("#results li").first().click();
  await page.waitForFunction(() => document.getElementById("page-view").classList.contains("open"));
  const pageView = await page.locator("#page-view").textContent();
  check("a result opens the page view", /passport/i.test(pageView), pageView);
  check("the page view holds the full text", pageView.length > 400, pageView.length);
  await page.keyboard.press("Escape");
  await page.waitForFunction(() => !document.getElementById("page-view").classList.contains("open"));
  check("escape returns to the results", (await page.locator("#results li").count()) > 0);

  // ---- the playground ----
  //
  // Lessons run real statements against a database created empty in the tab,
  // so "the tutorial works" means "the engine accepted every step" — checked
  // by running the first lesson and the vector one, which between them cover
  // DDL, DML, and a retrieval score.
  await page.goto(`${base}demo/playground/`, { waitUntil: "networkidle" });
  await page.waitForFunction(
    () => document.getElementById("pg-status").textContent.includes("ready"),
    { timeout: 60_000 },
  );

  await page.click("#run"); // lesson 1: CREATE TABLE
  await page.waitForFunction(() => document.querySelectorAll("#out .block").length > 0);
  const ddl = await page.locator("#out .block").first().textContent();
  check("the playground created a table", /schema changed/.test(ddl), ddl);

  // Lesson 2 ends in a SELECT: a row set arrives without a `kind`, and a
  // regression here once rendered it as "schema changed" instead of a table.
  await page.click("#lesson-2");
  await page.click("#run");
  await page.waitForFunction(() => document.querySelectorAll("#out table").length > 0);
  const books = await page.locator("#out table").textContent();
  check("a SELECT renders as a table of rows", /Dune|Neuromancer/.test(books), books);

  await page.click("#lesson-5"); // vectors, from scratch in a fresh table
  await page.click("#run");
  await page.waitForFunction(() => document.querySelectorAll("#out .block").length > 0);
  const blocks = await page.locator("#out .block").allTextContents();
  check("the vector lesson runs end to end", blocks.some((b) => /rows written|schema changed/.test(b)), blocks.join(" | "));
  check("the vector lesson ranks without errors", !blocks.some((b) => b.includes("Error")), blocks.join(" | "));

  check("nothing threw along the way", problems.length === 0, problems.join(" | "));
} finally {
  await browser.close();
  server.close();
}

console.log(failures === 0 ? "\nbrowser smoke test passed" : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
