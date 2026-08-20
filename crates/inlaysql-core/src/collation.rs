//! Collating sequences: how two `TEXT` values compare.
//!
//! SQLite has exactly three built-in collations and this engine has the same
//! three, spelled the same way — `BINARY`, `NOCASE` and `RTRIM`. There is no
//! fourth, and there is no way to register one: a name this file does not know
//! is refused at plan time rather than quietly treated as `BINARY`, because a
//! collation that silently is not the one asked for returns *fewer rows* than
//! the caller expected and reports success while doing it. That failure mode is
//! the whole reason this module exists (`docs/architecture.md`, Phase 3 item 7).
//!
//! # What a collation does and does not touch
//!
//! A collating sequence is consulted **only when both sides of a comparison are
//! `TEXT`**. Numbers compare as numbers and blobs compare as bytes whatever the
//! column was declared with, exactly as in SQLite. So `COLLATE NOCASE` on an
//! `INTEGER` column is accepted (SQLite accepts it too) and is simply never
//! consulted.
//!
//! # ASCII only, deliberately
//!
//! [`Collation::NoCase`] folds `A`–`Z` to `a`–`z` and **nothing else**. That is
//! not a simplification of SQLite's behaviour, it *is* SQLite's behaviour:
//! `sqlite3UpperToLower` is a 256-entry table that is the identity above `0x7f`,
//! so `'É' = 'é' COLLATE NOCASE` is false there and false here. Inventing
//! Unicode folding would make this engine disagree with the oracle the
//! differential fuzzer compares it against, and would make a `NOCASE` index and
//! a `NOCASE` scan disagree with each other the moment one of them was written
//! by a build with a different Unicode table.
//!
//! Accent folding is a further step again — MySQL's `utf8mb4_0900_ai_ci` does
//! it, `NOCASE` does not — and `docs/server.md` says so where a MySQL client
//! can read it.
//!
//! # Why folding is a byte transform and not just a comparator
//!
//! A scalar B-tree index stores a memcomparable encoding of its key
//! ([`crate::index`]), and the whole design rests on `memcmp` of the encoding
//! meaning the same thing as comparing the values. For a `NOCASE` column that
//! only holds if the *encoding* folds too — otherwise `WHERE name = 'ADA'`
//! answered from the index and the same query answered from a scan return
//! different rows, which is the divergence-by-access-path class this repository
//! treats as the worst kind of bug. [`Collation::fold`] is that transform, and
//! it is the same function on both sides by construction.

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;

use crate::error::{Error, Result};

/// A collating sequence: the rule two `TEXT` values are compared by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub enum Collation {
    /// Compare the UTF-8 bytes. SQLite's default, and this engine's behaviour
    /// before collations existed.
    #[default]
    Binary,
    /// Compare the UTF-8 bytes after folding ASCII `A`–`Z` to lower case.
    /// Nothing outside ASCII is touched — see the module docs.
    NoCase,
    /// Compare the UTF-8 bytes ignoring trailing spaces (`U+0020` only).
    RTrim,
}

impl Collation {
    /// The collation `name` spells, case-insensitively.
    ///
    /// An unknown name is an error naming it and naming the three that exist.
    /// SQLite's own message is `no such collation sequence: X`; this keeps that
    /// wording so a caller matching on it sees what it expects, and adds the
    /// list because there is no `CREATE COLLATION` here to add a fourth with.
    pub fn from_name(name: &str) -> Result<Self> {
        if name.eq_ignore_ascii_case("binary") {
            Ok(Collation::Binary)
        } else if name.eq_ignore_ascii_case("nocase") {
            Ok(Collation::NoCase)
        } else if name.eq_ignore_ascii_case("rtrim") {
            Ok(Collation::RTrim)
        } else {
            Err(Error::Unsupported(alloc::format!(
                "no such collation sequence: {name}; this engine has BINARY, NOCASE and RTRIM, \
                 and no way to register another"
            )))
        }
    }

