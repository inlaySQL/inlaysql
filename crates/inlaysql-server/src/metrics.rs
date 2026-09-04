//! The counters `SHOW STATUS` reports: what this server has been doing since
//! it started, and what this connection has been doing since it connected.
//!
//! # Why these are counters and not an exporter
//!
//! There is no HTTP server in this workspace and this is not the crate that
//! adds one — an exporter would be a listener, a router, a text format and a
//! second port to secure, all to carry numbers the client already has a
//! connection for. MySQL's own answer is `SHOW STATUS`, every monitoring agent
//! that speaks to MySQL already scrapes it, and it costs one match arm.
//!
//! # The two rules everything here follows
//!
//! **A name means what MySQL means by it, or it is not spelled like MySQL's.**
//! `Questions`, `Com_select`, `Bytes_sent`, `Threads_running`, `Uptime` and the
//! rest below carry exactly their upstream definitions, because an operator
//! reading them has years of MySQL habits and a dashboard built on them. Where
//! this server counts something MySQL has no variable for — its error
//! classification, its lumped `SHOW` counter — the name is prefixed
//! `Inlaysql_` so nobody can mistake it for a variable they already know.
//!
//! **Every number is maintained, or it is not reported.** This server has
//! twice shipped a variable it reported and did not honour
//! (`docs/enterprise-readiness.md`, blocker 10), and the lesson taken was that
//! reporting nothing beats reporting fiction. So the list below is short: it is
//! what is counted at a real choke point, and there is no arm that returns a
//! plausible zero. `Threads_connected` and `Threads_running` are not counted at
//! all — they are *derived*, from the same registry `SHOW PROCESSLIST` reads,
//! which is why the list and the count cannot disagree.
//!
//! # What it costs the statement being counted
//!
//! Two relaxed `fetch_add`s — one on this connection's own counters, one on the
//! server's — plus a borrowed scan of the statement's leading keyword that
//! allocates nothing. Bytes are accumulated in plain `u64`s inside
//! [`crate::packet::Stream`] and pushed into the atomics once per command, so a
//! result set of ten million rows costs the same two adds as a `PING`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::control::Registry;
use crate::errors::MysqlError;
use crate::sqltext::leading_keyword;

