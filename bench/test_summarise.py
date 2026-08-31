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


if __name__ == "__main__":
    unittest.main()
