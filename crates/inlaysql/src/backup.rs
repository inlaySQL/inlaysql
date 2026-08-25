//! Online backup: a consistent copy of a live database, taken without
//! stopping the writer.
//!
//! This is the file-and-path half of the operation; the argument for why the
//! copy is consistent lives with the copy itself, in
//! [`inlaysql_core::btree::backup`], and is worth reading before relying on
//! either. In one line: a committed root is already an immutable snapshot, so
//! a backup pins one and copies the pages it reaches — never a mix of two
//! commits, however many land while it runs.
//!
//! # Why this is not `vacuum`
//!
//! [`crate::vacuum`] rebuilds a database from its own SQL: it opens the source
//! read-write, holds the exclusive advisory lock for the whole copy *and* the
//! rename, and replays `CREATE TABLE`/`INSERT`/`CREATE INDEX` into a fresh
//! file. That makes it compaction, and it makes it unable to run at all beside
//! a live server, which holds that lock for its lifetime. It is also, table by
//! table, a sequence of separate statements — each one refreshing onto
//! whatever snapshot it landed on, so two tables can come from two different
//! commits.
//!
//! This writes no byte of the source, takes no lock of its own, and reads one
//! root. The trade is the other way round: the copy is not compacted (page ids
//! are preserved, so unreachable ids are holes in a sparse file) and it is a
//! *physical* copy, so it can only be restored by this build's format range,
//! where a SQL dump could in principle be replayed anywhere.
//!
//! # Restoring
//!
//! There is no restore command, and deliberately none: the file this produces
//! *is* a database. Point `Database::open` at it, or move it back over the
//! original while nothing has it open. Anything more would be a command whose
//! entire body is `fs::rename`, with a name that implies it knows something
//! about the file that it does not.

use std::fs;
use std::path::Path;

use crate::vacuum::{io_error, temp_path_beside};
use crate::{Database, Error, FileDevice, Result};

pub use inlaysql_core::btree::BackupSummary;

/// How [`backup`] reached the source database, which is what decides how
/// strong the copy's guarantee is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAccess {
    /// Opened read-write, holding the file's exclusive OS advisory lock for
    /// the whole copy.
    ///
    /// The strongest case, and the one to prefer: this handle registers a
    /// reader watermark, which pins the copied snapshot against page
    /// reclamation for as long as the copy runs — so the copy is sound even
    /// if `EngineOptions::page_reuse` is on. No writer outside this process
    /// can exist while the lock is held, and every writer inside it shares
    /// the registry the pin lives in.
    Exclusive,
    /// Opened read-only and lock-free, because another process — a running
    /// server — already holds the file open for writing.
    ///
    /// This is what makes the command work *online* at all, and it carries
    /// the one caveat the whole design has: a lock-free reader is invisible
    /// to the writer's reclaim proof, so a writer running with
    /// `EngineOptions::page_reuse` on (`serve --mysql --page-reuse`) could
    /// recycle a page underneath the copy. [`Database::backup_to`] refuses
    /// outright when the source's free list is non-empty, which catches that
    /// writer once it has actually freed anything — but an empty free list is
    /// not proof that reuse is off. Do not take a lock-free backup of a file
    /// a writer has page reuse enabled for.
    LockFree,
}

/// What one [`backup`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackupOutcome {
    /// The snapshot that was copied.
    pub summary: BackupSummary,
    /// How the source was opened — see [`SourceAccess`], which is the whole of
    /// what a caller has to reason about to know how much the copy is worth.
    pub access: SourceAccess,
}

