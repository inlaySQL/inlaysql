# Logic-bug testing: SQLancer, what we run instead, and how findings are triaged

A crash announces itself. A `WHERE` clause that silently drops a row does not,
and no amount of fuzzing for panics will find one.
[SQLancer](https://github.com/sqlancer/sqlancer) is the tool that made this
class of bug findable, and this page states exactly how much of it InlaySQL
has, what it does not have and why, and what happens when something is found.

## What runs today

| Technique | Where | Oracle | Runs |
| --- | --- | --- | --- |
| TLP (ternary logic partitioning) | `crates/inlaysql-core/tests/logic_bugs.rs` | the database against itself | every `cargo test` |
| PQS-style row retrieval | `crates/inlaysql-core/tests/logic_bugs.rs` | a row known to exist | every `cargo test` |
| Differential vs SQLite | `crates/inlaysql/tests/differential.rs` | SQLite | every `cargo test` (200 rounds); 50,000 in the trust workflow |

The differential test is the strongest of the three and the closest thing here
to SQLancer's own differential mode. The dialect's stated baseline is SQLite
compatibility, which makes SQLite the specification: a disagreement is a bug in
InlaySQL by definition, with no argument about which engine is right.

It generates a random table (integers, text, and `NULL` in both) and a random
predicate built from comparisons, `AND`, `OR`, `NOT` and `IS NULL`, then
compares the row sets both engines return. As of the run that added it, 50,000
seeds agree.

## What SQLancer itself would add, and what it needs first

SQLancer would bring generators tuned over years, more oracles (NoREC, CERT,
DQP) and a far wider expression grammar than the hundred lines in
`differential.rs`. It is worth having.

**The blocker is an interface, not effort.** SQLancer is a Java program that
drives a database over JDBC. InlaySQL is a Rust library that runs inside the
caller's process; it has no wire protocol and no JDBC driver, so there is
nothing for SQLancer to connect to. Reaching it needs one of:

1. **A JDBC driver over a socket protocol.** Most faithful, most work: a
   server mode, a wire protocol, and a driver implementing enough of
   `java.sql` for SQLancer's provider to run.
2. **A JDBC driver over the MCP server** (`crates/inlaysql-mcp`), which already
   speaks a line protocol with a `query` tool. Smaller: no new server, but the
   protocol is shaped for language models rather than for a driver, and its
   read-only refusals would have to be relaxed for a fuzzer to write.
3. **Port the oracles instead of the harness.** What `differential.rs` and
   `logic_bugs.rs` do. No JVM, no bridge, and it runs on every push — but the
   generator is ours, so it is only as good as we make it.

Today the project is on option 3 by choice: the dialect is small enough that a
generator covering it fits on a screen, and a bridge would be a substantial
amount of code testing a surface that is still growing. That trade should be
revisited once joins, aggregates and subqueries exist, because that is the
point where hand-written generators stop keeping up. Until then, this page is
the honest answer to "do you run SQLancer?": **no — here is what we run
instead, and here is what it costs to change that.**

## Triage

A logic-bug report is not a stack trace. It is a claim that two things that
must agree do not, and triaging it means finding out which one is wrong.

### 1. Reproduce from the seed

Every generated test prints the seed it failed on, and every generator is a
pure function of that seed.

```sh
cargo test -p inlaysql --test differential                 # prints seed and predicate
cargo test -p inlaysql-core --test logic_bugs -- --nocapture
```

A failure that does not reproduce from its seed is a bug in the test harness
and takes priority over the finding: it means the suite is not deterministic,
and nothing it reports can be trusted.

### 2. Shrink it by hand

The generated table is a dozen rows and the predicate a handful of terms, both
printed in the failure message. Delete rows until it passes, then put back the
last one; strip the predicate to its smallest failing sub-expression. The
result should be a table of two or three rows and one predicate.

### 3. Decide which engine is right

| Situation | Verdict |
| --- | --- |
| Differential test, both engines answered | SQLite is the specification. InlaySQL is wrong. |
| Differential test, InlaySQL returned `Unsupported`/`Parse` | Not a bug: a gap. The test skips these and fails only if too many rounds are skipped. |
| TLP partition broken | InlaySQL is inconsistent with itself. Always a bug, no comparison needed. |
| Row retrieval missed a row | Same: a row it can scan and cannot filter to is a bug. |
| Disagreement involves cross-type comparison in a `WHERE`, join `ON` or aggregate | The dialect now claims SQLite's affinity *comparison* rule too (AHL-486: a `TEXT` operand converts to a number before comparing against `INTEGER`/`REAL`/`NUMERIC` affinity, or the reverse renders as text against `TEXT` affinity — see `docs/server.md`'s "Comparison-time affinity" section), so this is a real bug, not a generator gap, for the same reason the `UNION`/`INTERSECT`/`EXCEPT` row below is. `differential.rs`'s `leaf()` generates the cross-affinity shape on purpose (arms 20-29, `a`/`b` against a literal of the other's storage class); most other generators in the file still stay type-consistent for everything *else* they were never built to exercise (dates read as text, function argument shapes, and so on), so check the specific generator only if the disagreement is not simply `a`/`b` against a cross-class literal. |
| Disagreement involves `UNION`/`INTERSECT`/`EXCEPT` mixing storage classes | Also a real bug: since AHL-477 fixed `mem_cmp`/`compare_values` to rank a cross-class pair by SQLite's own fixed storage-class order instead of raising or silently treating it as equal, `compound_queries_agree_with_sqlite` mixes `INTEGER`/`TEXT` arms on purpose, and a disagreement there is a total-order bug, not a generator smell. |

### 4. File it as a regression test, then fix it

The shrunk case becomes a named test next to the property that found it —
`crates/inlaysql-core/tests/logic_bugs.rs` for a self-consistency bug, or a
SQL Logic Test file in `crates/inlaysql/tests/sqllogictest/` when SQLite's
answer is the expected output. That way the fix is proved by a test that reads
like a specification rather than by a seed nobody will run again.

Only then change the engine. A logic bug fixed without a test that would have
caught it will come back, because the property that found it is random and may
not generate that shape again for a hundred thousand seeds.

### 5. What not to do

Do not raise the seed count to make a failing seed rarer, and do not narrow a
generator to avoid a shape that keeps failing. Both turn a finding into a
silence. If a shape genuinely is out of scope — a construct the dialect does
not implement — the generator should skip it *explicitly*, with a comment
saying so, and the skip should be counted so it cannot grow unnoticed.
