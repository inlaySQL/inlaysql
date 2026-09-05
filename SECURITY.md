# Security policy

InlaySQL is an embedded, serverless database: one file, no server, designed to
run inside the calling process. It is early, experimental software — version
0.0.1, never run in production by anyone. That design goal is not a promise
that data cannot leak, a certification, or a substitute for a
deployment-specific security review.

## Reporting a vulnerability

Please report suspected vulnerabilities privately through this repository's
**Security → Report a vulnerability** flow. Do not open a public issue,
discussion, or pull request with exploit details.

Include the affected revision, configuration, impact, and the smallest
reproduction you can safely provide. We will acknowledge the report,
investigate it, and coordinate disclosure with you. Do not access data that is
not yours or test against deployments you do not own.

The fuzzer findings behind several past fixes (parser panics, allocation
bombs, an exponential-backtracking denial of service) were fixed and
disclosed in the open because they were found by our own CI; anything
reported privately will be credited in the fix commit, with the reporter's
consent on naming.

## Scope

Three surfaces have different security models, and a report should say which
one it is about:

- **The embedded library** (`inlaysql`, `inlaysql-core`): the file *is* the
  credential. Anyone who can open the database file can read it, write it, and
  change it; there is no authentication layer by design, because the calling
  process is the trust boundary. The advisory file lock refuses a *second
  process*, and it is advisory — an OS-level guarantee is out of scope.
- **The MySQL wire server** (`inlaysql serve --mysql`): accounts, per-table
  grants and a superuser, enforced on every statement from its *plan*.
  Binds `127.0.0.1` unless told otherwise, and **refuses** a bind that reaches
  another machine unless the database has accounts of its own, the bootstrap
  password is not empty, and TLS is required — or `--plaintext-network` asserts
  a private segment, which the server then checks rather than takes on trust.
  **Plaintext by default** — no `CLIENT_SSL` is advertised, so a client cannot
  be quietly downgraded — and TLS only when a certificate is configured. See
  [`docs/server.md`](docs/server.md) for the model and its stated limits.
- **The WASM module** (browser, edge): runs in the page's origin, no
  capabilities beyond what the page already has. OPFS persistence is
  same-origin storage.

`no_std` in the core is part of the security posture, not just a portability
story: no syscalls, no threads, no wall clock reach the engine except through
traits, and `#![forbid(unsafe_code)]` holds everywhere except
`inlaysql-uring`, whose `unsafe` sits behind the `Device` trait seam.

## Threat model and known limitations

A public summary, in the same spirit the code documents itself: these are
material limitations, not an exhaustive list, and not promises that planned
controls will ship. The ranked, verification-status-tracked version of this
list is [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md).

- **Experimental durability.** Crash safety rests on deterministic
  simulation — thousands of seeded crash and torn-write schedules replayed
  byte-for-byte — which is a good way to find bugs and is **not** the same as
  years of real hardware, real power cuts and real filesystems.
- **The MySQL server's hard ceilings are named limits, not bugs to report:**
  one writer *process* per database file (advisory lock), ~1 MiB transaction
  and statement ceiling, 64 connections, cooperative statement timeouts that
  do not fire inside the SQL parser (fixed-cost parser denials of service
  have been found and fixed before — one more reason to report new ones
  privately), and no TLS on plaintext deployments.
- **First-committer-wins only.** There is no pessimistic locking;
  `SELECT ... FOR UPDATE` is refused. A lost-write race surfaces as MySQL
  error `1213`, whose documented remedy is to retry.
- **Unauthenticated surfaces are exactly as strong as the filesystem.** The
  CLI, the embedded API and the WASM module carry no accounts; anything that
  can read the file has read the database.
- **Known gaps are listed, not hidden.** The [What this is
  not](README.md#what-this-is-not) section of the README and
  [`docs/enterprise-readiness.md`](docs/enterprise-readiness.md) name the
  open items with their verification status — including no TLS on the server
  by default, no point-in-time recovery, and fully resident retrieval indexes
  per connection. A report about one of them is still welcome, but check the
  list first: an item with "open" next to it is known work, not a
  vulnerability we are hiding.

## Dependency posture

The dependency set is deliberately small, and the core crate is forbidden by
CI from taking any OS-facing dependency. The MySQL wire protocol, SHA-1
authentication and the MCP JSON-RPC are hand-rolled in-repo precisely because
the obvious crates would pull ~190 packages through the server. New
dependencies get read before they land; a `cargo update` that bumps
transitive code is reviewed like any other change.

## Supported versions

Security fixes are made on `main` and the latest tagged release. Older
releases may require upgrading — the on-disk format is pre-1.0 and its policy
is *recreate the database*, not migrate, so upgrading has never been the hard
part.
