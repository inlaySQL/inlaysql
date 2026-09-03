# Running everywhere: the WASM build

InlaySQL compiles to `wasm32-unknown-unknown` and runs, unchanged, in a browser
tab and on an edge runtime. This document is what that actually took, what it
costs, and how the two demos are kept honest.

Live demo: **<https://inlaysql.github.io/inlaysql/>** — published from
`main` by [`.github/workflows/wasm.yml`](../.github/workflows/wasm.yml).

## Why there was nothing to port

Nothing in the engine had to change. `inlaysql-core` is `no_std` and reaches the
outside world only through the traits in `inlaysql_core::traits` — `Storage`,
`FullTextIndex`, `VectorIndex`, `Clock`, `Rng`. The project rule that keeps the
core deterministic-simulation-testable is the same rule that makes it portable:
a crate that cannot open a file, start a thread or read a clock has nothing left
in it that a browser would refuse.

So `inlaysql-wasm` is not a port. It supplies two things:

- a `Device` backed by a `Vec<u8>` instead of a file, and
- a `wasm_bindgen` surface (`Database`, `embed`).

That is the whole crate. If something in core ever *does* block `wasm32` — a
syscall, a thread assumption, a `SystemTime::now()` — the fix belongs in core,
not here. The per-pull-request `wasm32` build exists to find that on the day it
lands rather than at release time.

## Persistence, and why it is not in Rust

The database is a `Vec<u8>` in the **same byte layout** the native build writes
to a file. `Database::export()` hands those bytes to JavaScript and
`Database.open(bytes)` takes them back:

```js
// Save to the origin-private file system.
const root = await navigator.storage.getDirectory();
const file = await root.getFileHandle("app.inlay", { create: true });
const writable = await file.createWritable();
await writable.write(db.export());
await writable.close();

// Load it again.
const bytes = new Uint8Array(await (await file.getFile()).arrayBuffer());
const db = Database.open(bytes);
```

Binding OPFS inside the Rust module was the obvious move and it is the wrong
one. OPFS's synchronous access handles only exist inside a worker, so binding
them would put a worker requirement on *every* embedder — including edge
runtimes, which have no OPFS at all but do have a key-value store. Handing the
bytes across is six lines of JavaScript, works in both places, and keeps the
module free of any assumption about where its file lives.

`export()` checkpoints first, so an exported file carries its BM25 and ANN
indexes rather than making whoever opens it rebuild them.

The consequence worth stating plainly: **`sync` is a no-op in this build.**
There is nothing underneath it to flush. Durability in a browser is whatever the
embedder does with the exported bytes, and the crash-safety guarantees in
[`recovery.md`](recovery.md) are about the native build's WAL, not about a tab
that gets closed. `crates/inlaysql-wasm/tests/portability.rs` covers the format
claim in both directions.

## The browser demo

```sh
./crates/inlaysql-wasm/build.sh --serve     # http://localhost:8000
```

`crates/inlaysql-wasm/www/index.html` opens a database, seeds it from
`corpus.json`, and runs hybrid search — all client-side, no server involved once
the page has loaded. It also saves to and loads from OPFS.

`crates/inlaysql-wasm/browser/` drives that page in headless Chromium on every
pull request. It is a separate directory on purpose: `www/` is published to
Pages verbatim and a `node_modules/` has no business being deployed.

### The demos directory

`www/` also carries `demo/`, staged there by `build.sh` from the demo sources
in `crates/inlaysql-wasm/demos/`. Each demo there is a worked answer to "who
is this for", built on the same build-once-natively / ship-as-an-asset pattern
the edge worker uses, and driven by the same browser smoke test.

The first is **[site-search](../crates/inlaysql-wasm/demos/site-search/README.md)** —
full-text plus vector search for a website with no backend at all, published at
`/demo/site-search/` on the Pages site. A database built at deploy time from
the site's pages is fetched as a static asset and queried in the visitor's
browser; there is no search API to attack, log or take down, which is the
deployment shape a government or compliance-bound static site needs.

The second is **[playground](../crates/inlaysql-wasm/demos/playground/README.md)** —
a guided tutorial and free SQL console at `/demo/playground/`, against a
database created empty in the tab. Where site-search shows the engine
answering, the playground shows it *working*: DDL, DML, vectors, BM25 and
hybrid fusion, every statement editable and every error verbatim.## Using it from a framework: React, Vue, jQuery, plain JS