/// One thing that is counted.
///
/// The discriminants index [`Metrics::counters`] directly, so the order here
/// and the order in [`DESCRIPTIONS`] are the same order, and a mismatch is a
/// compile error rather than a mislabelled number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Counter {
    // ------------------------------------------------------ statements
    /// Statements executed: `COM_QUERY` and `COM_STMT_EXECUTE`.
    Questions,
    /// `SELECT`, including the ones the shim answers from the catalog.
    ComSelect,
    /// `INSERT`.
    ComInsert,
    /// `REPLACE`.
    ComReplace,
    /// `UPDATE`.
    ComUpdate,
    /// `DELETE`.
    ComDelete,
    /// `CREATE TABLE` (and any other `CREATE` that is not an index).
    ComCreateTable,
    /// `DROP TABLE` (and any other `DROP` that is not an index).
    ComDropTable,
    /// `ALTER TABLE`.
    ComAlterTable,
    /// `CREATE INDEX`.
    ComCreateIndex,
    /// `DROP INDEX`.
    ComDropIndex,
    /// `BEGIN` / `START TRANSACTION`.
    ComBegin,
    /// `COMMIT`.
    ComCommit,
    /// `ROLLBACK`.
    ComRollback,
    /// `SET`, in every spelling — MySQL's own name for this counter.
    ComSetOption,
    /// `KILL`, as a statement. `COM_PROCESS_KILL` counts here too: it is the
    /// same operation with a different spelling.
    ComKill,
    /// Any `SHOW`, `DESCRIBE` or `EXPLAIN`. MySQL has one counter per `SHOW`
    /// form; this has one for all of them, which is why it is not spelled like
    /// MySQL's.
    InlaysqlComShow,
    /// `CREATE USER`, `GRANT`, `REVOKE` and the rest of the account
    /// statements — worth its own line because it is the set that changes who
    /// can do what.
    InlaysqlComAccount,
    /// Anything else, including a statement this server refused.
    InlaysqlComOther,

    // -------------------------------------------------- wire commands
    /// `COM_STMT_PREPARE`. Not a `Question`: nothing ran.
    ComStmtPrepare,
    /// `COM_STMT_EXECUTE`.
    ComStmtExecute,
    /// `COM_STMT_CLOSE`.
    ComStmtClose,
    /// `COM_STMT_RESET`.
    ComStmtReset,
    /// `COM_PING`.
    ComPing,
    /// `COM_INIT_DB`.
    ComInitDb,

    // ---------------------------------------------------------- bytes
    /// Bytes read from the client's socket, packet headers included.
    BytesReceived,
    /// Bytes written to the client's socket, packet headers included.
    BytesSent,

    // ---------------------------------------------- per-thread timing
    // AHL-555 (C2): where a connection thread's time actually goes, split at
    // the two boundaries the server can see without reaching into the
    // engine's commit path. See `crate::connection::Connection::serve` and
    // `Connection::commit` for where each is measured, and this module's
    // doc comment for the "one `Instant::now()` pair per phase, nothing
    // when nobody reads" cost rule.
    /// Nanoseconds a connection thread spent blocked in
    /// [`std::io::Read::read`], waiting for the client to send its next
    /// command. Measured around [`crate::packet::Stream::read_message`] in
    /// the command loop — before dispatch, before any statement work.
    InlaysqlThreadSocketWaitNs,
    /// Nanoseconds spent inside `Connection::dispatch_command`
    /// ([`crate::connection`]) — every command this server can be asked to
    /// run, from whichever command byte
    /// it was to the reply being ready to write.
    ///
    /// **For an autocommit write, this includes the commit.** The engine
    /// performs an autocommit statement's execution and its commit inside
    /// one opaque call ([`inlaysql::Database::execute_prepared`], which
    /// reaches `Engine::run_refreshed` → `end_write` → `commit_storage` with
    /// no seam this crate can see or time without adding a timestamp inside
    /// the engine's own commit path — out of bounds for this instrument, see
    /// `PERF.md`'s AHL-555 section). [`Counter::InlaysqlThreadCommitNs`]
    /// below is the *separable* remainder: the portion of this bucket that
    /// an explicit `COMMIT` spends inside [`inlaysql::Database::commit`], a
    /// sub-interval of this counter rather than additional wall time. On a
    /// purely-autocommit workload (this project's own OLTP write benchmark)
    /// that remainder is zero and the whole cost of a write, execution and
    /// commit together, is only visible here.
    InlaysqlThreadExecuteNs,
    /// Nanoseconds spent inside [`inlaysql::Database::commit`] specifically —
    /// the separable case: an explicit `BEGIN` ... `COMMIT`, or one of the
    /// places this server commits an open transaction before a statement
    /// that needs to see it committed (an account statement, `OPTIMIZE
    /// TABLE`, autocommit turning back on). Always a sub-interval of
    /// [`Counter::InlaysqlThreadExecuteNs`] for the same command, never
    /// counted twice as separate wall time.
    InlaysqlThreadCommitNs,
    /// How many commits this thread has caused — an explicit `COMMIT` that
    /// found a transaction open, or an autocommit write statement, counted
    /// the moment the engine call that performed it returns successfully.
    /// The denominator [`Counter::InlaysqlThreadCommitNs`] divides by is
    /// this counter restricted to the explicit case only; for the barrier
    /// *rate* this counter's own delta over wall time (or, server-wide,
    /// [`inlaysql::CommitStats::normal_tickets_flushed`]) is the number to
    /// use.
    InlaysqlThreadCommits,

    // --------------------------------------------------------- errors
    /// Every error packet this server sent. The sum of the ten below.
    InlaysqlErrorsTotal,
    /// A login, a privilege or a reserved table refused it (`1045`, `1044`,
    /// `1142`, `1227`, `1095`).
    InlaysqlErrorsAccessDenied,
    /// It did not parse (`1064`).
    InlaysqlErrorsSyntax,
    /// It parsed and this server does not implement it (`1235`).
    InlaysqlErrorsUnsupported,
    /// A `UNIQUE`, `NOT NULL`, `CHECK` or primary-key constraint refused it
    /// (`1062`, `1048`, `3819`).
    InlaysqlErrorsConstraint,
    /// Another writer committed first (`1213`). The one an application is
    /// expected to retry, and the one whose *rate* decides whether this
    /// workload suits an optimistic engine at all.
    InlaysqlErrorsConflict,
    /// `max_execution_time` stopped it (`3024`).
    InlaysqlErrorsTimeout,
    /// A `KILL` stopped it (`1317`).
    InlaysqlErrorsInterrupted,
    /// It named a table, column, index or database that is not there (`1146`,
    /// `1054`, `1049`, `1176`, `1109`).
    InlaysqlErrorsNoSuchObject,
    /// Everything else, including storage failures and the honest `1105`.
    InlaysqlErrorsOther,

    // -------------------------------------------------- the slow log
    /// Statements that ran longer than `long_query_time`. Always `0` when the
    /// slow-query log is off, which is the default — see
    /// [`crate::ServerOptions::slow_query_log_ms`].
    SlowQueries,

    // ------------------------------------------ connections (global only)
    /// Connection attempts the accept loop saw, whether or not they went on to
    /// authenticate. MySQL's definition exactly.
    Connections,
    /// Connections that never completed authentication — a wrong password, a
    /// client that asked for TLS, a handshake that did not parse, or one
    /// refused at the connection cap.
    AbortedConnects,
    /// Connections refused because `max_connections` was already reached.
    /// MySQL's own variable, and the one that says the cap is the problem.
    ConnectionErrorsMaxConnections,
    /// The high-water mark of [`Registry::live_count`].
    MaxUsedConnections,
}