    /// The name this collation is written with.
    pub fn name(self) -> &'static str {
        match self {
            Collation::Binary => "BINARY",
            Collation::NoCase => "NOCASE",
            Collation::RTrim => "RTRIM",
        }
    }

    /// Whether this is the default, which is what decides whether a catalog
    /// needs the format version that can record collations at all.
    pub fn is_binary(self) -> bool {
        self == Collation::Binary
    }

    /// The bytes this collation actually compares, for `bytes` interpreted as
    /// the UTF-8 of a `TEXT` value.
    ///
    /// Borrowed wherever the transform is the identity, which is every
    /// `BINARY` value and every `NOCASE` value that has no ASCII upper-case
    /// byte in it — so the common case allocates nothing.
    ///
    /// `NOCASE` folding is byte-wise, which is safe for UTF-8 precisely because
    /// it only ever touches bytes in `0x41..=0x5a`: those never appear inside a
    /// multi-byte sequence, whose continuation bytes are all `>= 0x80`.
    pub fn fold(self, bytes: &[u8]) -> Cow<'_, [u8]> {
        match self {
            Collation::Binary => Cow::Borrowed(bytes),
            Collation::NoCase => {
                if bytes.iter().any(u8::is_ascii_uppercase) {
                    Cow::Owned(
                        bytes
                            .iter()
                            .map(u8::to_ascii_lowercase)
                            .collect::<Vec<u8>>(),
                    )
                } else {
                    Cow::Borrowed(bytes)
                }
            }
            // SQLite's `rtrimCollFunc` ignores `0x20` and only `0x20` — not
            // tabs, not newlines, not `U+00A0`.
            Collation::RTrim => {
                let end = bytes
                    .iter()
                    .rposition(|byte| *byte != b' ')
                    .map_or(0, |last| last + 1);
                Cow::Borrowed(&bytes[..end])
            }
        }
    }

    /// Compare two `TEXT` values under this collation.
    ///
    /// `NOCASE` is written out rather than folding into two `Vec`s first: this
    /// runs once per row per comparison, and the fold is only needed to decide
    /// one byte at a time.
    pub fn compare(self, left: &str, right: &str) -> Ordering {
        let (left, right) = (left.as_bytes(), right.as_bytes());
        match self {
            Collation::Binary => left.cmp(right),
            // `sqlite3StrNICmp`: compare the folded bytes pairwise, and let the
            // shorter string decide when one is a prefix of the other.
            Collation::NoCase => {
                for (a, b) in left.iter().zip(right.iter()) {
                    let ordering = a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase());
                    if ordering != Ordering::Equal {
                        return ordering;
                    }
                }
                left.len().cmp(&right.len())
            }
            Collation::RTrim => self.fold(left).cmp(&self.fold(right)),
        }
    }
}

impl fmt::Display for Collation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The collation tag one column or index column encodes to in the catalog.
///
/// `BINARY` is `1` rather than `0` so that a zero byte in this position is
/// always a decoding bug rather than a plausible default.
pub(crate) const TAG_BINARY: u8 = 1;
pub(crate) const TAG_NOCASE: u8 = 2;
pub(crate) const TAG_RTRIM: u8 = 3;

impl Collation {
    /// The byte this collation is stored as. See [`TAG_BINARY`].
    pub(crate) fn tag(self) -> u8 {
        match self {
            Collation::Binary => TAG_BINARY,
            Collation::NoCase => TAG_NOCASE,
            Collation::RTrim => TAG_RTRIM,
        }
    }

    /// The collation a stored tag names.
    pub(crate) fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            TAG_BINARY => Ok(Collation::Binary),
            TAG_NOCASE => Ok(Collation::NoCase),
            TAG_RTRIM => Ok(Collation::RTrim),
            other => Err(Error::Corrupt(alloc::format!(
                "unknown collation tag {other}"
            ))),
        }
    }
}

/// The collation an index column was declared with, defaulting to `BINARY`
/// for a declaration that recorded none (every index written before catalog
/// version 6).
pub(crate) fn at(collations: &[Collation], position: usize) -> Collation {
    collations.get(position).copied().unwrap_or_default()
}

