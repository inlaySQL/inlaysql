//! A tiny checksum shared by every on-disk structure the storage engine writes.
//!
//! The header, the state block and each write-ahead-log entry carry one of
//! these so that a torn or partial write is detected on the next open. FNV-1a
//! is deliberately simple: it is only required to *detect* corruption, never to
//! resist an attacker, and its speed keeps the cost of checksumming a 24-byte
//! header negligible.

/// FNV-1a 64-bit checksum.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksum_is_deterministic() {
        assert_eq!(fnv1a(b"hello"), fnv1a(b"hello"));
        assert_ne!(fnv1a(b"hello"), fnv1a(b"world"));
    }

    #[test]
    fn flipping_one_byte_changes_the_checksum() {
        let mut bytes = *b"abcdefgh";
        let before = fnv1a(&bytes);
        bytes[3] ^= 0x01;
        assert_ne!(before, fnv1a(&bytes));
    }
}