/// How many [`Counter`]s there are. The `as usize` below is the discriminant of
/// the last variant, so adding one to the enum and forgetting this is a panic
/// on the first `record` rather than a silent overwrite of a neighbour.
const COUNT: usize = Counter::MaxUsedConnections as usize + 1;

/// Whether a counter means something per connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `SHOW SESSION STATUS`, which is what a bare `SHOW STATUS` means.
    Session,
    /// `SHOW GLOBAL STATUS`.
    Global,
}

/// Every counter's reported name and whether it has a per-session reading.
///
/// A counter with no per-session reading — a connection count, an uptime —
/// reports its **global** value under `SHOW SESSION STATUS` as well. That is
/// MySQL's own rule for a global-only status variable, and the alternative is
/// worse in both directions: a zero would be a lie, and omitting the row would
/// hide `Uptime` from the query most people type.
const DESCRIPTIONS: [(&str, bool); COUNT] = [
    ("Questions", true),
    ("Com_select", true),
    ("Com_insert", true),
    ("Com_replace", true),
    ("Com_update", true),
    ("Com_delete", true),
    ("Com_create_table", true),
    ("Com_drop_table", true),
    ("Com_alter_table", true),
    ("Com_create_index", true),
    ("Com_drop_index", true),
    ("Com_begin", true),
    ("Com_commit", true),
    ("Com_rollback", true),
    ("Com_set_option", true),
    ("Com_kill", true),
    ("Inlaysql_com_show", true),
    ("Inlaysql_com_account", true),
    ("Inlaysql_com_other", true),
    ("Com_stmt_prepare", true),
    ("Com_stmt_execute", true),
    ("Com_stmt_close", true),
    ("Com_stmt_reset", true),
    ("Com_ping", true),
    ("Com_init_db", true),
    ("Bytes_received", true),
    ("Bytes_sent", true),
    ("Inlaysql_thread_socket_wait_ns", true),
    ("Inlaysql_thread_execute_ns", true),
    ("Inlaysql_thread_commit_ns", true),
    ("Inlaysql_thread_commits", true),
    ("Inlaysql_errors_total", true),
    ("Inlaysql_errors_access_denied", true),
    ("Inlaysql_errors_syntax", true),
    ("Inlaysql_errors_unsupported", true),
    ("Inlaysql_errors_constraint", true),
    ("Inlaysql_errors_conflict", true),
    ("Inlaysql_errors_timeout", true),
    ("Inlaysql_errors_interrupted", true),
    ("Inlaysql_errors_no_such_object", true),
    ("Inlaysql_errors_other", true),
    ("Slow_queries", true),
    ("Connections", false),
    ("Aborted_connects", false),
    ("Connection_errors_max_connections", false),
    ("Max_used_connections", false),
];

