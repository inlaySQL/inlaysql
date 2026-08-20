//! Exact nearest neighbours by brute force.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::fusion::sort_by_score_desc;
use crate::row::{put_len, Cursor};
use crate::traits::{RowId, Scored, VectorIndex};

/// Exhaustive cosine-similarity search.
///
/// This is the oracle the approximate index is judged against: an HNSW index
/// is allowed to miss neighbours, so a test that wants to assert on *exact*
/// ranking uses this instead.
#[derive(Debug, Default, Clone)]
pub struct BruteForceVectorIndex {
    dim: usize,
    embeddings: BTreeMap<RowId, Vec<f32>>,
}

impl BruteForceVectorIndex {
    /// An empty index over vectors of the given dimension.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            embeddings: BTreeMap::new(),
        }
    }

    /// Number of indexed embeddings.
    pub fn len(&self) -> usize {
        self.embeddings.len()
    }

    /// Whether the index holds no embeddings.
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }
}

impl VectorIndex for BruteForceVectorIndex {
    fn insert(&mut self, id: RowId, embedding: &[f32]) -> Result<()> {
        if embedding.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "embedding has dimension {} but the index expects {}",
                embedding.len(),
                self.dim
            )));
        }
        self.embeddings.insert(id, embedding.to_vec());
        Ok(())
    }

    fn remove(&mut self, id: RowId) -> Result<()> {
        self.embeddings.remove(&id);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        Ok(())
    }

    // The reference index persists too, so a simulation exercises the same
    // save/load path the production index takes. There is no graph here, so the
    // encoding is the embeddings and nothing else.
    fn save(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        put_len(&mut out, self.dim);
        put_len(&mut out, self.embeddings.len());
        for (id, embedding) in &self.embeddings {
            out.extend_from_slice(&id.to_le_bytes());
            for value in embedding {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        Some(out)
    }

    fn load(&mut self, bytes: &[u8]) -> Result<()> {
        let mut cursor = Cursor::new(bytes);
        let dim = cursor.count(4)?;
        if dim != self.dim {
            return Err(Error::Corrupt(alloc::format!(
                "persisted vector index has dimension {dim} but the column declares {}",
                self.dim
            )));
        }
        let count = cursor.count(8)?;
        let mut embeddings = BTreeMap::new();
        for _ in 0..count {
            let id = RowId::from_le_bytes(cursor.array8()?);
            let mut embedding = Vec::with_capacity(dim);
            for _ in 0..dim {
                embedding.push(f32::from_le_bytes(cursor.array4()?));
            }
            embeddings.insert(id, embedding);
        }
        self.embeddings = embeddings;
        Ok(())
    }

    fn search(&self, query: &[f32], k: usize) -> Result<Vec<Scored>> {
        if query.len() != self.dim {
            return Err(Error::Type(alloc::format!(
                "query has dimension {} but the index expects {}",
                query.len(),
                self.dim
            )));
        }
        let mut hits: Vec<Scored> = self
            .embeddings
            .iter()
            .map(|(id, embedding)| Scored::new(*id, cosine_similarity(query, embedding)))
            .collect();
        sort_by_score_desc(&mut hits);
        hits.truncate(k);
        Ok(hits)
    }
}

/// Cosine similarity in `[-1, 1]`; zero vectors score 0 rather than NaN.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    let denominator = libm::sqrtf(norm_a) * libm::sqrtf(norm_b);
    if denominator == 0.0 {
        0.0
    } else {
        dot / denominator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn identical_vectors_score_one() {
        let v = vec![0.3, 0.4, 0.5];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn magnitude_does_not_affect_similarity() {
        let a = vec![1.0, 0.0];
        let long = vec![7.0, 0.0];
        assert!((cosine_similarity(&a, &long) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn zero_vectors_do_not_produce_nan() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn returns_the_closest_neighbour_first() {
        let mut index = BruteForceVectorIndex::new(2);
        index.insert(1, &[1.0, 0.0]).unwrap();
        index.insert(2, &[0.0, 1.0]).unwrap();
        index.insert(3, &[0.9, 0.1]).unwrap();
        let hits = index.search(&[1.0, 0.0], 2).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![1, 3]);
    }

    #[test]
    fn dimension_mismatch_is_an_error() {
        let mut index = BruteForceVectorIndex::new(3);
        assert!(index.insert(1, &[1.0]).is_err());
        index.insert(1, &[1.0, 0.0, 0.0]).unwrap();
        assert!(index.search(&[1.0, 0.0], 1).is_err());
    }
}
