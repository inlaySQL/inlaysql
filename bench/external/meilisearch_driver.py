"""Meilisearch on the exported corpus: vector search and a hybrid ranking.

Meilisearch is in the comparison because it is the dedicated search engine
people reach for instead of a database for exactly the workload this
project's retrieval story targets — unlike DuckDB and pgvector, it is not a
SQL engine with vectors added on, it is a search engine first. Same two
caveats as the other server rows, stated rather than hidden:

* **It is a server.** Every latency here includes a client round trip over a
  Docker network, which InlaySQL does not pay.
* **Its text ranking is not BM25.** Meilisearch ranks by its own rule chain
  (typo tolerance, word proximity, attribute, exactness, ...), not Okapi
  BM25 — closer in spirit to pgvector's `ts_rank_cd` than to DuckDB's `fts`
  extension. Its agreement with our reference hybrid ranking is a measure of
  how differently the two rank text, not a quality score.

**Fusion is ours, not Meilisearch's.** Meilisearch has its own built-in
hybrid mode (`semanticRatio` blends vector and text into one ranked list
server-side), but using it would compare two things at once — retrieval
quality *and* fusion algorithm — against a hybrid arm every other engine in
this comparison reaches by running vector-only and text-only separately and
fusing with `common.rrf`, the same reciprocal-rank-fusion InlaySQL's own
`fuse()` uses. So this driver does the same: `semanticRatio: 1.0` (vector
only) and a plain `q` (text only) are two separate requests, over-fetched
and fused client-side, keeping the fusion constant across every engine and
the comparison isolated to the raw rankings.

**One configuration, not two.** DuckDB and pgvector each get an exhaustive
row and an approximate-index row, because both expose that choice.
Meilisearch's vector search always goes through its own internal ANN index
(`arroy`) with no exhaustive-scan option in the search API, so there is only
one row here, not a missing one.

Vectors are supplied, not computed: the embedder is configured as
`"source": "userProvided"`, so Meilisearch never runs its own embedding
model over `body` — it indexes exactly the vectors the corpus already
carries, the same ones every other engine in this comparison ranks by.
"""

from __future__ import annotations

import os
import time

import requests

import common

URL = os.environ.get("MEILISEARCH_URL", "http://meilisearch:7700")
KEY = os.environ.get("MEILISEARCH_KEY", "bench")
HEADERS = {"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"}


def wait_ready(retries: int = 60) -> None:
    """Wait for the container to answer, the same reason every other driver's
    `connect()` retries: it starts in parallel with this one."""
    last: Exception | None = None
    for _ in range(retries):
        try:
            response = requests.get(f"{URL}/health", timeout=5)
            if response.ok:
                return
        except requests.RequestException as error:  # noqa: BLE001
            last = error
        time.sleep(1)
    raise RuntimeError(f"Meilisearch never became healthy: {last}")


def wait_task(task_uid: int) -> None:
    """Meilisearch's writes are asynchronous — enqueued, then processed by a
    background worker — so a query issued right after `POST documents`
    returns can race the index still building. Every write below blocks on
    this until Meilisearch itself says the task is done, the same guarantee
    a SQL `INSERT` gives for free."""
    while True:
        response = requests.get(f"{URL}/tasks/{task_uid}", headers=HEADERS, timeout=30)
        response.raise_for_status()
        payload = response.json()
        if payload["status"] == "succeeded":
            return
        if payload["status"] in ("failed", "canceled"):
            raise RuntimeError(f"Meilisearch task {task_uid} {payload['status']}: {payload.get('error')}")
        time.sleep(0.05)


# Batched rather than one request per document: this build phase is not what
# is being measured (the same reasoning pgvector's driver gives for using
# `COPY`), and Meilisearch's own indexing guidance is to batch writes.
DOCUMENT_BATCH = 500


def build(corpus: common.Corpus) -> None:
    requests.delete(f"{URL}/indexes/docs", headers=HEADERS, timeout=30)
    response = requests.post(
        f"{URL}/indexes", headers=HEADERS, json={"uid": "docs", "primaryKey": "id"}, timeout=30
    )
    response.raise_for_status()
    wait_task(response.json()["taskUid"])

    response = requests.patch(
        f"{URL}/indexes/docs/settings/embedders",
        headers=HEADERS,
        json={"custom": {"source": "userProvided", "dimensions": corpus.dim}},
        timeout=30,
    )
    response.raise_for_status()
    wait_task(response.json()["taskUid"])

    last_task = None
    for start in range(0, len(corpus.ids), DOCUMENT_BATCH):
        end = min(start + DOCUMENT_BATCH, len(corpus.ids))
        docs = [
            {
                "id": corpus.ids[i],
                "body": corpus.bodies[i],
                "_vectors": {"custom": corpus.embeddings[i]},
            }
            for i in range(start, end)
        ]
        response = requests.post(
            f"{URL}/indexes/docs/documents", headers=HEADERS, json=docs, timeout=120
        )
        response.raise_for_status()
        last_task = response.json()["taskUid"]
    wait_task(last_task)


def vector_search(embedding: list[float], k: int) -> list[int]:
    response = requests.post(
        f"{URL}/indexes/docs/search",
        headers=HEADERS,
        json={
            "vector": embedding,
            "hybrid": {"embedder": "custom", "semanticRatio": 1.0},
            "limit": k,
            "retrieveVectors": False,
        },
        timeout=30,
    )
    response.raise_for_status()
    return [hit["id"] for hit in response.json()["hits"]]


def text_search(text: str, k: int) -> list[int]:
    response = requests.post(
        f"{URL}/indexes/docs/search",
        headers=HEADERS,
        json={"q": text, "limit": k},
        timeout=30,
    )
    response.raise_for_status()
    return [hit["id"] for hit in response.json()["hits"]]


def measure(corpus: common.Corpus) -> None:
    wait_ready()
    started = time.perf_counter()
    build(corpus)
    build_seconds = time.perf_counter() - started

    k = corpus.top_k
    vector_timer = common.Timer()
    recall = 0.0
    for embedding, truth in zip(corpus.query_embeddings, corpus.vector_truth):
        rows = vector_timer.time(lambda embedding=embedding: vector_search(embedding, k))
        recall += common.overlap(rows, truth)

    # Over-fetch on each arm before fusing, the same margin every other
    # driver in this comparison uses: a row ranked 40th by vector and 1st by
    # text has to survive long enough to win.
    candidates = k * 4
    hybrid_timer = common.Timer()
    agreement = 0.0
    for text, embedding, truth in zip(
        corpus.query_texts, corpus.query_embeddings, corpus.hybrid_truth
    ):

        def fused(text=text, embedding=embedding):
            by_vector = vector_search(embedding, candidates)
            by_text = text_search(text, candidates)
            return common.rrf([by_vector, by_text], k)

        agreement += common.overlap(hybrid_timer.time(fused), truth)

    common.write_result(
        common.CORPUS,
        "meilisearch",
        "Meilisearch (arroy ANN + built-in ranking, RRF fused by this driver)",
        build_seconds,
        recall / len(corpus.query_texts),
        vector_timer,
        agreement / len(corpus.query_texts),
        hybrid_timer,
        "client/server: latency includes a round trip; single ANN configuration, no "
        "exhaustive-scan option in the search API; text ranked by Meilisearch's own rule "
        "chain, not BM25; hybrid fusion is this driver's own RRF (common.rrf), not "
        "Meilisearch's built-in semanticRatio blend, so every engine in this comparison is "
        "fused the same way",
    )


def main() -> None:
    corpus = common.load()
    measure(corpus)


if __name__ == "__main__":
    main()