impl Counter {
    /// Which `Com_*` counter a statement belongs to, from its leading keyword.
    ///
    /// Borrowed and allocation-free ([`leading_keyword`]), because this runs on
    /// every statement including the point read the rest of this engine is
    /// tuned around. It classifies the *client's* statement, before the shim
    /// has rewritten anything: a `SELECT` that the shim answers from the
    /// catalog is still a `SELECT` to whoever asked, and a MySQL `ALTER TABLE`
    /// that became three engine statements is still one `Com_alter_table`.
    pub fn for_statement(sql: &str) -> Self {
        let (word, rest) = leading_keyword(sql);
        // `eq_ignore_ascii_case` on a borrowed slice: no uppercasing, no
        // allocation, and a keyword is ASCII by definition.
        let is = |keyword: &str| word.eq_ignore_ascii_case(keyword);
        if is("SELECT") || is("WITH") || is("TABLE") || is("VALUES") {
            Counter::ComSelect
        } else if is("INSERT") {
            Counter::ComInsert
        } else if is("REPLACE") {
            Counter::ComReplace
        } else if is("UPDATE") {
            Counter::ComUpdate
        } else if is("DELETE") || is("TRUNCATE") {
            Counter::ComDelete
        } else if is("CREATE") || is("DROP") || is("ALTER") {
            classify_schema(word, rest)
        } else if is("BEGIN") || is("START") {
            Counter::ComBegin
        } else if is("COMMIT") {
            Counter::ComCommit
        } else if is("ROLLBACK") {
            Counter::ComRollback
        } else if is("SET") || is("USE") {
            Counter::ComSetOption
        } else if is("KILL") {
            Counter::ComKill
        } else if is("SHOW") || is("DESCRIBE") || is("DESC") || is("EXPLAIN") {
            Counter::InlaysqlComShow
        } else if is("GRANT") || is("REVOKE") {
            Counter::InlaysqlComAccount
        } else {
            Counter::InlaysqlComOther
        }
    }

    /// Which error counter a refusal belongs to, by the code the client is
    /// about to branch on.
    ///
    /// Grouped by *what an operator would do about it* rather than by SQLSTATE
    /// class: a spike in `conflict` means the workload is contending, a spike
    /// in `access_denied` means a credential or a grant is wrong, and a spike
    /// in `syntax` means something upstream is generating SQL this server does
    /// not take. Those are three different phone calls, and a single
    /// `Errors_total` cannot tell them apart.
    pub fn for_error(error: &MysqlError) -> Self {
        match error.code {
            1044 | 1045 | 1095 | 1142 | 1143 | 1227 => Counter::InlaysqlErrorsAccessDenied,
            1064 | 1149 => Counter::InlaysqlErrorsSyntax,
            1235 => Counter::InlaysqlErrorsUnsupported,
            1048 | 1062 | 3819 => Counter::InlaysqlErrorsConstraint,
            1213 => Counter::InlaysqlErrorsConflict,
            3024 => Counter::InlaysqlErrorsTimeout,
            1317 => Counter::InlaysqlErrorsInterrupted,
            1049 | 1054 | 1094 | 1109 | 1146 | 1176 => Counter::InlaysqlErrorsNoSuchObject,
            _ => Counter::InlaysqlErrorsOther,
        }
    }
}

