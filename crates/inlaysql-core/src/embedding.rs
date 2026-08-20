//! A stand-in embedder, so the examples, demos and benchmarks run with no model.
//!
//! **This is not a semantic embedding model.** It hashes character trigrams
//! into a fixed number of buckets and L2-normalises the result, which makes
//! two strings similar when they *spell* alike. Real deployments pass
//! embeddings from a real model straight into a `VECTOR` column; nothing in
//! the engine knows or cares where they came from.
//!
//! It is still useful for demonstrating hybrid retrieval, because it fails
//! differently from BM25: trigrams match across word forms ("database" vs
//! "databases") and survive typos, where the token-based BM25 index needs an
//! exact term hit.
//!
//! It lives in the core, rather than next to the file-backed database, because
//! every build has to agree on it. A database seeded natively and queried in a
//! browser tab only returns sensible neighbours if both sides bucket trigrams
//! identically — so this is one deterministic function shared by the CLI, the
//! WASM module and the benchmarks, not three that happen to look alike.

use alloc::vec;
use alloc::vec::Vec;

/// FNV-1a offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hash the character trigrams of `text` into a unit-length `dim`-vector.
///
/// Panics if `dim` is zero.
pub fn hashed_embedding(text: &str, dim: usize) -> Vec<f32> {
    assert!(dim > 0, "embedding dimension must be positive");

    let normalised: Vec<char> = text
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();

    let mut buckets = vec![0.0f32; dim];
    for window in normalised.windows(3) {
        if window.iter().all(|c| *c == ' ') {
            continue;
        }
        let hash = hash_chars(window);
        let bucket = (hash % dim as u64) as usize;
        // The sign comes from an independent bit of the hash, so unrelated
        // trigrams colliding in a bucket tend to cancel instead of piling up.
        let sign = if hash & (1 << 63) == 0 { 1.0 } else { -1.0 };
        buckets[bucket] += sign;
    }

    // `libm`, not `f32::sqrt`: the core is `no_std`, so it does its own float
    // maths rather than reaching for the platform's libm through `std`.
    let norm = libm::sqrtf(buckets.iter().map(|x| x * x).sum::<f32>());
    if norm > 0.0 {
        for bucket in &mut buckets {
            *bucket /= norm;
        }
    }
    buckets
}

fn hash_chars(chars: &[char]) -> u64 {
    let mut hash = FNV_OFFSET;
    for c in chars {
        for byte in (*c as u32).to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn output_is_unit_length() {
        let embedding = hashed_embedding("embedded database", 64);
        let norm = libm::sqrtf(embedding.iter().map(|x| x * x).sum::<f32>());
        assert!((norm - 1.0).abs() < 1e-5, "norm was {norm}");
    }

    #[test]
    fn it_is_deterministic() {
        assert_eq!(
            hashed_embedding("embedded database", 128),
            hashed_embedding("embedded database", 128)
        );
    }

    #[test]
    fn related_spellings_score_higher_than_unrelated_text() {
        let query = hashed_embedding("embedded database", 256);
        let related = hashed_embedding("embedded databases", 256);
        let unrelated = hashed_embedding("cast iron skillet cornbread", 256);
        assert!(cosine(&query, &related) > cosine(&query, &unrelated));
    }

    #[test]
    fn empty_text_is_all_zeroes_rather_than_nan() {
        let embedding = hashed_embedding("", 16);
        assert!(embedding.iter().all(|x| *x == 0.0));
    }

    /// The bytes of an embedding are part of the on-disk format: a database
    /// seeded by one build and queried by another only agrees if this function
    /// does. Pinning a few components makes an accidental change to the
    /// hashing loud rather than silently degrading every cross-build demo.
    #[test]
    fn the_hashing_is_pinned_across_builds() {
        let embedding = hashed_embedding("embedded database", 8);
        let rendered: Vec<i32> = embedding.iter().map(|x| (x * 1000.0) as i32).collect();
        assert_eq!(rendered, vec![0, -301, -603, 0, -603, 0, -301, 301]);
    }
}
