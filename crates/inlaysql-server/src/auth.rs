//! `mysql_native_password`, `caching_sha2_password`, and the SHA-1/SHA-256
//! they are defined in terms of.
//!
//! Both schemes are challenge-response, so the password itself does not cross
//! the wire on the *fast* path even though v1 of this server is plaintext
//! anyway. The server sends a random scramble in the handshake; the client
//! replies with
//!
//! ```text
//! mysql_native_password:   SHA1(password) XOR SHA1(scramble || SHA1(SHA1(password)))
//! caching_sha2_password:  SHA256(password) XOR SHA256(SHA256(SHA256(password)) || scramble)
//! ```
//!
//! and the server checks it. **The server does not know the password**, and
//! since AHL-497 it does not store one either: what it keeps per account is
//! the plugin's stage-two digest — `SHA1(SHA1(password))` or
//! `SHA256(SHA256(password))` — which is enough to *check* a token and not
//! enough to *make* one, since making one needs a preimage of that digest.
//! Both verifications below therefore run backwards: strip the scramble mask
//! off the client's token, recover the `SHA1(password)`/`SHA256(password)` it
//! claims, and hash that once more to see whether it lands on the digest on
//! disk. That the scramble is *unpredictable* is what stops a recorded login
//! being replayed later, which is why [`scramble`] insists on real
//! operating-system entropy.
//!
//! `caching_sha2_password`'s concatenation order — the stage-two digest
//! before the scramble, the *opposite* of `mysql_native_password`'s own
//! order — is cross-checked against MySQL's own `Generate_scramble::scramble()`
//! (`sql/auth/sha2_password_common.cc`) and against `go-sql-driver/mysql`'s
//! independent implementation, not assumed: getting it backwards would
//! produce a token that looks plausible and never matches a real client's.
//!
//! # Full authentication
//!
//! Real MySQL caches the SHA-256 hash of a password after its first
//! authentication over a secure channel, so that a later connection's fast
//! scramble can be checked against the cache without asking for the password
//! again — the "caching" the plugin is named for. This server has no such
//! cache and needs none: [`sha2_verifier`] is precisely the value that cache
//! would hold, and it is on disk from the moment the account is created, so
//! the fast scramble above is *always* checkable and there is no cache-miss
//! case to fall back from.
//!
//! What still needs handling is a client that does not attempt the fast
//! scramble — an empty first response, asking the server what to do. This
//! server answers `perform_full_authentication` and accepts the cleartext
//! password the client sends next, which is a widening of exposure only on
//! paper: v1 is documented plaintext-localhost (`docs/server.md`), so a
//! cleartext password crossing an already-plaintext connection reveals
//! nothing a network observer could not already read directly off the wire.
//! **A hash-only store did not cost this path**, which was the open question
//! when the store was designed: hashing what the client just sent and
//! comparing digests is the same check the fast path makes, so nothing had to
//! be weakened to keep it (see [`verify_caching_sha2_cleartext`]).
//! The RSA public-key exchange real MySQL falls back to on an *unencrypted*
//! connection without a cached hash is refused with a clear error instead of
//! implemented, in `connection.rs`'s authentication path.
//!
//! # Why SHA-1 and SHA-256 are implemented here
//!
//! Reaching for `sha1`/`sha2` would pull `digest`, `block-buffer`,
//! `crypto-common`, `generic-array` and `typenum` behind them, into a crate
//! whose whole point is that it adds nothing to the dependency tree (see
//! `docs/server.md`). Both are fixed, fully specified functions, and the
//! tests below check them against the published FIPS vectors, so "did we get
//! it right" is a question with an exact answer rather than a judgement call.
//!
//! Neither hash is used here as a signature primitive, where SHA-1's breaks
//! would be indefensible: both are the fixed key-derivation step their
//! protocol specifies, against a password both ends already know.

use std::io::Read;

/// The length of the `mysql_native_password` challenge, fixed by the
/// protocol.
pub const SCRAMBLE_LEN: usize = 20;

/// The name this server advertises as its default plugin, and completes
/// directly, alongside [`NATIVE_PASSWORD`].
pub const CACHING_SHA2_PASSWORD: &str = "caching_sha2_password";

/// The other plugin this server completes directly — offered via
/// `AuthSwitchRequest` to a client that named anything else.
pub const NATIVE_PASSWORD: &str = "mysql_native_password";

