// InlaySQL on Cloudflare Workers: a retrieval index as a static asset.
//
// The interesting part is what is *absent*. There is no database to connect
// to, no connection pool, no region to be far away from and no cold-start
// handshake: the index is bundled with the worker, and the query runs inside
// the isolate that received the request.
//
// The three imports are the whole deployment story:
//
//   - the JavaScript glue and the WASM module, exactly as the browser demo
//     loads them (one `pkg/`, two runtimes);
//   - `demo.inlay`, a database file written by the *native* build. Opening it
//     here is the portability claim being exercised by a real runtime rather
//     than asserted in a comment.

import { Database, initSync, embed } from "../www/pkg/inlaysql_wasm.js";
import module from "../www/pkg/inlaysql_wasm_bg.wasm";
import image from "./assets/demo.inlay";

// Workers hand `.wasm` imports over as a compiled `WebAssembly.Module`, so the
// module is instantiated synchronously at isolate startup rather than fetched.
// This is why the worker needs no `await init()` and no top-level await.
initSync({ module });

/**
 * The database, opened once per isolate and reused.
 *
 * Opening parses a header and a catalog, not the corpus — but an isolate
 * serves many requests, and there is no reason to pay even that per request.
 */
let db;
/** Vector width, read from the shipped schema rather than hard-coded here. */
let dim;

function database() {
  if (!db) {
    db = Database.open(new Uint8Array(image));
    const columns = JSON.parse(db.schema()).tables.find((t) => t.table === "docs")?.columns ?? [];
    const vector = columns.find((column) => column.type.startsWith("VECTOR("));
    if (!vector) throw new Error("the shipped database has no VECTOR column");
    dim = Number(vector.type.slice("VECTOR(".length, -1));
  }
  return db;
}

const json = (body, status = 200) =>
  new Response(JSON.stringify(body, null, 2) + "\n", {
    status,
    headers: {
      "content-type": "application/json; charset=utf-8",
      // The demo is meant to be called from the browser demo page too.
      "access-control-allow-origin": "*",
    },
  });

export default {
  fetch(request) {
    const url = new URL(request.url);

    if (request.method !== "GET") {
      return json({ error: "this worker is read-only; use GET" }, 405);
    }

    try {
      switch (url.pathname) {
        case "/":
          return json({
            what: "InlaySQL compiled to WASM, answering hybrid search at the edge",
            routes: {
              "/health": "module and database liveness",
              "/schema": "the shipped database's tables",
              "/search?q=…&limit=…": "BM25 fused with vector similarity, in one SQL statement",
            },
          });

        case "/health": {
          // Touching the database is the point: a health check that only
          // proves the isolate booted would stay green with a corrupt asset.
          const rows = JSON.parse(database().query("SELECT id FROM docs"));
          return json({
            ok: true,
            docs: rows.rows.length,
            dim,
            imageBytes: image.byteLength,
          });
        }

        case "/schema":
          return json(JSON.parse(database().schema()));

        case "/search": {
          const q = url.searchParams.get("q");
          if (!q) return json({ error: "give me a ?q=" }, 400);

          const limit = Number(url.searchParams.get("limit") ?? 5);
          if (!Number.isInteger(limit) || limit < 1 || limit > 50) {
            return json({ error: "limit must be an integer in 1..50" }, 400);
          }

          // One statement, two retrievers, one ranking — the same SQL the CLI
          // and the browser demo run. `embed` is the engine's own stand-in
          // embedder, so the query vector is bucketed exactly as the vectors
          // in the shipped file were.
          const found = JSON.parse(
            database().query(
              `SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
               FROM docs ORDER BY score DESC LIMIT ${limit}`,
              JSON.stringify([Array.from(embed(q, dim)), q]),
            ),
          );

          return json({
            query: q,
            results: found.rows.map(([id, body, score]) => ({ id, body, score })),
          });
        }

        default:
          return json({ error: `no route ${url.pathname}` }, 404);
      }
    } catch (error) {
      return json({ error: String(error) }, 500);
    }
  },
};
