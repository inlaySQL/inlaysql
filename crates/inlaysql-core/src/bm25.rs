//! An in-engine BM25 full-text index.
//!
//! Stage 4 moves retrieval out of borrowed crates and into the engine. This is
//! the full-text half: it replaces the `tantivy`-backed index in the production
//! crate with one that lives in `inlaysql-core`, so the engine owns its BM25
//! scoring end to end. It is a plain, deterministic Okapi BM25 over postings
//! lists — no stemming, no stop words, no language model — which is exactly the
//! predictable behaviour the deterministic simulation tests need.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fusion::sort_by_score_desc;
use crate::row::{put_len, put_string, Cursor};
use crate::traits::{FullTextIndex, RowId, Scored};

/// Term-frequency saturation. The Robertson/Zaragoza default.
const K1: f32 = 1.2;
/// Document-length normalisation strength. The Robertson/Zaragoza default.
const B: f32 = 0.75;
/// On-disk format of the persisted index. Bumped whenever the layout changes;
/// a mismatch makes the engine rebuild rather than misread.
const FORMAT_VERSION: u8 = 1;

/// Split text into lowercase alphanumeric terms.
///
/// Deliberately crude: no stemming, no stop words. It only has to be
/// predictable, so that two builds over the same rows agree exactly.
pub(crate) fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

/// An Okapi BM25 index over postings lists.
#[derive(Debug, Default, Clone)]
pub struct Bm25Index {
    /// term -> row -> term frequency
    postings: BTreeMap<String, BTreeMap<RowId, u32>>,
    /// row -> document length in terms
    lengths: BTreeMap<RowId, u32>,
}

impl Bm25Index {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.lengths.len()
    }

    /// Whether the index holds no documents.
    pub fn is_empty(&self) -> bool {
        self.lengths.is_empty()
    }

    fn average_length(&self) -> f32 {
        if self.lengths.is_empty() {
            return 0.0;
        }
        let total: u64 = self.lengths.values().map(|len| u64::from(*len)).sum();
        total as f32 / self.lengths.len() as f32
    }

    /// Serialise the index.
    ///
    /// ```text
    /// index    := u8 version, u32 term_count, term*, u32 doc_count, doc*
    /// term     := string term, u32 posting_count, posting*
    /// posting  := u64 row id, u32 term frequency
    /// doc      := u64 row id, u32 length in terms
    /// ```
    ///
    /// The postings are what make this worth storing: rebuilding them means
    /// re-reading and re-tokenising every document in the table.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(FORMAT_VERSION);
        put_len(&mut out, self.postings.len());
        for (term, postings) in &self.postings {
            put_string(&mut out, term);
            put_len(&mut out, postings.len());
            for (id, frequency) in postings {
                out.extend_from_slice(&id.to_le_bytes());
                out.extend_from_slice(&frequency.to_le_bytes());
            }
        }
        put_len(&mut out, self.lengths.len());
        for (id, length) in &self.lengths {
            out.extend_from_slice(&id.to_le_bytes());
            out.extend_from_slice(&length.to_le_bytes());
        }
        out
    }

    /// Parse bytes produced by [`Bm25Index::encode`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        let version = cursor.u8()?;
        if version != FORMAT_VERSION {
            return Err(Error::Corrupt(alloc::format!(
                "BM25 index format version {version} is not {FORMAT_VERSION}"
            )));
        }

        let mut index = Self::new();
        let term_count = cursor.count(8)?;
        for _ in 0..term_count {
            let term = cursor.string()?;
            let posting_count = cursor.count(12)?;
            let mut postings = BTreeMap::new();
            for _ in 0..posting_count {
                let id = RowId::from_le_bytes(cursor.array8()?);
                let frequency = u32::from_le_bytes(cursor.array4()?);
                postings.insert(id, frequency);
            }
            index.postings.insert(term, postings);
        }

        let doc_count = cursor.count(12)?;
        for _ in 0..doc_count {
            let id = RowId::from_le_bytes(cursor.array8()?);
            let length = u32::from_le_bytes(cursor.array4()?);
            index.lengths.insert(id, length);
        }
        Ok(index)
    }
}