/// `CREATE`/`DROP`/`ALTER` split by what follows them, because an index and a
/// table are different enough for an operator to want them apart.
///
/// `INDEX` is looked for only where the grammar can put it — immediately after
/// the verb, or after exactly one of the modifiers `CREATE` accepts. Scanning
/// further would file `CREATE TABLE index (...)` and `CREATE TABLE t (index
/// INTEGER)` as index creations, and a counter that is wrong about a table
/// called `index` is a counter somebody will one day chase.
fn classify_schema(verb: &str, rest: &str) -> Counter {
    let mut words = rest.split_whitespace();
    let first = words.next().unwrap_or("");
    let indexed = first.eq_ignore_ascii_case("index")
        || (["UNIQUE", "FULLTEXT", "SPATIAL", "VECTOR"]
            .iter()
            .any(|modifier| first.eq_ignore_ascii_case(modifier))
            && words
                .next()
                .is_some_and(|word| word.eq_ignore_ascii_case("index")));
    match (verb.as_bytes().first().map(u8::to_ascii_uppercase), indexed) {
        (Some(b'C'), true) => Counter::ComCreateIndex,
        (Some(b'C'), false) => Counter::ComCreateTable,
        (Some(b'D'), true) => Counter::ComDropIndex,
        (Some(b'D'), false) => Counter::ComDropTable,
        _ => Counter::ComAlterTable,
    }
}

/// A set of counters, used twice: once behind an `Arc` for the whole server,
/// and once owned by each connection for its own session.
///
/// The same type for both so there is exactly one definition of what each
/// number means. The per-session copy is an uncontended atomic on a cache line
/// nobody else touches, which costs what a plain increment costs.
pub struct Metrics {
    counters: [AtomicU64; COUNT],
    /// When this set started counting. On the server's copy that is process
    /// start, which is what `Uptime` reports; on a session's it is the
    /// connection's, and nothing reads it.
    started: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    /// A set of counters, all zero, counting from now.
    pub fn new() -> Self {
        Self {
            counters: std::array::from_fn(|_| AtomicU64::new(0)),
            started: Instant::now(),
        }
    }

    /// Add one to `counter`.
    ///
    /// `Relaxed`, deliberately: these are counters, not flags anything
    /// synchronises on, and the only thing a stronger ordering would buy is a
    /// fence on the statement path in exchange for a number being consistent
    /// with a different number at an instant nobody can observe anyway.
    pub fn record(&self, counter: Counter) {
        self.add(counter, 1);
    }

    /// Add `amount` to `counter`.
    pub fn add(&self, counter: Counter, amount: u64) {
        self.counters[counter as usize].fetch_add(amount, Ordering::Relaxed);
    }

    /// Raise `counter` to `value` if it is not there already — a high-water
    /// mark rather than a running total.
    pub fn record_max(&self, counter: Counter, value: u64) {
        self.counters[counter as usize].fetch_max(value, Ordering::Relaxed);
    }

