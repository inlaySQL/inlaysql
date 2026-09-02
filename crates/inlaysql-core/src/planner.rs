//! Small, optional statistics used by the rule-based planner.
//!
//! The statistics in this module are derived state. Rows and index entries
//! remain the source of truth; a missing, malformed or stale record simply
//! disables costing and leaves the executor on its existing rule-based path.
//! Keeping the format here, rather than in the catalog, also means a planner
//! experiment cannot change the catalog version or the row format.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::catalog::{Catalog, Index, IndexKind, Table};
use crate::error::{Error, Result};
use crate::row::{decode_row, put_len, put_string, Cursor};
use crate::traits::{Interrupt, Storage};
use crate::value::Value;

/// Metadata key for the optional planner statistics blob.
pub(crate) const STATS_META_KEY: &str = "planner_stats";

const STATS_MAGIC: &[u8; 4] = b"IPST";
const STATS_FORMAT_VERSION: u32 = 1;

/// Statistics for one leading prefix of a scalar B-tree index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexStats {
    /// Number of entries in the index. B-tree maintenance contributes one
    /// entry per stored row, including `NULL`, so this is the table row count
    /// for the current index format.
    pub entry_count: u64,
    /// Number of distinct encoded values in the index's leading column.
    pub distinct_prefix_count: u64,
}

impl IndexStats {
    /// Estimated number of rows in one equality group of the leading key.
    pub fn group_size(&self) -> u64 {
        if self.distinct_prefix_count == 0 {
            return 0;
        }
        self.entry_count
            .saturating_add(self.distinct_prefix_count - 1)
            / self.distinct_prefix_count
    }
}

/// Statistics for one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableStats {
    /// Number of live rows visible to the `ANALYZE` snapshot.
    pub row_count: u64,
    /// Leading-column statistics for the table's scalar B-tree indexes.
    pub indexes: BTreeMap<String, IndexStats>,
}

impl TableStats {
    /// Find index stats without requiring callers to canonicalise its name.
    pub fn index(&self, name: &str) -> Option<&IndexStats> {
        self.indexes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, stats)| stats)
    }
}

/// A complete statistics snapshot and the committed row version it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannerStats {
    /// The engine write version at which this snapshot was collected.
    pub data_version: u64,
    /// The catalog revision at which this snapshot was collected.
    pub schema_version: u64,
    /// Exact catalog bytes this snapshot was collected against.
    ///
    /// Row writes advance `data_version`; DDL can change the catalog without
    /// doing so. Keeping the catalog identity in the derived blob prevents a
    /// dropped-and-recreated table from inheriting stats for its predecessor
    /// after a reopen.
    pub catalog: Vec<u8>,
    /// Tables included in the snapshot, keyed by lowercased name.
    pub tables: BTreeMap<String, TableStats>,
}

impl PlannerStats {
    /// Empty stats at `data_version`.
    pub fn empty(data_version: u64) -> Self {
        Self {
            data_version,
            schema_version: 0,
            catalog: Vec::new(),
            tables: BTreeMap::new(),
        }
    }

    /// Whether this snapshot describes the engine's current committed rows.
    pub fn is_current(&self, data_version: u64) -> bool {
        self.data_version == data_version
    }

    /// Stamp this snapshot with the catalog it describes.
    pub fn stamp_catalog(&mut self, catalog: &Catalog, schema_version: u64) {
        self.catalog = catalog.encode();
        self.schema_version = schema_version;
    }

