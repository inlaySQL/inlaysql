//! The Stage 1 demo: one SQL statement, two retrievers, one ranking.
//!
//! ```sh
//! cargo run --example hybrid_search
//! ```
//!
//! It writes `target/hybrid_search_demo.inlay` — one file, the whole database —
//! and prints what each retriever finds on its own next to the fused result,
//! so you can see what the fusion is actually doing.

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, ResultSet, Value};

const DIM: usize = 384;

const CORPUS: &[(i64, &str)] = &[
    (
        1,
        "embedded databases keep the whole engine inside your process",
    ),
    (
        2,
        "rust gives you memory safety without a garbage collector",
    ),
    (
        3,
        "an embedded database written in rust with vector retrieval",
    ),
    (4, "cast iron skillet cornbread recipe with buttermilk"),
    (5, "approximate nearest neighbour search over embeddings"),
    (6, "a web framework for building sites quickly"),
    (
        7,
        "write ahead logging and crash recovery in storage engines",
    ),
    (8, "hybrid search combines keyword matching with embeddings"),
];

/// The keywords the BM25 arm searches for.
const LEXICAL_QUERY: &str = "embedded database";

/// What the vector arm searches for. In a real system this is the same user
/// question run through an embedding model, which is why it does not have to
/// share any words with the corpus. Here it is a paraphrase, embedded with the
/// hashing stand-in.
const SEMANTIC_QUERY: &str = "a storage engine that runs inside your application";

fn main() -> Result<(), inlaysql::Error> {
    let path = std::path::Path::new("target").join("hybrid_search_demo.inlay");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::open(&path)?;

    db.execute(
        "CREATE TABLE docs (id INTEGER, body TEXT, embedding VECTOR(384))",
        &[],
    )?;
    // A `TEXT` column gets a BM25 index and a `VECTOR` column an ANN index —
    // and only when asked. Before these lines there is no index, and a query
    // that scores `body` or `embedding` would fail rather than silently scan.
    db.execute("CREATE INDEX docs_body ON docs (body)", &[])?;
    db.execute("CREATE INDEX docs_embedding ON docs (embedding)", &[])?;

    for (id, body) in CORPUS {
        db.execute(
            "INSERT INTO docs (id, body, embedding) VALUES (?, ?, ?)",
            &[
                Value::Integer(*id),
                Value::Text(body.to_string()),
                // A real deployment puts model output here; this demo uses the
                // hashing stand-in from `inlaysql::embedding`.
                Value::Vector(hashed_embedding(body, DIM)),
            ],
        )?;
    }

    let query_embedding = Value::Vector(hashed_embedding(SEMANTIC_QUERY, DIM));
    let query_text = Value::Text(LEXICAL_QUERY.to_string());

    println!("keywords: {LEXICAL_QUERY:?}");
    println!("embedded query: {SEMANTIC_QUERY:?}\n");

    let vector_only = db.query(
        "SELECT id, body, vector_score(embedding, ?) AS score
         FROM docs ORDER BY score DESC LIMIT 3",
        std::slice::from_ref(&query_embedding),
    )?;
    print_ranking("vector search only", &vector_only);

    let text_only = db.query(
        "SELECT id, body, bm25_score(body, ?) AS score
         FROM docs ORDER BY score DESC LIMIT 3",
        std::slice::from_ref(&query_text),
    )?;
    print_ranking("BM25 only", &text_only);

    // The whole point of the spike: this is one ordinary SELECT. The planner
    // sees the retrieval functions, runs an ANN probe and a BM25 probe, and
    // fuses their rankings.
    let hybrid = db.query(
        "SELECT id, body, fuse(vector_score(embedding, ?), bm25_score(body, ?)) AS score
         FROM docs ORDER BY score DESC LIMIT 3",
        &[query_embedding, query_text],
    )?;
    print_ranking("hybrid (rank fusion)", &hybrid);

    println!(
        "Neither retriever's top hit wins outright: fusion promotes the row that\n\
         both of them ranked well.\n"
    );
    println!("database file: {}", path.display());
    Ok(())
}

fn print_ranking(title: &str, result: &ResultSet) {
    println!("{title}");
    for (rank, row) in result.rows.iter().enumerate() {
        let id = row[0].as_i64().unwrap_or_default();
        let body = row[1].as_str().unwrap_or_default();
        let score = row[2].as_f64().unwrap_or_default();
        println!("  {}. [{id}] {score:.4}  {body}", rank + 1);
    }
    println!();
}
