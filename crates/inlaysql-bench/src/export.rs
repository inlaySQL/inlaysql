//! Export the retrieval corpus so other engines can be asked the same
//! questions.
//!
//! DuckDB and pgvector cannot be linked into this binary — one is a separate
//! runtime, the other needs a PostgreSQL server — so comparing against them
//! means running them in containers. The risk with that is subtle and fatal:
//! it is very easy to end up with each engine answering a slightly different
//! question and to publish the difference as a performance result.
//!
//! So the corpus, the queries and the correct answers are generated **once**,
//! here, and written to disk. Every engine, including InlaySQL, then reads the
//! same files. `bench/compare.sh` runs the containers; `bench/external/` holds
//! the drivers.
//!
//! # Rounding is deliberate
//!
//! Embeddings are written with six decimal places, and the vectors InlaySQL is
//! measured on are read back from that text rather than kept in full
//! precision. Otherwise InlaySQL would be scored on `f32` values the other
//! engines never saw, and part of any recall difference would just be the
//! export format.
//!
//! # The two ground truths
//!
//! * **Vector** — exhaustive cosine similarity over the corpus. An objective
//!   answer: recall against it is a quality score for any engine.
//! * **Hybrid** — reciprocal rank fusion of the exhaustive cosine ranking and
//!   an exact BM25 ranking, which is what InlaySQL's `fuse()` computes when
//!   neither index approximates. Overlap with it says how close an engine's
//!   fused ranking is *to ours*, not how good that ranking is: an engine
//!   ranking text with `ts_rank` rather than BM25 will score lower without
//!   being worse. Read it as an agreement measure and read the latency as the
//!   result.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, Value};
use inlaysql_core::bm25::Bm25Index;
use inlaysql_core::fusion::{reciprocal_rank_fusion, DEFAULT_RRF_K};
use inlaysql_core::mem::{cosine_similarity, SeededRng};
use inlaysql_core::traits::{FullTextIndex, Scored};

use crate::{percentiles, synthetic_document, synthetic_query, Config};

/// Decimal places embeddings are written with.
const PRECISION: usize = 6;

/// A corpus row, exactly as every engine will see it.
struct Document {
    id: u64,
    body: String,
    embedding: Vec<f32>,
}

/// A query, exactly as every engine will see it.
struct Query {
    text: String,
    embedding: Vec<f32>,
}

pub fn run(config: &Config, directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(directory)?;
    let k = config.limit;

    let (corpus, queries) = generate(config);
    let vector_truth: Vec<Vec<u64>> = queries
        .iter()
        .map(|query| exact_vector_top_k(&corpus, &query.embedding, k))
        .collect();
    let reference_text = exact_bm25(&corpus);
    let hybrid_truth: Vec<Vec<u64>> = queries
        .iter()
        .map(|query| reference_hybrid_top_k(&corpus, &reference_text, query, k))
        .collect();

    write_corpus(&directory.join("corpus.csv"), &corpus)?;
    write_queries(&directory.join("queries.csv"), &queries)?;
    write_ranking(&directory.join("truth-vector.csv"), &vector_truth)?;
    write_ranking(&directory.join("truth-hybrid.csv"), &hybrid_truth)?;

    let manifest = format!(
        "{{\n  \"corpus\": {},\n  \"queries\": {},\n  \"dim\": {},\n  \"top_k\": {},\n  \
         \"seed\": {},\n  \"precision\": {}\n}}\n",
        corpus.len(),
        queries.len(),
        config.dim,
        k,
        config.seed,
        PRECISION
    );
    fs::write(directory.join("manifest.json"), manifest)?;

    // InlaySQL's own numbers, over the exported corpus, written in the same
    // shape the container drivers produce so one script can merge them.
    let result = measure_inlaysql(config, &corpus, &queries, k, &vector_truth, &hybrid_truth)?;
    fs::write(directory.join("results-inlaysql.json"), result)?;

    println!(
        "wrote the corpus, the queries and both ground truths to {}",
        directory.display()
    );
    println!("run bench/compare.sh to measure DuckDB and pgvector on the same files");
    Ok(())
}