/// `caching_sha2_password`'s `AuthMoreData` status byte: the fast scramble
/// matched, and the exchange is already over — the OK packet follows this
/// immediately.
pub const CACHING_SHA2_FAST_AUTH_SUCCESS: u8 = 0x03;
/// `caching_sha2_password`'s `AuthMoreData` status byte: this server has
/// nothing to check the fast scramble against (it never sent one), so the
/// client should complete full authentication — a cleartext password over
/// this already-plaintext connection.
pub const CACHING_SHA2_PERFORM_FULL_AUTHENTICATION: u8 = 0x04;
/// What a client sends instead of a cleartext password when it wants the
/// server's RSA public key first — refused here; see the module docs.
pub const CACHING_SHA2_REQUEST_PUBLIC_KEY: u8 = 0x02;

/// SHA-1, as specified in RFC 3174.
pub fn sha1(message: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];

    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 80];
        for (word, bytes) in w.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, &word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }

    let mut out = [0u8; 20];
    for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *slot = word.to_be_bytes();
    }
    out
}

/// Compare two secrets without leaking where they first differ.
///
/// The length is compared first and in the clear, which is not a leak worth
/// closing here: every secret this module compares is a fixed-width digest, so
/// a wrong length means a malformed packet rather than a nearly-right guess.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

/// What a stored `mysql_native_password` verifier says.
enum NativeVerifier {
    /// The account has no password: the client is expected to send no token.
    Empty,
    /// `SHA1(SHA1(password))`, the only thing the server needs to know.
    Stage2([u8; 20]),
    /// Not a verifier this server wrote. Never authenticates — a store that
    /// has been damaged or hand-edited must lock the account out, not open it.
    Malformed,
}

/// The `mysql_native_password` verifier for `password`, in MySQL's own
/// `mysql.user.authentication_string` spelling: `*` followed by the uppercase
/// hex of `SHA1(SHA1(password))`, or the empty string for an empty password.
///
/// **This is the whole of what is stored, and it is not the password.** The
/// protocol's fast path is checkable from the stage-two digest alone (see
/// [`verify_native`]), which is why a hash-only account store can still
/// complete every exchange this server implements. What it is *not* is a
/// password hash in the sense a login form's would be: it is unsalted and two
/// fast SHA-1s deep, because the plugin's own definition fixes it. See
/// `docs/server.md`, "Where users live", for the trade that makes and why the
/// alternative (a salted, iterated digest) would force every connection
/// through a cleartext exchange instead.
pub fn native_verifier(password: &str) -> String {
    if password.is_empty() {
        return String::new();
    }
    let stage2 = sha1(&sha1(password.as_bytes()));
    let mut out = String::with_capacity(1 + stage2.len() * 2);
    out.push('*');
    out.push_str(&hex(&stage2));
    out
}

fn native_verifier_of(verifier: &str) -> NativeVerifier {
    if verifier.is_empty() {
        return NativeVerifier::Empty;
    }
    let Some(digits) = verifier.strip_prefix('*') else {
        return NativeVerifier::Malformed;
    };
    match unhex::<20>(digits) {
        Some(stage2) => NativeVerifier::Stage2(stage2),
        None => NativeVerifier::Malformed,
    }
}

/// Whether `response` proves knowledge of the password behind `verifier`,
/// under `mysql_native_password`'s scramble.
///
/// The client sends `SHA1(password) XOR SHA1(challenge || stage2)`. The server
/// knows `stage2`, so it can strip the mask off, recover the client's claimed
/// `SHA1(password)`, and hash it once more: the result must be `stage2` again.
/// **That is why the password itself never has to be stored** — recovering
/// `stage2` from a token needs the password, and forging a token from `stage2`
/// alone needs a SHA-1 preimage.
///
/// The final comparison is constant-time: a caller cannot learn how much of a
/// guess was right by timing the rejection.
pub fn verify_native(verifier: &str, challenge: &[u8], response: &[u8]) -> bool {
    match native_verifier_of(verifier) {
        // The protocol's way of saying "there is no password" is to send no
        // bytes at all, not a hash of the empty string.
        NativeVerifier::Empty => response.is_empty(),
        NativeVerifier::Malformed => false,
        NativeVerifier::Stage2(stage2) => {
            if response.len() != 20 {
                return false;
            }
            let mut salted = Vec::with_capacity(challenge.len() + stage2.len());
            salted.extend_from_slice(challenge);
            salted.extend_from_slice(&stage2);
            let mask = sha1(&salted);

            let mut claimed = [0u8; 20];
            for (slot, (token, mask)) in claimed.iter_mut().zip(response.iter().zip(mask.iter())) {
                *slot = token ^ mask;
            }
            constant_time_eq(&sha1(&claimed), &stage2)
        }
    }
}

