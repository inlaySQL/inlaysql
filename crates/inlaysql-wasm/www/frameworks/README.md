# Using the WASM build from your framework

The browser module (`www/pkg/`, produced by `../build.sh`) is a plain ES
module: `init()` instantiates the engine, then everything is synchronous
JavaScript. That makes the integration the same three steps in every
framework — load, open, query — and this directory is the proof: four
self-contained pages, no build tools, each driving a real query against the
real engine, and each pinned by CI (`.github/workflows/wasm.yml` runs
`browser/frameworks.mjs` in headless Chromium on every push).

Live, runnable versions of every example here are published with the demo at
**[inlaysql.github.io/frameworks/](https://inlaysql.github.io/frameworks/)**:
[vanilla](https://inlaysql.github.io/frameworks/vanilla.html) ·
[jQuery](https://inlaysql.github.io/frameworks/jquery.html) ·
[React](https://inlaysql.github.io/frameworks/react.html) ·
[Vue](https://inlaysql.github.io/frameworks/vue.html).

## The three steps, in any framework

```js
import init, { Database } from "./pkg/inlaysql_wasm.js";

await init();                    // 1. instantiate the WASM module (once per page)
const db = new Database();       // 2. open a database (in memory, or from bytes)
const { columns, rows } = JSON.parse(
  db.query("SELECT id, body FROM docs WHERE id = ?", JSON.stringify([1])),
);                               // 3. ordinary SQL, JSON parameters
```

That is the whole API surface a UI needs: `execute(sql, params)` writes,
`query(sql, params)` reads, `db.export()` hands you the file bytes (persist
them anywhere — OPFS, IndexedDB, a POST to your server), `Database.open(bytes)`
reopens them. Parameters are a JSON array; an inner array of numbers binds as
a `VECTOR`.

## Plain JavaScript (and any "just a script tag" page)

[vanilla.html](https://inlaysql.github.io/frameworks/vanilla.html) is the
whole thing: one `<script type="module">`. Copy it, change the SQL, done. No
dependencies, no toolchain.

If the page is a classic (non-module) script — the typical jQuery-era layout —
load the module lazily with one dynamic `import()`, cache the promise so the
engine initialises exactly once, and call it from any handler:

```js
let enginePromise = null;
function inlaysql() {
  enginePromise ??= import("./pkg/inlaysql_wasm.js").then(async (module) => {
    await module.default();      // the glue exports init as its default export
    return module;
  });
  return enginePromise;
}
```

See [jquery.html](https://inlaysql.github.io/frameworks/jquery.html) for the
complete spelling: jQuery builds the table from `rows`, InlaySQL answers the
`LIKE ?` search — each keeps doing its own job.

## React

[react.html](https://inlaysql.github.io/frameworks/react.html) runs with no
bundler: an import map resolves React from esm.sh and the tree is spelled
`React.createElement` (browsers do not parse JSX — that is the one difference
from a bundled app). The pattern is what a Vite/CRA app writes:

```jsx
const engine = (async () => {
  await init();
  const db = new Database();
  // …seed…
  return db;
})();

function App() {
  const [rows, setRows] = useState(null);
  useEffect(() => {
    engine.then((db) => {
      setRows(JSON.parse(db.query("SELECT id, body FROM docs WHERE body LIKE ?",
        JSON.stringify(["%rust%"]))).rows);
    });
  }, []);
  // …render rows…
}
```

In a bundled app the import becomes `import init, { Database } from
"inlaysql-wasm"` (the npm package name, wired to `pkg/` with your bundler's
WASM support) — the component does not change. Keep the database behind a
module-level promise or a ref: it is cheap to hold, and one engine per page is
the intended shape.

## Vue

[vue.html](https://inlaysql.github.io/frameworks/vue.html) runs with no
bundler: the import map points `vue` at the **browser build with the template
compiler** (`vue/dist/vue.esm-browser.prod.js` — esm.sh's default export is
runtime-only, which silently renders nothing for a string template), and the
component is the same shape a Vite app writes:

```js
const engine = (async () => { await init(); return new Database(); })(); // …seeded…

createApp({
  setup() {
    const rows = ref([]);
    onMounted(async () => {
      const db = await engine;
      rows.value = JSON.parse(db.query("SELECT id, body FROM docs WHERE body LIKE ?",
        JSON.stringify(["%rust%"]))).rows;
    });
    return { rows };
  },
  // …template renders rows…
});
```

In a bundled app the import becomes the npm package; the component does not
change.

## Things worth knowing before you scale it up

- **One engine per page.** `new Database()` is cheap, but there is no reason
  for more than one; share the promise the examples share.
- **The database is a single file.** `db.export()` returns its bytes; store
  them in OPFS to survive reloads (the main demo page does exactly this), or
  open a `.inlay` built by the CLI or the server with
  `Database.open(bytes)`. The format is identical on native and in the
  browser — that is pinned by CI, not asserted in prose.
- **Queries are synchronous** — the engine runs in your page. For long
  queries or a shared database across tabs, put the module in a Web Worker
  and postMessage the queries; the same module loads unmodified in a worker.
- **Search is native.** `bm25_score(col, ?)`, `vector_score(col, ?)` and
  `fuse(...)` work exactly as the [SQL
  surface](../../README.md#the-sql-surface) describes; the site's own demo
  is one hybrid query over this module.
- **This module has no MySQL wire and no filesystem.** Serving a database to
  other processes is the `inlaysql serve --mysql` job; the browser module is
  for the page it runs in.

## What CI checks

`.github/workflows/wasm.yml` builds `pkg/` from source on every push and
drives every page in this directory with Playwright
(`browser/frameworks.mjs`): each page must reach `data-status="ok"` — which
it only sets after the engine answered a real query. A framework release that
breaks one of these patterns fails CI, not a visitor.
