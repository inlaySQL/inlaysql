# SQL playground (`demo/playground`)

**Live: <https://inlaysql.github.io/demo/playground/>** — published from
`main` alongside the [site-search demo](../site-search/README.md) by the same
`publish.yml` run.

A guided tutorial and a free SQL console against a real InlaySQL database
created **empty** in the visitor's tab. Nothing is canned: every lesson's
statements run through the same WASM module the other demos use, errors are
the engine's own messages, and the database keeps whatever the visitor did to
it until they reset.

## What it teaches

Seven lessons, in the order the concepts build:

1. **Create a table** — SQLite's dialect, `INTEGER PRIMARY KEY` and all
2. **Insert rows** — literals, and the `SELECT` that proves it landed
3. **Read it back** — `WHERE`, `ORDER BY`, `LIMIT`
4. **Change and remove** — `UPDATE`, `DELETE`, idempotence the hard way
5. **Vectors** — `VECTOR(8)`, the `embed('…')` helper binding a real vector
   parameter, and `vector_score` finding pages *about* a topic
6. **Full text** — `CREATE INDEX` on a text column and `bm25_score`
7. **Hybrid** — `fuse(vector_score(…), bm25_score(…))`, reciprocal rank fusion
8. **Free playground** — anything the dialect accepts

Every statement is editable before it runs (`Ctrl/⌘+Enter`), and **Reset
database** recreates the empty file, so the lessons can be broken and re-run
freely.

## Design notes

- **`embed('…')` in SQL** is a playground-side helper: it computes the
  embedding in JavaScript and binds it as a parameter. This mirrors how real
  applications pass model output — as bound parameters, never as SQL text.
  The dimension is 8 here so vectors stay visible; real tables use model
  widths like 384.
- **Statement splitting is on semicolons.** Tutorial SQL contains no
  semicolons inside strings; the console documents the same rule.
- **Errors render verbatim.** The engine refuses what it cannot honour rather
  than accepting and ignoring it — showing that honestly is part of the pitch.
- **No fixture, no build step.** The site-search demo ships a database built
  natively; this one starts from `Database.new()` and the lessons create
  everything. That contrast is deliberate: the same module serves both
  "index built at deploy time" and "schema from scratch, in the tab".

## Files

| File | Role |
| --- | --- |
| `index.html` | The whole demo: lessons, editor, console. Loads the module from `../../pkg/` like the other demos. |

## Run locally

```sh
./crates/inlaysql-wasm/build.sh --serve
# then open http://localhost:8000/demo/playground/
```