// ---------------------------------------------------------------- SHA-256

/// SHA-256, as specified in FIPS 180-4.
pub fn sha256(message: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    let bit_len = (message.len() as u64).wrapping_mul(8);
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().take(16).zip(chunk.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (slot, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *slot = word.to_be_bytes();
    }
    out
}

/// The `caching_sha2_password` verifier for `password`: the uppercase hex of
/// `SHA256(SHA256(password))`.
///
/// **This is deliberately not MySQL's own `$A$005$...` spelling**, which is a
/// salted, 5000-round SHA-256-crypt digest. That form is only usable on
/// MySQL's *full*-authentication path, where the client has already sent the
/// cleartext password — over TLS, or after an RSA exchange. This server has
/// neither, so storing it would mean every connection completing full
/// authentication over a plaintext link, which is strictly worse than what is
/// here. What is stored instead is exactly the value real MySQL keeps in its
/// in-memory *cache* (the "caching" the plugin is named for), which is what
/// the fast scramble is checked against — see [`verify_caching_sha2`], and
/// `docs/server.md` for the whole argument.
///
/// Unlike [`native_verifier`] there is no empty-password special case: the
/// plugin never skips the hash, so `SHA256("")` is hashed like any other
/// password would be.
pub fn sha2_verifier(password: &str) -> String {
    hex(&sha256(&sha256(password.as_bytes())))
}

/// `SHA256(SHA256(password))` back out of a stored verifier, or `None` if the
/// verifier is not one this server wrote — in which case the account never
/// authenticates, rather than authenticating anything.
fn sha2_stage2(verifier: &str) -> Option<[u8; 32]> {
    unhex::<32>(verifier)
}

/// Whether `response` proves knowledge of the password behind `verifier`,
/// under `caching_sha2_password`'s fast-authentication scramble.
///
/// The client sends
/// `XOR(SHA256(password), SHA256(SHA256(SHA256(password)) || scramble))` — see
/// the module docs for why the concatenation order is what it is, and the
/// opposite of `mysql_native_password`'s. As in [`verify_native`], the server
/// strips the mask (which it can build from `stage2` and the scramble),
/// recovers the claimed `SHA256(password)` and hashes it once more.
///
/// `false` for anything that is not exactly 32 bytes — the caller decides from
/// the length whether this was a fast-auth attempt at all, but this function
/// does not take it on faith.
pub fn verify_caching_sha2(verifier: &str, scramble: &[u8], response: &[u8]) -> bool {
    if response.len() != 32 {
        return false;
    }
    let Some(stage2) = sha2_stage2(verifier) else {
        return false;
    };
    let mut salted = Vec::with_capacity(stage2.len() + scramble.len());
    salted.extend_from_slice(&stage2);
    salted.extend_from_slice(scramble);
    let mask = sha256(&salted);

    let mut claimed = [0u8; 32];
    for (slot, (token, mask)) in claimed.iter_mut().zip(response.iter().zip(mask.iter())) {
        *slot = token ^ mask;
    }
    constant_time_eq(&sha256(&claimed), &stage2)
}

/// Whether `payload` — the bytes a client sends after
/// `perform_full_authentication` — is the password behind `verifier`,
/// NUL-terminated per the protocol or not (the terminator is a framing
/// convention, not part of the secret).
///
/// **This path survives the move to a hash-only store**, which was not
/// obvious: it used to compare the cleartext against a cleartext password held
/// in memory, and there is no longer one. It does not need one — the server
/// can hash what the client sent and compare *that* to the stored verifier,
/// which is the same check the fast path makes and needs nothing extra on
/// disk. The comparison is constant-time now too, which the cleartext one was
/// not; that is a free improvement rather than a fix for a live leak, since
/// the secret being compared here crossed this plaintext connection in the
/// clear immediately before the call.
pub fn verify_caching_sha2_cleartext(verifier: &str, payload: &[u8]) -> bool {
    let Some(stage2) = sha2_stage2(verifier) else {
        return false;
    };
    let cleartext = payload.strip_suffix(&[0u8]).unwrap_or(payload);
    constant_time_eq(&sha256(&sha256(cleartext)), &stage2)
}

// ------------------------------------------------------------------- hex

/// Uppercase hex, the spelling MySQL's own `authentication_string` uses.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    out.to_ascii_uppercase()
}

