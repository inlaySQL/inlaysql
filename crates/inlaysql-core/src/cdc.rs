//! Change data capture: what changed, in commit order.
//!
//! # What a change record is, and is not
//!
//! A record says **which row in which table changed, and how** — an insert, an
//! update or a delete. It does not carry the row's contents. That is a
//! deliberate trade, and the reasoning is worth stating because the other
//! choice looks more useful at first glance:
//!
//! * A row can be a kilobyte and a half once it carries an embedding. Copying
//!   every version of every row into a log inside the same file turns a
//!   bounded feature into an unbounded one.
//! * A consumer that is keeping up can read the row it was told about, and
//!   gets the current value — which is what an agent-memory pipeline actually
//!   wants.
//! * A consumer that has fallen behind cannot be served correctly either way:
//!   the row it missed has already been overwritten. Storing stale payloads
//!   would let it *believe* it was caught up, which is worse than telling it
//!   the truth.
//!
//! So the contract is: **the log tells you what changed; the database tells
//! you what it is now.** A consumer that needs the intermediate values must
//! read the log more often than the retention window.
//!
//! # Falling behind
//!
//! The log keeps a bounded number of the most recent statements. Reading it
//! returns the oldest version still available alongside the changes, so a
//! consumer can tell the difference between "nothing happened" and "you missed
//! something and must resynchronise from a full scan". Silently returning a
//! short list would make that indistinguishable.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{Error, Result};
use crate::row::{put_len, put_string, Cursor};
use crate::traits::RowId;

/// Metadata key prefix for one statement's change record.
pub(crate) const CDC_KEY_PREFIX: &str = "cdc:";

/// Metadata key holding the oldest change version still retained.
pub(crate) const CDC_FLOOR_KEY: &str = "cdc_floor";

/// How many statements' worth of changes are kept.
///
/// Bounded on purpose: an embedded database's log cannot be allowed to grow
/// without limit inside the file it is meant to keep small. A consumer polling
/// more often than this many writes never notices; one that does not is told
/// it fell behind rather than quietly served a short list.
pub(crate) const CDC_RETENTION: u64 = 4096;

/// How many expired change records [`crate::engine::Engine::trim_changes`]
/// drops in one go, instead of exactly one per commit.
///
/// The oldest surviving `cdc:` key and the newest one this commit is about to
/// write are on opposite ends of a range that, at [`CDC_RETENTION`] retained
/// records, spans many leaves of the shared row/metadata tree — nothing else
/// this commit touches is anywhere near it. Expiring one entry every commit
/// therefore means every commit past the retention window copy-on-writes a
/// *third*, distant root-to-leaf path purely for bookkeeping, on top of the
/// row's own path and the adjacent cluster of `next_row_id`/`write_version`/
/// the newest `cdc:` entry. AHL-480 profiled a steady-state durable-commit
/// loop with `sample(1)` and confirmed it: a second `F_FULLFSYNC` from a
/// WAL-region wrap (forced sooner by the extra dirty page every commit) was
/// showing up as ~2.9% of wall-clock on its own, on top of this trim's own
/// tree-walk cost, and batching it away roughly quintupled the measured gap
/// between wraps in a before/after run at this same commit.
///
/// Trimming in batches instead removes 63 out of every 64 commits' reason to
/// touch that distant leaf at all, at the cost of retaining up to this many
/// records past [`CDC_RETENTION`] before they are actually dropped — the
/// bound stays a bound, it is just `CDC_RETENTION..=CDC_RETENTION +
/// CDC_TRIM_BATCH - 1` wide instead of exact. No consumer-visible contract
/// changes: [`crate::cdc::Changes::lost`] is unaffected (a consumer that
/// fell behind by more than [`CDC_RETENTION`] is still told so, only
/// possibly with a few dozen more log statements than the tightest bound
/// would have reported).
pub(crate) const CDC_TRIM_BATCH: u64 = 64;

/// What happened to a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// The row did not exist before.
    Insert,
    /// The row existed and was rewritten.
    Update,
    /// The row existed and is gone.
    Delete,
}

impl ChangeKind {
    fn tag(self) -> u8 {
        match self {
            ChangeKind::Insert => 1,
            ChangeKind::Update => 2,
            ChangeKind::Delete => 3,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            1 => Ok(ChangeKind::Insert),
            2 => Ok(ChangeKind::Update),
            3 => Ok(ChangeKind::Delete),
            other => Err(Error::Corrupt(alloc::format!(
                "unknown change kind tag {other}"
            ))),
        }
    }