    /// Find a table without requiring callers to canonicalise its spelling.
    pub fn table(&self, name: &str) -> Option<&TableStats> {
        self.tables
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, stats)| stats)
    }

    /// Encode this snapshot as one self-contained metadata value.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(STATS_MAGIC);
        out.extend_from_slice(&STATS_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.data_version.to_le_bytes());
        out.extend_from_slice(&self.schema_version.to_le_bytes());
        put_len(&mut out, self.catalog.len());
        out.extend_from_slice(&self.catalog);
        put_len(&mut out, self.tables.len());
        for (table, stats) in &self.tables {
            put_string(&mut out, table);
            out.extend_from_slice(&stats.row_count.to_le_bytes());
            put_len(&mut out, stats.indexes.len());
            for (index, index_stats) in &stats.indexes {
                put_string(&mut out, index);
                out.extend_from_slice(&index_stats.entry_count.to_le_bytes());
                out.extend_from_slice(&index_stats.distinct_prefix_count.to_le_bytes());
            }
        }
        out
    }

    /// Decode a snapshot, refusing unknown versions and trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        if cursor.take(STATS_MAGIC.len())? != STATS_MAGIC {
            return Err(Error::Corrupt(
                "planner statistics have a bad magic".to_string(),
            ));
        }
        let format = u32::from_le_bytes(cursor.array4()?);
        if format != STATS_FORMAT_VERSION {
            return Err(Error::Corrupt(alloc::format!(
                "planner statistics format {format} is not supported"
            )));
        }
        let data_version = u64::from_le_bytes(cursor.array8()?);
        let schema_version = u64::from_le_bytes(cursor.array8()?);
        let catalog_len = cursor.u32()? as usize;
        let catalog = cursor.take(catalog_len)?.to_vec();
        let table_count = cursor.count(1)?;
        let mut tables = BTreeMap::new();
        for _ in 0..table_count {
            let table = cursor.string()?;
            let row_count = u64::from_le_bytes(cursor.array8()?);
            let index_count = cursor.count(1)?;
            let mut indexes = BTreeMap::new();
            for _ in 0..index_count {
                let index = cursor.string()?;
                let value = IndexStats {
                    entry_count: u64::from_le_bytes(cursor.array8()?),
                    distinct_prefix_count: u64::from_le_bytes(cursor.array8()?),
                };
                if indexes.insert(index, value).is_some() {
                    return Err(Error::Corrupt(
                        "planner statistics repeat an index".to_string(),
                    ));
                }
            }
            if tables
                .insert(table, TableStats { row_count, indexes })
                .is_some()
            {
                return Err(Error::Corrupt(
                    "planner statistics repeat a table".to_string(),
                ));
            }
        }
        if cursor.remaining() != 0 {
            return Err(Error::Corrupt(
                "planner statistics have trailing bytes".to_string(),
            ));
        }
        Ok(Self {
            data_version,
            schema_version,
            catalog,
            tables,
        })
    }
}

/// Collect exact row counts and leading-key cardinalities for one table.
///
/// This is intentionally an explicit scan. The first prototype is a correct
/// `ANALYZE` operation, not a background estimator or a write-maintained
/// counter. The index's own encoded prefix is used for distinctness, so the
/// estimate has the same collation and numeric equivalence as an index probe.
pub(crate) fn collect_table(
    storage: &dyn Storage,
    table: &Table,
    indexes: &[&Index],
    interrupt: &Interrupt,
) -> Result<TableStats> {
    let btree: Vec<(&Index, usize)> = indexes
        .iter()
        .filter(|index| index.kind == IndexKind::BTree)
        .map(|index| {
            let ordinal = table
                .require_column(index.column())
                .map(|(ordinal, _)| ordinal)?;
            Ok((*index, ordinal))
        })
        .collect::<Result<_>>()?;
    let mut distinct: Vec<BTreeSet<Vec<u8>>> = btree.iter().map(|_| BTreeSet::new()).collect();
    let mut row_count = 0u64;

    for result in crate::traits::RowScan::watched(storage, &table.name, interrupt) {
        let (_id, bytes) = result?;
        let row = decode_row(&bytes)?;
        row_count = row_count.saturating_add(1);
        for ((index, ordinal), values) in btree.iter().zip(distinct.iter_mut()) {
            let value = row.get(*ordinal).unwrap_or(&Value::Null);
            values.insert(crate::index::probe_prefix(
                &index.name,
                &[value],
                &index.collations,
            )?);
        }
    }

    let indexes = btree
        .into_iter()
        .zip(distinct)
        .map(|((index, _), values)| {
            (
                index.name.to_ascii_lowercase(),
                IndexStats {
                    entry_count: row_count,
                    distinct_prefix_count: values.len() as u64,
                },
            )
        })
        .collect();
    Ok(TableStats { row_count, indexes })
}

/// The two existing join access paths the first costed prototype may choose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinPath {
    /// Build the existing inner-side hash table.
    Hash,
    /// Probe the existing primary-key or scalar B-tree path per outer row.
    Probe,
}

/// One deterministic comparison between the existing hash and probe paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JoinDecision {
    /// The cheaper path, with hash winning an exact tie.
    pub path: JoinPath,
    /// Estimated work units for `path`.
    pub cost: u64,
    /// The other candidate's cost, if both paths were available.
    pub alternative_cost: Option<u64>,
}

