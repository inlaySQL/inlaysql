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


if __name__ == "__main__":
    unittest.main()
