// Run the worker on `workerd` and assert it actually answers.
//
//   npm run smoke            # from crates/inlaysql-wasm/edge
//
// `wrangler dev` in local mode runs the same runtime Cloudflare runs in
// production, with no account and no network deploy — which is the only reason
// this is a CI job rather than a deployment someone eyeballs occasionally.
//
// What it is really testing is not the routes. It is that a `wasm32` build of
// the engine, and a database file written by the *native* build, both survive a
// runtime with no filesystem, no threads and no clock to read.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

// `www/pkg` and the fixture are build output, not checked in. Say so plainly:
// without this the failure is an esbuild "could not resolve" a few screens up.
for (const [what, path] of [
  ["the WASM bundle", "../www/pkg/inlaysql_wasm.js"],
  ["the database image", "./assets/demo.inlay"],
]) {
  if (!existsSync(join(import.meta.dirname, path))) {
    console.error(`${what} (${path}) is missing — run ../build.sh first`);
    process.exit(1);
  }
}

const PORT = Number(process.env.PORT ?? 8787);
const BASE = `http://127.0.0.1:${PORT}`;
const STARTUP_TIMEOUT_MS = 120_000;

let failures = 0;

function check(what, condition, detail) {
  if (condition) {
    console.log(`  ok    ${what}`);
  } else {
    failures += 1;
    console.log(`  FAIL  ${what}${detail === undefined ? "" : ` — ${detail}`}`);
  }
}

async function get(path) {
  const response = await fetch(`${BASE}${path}`);
  return { status: response.status, body: await response.json() };
}

/** Poll `/health` until the runtime is up, or give up loudly. */
async function waitForWorker(child) {
  const deadline = Date.now() + STARTUP_TIMEOUT_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null) {
      throw new Error(`wrangler exited with ${child.exitCode} before serving`);
    }
    try {
      const response = await fetch(`${BASE}/health`);
      if (response.ok) return await response.json();
      // A 500 here is the worker running and failing, which is a real result:
      // report it rather than spinning until the timeout.
      throw new Error(`/health returned ${response.status}: ${await response.text()}`);
    } catch (error) {
      if (!(error instanceof TypeError)) throw error; // not "connection refused"
      await sleep(500);
    }
  }
  throw new Error(`worker did not come up within ${STARTUP_TIMEOUT_MS / 1000}s`);
}

const child = spawn(
  "npx",
  ["--no-install", "wrangler", "dev", "--port", String(PORT), "--ip", "127.0.0.1"],
  {
    cwd: import.meta.dirname,
    stdio: ["ignore", "inherit", "inherit"],
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" },
  },
);

try {
  const health = await waitForWorker(child);
  console.log(`\nworker up: ${JSON.stringify(health)}\n`);

  check("the shipped database opened", health.ok === true);
  check("all eight rows are there", health.docs === 8, `docs=${health.docs}`);
  check("the vector column survived the trip", health.dim === 384, `dim=${health.dim}`);

  const schema = await get("/schema");
  const docs = schema.body.tables.find((table) => table.table === "docs");
  check("the catalog came back", schema.status === 200 && docs !== undefined);
  check(
    "the column types are intact",
    docs?.columns.map((c) => c.type).join(",") === "INTEGER,TEXT,VECTOR(384)",
    JSON.stringify(docs?.columns),
  );

  // The claim this whole stage exists to prove: hybrid retrieval, in one SQL
  // statement, at the edge. Both arms have to contribute — a ranking that
  // BM25 alone would produce is not evidence the vector index survived.
  const lexical = await get("/search?q=embedded%20database&limit=3");
  console.log(`  /search?q=embedded database → ${JSON.stringify(lexical.body.results)}\n`);
  check("hybrid search answered", lexical.status === 200);
  check("it ranked three rows", lexical.body.results?.length === 3);
  check(
    "the top hit is about embedded databases",
    [1, 3].includes(lexical.body.results?.[0]?.id),
    `top was ${JSON.stringify(lexical.body.results?.[0])}`,
  );
  check(
    "scores come back descending",
    lexical.body.results?.every((row, i, all) => i === 0 || all[i - 1].score >= row.score),
  );

  // A query sharing no words with any row: BM25 can only return nothing, so
  // anything ranked here came from the vector index.
  const semantic = await get("/search?q=neighbour%20embeddings&limit=3");
  console.log(`  /search?q=neighbour embeddings → ${JSON.stringify(semantic.body.results)}\n`);
  check("the vector arm ranks on its own", semantic.body.results?.length > 0);

  const missing = await get("/search");
  check("a query with no ?q is a 400", missing.status === 400, `status=${missing.status}`);
  const bad = await get("/search?q=x&limit=999");
  check("an absurd limit is a 400", bad.status === 400, `status=${bad.status}`);
  const nowhere = await get("/nope");
  check("an unknown route is a 404", nowhere.status === 404, `status=${nowhere.status}`);

  const write = await fetch(`${BASE}/search?q=x`, { method: "POST" });
  check("the worker refuses writes", write.status === 405, `status=${write.status}`);
} finally {
  child.kill("SIGTERM");
  // wrangler spawns workerd; give it a moment to take its child with it.
  await sleep(500);
  if (child.exitCode === null) child.kill("SIGKILL");
}

console.log(failures === 0 ? "\nedge smoke test passed" : `\n${failures} check(s) failed`);
process.exit(failures === 0 ? 0 : 1);
