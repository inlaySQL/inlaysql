InlaySQL is an embedded, serverless SQL database in Rust — SQLite's model (one
file, no server) with MVCC concurrent writers and native vector/BM25
retrieval. The full pitch, architecture and benchmarks live in
[`README.md`](README.md). This file is only what changes how you should work
in this repo — read it before your first commit here.

## Hard rules

- **`inlaysql-core` is `no_std` and stays that way.** It cannot open a file,
  read the clock or start a thread — every effect arrives through the traits
  in `inlaysql_core::traits`. The `determinism` job in
  `.github/workflows/ci.yml` fails the build if `#![no_std]` disappears from
  `crates/inlaysql-core/src/lib.rs` or if an OS-facing crate (`libc`, `redb`,
  `tantivy`, `instant-distance`, `mio`, `tokio`, `socket2`, `io-uring`,
  `rusqlite`, `libsqlite3-sys`) enters its dependency tree. Never add a
  `std`-only dependency to `inlaysql-core`.
- **`inlaysql` and `inlaysql-core` are `#![forbid(unsafe_code)]`.** `unsafe`
  is confined to `inlaysql-uring`, behind the `Device` trait seam.
- **SQLite's dialect is the baseline.** `VECTOR(n)` and the retrieval
  functions (`vector_score`, `bm25_score`, `fuse`/`rrf`) are the addition on
  top of it. Full Postgres parity is an explicit non-goal — don't add
  Postgres-only syntax or semantics.
- **MySQL compatibility is a shim in `inlaysql-server`, never a dialect change
  in `inlaysql-core`.** `inlaysql serve --mysql` speaks the MySQL wire
  protocol, and everything MySQL-shaped lives on that side of the seam:
  `AUTO_INCREMENT`, `ENGINE=`, `CHARSET`/`COLLATE`, `UNSIGNED`, `SHOW ...`,
  `information_schema`, MySQL error codes, MySQL function names. Core keeps
  SQLite's dialect and knows nothing about any of it. If a MySQL feature seems
  to need a core change, that is the signal to check whether it is really a
  *SQLite* feature core is missing — add it in SQLite's spelling if so, and
  translate in the shim if not.
- **A clause this project cannot honour is refused, never accepted and
  ignored.** This is the bug class that has cost this repo the most: `INSERT
  ... ON CONFLICT`, `RETURNING`, every `CREATE TABLE` constraint, `WITH`, and
  `ORDER BY 1` all parsed and were silently discarded, so statements reported
  success while doing something else. Prefer a loud `Error::Unsupported` and a
  test that pins the refusal (see
  `crates/inlaysql/tests/sqllogictest/unsupported.test`). Where the shim drops
  a clause it genuinely cannot represent, it reports a `1618` warning that
  names it rather than staying quiet.
- **`crates/inlaysql-server` depends on `inlaysql` and nothing else.** The
  protocol, authentication and SHA-1 are hand-rolled because the obvious crate
  pulls ~190 packages, which is the same trade `inlaysql-mcp` made when it
  hand-rolled JSON-RPC rather than take a Tokio-based SDK. There is no async
  runtime anywhere in this workspace; the server is thread-per-connection, one
  `Database` handle each.
- **Every published benchmark number regenerates from a script in this repo**
  (`bench/run.sh`, `bench/compare.sh`). Don't add a performance claim to
  `README.md` or `bench/README.md` that isn't backed by one of those scripts.
  A number nobody can reproduce is worse than no number.
- **A change to the storage engine, WAL, recovery path or an index format
  needs a DST pass**, not just `cargo test`. `inlaysql_core::mem` provides a
  full in-memory environment (`BTreeMap` storage, logical clock, seeded PRNG)
  so a workload replays byte-for-byte on any machine:

  ```sh
  cargo test --release -p inlaysql-core --test dst_sweep -- --ignored
  cargo test --release -p inlaysql --test index_recovery_dst -- --ignored
  ```

  Thousands of seeds, minutes not seconds — CI only runs these on `main`, on
  a tag, or on a PR labelled `full-ci`. Run them locally yourself when
  touching `btree`, `wal`, `sim`, `hnsw`, `hnsw_paged` or `bm25`.

## Commands that match CI

