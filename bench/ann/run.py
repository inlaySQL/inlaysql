"""Run `module.InlaySQL` under the `ann-benchmarks` protocol, without Docker.

`ann-benchmarks` proper runs every algorithm in its own container and drives it
from `run.py`/`ann_benchmarks/runner.py`. That is the canonical way to produce a
number and `bench/README.md` says how to do it. This script is the same protocol
in one file, so the adapter can be exercised — and a real recall/QPS curve
published — from a checkout of *this* repository with nothing but Python and a
`cargo build`.

It is a reimplementation of the measurement, not a reinterpretation of it. Every
definition below is `ann-benchmarks`', named where it lives upstream:

* the corpus, the queries and the **ground-truth neighbours** are the published
  HDF5 file, downloaded unmodified from ann-benchmarks.com. Nothing here
  computes an oracle, and nothing here subsets the corpus: the `neighbors`
  array is the truth for the *whole* `train` set, so a run over part of it
  would be scored against neighbours that are not in the index.
* `angular` distance is Euclidean distance between L2-normalised vectors
  (`ann_benchmarks/distance.py`), which is what the dataset's own `distances`
  array holds — not `1 - cos`. The two rank identically and their *values*
  differ, and the recall threshold below compares values.
* recall is `knn` from `ann_benchmarks/plotting/metrics.py`: for each query,
  count how many of the returned top-`count` distances are within
  `true_distances[count - 1] + epsilon`, `epsilon = 1e-3`. Distance-based, not
  id-based, so a tie at the k-th neighbour is not scored as a miss.
* QPS is `1.0 / best_search_time`, and `best_search_time` is the *minimum*
  across `--runs` passes of `(sum of per-query wall clock) / len(test)`
  (`ann_benchmarks/runner.py::run_individual_query`).

Results are written in `ann-benchmarks`' own result layout —
`results/<dataset>/<count>/inlaysql/<args>.hdf5`, with its `times`, `neighbors`
and `distances` datasets and its attribute names — so an `ann-benchmarks`
checkout can plot or export these files alongside every other engine's without
converting anything.

    python bench/ann/run.py --dataset random-xs-20-angular
    python bench/ann/run.py --dataset glove-25-angular --runs 5
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
import urllib.request

import h5py
import numpy

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from module import InlaySQL  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
DATA = os.path.join(HERE, "data")
RESULTS = os.path.join(HERE, "results")

# Where ann-benchmarks publishes its datasets. Same URL its own
# `ann_benchmarks/datasets.py` downloads from.
BASE_URL = "http://ann-benchmarks.com"

# The `ef_search` sweep — the candidate list the graph walk holds, set per
# connection with `SET inlaysql_hnsw_ef_search`. The same values pgvector's own
# ann-benchmarks config sweeps, so the two engines' curves are sampled at the
# same operating points.
#
# This used to be an over-fetch factor (`LIMIT n * over_fetch`), because
# `ef_search` lived only in Rust. At k = 10 the first three points of that
# sweep all ran the identical walk — `ef = max(64, 2k)` ignores an over-fetch
# below 3.2x — and produced three copies of one recall number.
#
# Every value has to be at least `--count`: a beam narrower than the answer
# cannot hold it, and this engine refuses that rather than returning a short
# list. `sweep_for` drops the ones that are.
DEFAULT_QUERY_ARGS = [10, 20, 40, 80, 120, 200, 400, 800]

EPSILON = 1e-3


# --------------------------------------------------------------------- dataset


def dataset_path(name: str) -> str:
    os.makedirs(DATA, exist_ok=True)
    path = os.path.join(DATA, f"{name}.hdf5")
    if not os.path.exists(path):
        url = f"{BASE_URL}/{name}.hdf5"
        print(f"downloading {url}", file=sys.stderr)
        temporary = path + ".part"
        # A User-Agent, because the bucket in front of ann-benchmarks.com
        # answers urllib's default with 403. Streamed rather than read whole:
        # the larger datasets are hundreds of megabytes.
        request = urllib.request.Request(url, headers={"User-Agent": "inlaysql-bench/1"})
        with urllib.request.urlopen(request) as response, open(temporary, "wb") as handle:
            shutil.copyfileobj(response, handle)
        os.rename(temporary, path)
    return path


# --------------------------------------------------------------------- metrics


def normalise(X: numpy.ndarray) -> numpy.ndarray:
    norms = numpy.linalg.norm(X, axis=-1, keepdims=True)
    return X / numpy.where(norms == 0, 1.0, norms)


def distances_to(query: numpy.ndarray, points: numpy.ndarray, metric: str) -> numpy.ndarray:
    """`ann_benchmarks/distance.py`, for the metrics this adapter can answer.

    `angular` is `scipy.spatial.distance.cosine`, i.e. `1 - cos`, and NOT the
    Euclidean distance between normalised vectors. The two rank identically, so
    getting this wrong does not change which neighbours are returned — it
    changes their *values*, and the recall test is a comparison of values
    against `true_distances[k - 1] + 1e-3`. Written the wrong way round it
    scores a perfect answer as recall 0.0000, which is exactly what the first
    run of this file did. Checked against the published
    `random-xs-20-angular.hdf5`: its `distances[0][0]` is 0.01023567, which is
    `1 - cos` (the normalised Euclidean is 0.14307807).
    """
    if metric == "angular":
        return 1.0 - normalise(points) @ normalise(query)
    if metric == "euclidean":
        return numpy.linalg.norm(points - query, axis=-1)
    raise NotImplementedError(f"no distance function for metric '{metric}'")


def recall_at(true_distances: numpy.ndarray, run_distances: numpy.ndarray, count: int) -> float:
    """`knn` from `ann_benchmarks/plotting/metrics.py`, verbatim in behaviour."""
    hits = 0
    for truth, got in zip(true_distances, run_distances):
        threshold = truth[count - 1] + EPSILON
        hits += int(numpy.count_nonzero(numpy.asarray(got[:count]) <= threshold))
    return hits / (len(run_distances) * count)


# --------------------------------------------------------------------- results


def result_path(dataset: str, count: int, arguments: list) -> str:
    """`ann_benchmarks/results.py::build_result_filepath`, same scheme."""
    encoded = re.sub(r"\W+", "_", json.dumps(arguments, sort_keys=True)).strip("_")
    directory = os.path.join(RESULTS, dataset, str(count), "inlaysql")
    os.makedirs(directory, exist_ok=True)
    return os.path.join(directory, f"{encoded}.hdf5")


def store(path: str, attrs: dict, results: list, count: int) -> None:
    """`ann_benchmarks/results.py::store_results`, same datasets and padding."""
    with h5py.File(path, "w") as handle:
        for key, value in attrs.items():
            handle.attrs[key] = value
        times = handle.create_dataset("times", (len(results),), "f")
        neighbors = handle.create_dataset("neighbors", (len(results), count), "i")
        distances = handle.create_dataset("distances", (len(results), count), "f")
        for i, (elapsed, pairs) in enumerate(results):
            times[i] = elapsed
            neighbors[i] = [n for n, _ in pairs] + [-1] * (count - len(pairs))
            distances[i] = [d for _, d in pairs] + [float("inf")] * (count - len(pairs))


# ------------------------------------------------------------------ the harness


def run_query_group(
    algo: InlaySQL,
    train: numpy.ndarray,
    test: numpy.ndarray,
    metric: str,
    count: int,
    runs: int,
) -> tuple[dict, list]:
    """`ann_benchmarks/runner.py::run_individual_query`, single-query mode."""
    best_search_time = float("inf")
    results: list = []
    for _ in range(runs):
        pass_results = []
        for query in test:
            started = time.time()
            candidates = algo.query(query, count)
            elapsed = time.time() - started
            ids = [int(i) for i in candidates]
            if ids:
                got = distances_to(query, train[ids], metric)
            else:
                got = numpy.empty(0)
            pass_results.append((elapsed, list(zip(ids, (float(d) for d in got)))))
        total = sum(elapsed for elapsed, _ in pass_results)
        search_time = total / len(test)
        if search_time < best_search_time:
            best_search_time = search_time
            results = pass_results
    candidates_per_query = sum(len(pairs) for _, pairs in results) / len(results)
    attrs = {
        "batch_mode": False,
        "best_search_time": best_search_time,
        "candidates": candidates_per_query,
        "expect_extra": False,
        "name": str(algo),
        "run_count": runs,
        "distance": metric,
        "count": count,
    }
    attrs.update(algo.get_additional())
    return attrs, results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", default="random-xs-20-angular")
    parser.add_argument("--count", type=int, default=10, help="k, the number of neighbours")
    parser.add_argument("--runs", type=int, default=5, help="passes over the query set")
    parser.add_argument(
        "--query-args",
        default=",".join(str(a) for a in DEFAULT_QUERY_ARGS),
        help="ef_search values to sweep (each must be >= --count)",
    )
    parser.add_argument("--quantization", default="exact", choices=["exact", "int8"])
    parser.add_argument(
        "--max-queries",
        type=int,
        default=0,
        help="use only the first N test queries (0 = all). The corpus is NEVER subset: "
        "the ground truth would stop being the truth. Fewer queries only widens the "
        "confidence interval on recall.",
    )
    options = parser.parse_args()
    query_args = [int(a) for a in options.query_args.split(",") if a.strip()]

    with h5py.File(dataset_path(options.dataset), "r") as handle:
        metric = handle.attrs["distance"]
        metric = metric.decode() if isinstance(metric, bytes) else str(metric)
        train = numpy.array(handle["train"], dtype=numpy.float32)
        test = numpy.array(handle["test"], dtype=numpy.float32)
        true_distances = numpy.array(handle["distances"], dtype=numpy.float32)
    if options.max_queries:
        test = test[: options.max_queries]
        true_distances = true_distances[: options.max_queries]

    header = [
        f"date:    {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}",
        f"commit:  {_git('rev-parse', '--short', 'HEAD')}",
        f"dirty:   {'yes' if _git('status', '--porcelain') else 'no'}",
        f"rustc:   {_shell('rustc', '--version')}",
        f"host:    {_shell('uname', '-srm')}",
        f"dataset: {options.dataset} ({len(train)} train x {train.shape[1]}, "
        f"{len(test)} queries, metric {metric})",
        f"protocol: ann-benchmarks; recall@{options.count} vs the dataset's own "
        f"`neighbors`/`distances`; QPS = 1/best_search_time over {options.runs} runs",
        f"seam:    MySQL wire (inlaysql serve --mysql), quantization={options.quantization}",
    ]
    print("\n".join(header))

    algo = InlaySQL(metric, {"quantization": options.quantization})
    started = time.time()
    memory_before = algo.get_memory_usage()
    try:
        algo.fit(train)
        build_time = time.time() - started
        memory_after = algo.get_memory_usage()
        index_size = (
            memory_after - memory_before
            if memory_before is not None and memory_after is not None
            else -1
        )
        raw_rss = algo.server_rss_kb() or 0.0
        if index_size >= 0:
            cost = (
                f"index: {index_size / 1024:.0f} MiB "
                f"({index_size * 1024 / max(1, len(train)):.0f} B/vector, "
                f"{index_size * 1024 / max(1, train.nbytes):.1f}x the raw f32 corpus)"
            )
        else:
            cost = "index: unmeasured (no server process to read RSS from)"
        print(
            f"\nbuild:   {build_time:.1f}s  ({algo.load_seconds:.1f}s loading over the wire, "
            f"{algo.graph_seconds:.1f}s building the graph on the first read)"
            f"\nmemory:  {cost}   server RSS: {raw_rss / 1024:.0f} MiB"
            f"\nplan:    {algo.get_additional()['inlaysql_plan']}"
        )
        print(
            f"\n{'ef_search':>10} {'recall@' + str(options.count):>11} "
            f"{'QPS':>10} {'p50 ms':>9} {'p95 ms':>9} {'fmt us':>8}"
        )
        rows = []
        for ef_search in sweep_for(query_args, options.count):
            algo.set_query_arguments(ef_search)
            attrs, results = run_query_group(
                algo, train, test, metric, options.count, options.runs
            )
            run_distances = [[d for _, d in pairs] for _, pairs in results]
            recall = recall_at(true_distances, run_distances, options.count)
            attrs.update(
                {
                    "build_time": build_time,
                    "index_size": index_size,
                    "algo": "inlaysql",
                    "dataset": options.dataset,
                }
            )
            store(
                result_path(
                    options.dataset,
                    options.count,
                    [metric, {"quantization": options.quantization}, ef_search],
                ),
                attrs,
                results,
                options.count,
            )
            samples = sorted(elapsed for elapsed, _ in results)
            qps = 1.0 / attrs["best_search_time"]
            print(
                f"{ef_search:>10} {recall:>11.4f} {qps:>10.1f} "
                f"{samples[len(samples) // 2] * 1e3:>9.3f} "
                f"{samples[int(len(samples) * 0.95)] * 1e3:>9.3f} "
                f"{attrs['inlaysql_literal_format_us']:>8.1f}"
            )
            rows.append((ef_search, recall, qps))
    finally:
        algo.done()

    print(f"\nresults written under {os.path.relpath(RESULTS, ROOT)}/{options.dataset}/"
          f"{options.count}/inlaysql/ in ann-benchmarks' own HDF5 layout")
    return 0


def sweep_for(query_args, count):
    """The sweep points this run can actually ask for, narrowest first.

    A candidate list narrower than the `LIMIT` cannot hold the answer, and the
    engine refuses that query rather than returning a short list — so a sweep
    point below `--count` would abort the run instead of producing a fast, bad
    recall number. Dropped points are named on stderr rather than skipped
    silently: which operating points an engine cannot be asked for is part of
    what a benchmark is measuring.
    """
    usable = [ef for ef in query_args if ef >= count]
    dropped = [ef for ef in query_args if ef < count]
    if dropped:
        print(
            f"skipping ef_search {dropped}: below --count {count}, which this engine "
            f"refuses (a beam narrower than the answer cannot hold it)",
            file=sys.stderr,
        )
    return usable or [count]


def _shell(*command: str) -> str:
    try:
        return subprocess.run(command, capture_output=True, text=True, timeout=30).stdout.strip()
    except Exception:  # noqa: BLE001
        return "unknown"


def _git(*args: str) -> str:
    return _shell("git", "-C", ROOT, *args)


if __name__ == "__main__":
    raise SystemExit(main())