    /// Seconds since this set started counting.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

/// Every status variable and its value, in name order, for `SHOW STATUS`.
///
/// `session` is the asking connection's own set and `global` the server's;
/// which one a row is read from is [`DESCRIPTIONS`]'s second column and the
/// requested `scope`. `registry` supplies the two numbers that are counted
/// nowhere — the connection counts — because deriving them from the list
/// `SHOW PROCESSLIST` shows is what stops the two ever disagreeing.
/// `commit_stats` supplies four more the same way: read live off the shared
/// [`inlaysql::FileDevice`] rather than counted by this module, and `None`
/// only if the server's own keeper handle could not report (never expected
/// in practice — see `Server::run`).
pub fn status_variables(
    scope: Scope,
    session: &Metrics,
    global: &Metrics,
    registry: &Registry,
    commit_stats: Option<inlaysql::CommitStats>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(COUNT + 20);
    for (index, (name, per_session)) in DESCRIPTIONS.iter().enumerate() {
        let from = if *per_session && scope == Scope::Session {
            session
        } else {
            global
        };
        out.push((
            (*name).to_string(),
            from.counters[index].load(Ordering::Relaxed).to_string(),
        ));
    }
    // Uptime is the server's however it is asked for: a per-connection uptime
    // would be a number MySQL does not have and nobody's dashboard wants.
    out.push(("Uptime".to_string(), global.uptime_secs().to_string()));
    out.push((
        "Threads_connected".to_string(),
        registry.live_count().to_string(),
    ));
    out.push((
        "Threads_running".to_string(),
        registry.running_count().to_string(),
    ));
    // The commit-batching counters: global-only, like the connection counts
    // above, and for the same reason — a per-session flush count would not
    // mean anything, since every handle on this file shares one
    // `CommitCoordinator`. `Inlaysql_normal_commit_tickets /
    // Inlaysql_normal_commit_flushes` is commits landed per `fsync`, the
    // server-side answer to the question `INLAYSQL_COMMIT_STATS` could
    // previously only answer for a process that exits normally.
    let stats = commit_stats.unwrap_or_default();
    out.push((
        "Inlaysql_commit_flushes".to_string(),
        stats.flushes.to_string(),
    ));
    out.push((
        "Inlaysql_commit_tickets".to_string(),
        stats.tickets_flushed.to_string(),
    ));
    out.push((
        "Inlaysql_normal_commit_flushes".to_string(),
        stats.normal_flushes.to_string(),
    ));
    out.push((
        "Inlaysql_normal_commit_tickets".to_string(),
        stats.normal_tickets_flushed.to_string(),
    ));
    // AHL-555 (C2): the rest of `inlaysql::CommitStats` — already computed
    // by `CommitCoordinator` for every commit, on the assumption nothing
    // outside `crates/inlaysql` could use it yet. It can: this is the answer
    // to "is a writer's time going into the gate, into the barrier itself,
    // or into waiting for someone else's barrier to finish" without adding a
    // single timer to the engine's commit path — `gate_wait_ns` is time
    // blocked *acquiring* the gate (queued behind another writer's hold),
    // `gate_hold_ns` is time spent doing this writer's own work once it has
    // the gate (rebase, WAL encode, the record and page writes), the
    // `_racing_`/`_racing_start_` splits of `gate_hold_ns` say whether that
    // hold started or ran while a flush was already in flight — the direct
    // measurement of whether the next gate holder proceeds while a barrier
    // is running rather than serialising behind it — `follower_wait_ns` is
    // time spent parked waiting for someone else's flush to finish rather
    // than leading one, `gather_spin_ns` is a leader's adaptive coalescing
    // window, `fsync_ns` is the barrier call itself, `post_ns` is waking the
    // followers it just satisfied, and `gap_ns` is dead time between one
    // cycle ending and the next leader being elected. Every one of these is
    // global, like the four above, for the same reason: one file, one
    // `CommitCoordinator`, shared by every connection.
    out.push((
        "Inlaysql_commit_gate_wait_ns".to_string(),
        stats.gate_wait_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_waits".to_string(),
        stats.gate_waits.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_hold_ns".to_string(),
        stats.gate_hold_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_hold_racing_ns".to_string(),
        stats.gate_hold_racing_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_hold_racing_count".to_string(),
        stats.gate_hold_racing_count.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_hold_racing_start_ns".to_string(),
        stats.gate_hold_racing_start_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gate_hold_racing_start_count".to_string(),
        stats.gate_hold_racing_start_count.to_string(),
    ));
    out.push((
        "Inlaysql_commit_follower_wait_ns".to_string(),
        stats.follower_wait_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_follower_waits".to_string(),
        stats.follower_waits.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gather_spin_ns".to_string(),
        stats.gather_spin_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_fsync_ns".to_string(),
        stats.fsync_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_post_ns".to_string(),
        stats.post_ns.to_string(),
    ));
    out.push((
        "Inlaysql_commit_gap_ns".to_string(),
        stats.gap_ns.to_string(),
    ));
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_counter_has_a_name_and_no_name_is_used_twice() {
        let mut names: Vec<&str> = DESCRIPTIONS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two counters share a name");
        assert_eq!(before, COUNT);
    }

    /// The same "no name twice" rule, but for the *whole* `SHOW STATUS`
    /// surface — the [`DESCRIPTIONS`]-driven counters above, `Uptime`,
    /// `Threads_connected`/`Threads_running`, and the `CommitStats`-derived
    /// rows [`status_variables`] pushes by hand (AHL-555, C2). Those last
    /// two groups are not covered by
    /// `every_counter_has_a_name_and_no_name_is_used_twice`, which only
    /// inspects [`DESCRIPTIONS`] — this is the test that would catch one of
    /// them colliding with a `Counter` name or with each other.
    #[test]
    fn the_full_status_output_has_no_duplicate_name_either() {
        let session = Metrics::new();
        let global = Metrics::new();
        let registry = Registry::new();
        let rows = status_variables(Scope::Global, &session, &global, &registry, None);
        let mut names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "two status rows share a name");
    }