/// Exactly `N` bytes of hex, or `None`. Strict about the length on purpose: a
/// short verifier decoded leniently would compare equal to a short digest.
fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    for (slot, pair) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        *slot = ((high << 4) | low) as u8;
    }
    Some(out)
}

/// A fresh challenge, from operating-system entropy.
///
/// Fails rather than falling back to something guessable: a predictable
/// scramble turns a captured login into a replayable one, and a server that
/// cannot get entropy should say so instead of quietly getting weaker.
///
/// The bytes are drawn from a range that excludes NUL, because the scramble
/// travels through NUL-terminated fields in the handshake packet.
pub fn scramble() -> std::io::Result<[u8; SCRAMBLE_LEN]> {
    let mut raw = [0u8; SCRAMBLE_LEN];
    let mut urandom = std::fs::File::open("/dev/urandom")?;
    urandom.read_exact(&mut raw)?;
    // Map into 0x01..=0x7f: printable-ish, never NUL, and a whole number of
    // buckets over the byte range so the mapping stays uniform.
    for byte in raw.iter_mut() {
        *byte = (*byte % 127) + 1;
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// What a `mysql_native_password` *client* sends. Written out here rather
    /// than called from the library on purpose: the library no longer has a
    /// forward direction to call — it only ever checks — so a test that built
    /// its token with the server's own code would be testing nothing.
    fn native_token(password: &str, challenge: &[u8]) -> Vec<u8> {
        if password.is_empty() {
            return Vec::new();
        }
        let stage1 = sha1(password.as_bytes());
        let stage2 = sha1(&stage1);
        let mut salted = challenge.to_vec();
        salted.extend_from_slice(&stage2);
        let mask = sha1(&salted);
        stage1.iter().zip(mask.iter()).map(|(a, b)| a ^ b).collect()
    }

    /// What a `caching_sha2_password` client sends on the fast path. Note the
    /// concatenation order: the stage-two digest *before* the scramble, the
    /// opposite of `mysql_native_password`'s own.
    fn caching_sha2_token(password: &str, scramble: &[u8]) -> [u8; 32] {
        let stage1 = sha256(password.as_bytes());
        let stage2 = sha256(&stage1);
        let mut salted = stage2.to_vec();
        salted.extend_from_slice(scramble);
        let mask = sha256(&salted);
        let mut token = [0u8; 32];
        for (slot, (a, b)) in token.iter_mut().zip(stage1.iter().zip(mask.iter())) {
            *slot = a ^ b;
        }
        token
    }

    /// The published RFC 3174 / FIPS 180-1 vectors. These are the whole reason
    /// a hand-written hash is defensible.
    #[test]
    fn sha1_matches_the_published_vectors() {
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            hex(&sha1(&[b'a'; 1_000_000])),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    /// Padding is the part of SHA-1 that is easy to get wrong only at the block
    /// boundary, so walk across it.
    #[test]
    fn sha1_handles_every_length_around_a_block_boundary() {
        // Independently known value for 64 bytes of 'a'.
        assert_eq!(
            hex(&sha1(&[b'a'; 64])),
            "0098ba824b5c16427bd7a1122a5a442a25ec644d"
        );
        // No length may panic or produce a short digest.
        for length in 0..200 {
            assert_eq!(sha1(&vec![b'x'; length]).len(), 20);
        }
    }

    #[test]
    fn a_correct_token_verifies() {
        let challenge = [7u8; SCRAMBLE_LEN];
        let token = native_token("hunter2", &challenge);
        assert_eq!(token.len(), 20);
        assert!(verify_native(
            &native_verifier("hunter2"),
            &challenge,
            &token
        ));
    }

    #[test]
    fn a_wrong_password_is_refused() {
        let challenge = [7u8; SCRAMBLE_LEN];
        let token = native_token("hunter2", &challenge);
        assert!(!verify_native(
            &native_verifier("hunter3"),
            &challenge,
            &token
        ));
        assert!(!verify_native(&native_verifier(""), &challenge, &token));
    }

    /// The property the challenge exists for: the same password produces a
    /// different token under a different scramble, so a captured login cannot
    /// be replayed against the next one.
    #[test]
    fn a_token_does_not_transfer_between_challenges() {
        let token = native_token("hunter2", &[7u8; SCRAMBLE_LEN]);
        assert!(!verify_native(
            &native_verifier("hunter2"),
            &[8u8; SCRAMBLE_LEN],
            &token
        ));
    }

    #[test]
    fn an_empty_password_expects_an_empty_token() {
        let challenge = [7u8; SCRAMBLE_LEN];
        let empty = native_verifier("");
        assert!(empty.is_empty(), "an empty password stores no digest");
        assert!(verify_native(&empty, &challenge, &[]));
        assert!(!verify_native(&empty, &challenge, &[0u8; 20]));
        // And a real password is never satisfied by sending nothing.
        assert!(!verify_native(&native_verifier("hunter2"), &challenge, &[]));
    }

    /// The stored verifier is MySQL's own `authentication_string` spelling, so
    /// an operator can recognise it — and, more to the point, so that what is
    /// on disk is visibly a digest rather than a password. Checked against a
    /// value MySQL itself produces for this password.
    #[test]
    fn the_native_verifier_is_mysqls_own_spelling() {
        let verifier = native_verifier("hunter2");
        assert_eq!(verifier.len(), 41);
        assert!(verifier.starts_with('*'));
        assert_eq!(verifier, "*58815970BE77B3720276F63DB198B1FA42E5CC02");
        assert!(!verifier.contains("hunter2"));
    }

    /// A store that has been damaged or hand-edited must lock the account out
    /// rather than open it: every malformed verifier below refuses every
    /// token, including the empty one an unset password would accept.
    #[test]
    fn a_malformed_verifier_never_authenticates() {
        let challenge = [7u8; SCRAMBLE_LEN];
        for verifier in [
            "*",
            "*zz",
            "hunter2",
            "*58815970BE77B3720276F63DB198B1FA42E5CC0",
        ] {
            assert!(!verify_native(verifier, &challenge, &[]));
            assert!(!verify_native(verifier, &challenge, &[0u8; 20]));
            assert!(!verify_native(
                verifier,
                &challenge,
                &native_token("hunter2", &challenge)
            ));
        }
        assert!(!verify_caching_sha2("", &challenge, &[0u8; 32]));
        assert!(!verify_caching_sha2_cleartext("", b"hunter2"));
    }

    // -------------------------------------------------------------- SHA-256

    /// The published FIPS 180-4 vectors — the same four messages the SHA-1
    /// test above uses, so the two are checked the same way.
    #[test]
    fn sha256_matches_the_published_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&sha256(&[b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn sha256_handles_every_length_around_a_block_boundary() {
        for length in 0..200 {
            assert_eq!(sha256(&vec![b'x'; length]).len(), 32);
        }
    }

    // --------------------------------------------------- caching_sha2_password

    #[test]
    fn a_correct_caching_sha2_token_verifies() {
        let scramble = [7u8; SCRAMBLE_LEN];
        let token = caching_sha2_token("hunter2", &scramble);
        assert_eq!(token.len(), 32);
        assert!(verify_caching_sha2(
            &sha2_verifier("hunter2"),
            &scramble,
            &token
        ));
    }

    #[test]
    fn a_wrong_password_is_refused_under_caching_sha2_too() {
        let scramble = [7u8; SCRAMBLE_LEN];
        let token = caching_sha2_token("hunter2", &scramble);
        assert!(!verify_caching_sha2(
            &sha2_verifier("hunter3"),
            &scramble,
            &token
        ));
        assert!(!verify_caching_sha2(&sha2_verifier(""), &scramble, &token));
    }

    /// Same property `a_token_does_not_transfer_between_challenges` pins for
    /// `mysql_native_password`: a captured login cannot be replayed against
    /// the next one, because the scramble is folded into the token.
    #[test]
    fn a_caching_sha2_token_does_not_transfer_between_scrambles() {
        let token = caching_sha2_token("hunter2", &[7u8; SCRAMBLE_LEN]);
        assert!(!verify_caching_sha2(
            &sha2_verifier("hunter2"),
            &[8u8; SCRAMBLE_LEN],
            &token
        ));
    }

    /// Unlike `mysql_native_password`, an empty password has no special
    /// empty-token case — the plugin always sends the full 32-byte scramble,
    /// so the verifier for an empty password is a real digest rather than an
    /// empty string.
    #[test]
    fn an_empty_password_still_produces_a_real_caching_sha2_token() {
        let scramble = [7u8; SCRAMBLE_LEN];
        let token = caching_sha2_token("", &scramble);
        assert_eq!(token.len(), 32);
        assert_eq!(sha2_verifier("").len(), 64);
        assert!(verify_caching_sha2(&sha2_verifier(""), &scramble, &token));
        assert!(!verify_caching_sha2(
            &sha2_verifier("hunter2"),
            &scramble,
            &token
        ));
    }

    /// Cross-checked against an independent implementation (Python's
    /// `hashlib`, applying the same formula the module docs describe) rather
    /// than only against itself — a wrong concatenation order would still
    /// pass every test above, since the token this test builds and the
    /// verification it feeds would agree with each other regardless.
    #[test]
    fn a_caching_sha2_token_matches_an_independent_implementation() {
        let token = caching_sha2_token("hunter2", b"01234567890123456789");
        assert_eq!(
            hex(&token),
            "3b4b79ce45e83d74679f78492419a76633c10b5a033ec15503568e463dd3712e"
        );
        // And the stored verifier is the double digest, independently checked
        // the same way: it must not be, or contain, the password.
        assert_eq!(
            sha2_verifier("hunter2"),
            "A3E27AB2948B680E60D429860FDD62B24763CD0E02518B9CDC90D1387247495B"
        );
    }

    #[test]
    fn a_response_of_the_wrong_length_is_never_a_caching_sha2_match() {
        let scramble = [7u8; SCRAMBLE_LEN];
        assert!(!verify_caching_sha2(&sha2_verifier(""), &scramble, &[]));
        assert!(!verify_caching_sha2(
            &sha2_verifier("hunter2"),
            &scramble,
            &[0u8; 31]
        ));
        assert!(!verify_caching_sha2(
            &sha2_verifier("hunter2"),
            &scramble,
            &[0u8; 33]
        ));
    }

    /// The path the hash-only store might have cost and did not: the client
    /// sends cleartext, the server hashes it and compares digests.
    #[test]
    fn full_authentication_accepts_the_cleartext_password_either_way() {
        let hunter2 = sha2_verifier("hunter2");
        let empty = sha2_verifier("");
        assert!(verify_caching_sha2_cleartext(&hunter2, b"hunter2"));
        assert!(verify_caching_sha2_cleartext(&hunter2, b"hunter2\0"));
        assert!(!verify_caching_sha2_cleartext(&hunter2, b"hunter3"));
        assert!(verify_caching_sha2_cleartext(&empty, b""));
        assert!(verify_caching_sha2_cleartext(&empty, b"\0"));
        assert!(!verify_caching_sha2_cleartext(&empty, b"anything"));
    }

    #[test]
    fn hex_round_trips_and_refuses_the_wrong_length() {
        // `super::hex`, not the lowercase helper this test module defines for
        // reading published vectors: the stored spelling is uppercase.
        let bytes = [0x00u8, 0x0f, 0xa5, 0xff];
        assert_eq!(super::hex(&bytes), "000FA5FF");
        assert_eq!(unhex::<4>("000FA5FF"), Some(bytes));
        assert_eq!(unhex::<4>("000fa5ff"), Some(bytes));
        assert_eq!(unhex::<4>("000FA5F"), None);
        assert_eq!(unhex::<4>("000FA5FFFF"), None);
        assert_eq!(unhex::<4>("000FA5FG"), None);
    }

    #[test]
    fn a_scramble_is_random_and_free_of_nul() {
        let first = scramble().expect("system entropy");
        let second = scramble().expect("system entropy");
        assert_ne!(first, second, "two scrambles must not be identical");
        assert!(first.iter().all(|&b| b != 0));
    }
}