/// A name for a collation that came out of a catalog, for error messages.
pub(crate) fn describe(collations: &[Collation]) -> String {
    let mut out = String::new();
    for (index, collation) in collations.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(collation.name());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn the_three_names_parse_in_any_case_and_nothing_else_does() {
        for (name, expected) in [
            ("binary", Collation::Binary),
            ("BINARY", Collation::Binary),
            ("NoCase", Collation::NoCase),
            ("NOCASE", Collation::NoCase),
            ("rtrim", Collation::RTrim),
            ("RTRIM", Collation::RTrim),
        ] {
            assert_eq!(Collation::from_name(name).unwrap(), expected);
        }
        let err = Collation::from_name("utf8mb4_unicode_ci").unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
        assert!(err.to_string().contains("utf8mb4_unicode_ci"), "{err}");
    }

    /// The exact boundary SQLite's `sqlite3UpperToLower` draws: ASCII letters
    /// fold, and the two bytes on either side of them do not.
    #[test]
    fn nocase_folds_ascii_letters_and_nothing_else() {
        let nocase = Collation::NoCase;
        assert_eq!(nocase.compare("ADA", "ada"), Ordering::Equal);
        assert_eq!(nocase.compare("AdA", "aDa"), Ordering::Equal);
        // Above ASCII, nothing is folded — this is the accent gap, on purpose.
        assert_ne!(nocase.compare("É", "é"), Ordering::Equal);
        assert_ne!(nocase.compare("STRASSE", "straße"), Ordering::Equal);
        // `@` is 0x40 and `[` is 0x5b: the bytes either side of `A`..`Z`.
        assert_ne!(nocase.compare("@", "`"), Ordering::Equal);
        assert_ne!(nocase.compare("[", "{"), Ordering::Equal);
    }

    /// Folding to *lower* is not interchangeable with folding to upper: it
    /// decides where `_` (0x5f) sorts relative to a letter. SQLite folds down,
    /// so `'A' > '_'` under NOCASE even though `'A' < '_'` under BINARY.
    #[test]
    fn nocase_folds_downward_which_is_observable_in_the_ordering() {
        assert_eq!(Collation::Binary.compare("A", "_"), Ordering::Less);
        assert_eq!(Collation::NoCase.compare("A", "_"), Ordering::Greater);
    }

    #[test]
    fn nocase_falls_back_to_length_when_one_is_a_prefix() {
        assert_eq!(Collation::NoCase.compare("AB", "abc"), Ordering::Less);
        assert_eq!(Collation::NoCase.compare("abc", "AB"), Ordering::Greater);
        assert_eq!(Collation::NoCase.compare("", ""), Ordering::Equal);
    }

    #[test]
    fn rtrim_ignores_trailing_spaces_only() {
        let rtrim = Collation::RTrim;
        assert_eq!(rtrim.compare("a", "a   "), Ordering::Equal);
        assert_eq!(rtrim.compare("   ", ""), Ordering::Equal);
        // Leading and interior spaces still count, and a tab is not a space.
        assert_ne!(rtrim.compare(" a", "a"), Ordering::Equal);
        assert_ne!(rtrim.compare("a\t", "a"), Ordering::Equal);
        assert_eq!(rtrim.compare("a ", "ab"), Ordering::Less);
    }

    /// The property the index encoding depends on: folding then comparing bytes
    /// is the same verdict as the collation's own comparison, for every pair.
    #[test]
    fn folding_then_comparing_bytes_agrees_with_the_collation() {
        let corpus = vec![
            "", " ", "  ", "a", "A", "ab", "AB", "aB", "Ab", "abc", "a ", "A ", "a\t", "_", "@",
            "[", "{", "`", "é", "É", "日本", "ADA", "ada", "Ada",
        ];
        for collation in [Collation::Binary, Collation::NoCase, Collation::RTrim] {
            for left in &corpus {
                for right in &corpus {
                    let folded = collation
                        .fold(left.as_bytes())
                        .cmp(&collation.fold(right.as_bytes()));
                    assert_eq!(
                        folded,
                        collation.compare(left, right),
                        "{collation} on {left:?} vs {right:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn folding_borrows_wherever_it_can() {
        assert!(matches!(
            Collation::Binary.fold(b"ADA"),
            Cow::Borrowed(b"ADA")
        ));
        assert!(matches!(
            Collation::NoCase.fold(b"ada"),
            Cow::Borrowed(b"ada")
        ));
        assert!(matches!(Collation::NoCase.fold(b"ADA"), Cow::Owned(_)));
    }

    #[test]
    fn tags_round_trip_and_an_unknown_one_is_corrupt() {
        for collation in [Collation::Binary, Collation::NoCase, Collation::RTrim] {
            assert_eq!(Collation::from_tag(collation.tag()).unwrap(), collation);
        }
        assert!(matches!(
            Collation::from_tag(0).unwrap_err(),
            Error::Corrupt(_)
        ));
        assert!(matches!(
            Collation::from_tag(9).unwrap_err(),
            Error::Corrupt(_)
        ));
    }
}
