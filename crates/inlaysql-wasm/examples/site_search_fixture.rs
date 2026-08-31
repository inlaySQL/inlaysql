//! Build the search index the static-site demo ships.
//!
//! ```sh
//! cargo run -p inlaysql-wasm --example site_search_fixture
//! ```
//!
//! This is the *website search* story in one file, and it is the same story
//! the edge fixture tells: a retrieval index is **built once, natively** — at
//! deploy time, from the site's own pages — and shipped as a static asset
//! beside the HTML. The visitor's browser opens it with the WASM module and
//! answers hybrid queries with no backend behind it, which is the deployment
//! shape you want for a site that must not have a server at all.
//!
//! `pages.json` stands in for whatever a real site would use: a sitemap, a
//! crawl, or the build system's page list. A real corpus would also put its
//! own embedding model's output in the `VECTOR` column instead of calling
//! `hashed_embedding` — the demo uses the stand-in so it needs no model.
//!
//! It writes `demos/site-search/site.inlay`, which is git-ignored: it
//! regenerates from `pages.json` and must never be edited by hand.

use std::path::{Path, PathBuf};

use inlaysql::embedding::hashed_embedding;
use inlaysql::{Database, Value};

/// The corpus the demo page indexes, compiled in so the fixture and the page
/// cannot drift apart without someone noticing.
const PAGES: &str = include_str!("../demos/site-search/pages.json");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (dim, pages) = parse(PAGES)?;

    let path = fixture_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A fixture is built from scratch every time. Reopening a stale one would
    // append to it, and the demo would ship whatever the last run left.
    let _ = std::fs::remove_file(&path);

    let mut db = Database::open(&path)?;
    db.execute(
        &format!(
            "CREATE TABLE pages (
                id INTEGER PRIMARY KEY,
                path TEXT,
                title TEXT,
                body TEXT,
                embedding VECTOR({dim})
            )"
        ),
        &[],
    )?;
    // BM25 ranks `body`; `title` is stored and returned but not indexed, so a
    // title-only match is found through its body rather than twice.
    db.execute("CREATE INDEX pages_body ON pages (body)", &[])?;
    db.execute("CREATE INDEX pages_embedding ON pages (embedding)", &[])?;
    for (id, path, title, body) in &pages {
        db.execute(
            "INSERT INTO pages (id, path, title, body, embedding) VALUES (?, ?, ?, ?, ?)",
            &[
                Value::Integer(*id),
                Value::Text(path.clone().into()),
                Value::Text(title.clone().into()),
                Value::Text(body.clone().into()),
                Value::Vector(hashed_embedding(body, dim)),
            ],
        )?;
    }
    // Checkpoint so the shipped file carries its BM25 and ANN indexes. Without
    // this every visitor's browser rebuilds them on open, which is exactly
    // the latency a static-site search is trying not to pay.
    db.checkpoint()?;
    drop(db);

    let bytes = std::fs::metadata(&path)?.len();
    println!(
        "{}: {} pages, VECTOR({dim}), {bytes} bytes ({} KiB)",
        path.display(),
        pages.len(),
        bytes / 1024
    );
    Ok(())
}

fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("demos")
        .join("site-search")
        .join("site.inlay")
}

/// One page as the fixture inserts it: `id`, `path`, `title`, `body`.
type Page = (i64, String, String, String);

/// Pull `dim` and the rows out of `pages.json`.
///
/// Hand-rolled rather than anything heavier: this crate's dependency list is a
/// shipped WASM module's dependency list, and a build-time example is not a
/// reason to add to it.
fn parse(json: &str) -> Result<(usize, Vec<Page>), String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("pages.json is not JSON: {error}"))?;

    let dim = value["dim"]
        .as_u64()
        .ok_or("pages.json needs a numeric `dim`")? as usize;
    if dim == 0 {
        return Err("pages.json `dim` must be positive".into());
    }

    let pages = value["pages"]
        .as_array()
        .ok_or("pages.json needs a `pages` array")?
        .iter()
        .map(|page| {
            let path = page["path"].as_str().ok_or("a page has no `path` string")?;
            let title = page["title"]
                .as_str()
                .ok_or("a page has no `title` string")?;
            let body = page["body"].as_str().ok_or("a page has no `body` string")?;
            Ok((path.to_string(), title.to_string(), body.to_string()))
        })
        .collect::<Result<Vec<_>, String>>()?;

    if pages.is_empty() {
        return Err("pages.json holds no pages".into());
    }

    Ok((
        dim,
        pages
            .into_iter()
            .enumerate()
            .map(|(id, (path, title, body))| (id as i64, path, title, body))
            .collect(),
    ))
}
