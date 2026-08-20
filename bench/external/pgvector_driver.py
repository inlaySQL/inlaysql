"""pgvector on the exported corpus: vector search and a hybrid ranking.

pgvector is the reference implementation of "vectors in a real SQL database",
so it is the baseline InlaySQL's retrieval story is measured against. It is
also the comparison most likely to be read unfairly, so:

* **It is a server.** Every latency here includes a client round trip over a
  Docker network, which InlaySQL — a library in the caller's process — does not
  pay. That is a genuine difference between the two designs, not a measurement
  artifact, but it is the reason a "3x" here means less than it looks.
* **Its text ranking is not BM25.** PostgreSQL full-text search ranks with
  `ts_rank_cd`, a different function. The hybrid arm fuses that with the vector
  ranking using the same RRF constant InlaySQL uses, so the *fusion* is
  identical and the *text score* is not. Its agreement with our reference
  hybrid ranking is therefore a measure of how differently the two rank text —
  not a quality score. Read the latency as the result.

Two vector configurations, for the same reason DuckDB has two: exhaustive
(exact, what you get with no index) and HNSW (approximate, like-for-like
against ours).
"""

from __future__ import annotations

import os
import time

import psycopg

import common

DSN = os.environ.get("PG_DSN", "postgresql://postgres:postgres@pgvector:5432/postgres")


def connect(retries: int = 60) -> psycopg.Connection:
    """Wait for the container to accept connections.

    The database starts in parallel with this driver, so a refused connection
    at second zero is expected and a refused connection at second sixty is a
    failure worth reporting.
    """
    last: Exception | None = None
    for _ in range(retries):
        try:
            return psycopg.connect(DSN, autocommit=True)
        except Exception as error:  # noqa: BLE001
            last = error
            time.sleep(1)
    raise RuntimeError(f"pgvector never accepted a connection: {last}")


def build(connection: psycopg.Connection, corpus: common.Corpus, use_index: bool) -> None:
    with connection.cursor() as cursor:
        cursor.execute("CREATE EXTENSION IF NOT EXISTS vector")
        cursor.execute("DROP TABLE IF EXISTS docs")
        cursor.execute(
            f"CREATE TABLE docs (id bigint PRIMARY KEY, body text, embedding vector({corpus.dim}))"
        )
        # COPY rather than a million INSERTs: the load is not what is being
        # measured, and a slow load would push the build column into noise.
        with cursor.copy("COPY docs (id, body, embedding) FROM STDIN") as copy:
            for identifier, body, embedding in zip(
                corpus.ids, corpus.bodies, corpus.embeddings
            ):
                copy.write_row((identifier, body, "[" + ",".join(f"{v:.6f}" for v in embedding) + "]"))
        # A GIN index over the text vector, so the text arm is indexed on both
        # sides rather than one engine scanning and the other not.
        cursor.execute(
            "CREATE INDEX docs_fts ON docs USING GIN (to_tsvector('english', body))"
        )
        if use_index:
            cursor.execute(
                "CREATE INDEX docs_hnsw ON docs USING hnsw (embedding vector_cosine_ops)"
            )
        cursor.execute("ANALYZE docs")


def measure(corpus: common.Corpus, use_index: bool, slug: str, engine: str, notes: str) -> None:
    connection = connect()
    started = time.perf_counter()
    build(connection, corpus, use_index)
    build_seconds = time.perf_counter() - started

    k = corpus.top_k
    candidates = k * 4
    # `<=>` is cosine distance, so ascending distance is descending similarity:
    # the same ordering InlaySQL produces.
    vector_sql = "SELECT id FROM docs ORDER BY embedding <=> %s::vector LIMIT %s"
    text_sql = (
        "SELECT id FROM docs "
        "WHERE to_tsvector('english', body) @@ plainto_tsquery('english', %s) "
        "ORDER BY ts_rank_cd(to_tsvector('english', body), plainto_tsquery('english', %s)) DESC "
        "LIMIT %s"
    )

    def literal(embedding: list[float]) -> str:
        return "[" + ",".join(f"{value:.6f}" for value in embedding) + "]"

    # PostgreSQL will ignore an HNSW index it does not think is worth using, so
    # the plan is read rather than assumed. A row labelled "HNSW" that was
    # actually a sequential scan would be the most misleading kind of number.
    with connection.cursor() as cursor:
        cursor.execute(
            "EXPLAIN " + vector_sql, (literal(corpus.query_embeddings[0]), k)
        )
        plan = " ".join(row[0] for row in cursor.fetchall())
    scan = "HNSW index scan" if "docs_hnsw" in plan else "sequential scan"
    if use_index and "docs_hnsw" not in plan:
        notes = f"{notes}; WARNING: the index exists but the planner chose a {scan}"
    else:
        notes = f"{notes}; plan: {scan}"

    vector_timer = common.Timer()
    recall = 0.0
    with connection.cursor() as cursor:
        for embedding, truth in zip(corpus.query_embeddings, corpus.vector_truth):
            def run():
                cursor.execute(vector_sql, (literal(embedding), k))
                return cursor.fetchall()

            rows = vector_timer.time(run)
            recall += common.overlap([row[0] for row in rows], truth)

        hybrid_timer = common.Timer()
        agreement = 0.0
        for text, embedding, truth in zip(
            corpus.query_texts, corpus.query_embeddings, corpus.hybrid_truth
        ):
            def fused():
                cursor.execute(vector_sql, (literal(embedding), candidates))
                by_vector = [row[0] for row in cursor.fetchall()]
                cursor.execute(text_sql, (text, text, candidates))
                by_text = [row[0] for row in cursor.fetchall()]
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
        slug="pgvector",
        engine="pgvector (exhaustive + ts_rank)",
        notes="client/server: latency includes a round trip; exhaustive, so a recall shortfall is tie-breaking; text ranked by ts_rank_cd, not BM25",
    )
    measure(
        corpus,
        use_index=True,
        slug="pgvector-hnsw",
        engine="pgvector (HNSW + ts_rank)",
        notes="client/server: latency includes a round trip; approximate index, like-for-like vs ours",
    )


if __name__ == "__main__":
    main()