/// Pick between the existing operators using integer work units.
///
/// The constants are deliberately small calibration knobs. They are not a
/// performance promise; their important properties are determinism,
/// saturating arithmetic, and the fact that the fallback is unavailable when
/// an input is unknown. `group_size` is one for an integer primary key and is
/// derived from the leading prefix stats for a secondary index.
pub(crate) fn choose_join(
    outer_rows: u64,
    inner_rows: u64,
    group_size: Option<u64>,
    hash_available: bool,
) -> Option<JoinDecision> {
    // A probe pays a tree descent and a row fetch for each candidate. The
    // current InlaySQL path is row-at-a-time, so its descent is materially
    // more expensive than one integer work unit; the constant is calibrated
    // against the clean joins baseline. This deliberately keeps the measured
    // full secondary-index shape on the existing hash path while a limited
    // query still wins with probes.
    const PROBE_DESCENT_COST: u64 = 12;
    // Every outer row pays the join loop itself — decode, key evaluation,
    // the per-row probe machinery, output assembly — whichever inner path
    // answers it, and that is several units, not one. This is what the
    // first costing got wrong: with the outer row priced at one unit and a
    // hash-built inner row at two, the model preferred to build the smaller
    // table and drive from the larger one, and AHL-512 duly swapped the
    // 20k-users × 160k-posts join into posts-driving. Measured, that plan
    // is ~3x slower (4.8 ms → 14.5 ms, `PERF.md` 2026-09-02): the same
    // 160k rows come out either way, but one order pays 160k probes and the
    // other 20k. The outer side should be the *smaller* table, and this
    // constant is what makes the arithmetic say so. It applies to both
    // paths, so it does not move the hash-versus-probe choice on its own.
    const OUTER_ROW_COST: u64 = 4;
    let probe_cost = group_size.map(|group| {
        outer_rows.saturating_mul(
            PROBE_DESCENT_COST
                .saturating_add(group.max(1))
                .saturating_add(OUTER_ROW_COST),
        )
    });
    let hash_cost = hash_available.then(|| {
        inner_rows
            .saturating_mul(2)
            .saturating_add(outer_rows.saturating_mul(1 + OUTER_ROW_COST))
    });

    match (hash_cost, probe_cost) {
        (None, None) => None,
        (Some(cost), None) => Some(JoinDecision {
            path: JoinPath::Hash,
            cost,
            alternative_cost: None,
        }),
        (None, Some(cost)) => Some(JoinDecision {
            path: JoinPath::Probe,
            cost,
            alternative_cost: None,
        }),
        (Some(hash), Some(probe)) if hash <= probe => Some(JoinDecision {
            path: JoinPath::Hash,
            cost: hash,
            alternative_cost: Some(probe),
        }),
        (Some(hash), Some(probe)) => Some(JoinDecision {
            path: JoinPath::Probe,
            cost: probe,
            alternative_cost: Some(hash),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_round_trip_without_trailing_bytes() {
        let mut stats = PlannerStats::empty(7);
        stats.tables.insert(
            "posts".to_string(),
            TableStats {
                row_count: 160_000,
                indexes: alloc::collections::BTreeMap::from([(
                    "posts_user_id".to_string(),
                    IndexStats {
                        entry_count: 160_000,
                        distinct_prefix_count: 20_000,
                    },
                )]),
            },
        );
        assert_eq!(PlannerStats::decode(&stats.encode()).unwrap(), stats);
    }

    #[test]
    fn malformed_stats_are_rejected() {
        let mut bytes = PlannerStats::empty(0).encode();
        bytes.push(1);
        assert!(matches!(
            PlannerStats::decode(&bytes),
            Err(Error::Corrupt(_))
        ));
    }

    #[test]
    fn cost_choice_matches_the_join_evidence() {
        let pk = choose_join(160_000, 20_000, Some(1), true).unwrap();
        assert_eq!(pk.path, JoinPath::Hash);
        let secondary = choose_join(20_000, 160_000, Some(8), true).unwrap();
        assert_eq!(secondary.path, JoinPath::Hash);
        let limited = choose_join(10, 160_000, Some(8), true).unwrap();
        assert_eq!(limited.path, JoinPath::Probe);
    }

    /// The same join written both ways round: driving from the smaller
    /// table must cost less, because that is what the measurement says
    /// (`PERF.md`, 2026-09-02 — 4.8 ms users-driving against 14.5 ms
    /// posts-driving for the same 160k output rows). The first costing had
    /// this backwards and the reorder swapped the fast order into the slow
    /// one.
    #[test]
    fn driving_from_the_smaller_table_costs_less() {
        let users_driving = choose_join(20_000, 160_000, Some(8), true).unwrap();
        let posts_driving = choose_join(160_000, 20_000, Some(1), true).unwrap();
        assert!(
            users_driving.cost < posts_driving.cost,
            "users-driving {} should be cheaper than posts-driving {}",
            users_driving.cost,
            posts_driving.cost
        );
    }

    #[test]
    fn unknown_probe_stats_do_not_invent_a_plan() {
        assert!(choose_join(20_000, 160_000, None, false).is_none());
    }
}