    /// The name used in the CLI and MCP surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeKind::Insert => "insert",
            ChangeKind::Update => "update",
            ChangeKind::Delete => "delete",
        }
    }
}

/// One row's change, within one committed statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The write version of the statement that made this change. Every change
    /// from one statement shares it, and it increases with commit order.
    pub version: u64,
    /// The table the row belongs to, as the catalog spells it.
    pub table: String,
    /// The row that changed.
    pub id: RowId,
    /// What happened to it.
    pub kind: ChangeKind,
}

/// The answer to a [`crate::Engine::changes`] call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    /// Changes after the requested version, in commit order.
    pub changes: Vec<Change>,
    /// The version the caller should ask from next time.
    pub version: u64,
    /// The newest version that is no longer in the log.
    ///
    /// If this is greater than the version the caller asked from, changes were
    /// dropped before they could be read: the caller has fallen behind and has
    /// to resynchronise from a scan rather than from the log. See
    /// [`Changes::lost`].
    pub floor: u64,
}

impl Changes {
    /// Whether changes were dropped before this call could read them.
    pub fn lost(&self, requested_from: u64) -> bool {
        self.floor > requested_from
    }
}

/// One statement's worth of changes, as stored.
///
/// ```text
/// record := u32 count, entry*
/// entry  := string table, u64 row id, u8 kind
/// ```
pub(crate) fn encode_record(entries: &[(String, RowId, ChangeKind)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_len(&mut out, entries.len());
    for (table, id, kind) in entries {
        put_string(&mut out, table);
        out.extend_from_slice(&id.to_le_bytes());
        out.push(kind.tag());
    }
    out
}

/// Parse a record written by [`encode_record`], attaching `version` to each.
pub(crate) fn decode_record(version: u64, bytes: &[u8]) -> Result<Vec<Change>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.count(13)?;
    let mut changes = Vec::with_capacity(count);
    for _ in 0..count {
        let table = cursor.string()?;
        let id = RowId::from_le_bytes(cursor.array8()?);
        let kind = ChangeKind::from_tag(cursor.u8()?)?;
        changes.push(Change {
            version,
            table,
            id,
            kind,
        });
    }
    Ok(changes)
}

/// The metadata key one statement's changes live under.
pub(crate) fn record_key(version: u64) -> String {
    // Fixed-width hex so the keys sort in version order, which is the order a
    // consumer has to read them in.
    alloc::format!("{CDC_KEY_PREFIX}{version:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;

    #[test]
    fn a_record_round_trips() {
        let entries = vec![
            ("docs".to_string(), 1, ChangeKind::Insert),
            ("docs".to_string(), 2, ChangeKind::Delete),
            ("other".to_string(), 99, ChangeKind::Update),
        ];
        let changes = decode_record(7, &encode_record(&entries)).unwrap();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].version, 7);
        assert_eq!(changes[1].kind, ChangeKind::Delete);
        assert_eq!(changes[2].table, "other");
        assert_eq!(changes[2].id, 99);
    }

    #[test]
    fn an_empty_record_round_trips() {
        assert!(decode_record(1, &encode_record(&[])).unwrap().is_empty());
    }

    #[test]
    fn a_truncated_record_is_rejected_not_panicked() {
        let bytes = encode_record(&[("docs".to_string(), 1, ChangeKind::Insert)]);
        for cut in 0..bytes.len() {
            assert!(
                decode_record(1, &bytes[..cut]).is_err(),
                "a {cut}-byte prefix decoded as a whole record"
            );
        }
    }

    #[test]
    fn keys_sort_in_version_order() {
        // The consumer reads them in key order, so this is load-bearing.
        let mut keys = vec![record_key(10), record_key(2), record_key(1000)];
        keys.sort();
        assert_eq!(keys, vec![record_key(2), record_key(10), record_key(1000)]);
    }

    #[test]
    fn falling_behind_is_reported() {
        let changes = Changes {
            changes: Vec::new(),
            version: 100,
            floor: 50,
        };
        assert!(
            changes.lost(10),
            "a consumer at 10 with a floor of 50 is behind"
        );
        assert!(!changes.lost(50));
        assert!(!changes.lost(80));
    }
}