impl Database {
    /// Write a consistent copy of this database's committed state to `path`.
    ///
    /// Other handles — other connections, other processes — keep committing
    /// throughout; the copy is one committed snapshot regardless. What lands
    /// after the snapshot is simply not in it.
    ///
    /// ```no_run
    /// use inlaysql::Database;
    ///
    /// let mut db = Database::open("app.inlay")?;
    /// let copy = db.backup_to("app-2026-08-25.inlay")?;
    /// assert!(copy.pages > 0);
    /// # Ok::<(), inlaysql::Error>(())
    /// ```
    ///
    /// The result opens as an ordinary database: same page size, same format
    /// version, an empty write-ahead log and a state block already naming the
    /// root, so opening it replays and recovers nothing.
    ///
    /// # It refuses to overwrite
    ///
    /// An existing `path` is [`Error::Storage`], never a replacement. Two
    /// mistakes are worth this much friction: naming the live database as the
    /// destination (which the final rename would otherwise carry out
    /// perfectly, destroying it), and silently discarding the previous
    /// backup — which is the one file anybody reaches for when the current
    /// one has already gone wrong. Remove or rename the old copy first.
    ///
    /// # Failure leaves nothing behind
    ///
    /// The copy is written to a temporary file beside `path` and moved into
    /// place with one `rename`, atomic on the same filesystem. A failure at
    /// any point — a full disk, a corrupt source page, the refusal described
    /// in [`inlaysql_core::btree::backup`] — removes the temporary file and
    /// never creates `path`, so a backup that exists is a backup that
    /// finished. It is never a partially written file that opens as an empty
    /// database.
    ///
    /// # Cost
    ///
    /// One read and one write of every *live* page, plus a `sync`. Space for
    /// a second copy of the live data, not of the whole file: pages the
    /// snapshot no longer reaches are never written, so a database that has
    /// grown large from deletes backs up at its live size.
    pub fn backup_to(&mut self, path: impl AsRef<Path>) -> Result<BackupSummary> {
        let path = path.as_ref();
        if path.exists() {
            return Err(Error::Storage(format!(
                "{} already exists; backup_to never overwrites — remove or rename \
                 it first, or name a destination that does not exist",
                path.display()
            )));
        }
        let tmp_path = temp_path_beside(path, "backup")?;
        let result = (|| -> Result<BackupSummary> {
            // A read-write device on the *destination*, which is a file this
            // process has just made up and nothing else can be holding. The
            // source is untouched: `Engine::backup_to` only reads it.
            let mut dest = FileDevice::open(&tmp_path)?;
            let summary = self.engine.backup_to(&mut dest)?;
            drop(dest);
            fs::rename(&tmp_path, path).map_err(io_error)?;
            Ok(summary)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        result
    }
}

/// Copy the database at `source` to `destination`, without stopping whatever
/// is writing to it.
///
/// This is [`Database::backup_to`] with the opening decided for you, which is
/// the part a command-line caller cannot make well: it prefers a read-write
/// handle (exclusive lock, pinned snapshot, sound even under page reuse) and
/// falls back to a lock-free read-only handle when another process already
/// holds the file for writing — the case a live server is, and the reason this
/// command exists. [`BackupOutcome::access`] reports which happened, because
/// the two are not equally strong and a caller that cannot tell them apart
/// cannot know what it has.
///
/// `source` must exist. It is never created — the same refusal
/// [`crate::vacuum`] makes, for the same reason: a typo'd path that silently
/// "backed up" a database that never existed produces an empty file with a
/// reassuring name.
pub fn backup(source: impl AsRef<Path>, destination: impl AsRef<Path>) -> Result<BackupOutcome> {
    let source = source.as_ref();
    if !source.exists() {
        return Err(Error::Storage(format!(
            "{} does not exist; backup only copies an existing database",
            source.display()
        )));
    }

    let (mut database, access) = match Database::open(source) {
        Ok(database) => (database, SourceAccess::Exclusive),
        // The read-write attempt's error is deliberately discarded rather
        // than reported: the only reason to try read-write first is the
        // stronger pin, and every reason it can fail that is *not* "a writer
        // holds the lock" — a missing file, a foreign header, a format version
        // from the future — fails the read-only open below too, with the same
        // message, since both paths go through the same `parse_header`. So the
        // error a caller sees always describes the file rather than the
        // attempt, and the fallback never hides a real problem behind a
        // lock-contention story.
        Err(_) => (Database::open_read_only(source)?, SourceAccess::LockFree),
    };
    let summary = database.backup_to(destination)?;
    Ok(BackupOutcome { summary, access })
}
