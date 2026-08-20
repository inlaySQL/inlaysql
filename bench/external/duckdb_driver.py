"""DuckDB on the exported corpus: vector search and BM25 hybrid.

DuckDB is in the comparison because it is the other embedded engine people
reach for, and because its `fts` extension implements real Okapi BM25 — so the
hybrid row is fusing the same *kind* of text score InlaySQL fuses, not a
different ranking function wearing the same name.

Two vector configurations are measured when both are available:

* **exhaustive** — cosine distance over every row. Exact, so its recall is 1.0
  up to tie-breaking; latency grows with the corpus. This is DuckDB's answer
  out of the box.
* **vss (HNSW)** — the `vss` extension's approximate index, which is the
  like-for-like comparison against InlaySQL's HNSW. It is skipped, loudly, if
  the extension cannot be installed, because a silently-missing index would
  look like a DuckDB result rather than a missing measurement.
"""

from __future__ import annotations

import time

import duckdb

import common


def build(corpus: common.Corpus, use_index: bool):
    connection = duckdb.connect()
    connection.execute("INSTALL fts; LOAD fts;")
    connection.execute(
        f"CREATE TABLE docs (id BIGINT, body VARCHAR, embedding FLOAT[{corpus.dim}])"
    )
    connection.executemany(
        "INSERT INTO docs VALUES (?, ?, ?)",
        list(zip(corpus.ids, corpus.bodies, corpus.embeddings)),
    )
    # DuckDB's FTS builds a BM25 index over the text column; without it the
    # hybrid arm would have no text ranking to fuse.
    connection.execute("PRAGMA create_fts_index('docs', 'id', 'body')")
    if use_index:
        connection.execute("INSTALL vss; LOAD vss;")
        connection.execute("SET hnsw_enable_experimental_persistence = true")
        connection.execute(
            "CREATE INDEX docs_hnsw ON docs USING HNSW (embedding) WITH (metric = 'cosine')"
        )
    return connection


def measure(corpus: common.Corpus, use_index: bool, slug: str, engine: str, notes: str) -> None:
    started = time.perf_counter()
    connection = build(corpus, use_index)
    build_seconds = time.perf_counter() - started

    k = corpus.top_k
    # Cosine *distance* ascending, which orders identically to cosine
    # similarity descending — the metric InlaySQL ranks by — and is the only
    # spelling DuckDB's optimizer rewrites into an HNSW index scan. Written
    # this way for both configurations so the exhaustive and indexed rows
    # differ in the index, not in the query.
    vector_sql = (
        "SELECT id FROM docs "
        "ORDER BY array_cosine_distance(embedding, ?::FLOAT[{dim}]) LIMIT {k}"
    ).format(dim=corpus.dim, k=k)
    # Over-fetch on each arm before fusing: a row ranked 40th by vector and 1st
    # by BM25 has to survive long enough to win. InlaySQL over-fetches too.
    candidates = k * 4
    text_sql = (
        "SELECT id FROM (SELECT id, fts_main_docs.match_bm25(id, ?) AS score FROM docs) "
        "WHERE score IS NOT NULL ORDER BY score DESC LIMIT {n}"
    ).format(n=candidates)
    vector_candidates_sql = (
        "SELECT id FROM docs "
        "ORDER BY array_cosine_distance(embedding, ?::FLOAT[{dim}]) LIMIT {n}"
    ).format(dim=corpus.dim, n=candidates)

    # An index nobody's query plan uses is not a measurement of that index, so
    # the plan is read rather than assumed, and what it said goes in the notes.
    plan = connection.execute(
        "EXPLAIN " + vector_sql, [corpus.query_embeddings[0]]
    ).fetchall()[0][1]
    scan = "HNSW index scan" if "HNSW" in plan else "sequential scan"
    if use_index and "HNSW" not in plan:
        notes = f"{notes}; WARNING: the index exists but the plan used a {scan}"
    else:
        notes = f"{notes}; plan: {scan}"

    vector_timer = common.Timer()
    recall = 0.0
    for embedding, truth in zip(corpus.query_embeddings, corpus.vector_truth):
        rows = vector_timer.time(lambda: connection.execute(vector_sql, [embedding]).fetchall())
        recall += common.overlap([row[0] for row in rows], truth)

    hybrid_timer = common.Timer()
    agreement = 0.0
    for text, embedding, truth in zip(
        corpus.query_texts, corpus.query_embeddings, corpus.hybrid_truth
    ):

        def fused():
            by_vector = [
                row[0]
                for row in connection.execute(vector_candidates_sql, [embedding]).fetchall()
            ]
            by_text = [row[0] for row in connection.execute(text_sql, [text]).fetchall()]
            return common.rrf([by_vector, by_text], k)

        agreement += common.overlap(hybrid_timer.time(fused), truth)

    connection.close()
    common.write_result(
        common.CORPUS,
        slug,
        engine,
        build_seconds,
        recall / len(corpus.query_texts),
        vector_timer,
        agreement / len(corpus.query_texts),
        hybrid_timer,
        notes,
    )


def main() -> None:
    corpus = common.load()
    measure(
        corpus,
        use_index=False,
        slug="duckdb",
        engine="DuckDB (exhaustive + fts BM25)",
        notes="in-process, no server; exhaustive scan, so any recall shortfall is tie-breaking, not approximation",
    )
    try:
        measure(
            corpus,
            use_index=True,
            slug="duckdb-vss",
            engine="DuckDB (vss HNSW + fts BM25)",
            notes="in-process; approximate index, like-for-like against InlaySQL's HNSW",
        )
    except Exception as error:  # noqa: BLE001 — a missing extension is not a crash
        # Loud, not silent: a missing row in the table has to be explained by
        # the log, or a reader will read its absence as "we did not bother".
        print(f"::skipped:: DuckDB vss (HNSW) unavailable: {error}")


if __name__ == "__main__":
    main()
