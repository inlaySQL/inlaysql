#!/usr/bin/env python3
"""Regression tests for the repeated-run benchmark summariser."""

from __future__ import annotations

import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location(
    "summarise", Path(__file__).with_name("summarise.py")
)
assert SPEC is not None and SPEC.loader is not None
summarise = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = summarise
SPEC.loader.exec_module(summarise)


BARRIER_SPEC = importlib.util.spec_from_file_location(
    "summarise_barrier", Path(__file__).with_name("summarise_barrier.py")
)
assert BARRIER_SPEC is not None and BARRIER_SPEC.loader is not None
summarise_barrier = importlib.util.module_from_spec(BARRIER_SPEC)
sys.modules[BARRIER_SPEC.name] = summarise_barrier
BARRIER_SPEC.loader.exec_module(summarise_barrier)


class ParseTests(unittest.TestCase):
    def test_comparison_crossing_parity_is_not_a_measured_row(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            slower = Path(directory, "slower.txt")
            faster = Path(directory, "faster.txt")
            slower.write_text(
                "engine joins/s\n"
                "InlaySQL 99\n"
                "SQLite 100\n"
                "InlaySQL is 1.01x slower than SQLite (index)\n",
                encoding="utf-8",
            )
            faster.write_text(
                "engine joins/s\n"
                "InlaySQL 101\n"
                "SQLite 100\n"
                "InlaySQL is 1.01x faster than SQLite (index)\n",
                encoding="utf-8",
            )

            self.assertEqual(summarise.main([str(slower), str(faster)]), 0)
            self.assertEqual(len(summarise.measured(summarise.parse(str(slower)))), 2)
            self.assertEqual(len(summarise.measured(summarise.parse(str(faster)))), 2)


class ColumnNamingTests(unittest.TestCase):
    """A metric's column name is what tells the reader whether a wide spread is
    expected (`max`, one unlucky sample) or is the measurement falling apart
    (`p50`). `bench/compare.sh`'s tables — the ones deciding whether we beat
    MySQL, PostgreSQL, pgvector or DuckDB — are ruled under their headers and
    divided into groups by `|`, and both of those used to defeat the naming.
    """

    # The OLTP table `bench/compare.sh` prints, trimmed to two rows: a ruled
    # header, `|`-divided column groups, and two-word column names on either
    # side of the divider.
    OLTP = (
        "engine                    write ops/s      p50      p95 |  read ops/s      p50      p95\n"
        "---------------------------------------------------------------------------------------\n"
        "InlaySQL                        278.8   3.82ms   4.74ms |    812408.1   1.00us   4.00us\n"
        "MySQL 8                        1598.8 572.00us 789.00us |     10594.5  95.00us 103.00us\n"
    )

    def _first_row_columns(self, text: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory, "run.txt")
            path.write_text(text, encoding="utf-8")
            row = summarise.measured(summarise.parse(str(path)))[0]
            return summarise.columns(row.header, row.spans)

    def test_a_rule_under_the_header_is_not_mistaken_for_the_header(self) -> None:
        # Before: the `-----` line had no numbers, so it became the header, and
        # every value under it was named col1..colN.
        self.assertNotIn("col1", self._first_row_columns(self.OLTP))

    def test_columns_are_named_across_a_group_divider(self) -> None:
        # Word order would count `|` as a name and shift everything after it;
        # two-word names (`write ops/s`) would shift everything after them too.
        self.assertEqual(
            self._first_row_columns(self.OLTP),
            ["ops/s", "p50", "p95", "ops/s", "p50", "p95"],
        )

    def test_an_unruled_table_still_names_its_columns(self) -> None:
        # `bench/run.sh`'s own tables have no rule and no divider. They were
        # named correctly before this change and have to stay that way.
        unruled = (
            "engine                   ops/s      p50      p95      p99      max\n"
            "InlaySQL                   250   3.91ms   4.66ms   7.98ms  12.76ms\n"
        )
        self.assertEqual(
            self._first_row_columns(unruled),
            ["ops/s", "p50", "p95", "p99", "max"],
        )

    def test_a_table_with_no_header_at_all_falls_back_to_positions(self) -> None:
        self.assertEqual(self._first_row_columns("InlaySQL 250 3.91ms\n"), ["col1", "col2"])


class ContaminationTests(unittest.TestCase):
    """`bench/run.sh` now samples load throughout a run and marks the result
    file CONTAMINATED, on a `load:` line, when a sample exceeded the gate
    mid-run. That line is provenance (`DROP` strips it from `parse()`'s
    measured lines), so the only way this flag can do its job is if
    `summarise.py` checks for it independently of `parse()` and refuses to
    let it go missing when runs are combined.
    """

    def _write_pair(self, directory: str, *, contaminate_one: bool) -> tuple[Path, Path]:
        clean_a = Path(directory, "a.txt")
        clean_b = Path(directory, "b.txt")
        body_a = "engine ops/s\nInlaySQL 100\n"
        body_b = "engine ops/s\nInlaySQL 102\n"
        if contaminate_one:
            body_a += (
                "\nload:   *** CONTAMINATED: load exceeded "
                "BENCH_MAX_LOAD_PER_CPU=0.25 during this run (max 9.0/18) — "
                "do not treat these numbers as clean; see PERF.md §4 ***\n"
            )
        clean_a.write_text(body_a, encoding="utf-8")
        clean_b.write_text(body_b, encoding="utf-8")
        return clean_a, clean_b

    def test_clean_runs_report_no_contamination(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            a, b = self._write_pair(directory, contaminate_one=False)
            self.assertEqual(summarise.contaminated(str(a)), False)
            self.assertEqual(summarise.contaminated(str(b)), False)
            self.assertEqual(summarise.warn_contaminated([str(a), str(b)]), [])

    def test_one_contaminated_run_is_detected_and_named(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            a, b = self._write_pair(directory, contaminate_one=True)
            self.assertEqual(summarise.contaminated(str(a)), True)
            self.assertEqual(summarise.contaminated(str(b)), False)
            self.assertEqual(summarise.warn_contaminated([str(a), str(b)]), [str(a)])

    def test_contamination_survives_being_combined_with_a_clean_run(self) -> None:
        # The load: line must not turn into a measured `Line` and break the
        # cross-run structural comparison just because it also happens to be
        # the run this test wants flagged.
        with tempfile.TemporaryDirectory() as directory:
            a, b = self._write_pair(directory, contaminate_one=True)
            self.assertEqual(len(summarise.measured(summarise.parse(str(a)))), 1)
            self.assertEqual(len(summarise.measured(summarise.parse(str(b)))), 1)
            self.assertEqual(summarise.main([str(a), str(b)]), 0)


def _run(
    writers: int, ops: int, idle_pct: float, hold: float, pipeline: bool = True
) -> str:
    """One writer level's worth of concurrency-suite output, as the suite prints it.

    `pipeline=False` is the line as it reads once AHL-566 took the pipeline's
    two counters back off it.
    """
    tail = (
        "; pipeline 12 handoffs (83% of barriers), "
        "overlapped gather 0.679 ms/barrier"
        if pipeline
        else ""
    )
    return (
        f"  barriers: {writers} writers, 181 normal flushes over 2400 commits "
        f"(0.075 syncs/commit, 13.33 commits/sync)\n"
        f"  barrier cycle: {writers} writers, 470.0 barriers/s — fsync 1.169 ms, "
        f"interval 2.128 ms, idle 0.959 ms ({idle_pct}% of the wall clock has no "
        f"flush in flight); coordinator gather 0.827 post 0.049 gap 0.221 "
        f"ms/barrier{tail}\n"
        f"  buckets: {writers} writers, busy 6097.5 ms over 2400 commits — "
        f"gate_wait 25.0%, gate_hold 3.3%, follower_wait 63.3%, gather_spin 2.5%, "
        f"fsync 3.6%, post 0.1%, pre-gate residual 2.2% (2404 gate waits, 2247 "
        f"racing holds)\n"
        f"  gate hold: {writers} writers, 2404 holds, {hold} ms mean — "
        f"read 0.008 (12173 calls), state 0.000 (16), wal 0.005 (2416, 14.6 KiB), "
        f"data 0.018 (2400, 23.3 KiB), of which extend 0.010 (9 extensions); "
        f"device 0.030 ms (35.8%), residual 0.054 ms (64.2%), 3 commit-point "
        f"misses\n"
        f"InlaySQL (parallel WAL regions)  {writers}  {ops}  2400  0.0%\n"
        f"SQLite (journal, sync=FULL, fullfsync)  {writers}  179  2400  0.0%\n"
    )


class BarrierSummaryTests(unittest.TestCase):
    """The paired-log summariser behind AHL-566's re-run and its A/A control."""

    def test_a_repetition_missing_one_arm_drops_out_of_the_pairing(self) -> None:
        # The whole point of the interleaved design is that a ratio compares
        # two arms from the *same* moment. A summariser that paired by
        # position instead of by repetition would silently divide rep 2's
        # `on` by rep 1's `off` here and report 2.00 for a harness that
        # measured 1.00.
        log = (
            "=== rep 1 arm off ===\n" + _run(16, 1000, 45.1, 0.084) +
            "=== rep 2 arm on ===\n" + _run(16, 2000, 45.1, 0.084) +
            "=== rep 3 arm off ===\n" + _run(16, 1000, 45.1, 0.084) +
            "=== rep 3 arm on ===\n" + _run(16, 1100, 45.1, 0.084)
        )
        runs = summarise_barrier.parse(log)
        self.assertEqual(
            summarise_barrier.ratios(runs, "on", "off", "ops", 16), [1.1]
        )

    def test_duty_is_the_share_with_a_flush_in_flight_not_the_idle_share(self) -> None:
        # The suite prints the *idle* percentage; every PERF.md table quotes
        # the duty cycle. Reporting one as the other flips the direction of
        # the headline metric of AHL-561 through AHL-566.
        runs = summarise_barrier.parse("=== rep 1 arm a ===\n" + _run(16, 1000, 45.1, 0.084))
        self.assertAlmostEqual(runs[(1, "a", 16)]["duty"], 54.9)

    def test_metrics_are_attributed_to_the_arm_in_scope_when_the_order_flips(self) -> None:
        # Arm order flips every repetition, so an off-by-one in the header
        # handling shows up only on the even repetitions — and would credit
        # the pipeline arm with the control's numbers half the time.
        log = (
            "=== rep 1 arm off ===\n" + _run(8, 100, 45.1, 0.084) +
            "=== rep 1 arm on ===\n" + _run(8, 200, 45.1, 0.084) +
            "=== rep 2 arm on ===\n" + _run(8, 400, 45.1, 0.084) +
            "=== rep 2 arm off ===\n" + _run(8, 100, 45.1, 0.084)
        )
        runs = summarise_barrier.parse(log)
        self.assertEqual(runs[(2, "on", 8)]["ops"], 400.0)
        self.assertEqual(
            summarise_barrier.ratios(runs, "on", "off", "ops", 8), [2.0, 4.0]
        )

    def test_throughput_comes_from_the_inlaysql_row_and_not_sqlites(self) -> None:
        runs = summarise_barrier.parse("=== rep 1 arm a ===\n" + _run(4, 3456, 45.1, 0.084))
        self.assertEqual(runs[(1, "a", 4)]["ops"], 3456.0)

    def test_each_writer_level_keeps_its_own_row(self) -> None:
        # Every line the suite prints is prefixed with its writer count; a
        # parser that ignored it would collapse five levels into one and make
        # the per-level noise floor this script exists to produce meaningless.
        log = "=== rep 1 arm a ===\n" + _run(1, 1300, 5.7, 0.037) + _run(16, 6500, 45.1, 0.084)
        runs = summarise_barrier.parse(log)
        self.assertEqual(runs[(1, "a", 1)]["ops"], 1300.0)
        self.assertEqual(runs[(1, "a", 16)]["ops"], 6500.0)
        self.assertAlmostEqual(runs[(1, "a", 1)]["duty"], 94.3)

    def test_output_before_the_first_arm_header_is_not_attributed_to_an_arm(self) -> None:
        # `FILESYSTEM:` and a cargo build precede the first header; a warm-up
        # run appearing there would be counted as rep 0 of some arm.
        log = "FILESYSTEM: /dev/vdb1 btrfs\n" + _run(16, 9999, 45.1, 0.084)
        self.assertEqual(summarise_barrier.parse(log), {})

    def test_a_log_without_the_retracted_pipeline_clause_still_parses(self) -> None:
        # AHL-562 added `pipeline N handoffs ... overlapped gather` to the
        # `barrier cycle` line and AHL-566 removed it again. A parser anchored
        # on the longer form would silently return nothing for every log this
        # harness produces from now on — nothing, not an error, because the
        # line simply would not match.
        runs = summarise_barrier.parse(
            "=== rep 1 arm a ===\n" + _run(16, 6500, 45.1, 0.084, pipeline=False)
        )
        self.assertEqual(runs[(1, "a", 16)]["ops"], 6500.0)
        self.assertAlmostEqual(runs[(1, "a", 16)]["duty"], 54.9)
        self.assertNotIn("handoff_pct", runs[(1, "a", 16)])


if __name__ == "__main__":
    unittest.main()