The module is a plain ES module — `init()`, `new Database()`, `query()` — so
the integration is the same three steps everywhere. The worked examples for
each spelling live in `crates/inlaysql-wasm/www/frameworks/` (guide:
[frameworks/README.md](../crates/inlaysql-wasm/www/frameworks/README.md)),
are published live at `/frameworks/` on the Pages site, and are driven in
headless Chromium by the same workflow job that checks the demo page: each
example must answer a real query, so a framework release that breaks one of
these patterns fails CI rather than a visitor. The no-bundler spellings have
one non-obvious requirement each — React without JSX is `createElement`, Vue
needs the browser build *with* the template compiler (esm.sh's default is
runtime-only and silently renders nothing for a string template) — which is
exactly why they are pinned by a test instead of asserted in prose.

## The edge worker

```sh
cd crates/inlaysql-wasm/edge
npm ci
npm run smoke          # runs it on workerd and asserts it answers
npm run dev            # http://localhost:8787
```

The worker's shape is the argument for it:

```js
import { Database, initSync, embed } from "../www/pkg/inlaysql_wasm.js";
import module from "../www/pkg/inlaysql_wasm_bg.wasm";
import image from "./assets/demo.inlay";

initSync({ module });
```

A retrieval index is built **once, natively** — where you have a model, a corpus
and as much time as you like — and shipped to the edge as a static asset. There
is no database to connect to, no pool, no region to be far from: the index is in
the bundle and the query runs in the isolate that took the request. Locally it
answers in single-digit milliseconds.

The shipped `demo.inlay` is written by `cargo run -p inlaysql-wasm --example
edge_fixture` using the ordinary file-backed `inlaysql` crate. That the worker
can open it is the portability claim being exercised by a real runtime rather
than asserted in a comment.

`wrangler dev` runs `workerd`, the runtime Cloudflare runs in production,
entirely locally — no account, nothing deployed. That is what makes the edge
check a CI job instead of something someone remembers to try.

## The same embedder everywhere

`embed(text, dim)` exported from the module is
`inlaysql_core::embedding::hashed_embedding` — the same function the CLI, the
examples and the benchmarks call, which is why it lives in the core rather than
next to the file-backed database.

This matters more than it looks. A database seeded natively and queried in a
browser only returns sensible neighbours if both sides bucket trigrams
identically; a JavaScript lookalike would drift and the failure would show up as
*slightly worse rankings*, which is the kind of bug nobody files.
`the_hashing_is_pinned_across_builds` in `embedding.rs` pins the output so a
change to it is loud.

It is a stand-in, not a model: it hashes character trigrams, so it matches
strings that *spell* alike. Real applications put their own model's output into
the `VECTOR` column and never call it.

## Size

The target is single-digit MB compressed. Where it stands:

| | raw | gzipped |
| --- | ---: | ---: |
| module (`inlaysql_wasm_bg.wasm`) | 2.0 MB | 661 KiB |
| edge database image (8 docs, `VECTOR(384)`) | 1.4 MB | 18 KiB |

The module is built under the `release-wasm` profile — `opt-level = "s"`, LTO,
one codegen unit, `panic = "abort"`, stripped — kept separate from `release` so
the benchmarks are not quietly measured under size-optimised flags.

Every build prints both numbers, CI publishes them to the job summary, and the
job fails outright above 5 MB gzipped. The number is reported on every build
rather than checked occasionally, because size regressions arrive one dependency
at a time.

The database image compresses to a fraction of its size because the file is
mostly sparse pages; do not read 1.4 MB as the cost of eight rows.

## What CI checks

[`wasm.yml`](../.github/workflows/wasm.yml), on every push and pull request:

1. `wasm32-unknown-unknown` builds, and the module size is reported.
2. The native/WASM format portability tests pass.
3. The demo page loads in headless Chromium, ranks rows, runs ad-hoc SQL and
   round-trips the database through OPFS.
4. The worker serves hybrid search on `workerd`, from a database file the native
   build wrote.
5. From `main` only, the demo is published to GitHub Pages.

It runs on GitHub-hosted runners, same as `ci.yml` now does — that used to be
the distinction (AHL-369: the self-hosted runner never had `rustup` on its
PATH), but the split survived the fix and is no longer a workaround. These
jobs need `wasm-bindgen-cli`, headless Chromium and `workerd`, which the image
that builds the workspace for `ci.yml` does not carry; `ci.yml`'s jobs need an
image that can build the workspace, which these do not. The split is by
capability, not runner availability, and it stays.
