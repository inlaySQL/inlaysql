# Changelog

The release notes on GitHub describe the *artifacts* a tag ships — which
library, which client file, which checksum. This file is the other half:
what changed between versions. A ticket id of the form `AHL-NNN` is the
handle the work was done under; the measurement behind it is in
[`PERF.md`](PERF.md)'s dated sections, and the change itself is in
`git log`.

## [Unreleased]

### Fixed
- FFI: a vector component that arrives as a JSON integer (`1`, not `1.0`)
  folds in instead of failing the parameter — every wrapper's JSON writer
  spells a whole float that way.

### Docs
- Roadmap reordered around adoption (README, PLAN 2026-09-14 edition).

## [0.0.6] - 2026-09-08

### Added
- Ruby, C# and Java clients reach the same surface as PHP and Python.
- The PHP and Python clients grow a real surface, and the ABI reports the
  row a write inserted.

### Changed
- Every client file and every library is its own release asset, and one
  command fetches the pair.

### Docs
- The version badge, the experimental banner and the release link say 0.0.5.

## [0.0.5] - 2026-09-08

### Performance
- A read window shorter than the clock's ramp was measuring the ramp; the
  point-read row republishes at 2.10x SQLite WAL (T0.1, T0.2).
- The batch-insert loss is attributed: 0.88x, of which the writes are 1.6%
  (AHL-570).

### Changed
- The writer sweep is fresh and wide again, and goes past eight writers
  (AHL-569).
- Workspace version synced to 0.0.4; the tracked plan copy retired in
  favour of the queue's real home.

### Fixed
- A figure a regeneration retired stops counting as current in the
  benchmark sync check.

### Docs
- A delta inside its own band is a tie, not a result — the 0.88x batch cell
  is published as one (AHL-571).
- The point-read window's method change is stated where the numbers are
  read.

## [0.0.4] - 2026-09-06

### Added
- `inlaysql user list` — the other half of asking who can reach a database.
- Accounts can be created without starting a server.
- `--plaintext-network` asserts a fact about the segment, and is checked
  (AHL-556).
- Four fuzz targets for the bytes an unauthenticated peer can send; the
  eight targets get their own ninety-minute schedule.

### Performance
- The reservation gate's hold is instrumented phase by phase, split, and
  halved (AHL-563).
- A commit record stops copying each page's zero hole — the record was 73%
  zero — and its size budget stays a bound, O(dirty) (AHL-564).
- The free list stops rereading the rows it has already rejected (AHL-565).
- The key comparison stops calling memcmp on every descent (AHL-559).

### Changed
- A database is not served to the network in a posture that gives it away
  (AHL-556).
- Flush pipelining engaged on 88% of barriers and moved nothing; it is
  deleted, not left behind a flag (AHL-562, AHL-566).

### Fixed
- A packet's payload is read as it arrives, not as it is claimed, and the
  SSLRequest shortcut follows the handshake phase rather than a flag.

### Docs
- The README and the homepage become landing pages, and the detail moves
  into `docs/` (AHL-567).
- Fifth, sixth and seventh `run.sh` editions; the cross-engine tables
  regenerate and the containerised write reads as a tie.

## [0.0.3] - 2026-09-05

### Fixed
- The client wrappers find the library beside the archive root (AHL-546).

## [0.0.2] - 2026-09-05

### Added
- Wrapper classes for PHP, Python, Ruby, C# and Java, shipped in every
  release (AHL-546).
- A connection thread's own time, split three ways, so the server's write
  wall can be named — it is the single-writer gate (AHL-555).

### Performance
- A commit's barrier stops paying to grow the file (AHL-553).
- A row-id-shaped comparator was built, measured, and did not clear the bar
  (AHL-554).

### Fixed
- A `TIME` parameter's day count can no longer panic the connection thread.
- The differential campaign finishes again — expensive shapes take a
  ceiling.
- The checkpoint race test queues one writer rather than all six at once.

### Docs
- An install section, a layout map and a testing guide, now that a release
  exists.
- The simple client story, with a tested loader per language (AHL-546).
- SQLite's in-process range figure prints beside the servers', so the two
  verdicts read as one fact.

## [0.0.1.beta] - 2026-09-04

### Added
- First public release: an embedded SQL engine with a `no_std` core over a
  single file, with vector and full-text retrieval as first-class SQL.
- A MySQL-wire server (TLS, accounts and per-table privileges, statement
  timeouts and `KILL`, `SHOW PROCESSLIST`), a C ABI, a WebAssembly build
  and an MCP mode.
- A benchmark harness with a quiet-machine gate, an A/A noise floor and
  published cross-engine tables against SQLite, MySQL 8.4 and
  PostgreSQL 17.

[Unreleased]: https://github.com/inlaySQL/inlaysql/compare/v0.0.6...HEAD
[0.0.6]: https://github.com/inlaySQL/inlaysql/compare/v0.0.5...v0.0.6
[0.0.5]: https://github.com/inlaySQL/inlaysql/compare/v0.0.4...v0.0.5
[0.0.4]: https://github.com/inlaySQL/inlaysql/compare/v0.0.3...v0.0.4
[0.0.3]: https://github.com/inlaySQL/inlaysql/compare/v0.0.2...v0.0.3
[0.0.2]: https://github.com/inlaySQL/inlaysql/compare/v0.0.1.beta...v0.0.2
[0.0.1.beta]: https://github.com/inlaySQL/inlaysql/releases/tag/v0.0.1.beta