/// The corpus and queries, rounded to what the CSV can hold.
fn generate(config: &Config) -> (Vec<Document>, Vec<Query>) {
    let mut rng = SeededRng::new(config.seed);
    let corpus: Vec<Document> = (1..=config.docs as u64)
        .map(|id| {
            let body = synthetic_document(&mut rng);
            let embedding = round(&hashed_embedding(&body, config.dim));
            Document {
                id,
                body,
                embedding,
            }
        })
        .collect();
    let queries: Vec<Query> = (0..config.queries)
        .map(|_| {
            let text = synthetic_query(&mut rng);
            let embedding = round(&hashed_embedding(&text, config.dim));
            Query { text, embedding }
        })
        .collect();
    (corpus, queries)
}

/// Round through the exported text, so memory and file agree exactly.
fn round(embedding: &[f32]) -> Vec<f32> {
    embedding
        .iter()
        .map(|value| {
            format!("{value:.PRECISION$}")
                .parse()
                .expect("a formatted float parses back")
        })
        .collect()
}

fn exact_vector_top_k(corpus: &[Document], query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<Scored> = corpus
        .iter()
        .map(|doc| Scored::new(doc.id, cosine_similarity(query, &doc.embedding)))
        .collect();
    inlaysql_core::fusion::sort_by_score_desc(&mut scored);
    scored.into_iter().take(k).map(|s| s.id).collect()
}

/// An exact BM25 index over the whole corpus — the engine's own, which is not
/// approximate, so it is its own reference.
fn exact_bm25(corpus: &[Document]) -> Bm25Index {
    let mut index = Bm25Index::new();
    for doc in corpus {
        index.insert(doc.id, &doc.body).expect("in-memory insert");
    }
    index.commit().expect("in-memory commit");
    index
}

/// The reference hybrid ranking: RRF over exact vector and exact BM25.
fn reference_hybrid_top_k(
    corpus: &[Document],
    index: &Bm25Index,
    query: &Query,
    k: usize,
) -> Vec<u64> {
    let mut vector: Vec<Scored> = corpus
        .iter()
        .map(|doc| Scored::new(doc.id, cosine_similarity(&query.embedding, &doc.embedding)))
        .collect();
    inlaysql_core::fusion::sort_by_score_desc(&mut vector);
    let text = index
        .search(&query.text, corpus.len(), None)
        .expect("bm25 search");

    reciprocal_rank_fusion(&[vector, text], DEFAULT_RRF_K)
        .into_iter()
        .take(k)
        .map(|s| s.id)
        .collect()
}

fn write_corpus(path: &Path, corpus: &[Document]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(file, "id,body,embedding")?;
    for doc in corpus {
        writeln!(
            file,
            "{},{},\"{}\"",
            doc.id,
            doc.body,
            literal(&doc.embedding)
        )?;
    }
    file.flush()
}

fn write_queries(path: &Path, queries: &[Query]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(file, "qid,text,embedding")?;
    for (index, query) in queries.iter().enumerate() {
        writeln!(
            file,
            "{},{},\"{}\"",
            index,
            query.text,
            literal(&query.embedding)
        )?;
    }
    file.flush()
}

fn write_ranking(path: &Path, rankings: &[Vec<u64>]) -> std::io::Result<()> {
    let mut file = std::io::BufWriter::new(fs::File::create(path)?);
    writeln!(file, "qid,rank,id")?;
    for (qid, ids) in rankings.iter().enumerate() {
        for (rank, id) in ids.iter().enumerate() {
            writeln!(file, "{qid},{rank},{id}")?;
        }
    }
    file.flush()
}

/// A vector as `[1.000000,2.000000]`.
///
/// That is pgvector's own input format and a cast DuckDB accepts, so one
/// column feeds both without a per-engine conversion that could change the
/// values.
fn literal(embedding: &[f32]) -> String {
    let mut out = String::with_capacity(embedding.len() * (PRECISION + 4));
    out.push('[');
    for (index, value) in embedding.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&format!("{value:.PRECISION$}"));
    }
    out.push(']');
    out
}