    /// The per-thread timing counters this session added (AHL-555, C2):
    /// `Inlaysql_thread_commit_ns` is a sub-interval of
    /// `Inlaysql_thread_execute_ns`, never additional wall time, and both
    /// are plain adds on `Metrics` like every other counter — this pins the
    /// wiring (name, session/global split) the way
    /// `session_scope_reports_this_connection_and_global_reports_the_server`
    /// pins `Questions`.
    #[test]
    fn thread_timing_counters_accumulate_nanoseconds_session_and_global() {
        let session = Metrics::new();
        let global = Metrics::new();
        let registry = Registry::new();

        session.add(Counter::InlaysqlThreadSocketWaitNs, 1_500);
        global.add(Counter::InlaysqlThreadSocketWaitNs, 1_500);
        session.add(Counter::InlaysqlThreadExecuteNs, 9_000);
        global.add(Counter::InlaysqlThreadExecuteNs, 9_000);
        session.add(Counter::InlaysqlThreadCommitNs, 4_000);
        global.add(Counter::InlaysqlThreadCommitNs, 4_000);
        session.record(Counter::InlaysqlThreadCommits);
        global.record(Counter::InlaysqlThreadCommits);

        let read = |name: &str| -> String {
            status_variables(Scope::Session, &session, &global, &registry, None)
                .into_iter()
                .find(|(variable, _)| variable == name)
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("{name} is not reported"))
        };