impl FullTextIndex for Bm25Index {
    fn insert(&mut self, id: RowId, text: &str) -> Result<()> {
        self.remove(id)?;
        let tokens = tokenize(text);
        self.lengths.insert(id, tokens.len() as u32);
        for token in tokens {
            *self
                .postings
                .entry(token)
                .or_default()
                .entry(id)
                .or_insert(0) += 1;
        }
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        if self.lengths.remove(&id).is_none() {
            return Ok(());
        }
        self.postings.retain(|_, docs| {
            docs.remove(&id);
            !docs.is_empty()
        });
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    fn save(&self) -> Option<Vec<u8>> {
        Some(self.encode())
    }

    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        *self = Self::decode(bytes)?;
        Ok(())
    }

    fn search(&self, query: &str, k: usize) -> Result<Vec<Scored>> {
        let doc_count = self.lengths.len();
        if doc_count == 0 || k == 0 {
            return Ok(Vec::new());
        }
        let average_length = self.average_length();

        let mut scores: BTreeMap<RowId, f32> = BTreeMap::new();
        for term in tokenize(query) {
            let Some(postings) = self.postings.get(&term) else {
                continue;
            };
            let document_frequency = postings.len() as f32;
            let idf = libm::logf(
                1.0 + (doc_count as f32 - document_frequency + 0.5) / (document_frequency + 0.5),
            );
            for (id, frequency) in postings {
                let frequency = *frequency as f32;
                let length = *self.lengths.get(id).unwrap_or(&0) as f32;
                let normalisation = K1 * (1.0 - B + B * length / average_length.max(f32::EPSILON));
                *scores.entry(*id).or_insert(0.0) +=
                    idf * (frequency * (K1 + 1.0)) / (frequency + normalisation);
            }
        }

        let mut hits: Vec<Scored> = scores
            .into_iter()
            .map(|(id, score)| Scored::new(id, score))
            .collect();
        sort_by_score_desc(&mut hits);
        hits.truncate(k);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Bm25Index {
        let mut index = Bm25Index::new();
        index.insert(1, "embedded rust database engine").unwrap();
        index.insert(2, "rust web framework").unwrap();
        index.insert(3, "cooking with cast iron").unwrap();
        index
    }

    #[test]
    fn ranks_the_more_specific_match_first() {
        let hits = index().search("embedded database", 10).unwrap();
        assert_eq!(hits[0].id, 1);
    }

    #[test]
    fn rare_terms_outweigh_common_ones() {
        // "rust" appears in two documents, "framework" in one.
        let hits = index().search("rust framework", 10).unwrap();
        assert_eq!(hits[0].id, 2);
    }

    #[test]
    fn unknown_terms_match_nothing() {
        assert!(index().search("quantum", 10).unwrap().is_empty());
    }

    #[test]
    fn reindexing_replaces_the_old_document() {
        let mut index = index();
        index.insert(1, "cooking").unwrap();
        let hits = index.search("embedded", 10).unwrap();
        assert!(hits.is_empty(), "stale postings survived: {hits:?}");
    }

    #[test]
    fn removal_drops_the_document() {
        let mut index = index();
        index.remove(2).unwrap();
        let hits = index.search("rust", 10).unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            alloc::vec![1]
        );
    }

    #[test]
    fn results_respect_k() {
        assert_eq!(index().search("rust", 1).unwrap().len(), 1);
    }

    #[test]
    fn a_restored_index_scores_identically() {
        let original = index();
        let mut restored = Bm25Index::new();
        restored.load(&original.save().unwrap()).unwrap();

        for query in ["rust", "embedded database", "cooking iron", "absent"] {
            assert_eq!(
                original.search(query, 10).unwrap(),
                restored.search(query, 10).unwrap(),
                "scores diverged for `{query}`"
            );
        }
    }

    #[test]
    fn an_empty_index_round_trips() {
        let mut restored = Bm25Index::new();
        restored.load(&Bm25Index::new().save().unwrap()).unwrap();
        assert!(restored.is_empty());
    }

    #[test]
    fn a_truncated_encoding_is_rejected_not_panicked() {
        let bytes = index().encode();
        for cut in [0, 1, 5, bytes.len() / 2, bytes.len() - 1] {
            assert!(
                Bm25Index::decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix decoded as a whole index"
            );
        }
    }

    #[test]
    fn a_future_format_version_is_refused() {
        let mut bytes = index().encode();
        bytes[0] = FORMAT_VERSION + 1;
        assert!(matches!(
            Bm25Index::decode(&bytes),
            Err(crate::error::Error::Corrupt(_))
        ));
    }
}