`.github/workflows/ci.yml`'s `check` job, in order — run these before opening
a PR, not after:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python3 bench/test_summarise.py
cargo run --example hybrid_search -p inlaysql
cargo run -p inlaysql --bin sqllogictest -- crates/inlaysql/tests/sqllogictest/*.test
cargo test -p inlaysql --test backends -- --nocapture
cargo test -p inlaysql-mcp --test client
cargo test -p inlaysql-core --test logic_bugs -- --nocapture
```

`inlaysql-bench` builds `rusqlite` and `sqlite-vec` from bundled C sources, so
a full workspace test run needs a C compiler available.

**The list above is one of four jobs `ci.yml` gates a merge on**, and the
other three fail a merge just as hard. Run all of them on Linux, on the same
toolchain CI installs, without installing anything or writing into the host's
`target/`:

```sh
./docker/test.sh                  # every gating job: determinism, fuzz targets, then the list above
./docker/test.sh check            # just the list above
./docker/test.sh fuzz             # `cargo +nightly check` over fuzz/
./docker/test.sh determinism      # core stays no_std with no OS-facing dependency
./docker/test.sh sweep            # the DST sweeps
./docker/test.sh all              # the gate plus the sweeps
./docker/test.sh shell            # a shell in the same image
./docker/test.sh --toolchain 1.91 check   # pin a Rust, e.g. to bisect a lint
```

**Do not pin this sandbox to an older toolchain than CI uses.** It used to
pin `rust:1.91` while CI's action installed floating `stable`, and that gap is
precisely how a green local run stopped predicting a green CI run: clippy
gains lints with every release, and on 2026-08-19 a `stable` that had reached
1.97 failed `-D warnings` on a `match` 1.91 had never objected to. The image
now follows `stable` and sets `RUSTFLAGS=-D warnings` the way `ci.yml`'s `env:`
does, so a warning anywhere fails here exactly as it does there.

Two things this sandbox still does not cover, both by capability rather than
oversight: `wasm.yml` (needs `wasm-bindgen-cli`, a browser and `workerd`) and
`trust.yml`'s benchmark and fuzzing campaigns (need Docker-in-Docker and
hours). For those, read the workflow.

Prefer this when the host is macOS. CI is Ubuntu, and two things only get
exercised there: the `io_uring` backend, which does not exist on macOS at all,
and the C toolchain, which on macOS depends on whichever SDK `xcode-select`
points at — a beta SDK breaks the bundled SQLite build for reasons that have
nothing to do with this repo. Note that a container under Docker's default
seccomp profile is *not* allowed `io_uring` either, so those tests skip rather
than fail there; only CI really covers that backend.

## Layout

Full map: [`README.md#layout`](README.md#layout). Short version:

| Crate | What it is |
| --- | --- |
| `inlaysql-core` | SQL, planner, executor, storage, retrieval — `no_std` |
| `inlaysql` | file-backed `Device`, `Database`, `AsyncDatabase` — `std` |
| `inlaysql-uring` | `io_uring` `Device` backend, Linux only |
| `inlaysql-mcp` | the `inlaysql` CLI, and MCP server mode |
| `inlaysql-server` | the MySQL wire protocol and its dialect shim — depends on `inlaysql` alone |
| `inlaysql-wasm` | the engine compiled to WebAssembly, plus the browser and edge demos |
| `inlaysql-bench` | benchmark harness, including the SQLite/DuckDB/pgvector comparisons |

## Docs to read before touching...

| Area | Doc |
| --- | --- |
| Anything — testing philosophy, what's covered and what isn't | [`TESTING.md`](TESTING.md) |
| The load-bearing design decisions, and what each rules out | [`docs/architecture.md`](docs/architecture.md) |
| Where a point read's time goes, and what to optimise next | [`PERF.md`](PERF.md) |
| MySQL server mode, its security posture and its divergences | [`docs/server.md`](docs/server.md) |
| What stops a production deployment today, ranked, with verification status | [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md) |
| Crash recovery / WAL | [`docs/recovery.md`](docs/recovery.md) |
| Retrieval indexes (BM25, HNSW, staleness) | [`docs/indexes.md`](docs/indexes.md) |
| MCP server mode | [`docs/mcp.md`](docs/mcp.md) |
| WASM build | [`docs/wasm.md`](docs/wasm.md) |
| SQLancer / fuzzing | [`docs/sqlancer.md`](docs/sqlancer.md) |
| Benchmark methodology | [`bench/README.md`](bench/README.md) |
| The current published numbers, wins and losses | [`BENCHMARK.md`](BENCHMARK.md) |

## PRs

Conventional Commits titles, linking the driving issue key — e.g.
`feat(core): incremental HNSW maintenance instead of rebuilding (AHL-381)`.
That is the convention every commit on `main` already follows.