        assert_eq!(read("Inlaysql_thread_socket_wait_ns"), "1500");
        assert_eq!(read("Inlaysql_thread_execute_ns"), "9000");
        assert_eq!(read("Inlaysql_thread_commit_ns"), "4000");
        assert_eq!(read("Inlaysql_thread_commits"), "1");
    }

    /// The naming rule, enforced rather than remembered: a name that is not
    /// MySQL's own must say so, so an operator cannot mistake this server's
    /// error buckets for a variable their dashboard already understands.
    #[test]
    fn a_name_this_server_invented_is_prefixed() {
        for (name, _) in DESCRIPTIONS {
            let mysqls_own = name.starts_with("Com_")
                || matches!(
                    name,
                    "Questions"
                        | "Bytes_received"
                        | "Bytes_sent"
                        | "Slow_queries"
                        | "Connections"
                        | "Aborted_connects"
                        | "Connection_errors_max_connections"
                        | "Max_used_connections"
                );
            assert!(
                mysqls_own || name.starts_with("Inlaysql_"),
                "`{name}` is neither a MySQL status variable nor marked as this server's own"
            );
        }
    }

    #[test]
    fn statements_are_classified_by_their_leading_keyword() {
        let cases = [
            ("SELECT 1", Counter::ComSelect),
            ("  select * from t", Counter::ComSelect),
            ("/* trace:abc */ SELECT 1", Counter::ComSelect),
            ("-- a comment\nSELECT 1", Counter::ComSelect),
            ("INSERT INTO t VALUES (1)", Counter::ComInsert),
            ("REPLACE INTO t VALUES (1)", Counter::ComReplace),
            ("UPDATE t SET a = 1", Counter::ComUpdate),
            ("DELETE FROM t", Counter::ComDelete),
            ("CREATE TABLE t (a INTEGER)", Counter::ComCreateTable),
            ("CREATE INDEX i ON t (a)", Counter::ComCreateIndex),
            ("CREATE UNIQUE INDEX i ON t (a)", Counter::ComCreateIndex),
            ("CREATE FULLTEXT INDEX i ON t (a)", Counter::ComCreateIndex),
            ("DROP INDEX i ON t", Counter::ComDropIndex),
            ("DROP TABLE t", Counter::ComDropTable),
            ("ALTER TABLE t ADD COLUMN b TEXT", Counter::ComAlterTable),
            ("BEGIN", Counter::ComBegin),
            ("START TRANSACTION", Counter::ComBegin),
            ("COMMIT", Counter::ComCommit),
            ("ROLLBACK", Counter::ComRollback),
            ("SET NAMES utf8mb4", Counter::ComSetOption),
            ("KILL QUERY 4", Counter::ComKill),
            ("SHOW TABLES", Counter::InlaysqlComShow),
            ("EXPLAIN SELECT 1", Counter::InlaysqlComShow),
            ("GRANT SELECT ON *.* TO 'a'", Counter::InlaysqlComAccount),
            ("SAVEPOINT s", Counter::InlaysqlComOther),
            ("", Counter::InlaysqlComOther),
        ];
        for (sql, expected) in cases {
            assert_eq!(Counter::for_statement(sql), expected, "for `{sql}`");
        }
    }

    /// A `CREATE TABLE` whose *column* is called `index` must not be counted as
    /// an index creation — the word has to come before the target, not
    /// anywhere in the statement.
    #[test]
    fn a_column_called_index_is_still_a_table() {
        assert_eq!(
            Counter::for_statement("CREATE TABLE t (index INTEGER, other TEXT)"),
            Counter::ComCreateTable
        );
    }

    #[test]
    fn errors_are_bucketed_by_what_an_operator_would_do_about_them() {
        let cases = [
            (1045, Counter::InlaysqlErrorsAccessDenied),
            (1142, Counter::InlaysqlErrorsAccessDenied),
            (1064, Counter::InlaysqlErrorsSyntax),
            (1235, Counter::InlaysqlErrorsUnsupported),
            (1062, Counter::InlaysqlErrorsConstraint),
            (1213, Counter::InlaysqlErrorsConflict),
            (3024, Counter::InlaysqlErrorsTimeout),
            (1317, Counter::InlaysqlErrorsInterrupted),
            (1146, Counter::InlaysqlErrorsNoSuchObject),
            (1105, Counter::InlaysqlErrorsOther),
        ];
        for (code, expected) in cases {
            let error = MysqlError::new(code, "HY000", "");
            assert_eq!(Counter::for_error(&error), expected, "for code {code}");
        }
    }

    /// The session/global split, which is the part that would be easiest to
    /// get quietly wrong: a per-session counter answers with this connection's
    /// number under `SHOW STATUS` and the server's under `SHOW GLOBAL STATUS`,
    /// and a global-only one answers with the server's either way.
    #[test]
    fn session_scope_reports_this_connection_and_global_reports_the_server() {
        let session = Metrics::new();
        let global = Metrics::new();
        let registry = Registry::new();

        session.record(Counter::Questions);
        for _ in 0..7 {
            global.record(Counter::Questions);
        }
        global.record(Counter::Connections);

        let read = |scope, name: &str| -> String {
            status_variables(scope, &session, &global, &registry, None)
                .into_iter()
                .find(|(variable, _)| variable == name)
                .map(|(_, value)| value)
                .unwrap_or_else(|| panic!("{name} is not reported"))
        };

        assert_eq!(read(Scope::Session, "Questions"), "1");
        assert_eq!(read(Scope::Global, "Questions"), "7");
        // Global-only: the same answer under both, because a per-connection
        // reading of it would not mean anything.
        assert_eq!(read(Scope::Session, "Connections"), "1");
        assert_eq!(read(Scope::Global, "Connections"), "1");
    }

    /// The connection counts are read off the registry rather than kept
    /// alongside it, so there is no second copy to fall out of step.
    #[test]
    fn the_thread_counts_come_from_the_registry_the_process_list_reads() {
        use crate::control::{Control, Doing};
        use std::sync::Arc;

        let registry = Registry::new();
        let session = Metrics::new();
        let global = Metrics::new();
        let read = |name: &str| -> String {
            status_variables(Scope::Global, &session, &global, &registry, None)
                .into_iter()
                .find(|(variable, _)| variable == name)
                .map(|(_, value)| value)
                .unwrap()
        };

        assert_eq!(read("Threads_connected"), "0");
        let control = Arc::new(Control::new(1, "127.0.0.1:1".into(), 0, false));
        registry.register(&control);
        assert_eq!(read("Threads_connected"), "1");
        assert_eq!(read("Threads_running"), "0");
        control.command_began(Doing::Query);
        assert_eq!(read("Threads_running"), "1");
        registry.forget(1);
        assert_eq!(read("Threads_connected"), "0");
    }
}