/// Run the exported workload through the real SQL surface.
fn measure_inlaysql(
    config: &Config,
    corpus: &[Document],
    queries: &[Query],
    k: usize,
    vector_truth: &[Vec<u64>],
    hybrid_truth: &[Vec<u64>],
) -> Result<String, Box<dyn std::error::Error>> {
    let path = std::path::Path::new("target").join("bench-export.inlay");
    let _ = fs::remove_file(&path);
    let mut db = Database::open(&path)?;
    db.execute(
        &format!(
            "CREATE TABLE docs (id INTEGER PRIMARY KEY, body TEXT, embedding VECTOR({}))",
            config.dim
        ),
        &[],
    )?;
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;

    let started = Instant::now();
    for doc in corpus {
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(doc.id as i64),
                Value::Text(doc.body.clone().into()),
                Value::Vector(doc.embedding.clone()),
            ],
        )?;
    }
    let vector_sql = format!(
        "SELECT id, vector_score(embedding, ?) AS score FROM docs ORDER BY score DESC LIMIT {k}"
    );
    let hybrid_sql = format!(
        "SELECT id, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score \
         FROM docs ORDER BY score DESC LIMIT {k}"
    );
    // The indexes are built on the first read, so that read carries the load
    // cost. Charging it to "build" keeps it out of the latencies.
    db.query(&vector_sql, &[Value::Vector(queries[0].embedding.clone())])?;
    let build = started.elapsed();

    let mut vector_samples = Vec::new();
    let mut vector_recall = 0.0;
    for (query, truth) in queries.iter().zip(vector_truth) {
        let at = Instant::now();
        let rows = db.query(&vector_sql, &[Value::Vector(query.embedding.clone())])?;
        vector_samples.push(at.elapsed());
        vector_recall += overlap(&ids(&rows), truth);
    }

    let mut hybrid_samples = Vec::new();
    let mut hybrid_agreement = 0.0;
    for (query, truth) in queries.iter().zip(hybrid_truth) {
        let at = Instant::now();
        let rows = db.query(
            &hybrid_sql,
            &[
                Value::Vector(query.embedding.clone()),
                Value::Text(query.text.clone().into()),
            ],
        )?;
        hybrid_samples.push(at.elapsed());
        hybrid_agreement += overlap(&ids(&rows), truth);
    }

    let _ = fs::remove_file(&path);
    Ok(result_json(
        "InlaySQL (HNSW + BM25)",
        build,
        vector_recall / queries.len() as f64,
        &vector_samples,
        hybrid_agreement / queries.len() as f64,
        &hybrid_samples,
        "one process, no server; embeddings are hashed bag-of-words, so text and vector \
         agree and hybrid means something — easier for ANN than the random vectors in \
         the `vectors` suite",
    ))
}

fn ids(rows: &inlaysql::ResultSet) -> Vec<u64> {
    rows.rows
        .iter()
        .filter_map(|row| match row[0] {
            Value::Integer(id) => Some(id as u64),
            _ => None,
        })
        .collect()
}

/// Fraction of `truth` that `got` contains.
fn overlap(got: &[u64], truth: &[u64]) -> f64 {
    if truth.is_empty() {
        return 1.0;
    }
    got.iter().filter(|id| truth.contains(id)).count() as f64 / truth.len() as f64
}

/// The result shape every engine's driver writes, so `report.py` can merge
/// them without knowing which engine produced which file.
#[allow(clippy::too_many_arguments)]
fn result_json(
    engine: &str,
    build: Duration,
    vector_recall: f64,
    vector_samples: &[Duration],
    hybrid_agreement: f64,
    hybrid_samples: &[Duration],
    notes: &str,
) -> String {
    let (vp50, vp95, vmax) = percentiles(vector_samples);
    let (hp50, hp95, hmax) = percentiles(hybrid_samples);
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    format!(
        "{{\n  \"engine\": \"{engine}\",\n  \"build_seconds\": {:.3},\n  \
         \"vector\": {{\"recall\": {:.4}, \"p50_ms\": {:.3}, \"p95_ms\": {:.3}, \"max_ms\": {:.3}}},\n  \
         \"hybrid\": {{\"agreement\": {:.4}, \"p50_ms\": {:.3}, \"p95_ms\": {:.3}, \"max_ms\": {:.3}}},\n  \
         \"notes\": \"{notes}\"\n}}\n",
        build.as_secs_f64(),
        vector_recall,
        ms(vp50),
        ms(vp95),
        ms(vmax),
        hybrid_agreement,
        ms(hp50),
        ms(hp95),
        ms(hmax),
    )
}
