//! Accounts and privileges: who may connect, and what each of them may run.
//!
//! Before this existed the server had one user, one password held in memory,
//! and no notion of a privilege — `docs/enterprise-readiness.md` blocker 9.
//! This module is the whole of the replacement: where accounts live, how a
//! password is stored, the statements that manage both, and the single
//! decision every statement is measured against.
//!
//! # Where users live
//!
//! In the database file, as two ordinary tables the engine has no idea are
//! special ([`USER_TABLE`], [`GRANT_TABLE`]). That is decision **D1** applied
//! to authentication exactly as it is applied to dialect: `inlaysql-core`
//! gains nothing — no user concept, no `GRANT` syntax, no enforcement — and
//! everything MySQL-shaped is built in this crate on top of storage the engine
//! already had. One consequence follows immediately and is stated here rather
//! than discovered later: **these privileges guard the wire server and nothing
//! else.** Anything that can open the file — the embedded API, the CLI,
//! `serve --mcp` — bypasses all of it, because the file *is* the credential
//! there. This is the same line SQLite draws, and the same line a MySQL
//! `datadir` draws.
//!
//! Both tables are named with
//! [`RESERVED_TABLE_PREFIX`](inlaysql::RESERVED_TABLE_PREFIX) and are
//! invisible and untouchable through SQL: they are filtered out of
//! `SHOW TABLES`,
//! `information_schema` and `SHOW COLUMNS`, and *any* statement that names one
//! is refused — for a superuser too, since `SELECT * FROM __inlaysql_user`
//! would otherwise hand over every verifier on the machine and
//! `UPDATE __inlaysql_user SET privileges = ...` would be a second, unaudited
//! `GRANT`. They are reached only through the statements below.
//!
//! # What is stored instead of a password
//!
//! Never the password. An account carries the *verifier* each plugin's
//! challenge-response is defined in terms of — `SHA1(SHA1(password))` for
//! `mysql_native_password`, `SHA256(SHA256(password))` for
//! `caching_sha2_password` — and [`crate::auth`] checks a login by running the
//! exchange backwards rather than by knowing the secret.
//!
//! **An account carries both verifiers by default, and that is a deliberate
//! trade rather than an oversight.** A verifier is per-plugin and neither can
//! be derived from the other, so one verifier means one plugin, and a client
//! that only speaks the other cannot log in at all — which would break every
//! older PDO and `mysql` CLI the moment an account was created. `IDENTIFIED
//! WITH <plugin> BY <password>` stores one and only one, for an operator who
//! would rather have that. What both cost is the same thing: they are
//! unsalted and only two fast hashes deep, because the plugins' own
//! definitions fix them, so a stolen database file is a stolen password list
//! against an offline attack. The alternative — MySQL's salted, iterated
//! `$A$005$` digest — cannot answer a fast scramble at all, so it would force
//! every connection to send its password in cleartext over a link this server
//! does not encrypt. Given the choice between weakening the file at rest and
//! weakening the wire, this weakens the file, and says so.
//!
//! # The privilege model, and what is left out
//!
//! Seven privileges — `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `CREATE`,
//! `DROP`, `ALTER` — each grantable globally (`ON *.*`) or on one table
//! (`ON db.tbl`), plus `GRANT OPTION`, which exists only in the one
//! combination that makes a superuser. Left out, and refused rather than
//! silently ignored wherever it can be written down:
//!
//! * **Column-level and row-level privileges.** `GRANT SELECT (email) ON ...`
//!   is refused; nothing here can filter a projection.
//! * **Host-based access control.** `'app'@'%'` is accepted because `%` means
//!   "any host", which is what this server implements. Any other host is
//!   refused, rather than accepted and ignored — which would turn
//!   `'app'@'localhost'` into an account reachable from anywhere.
//! * **Roles, `PROXY`, `SHOW GRANTS ... USING`, routine and tablespace
//!   privileges**, and MySQL's administrative privileges (`RELOAD`,
//!   `PROCESS`, `SHUTDOWN`, …) — there is nothing behind any of them here.
//! * **Partial delegation.** `WITH GRANT OPTION` on anything narrower than
//!   `ALL PRIVILEGES ON *.*` is refused: one level of delegation is
//!   implemented, and pretending to a second would let an account hand out
//!   rights this server would not then enforce the boundary of.
//! * **Schema-name enforcement.** One file is one schema, so `ON app.*` is a
//!   global grant. A qualifier that is neither `*`, the server's own schema
//!   name, nor the connection's current database is refused rather than
//!   quietly treated as this one.
//! * **Hiding metadata.** Any authenticated account can run `SHOW TABLES` and
//!   `DESCRIBE`. Real MySQL shows only what you hold a privilege on; this does
//!   not, so table and column *names* are readable by every account even where
//!   their contents are not.

use std::collections::BTreeMap;
use std::fmt;

use inlaysql::{Catalog, Database, ResultSet, TableAccess, Value};

use crate::auth;
use crate::errors::{from_engine, MysqlError};
use crate::session::Session;
use crate::shim::DEFAULT_SCHEMA;
use crate::sqltext::{first_word, split_top_level, starts_with_keyword, strip_keyword};

/// The prefix that marks a table as belonging to a layer above the engine.
///
/// One rule, declared in the crate every layer depends on, rather than a list
/// of names to keep in step: any table whose name starts with this is hidden
/// from every metadata answer here and refused to every statement — and the
/// MCP server applies the same rule, so an agent pointed at the same file
/// cannot read the account store either.
pub use inlaysql::is_reserved_table_name as is_reserved;

/// One row per account.
const USER_TABLE: &str = "__inlaysql_user";
/// One row per (account, table) grant. Global grants live on the account row.
const GRANT_TABLE: &str = "__inlaysql_grant";

// =====================================================================
// privileges
// =====================================================================

/// A set of privileges, held globally or on one table.
///
/// A bitmask rather than a set of rows per privilege: a grant is read on every
/// statement (see [`enforce`]), and the whole of a check has to be an integer
/// comparison for that to be affordable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Privileges(u32);

impl Privileges {
    /// No privileges at all — MySQL's `USAGE`.
    pub const NONE: Self = Self(0);
    /// Read rows.
    pub const SELECT: Self = Self(1 << 0);
    /// Add rows.
    pub const INSERT: Self = Self(1 << 1);
    /// Rewrite rows.
    pub const UPDATE: Self = Self(1 << 2);
    /// Remove rows.
    pub const DELETE: Self = Self(1 << 3);
    /// Create a table, or an index over one.
    pub const CREATE: Self = Self(1 << 4);
    /// Drop a table or an index.
    pub const DROP: Self = Self(1 << 5);
    /// Change a table's definition.
    pub const ALTER: Self = Self(1 << 6);
    /// Administer accounts. Only ever held globally, and only alongside
    /// [`Privileges::ALL`] — see the module docs on partial delegation.
    pub const GRANT_OPTION: Self = Self(1 << 7);
    /// Everything except [`Privileges::GRANT_OPTION`], which is what MySQL's
    /// own `ALL PRIVILEGES` means too.
    pub const ALL: Self = Self(0b0111_1111);

    /// Every privilege in `other` is in `self`. `NONE` is contained by
    /// everything, which is what makes `USAGE` a grant of nothing rather than
    /// a refusal.
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The union.
    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// `self` with everything in `other` removed.
    pub fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether this is the empty set.
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The stored representation.
    fn bits(self) -> i64 {
        self.0 as i64
    }

    /// A stored mask, with anything this build does not define discarded.
    ///
    /// A file written by a newer build could carry a bit this one has no
    /// meaning for; keeping it would grant an unknown privilege, so it is
    /// dropped. Erring towards less access is the only safe direction.
    fn from_bits(bits: i64) -> Self {
        Self((bits as u32) & (Self::ALL.0 | Self::GRANT_OPTION.0))
    }

    /// The names MySQL spells these with, in MySQL's own `SHOW GRANTS` order.
    fn names(self) -> Vec<&'static str> {
        const ORDER: &[(Privileges, &str)] = &[
            (Privileges::SELECT, "SELECT"),
            (Privileges::INSERT, "INSERT"),
            (Privileges::UPDATE, "UPDATE"),
            (Privileges::DELETE, "DELETE"),
            (Privileges::CREATE, "CREATE"),
            (Privileges::DROP, "DROP"),
            (Privileges::ALTER, "ALTER"),
        ];
        ORDER
            .iter()
            .filter(|(bit, _)| self.contains(*bit))
            .map(|(_, name)| *name)
            .collect()
    }

    /// How a denial names the one privilege that was missing.
    fn name(self) -> String {
        match self.names().as_slice() {
            [] if self.contains(Privileges::GRANT_OPTION) => "GRANT OPTION".to_string(),
            [] => "USAGE".to_string(),
            names => names.join(", "),
        }
    }
}

/// One thing a statement needs before it may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    /// The table the privilege has to cover, lowercased; `None` when only a
    /// *global* grant will do, because this server could not attribute the
    /// access to a single table (`DROP INDEX` naming an index the catalog has
    /// no record of, for instance). Requiring the global grant there is the
    /// default-deny direction: a table grant can never satisfy it.
    pub table: Option<String>,
    /// The privilege, always exactly one bit.
    pub privilege: Privileges,
}

/// What one statement needs before it may run.
///
/// Produced by exactly two functions — [`shim_requirement`] for the statements
/// this server answers itself, [`plan_requirement`] for the ones the engine
/// runs — and consumed by exactly one, [`enforce`]. That shape is the point:
/// there is one place that decides, and a statement that reaches neither
/// producer has no path to the engine at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    /// Nothing beyond a live account: session state, transaction control and
    /// metadata. See the module docs for what that deliberately does not hide.
    Authenticated,
    /// Superuser only — the account statements below.
    Administrative,
    /// Every one of these, or a global grant that covers it.
    Needs(Vec<Need>),
    /// **Default deny.** Nothing here could work out what this statement
    /// needs, so it is refused rather than allowed. The string says what was
    /// not understood, because a refusal a caller cannot act on is its own
    /// kind of failure.
    Undetermined(String),
}

// =====================================================================
// accounts
// =====================================================================

/// One account, as stored.
#[derive(Clone)]
pub struct Account {
    /// The login name, case-sensitively as MySQL treats it.
    pub name: String,
    /// The `mysql_native_password` verifier, or `None` if this account has
    /// none and cannot authenticate under that plugin.
    native: Option<String>,
    /// The `caching_sha2_password` verifier, likewise.
    sha2: Option<String>,
    /// Privileges held on every table.
    global: Privileges,
}

/// Written by hand rather than derived, for the same reason
/// `HandshakeResponse`'s is: a verifier is not the password, but it is what an
/// offline attack starts from, and it has no business in a log line or a panic
/// message.
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("name", &self.name)
            .field("native", &self.native.as_ref().map(|_| "<redacted>"))
            .field("sha2", &self.sha2.as_ref().map(|_| "<redacted>"))
            .field("global", &self.global)
            .finish()
    }
}

impl Account {
    /// Whether this account may administer accounts.
    ///
    /// Exactly `GRANT ALL PRIVILEGES ON *.* ... WITH GRANT OPTION`, and
    /// nothing narrower — see the module docs on partial delegation.
    pub fn is_superuser(&self) -> bool {
        self.global
            .contains(Privileges::ALL.with(Privileges::GRANT_OPTION))
    }

    /// The verifier for `plugin`, if this account has one.
    fn verifier(&self, plugin: &str) -> Option<&str> {
        match plugin {
            auth::NATIVE_PASSWORD => self.native.as_deref(),
            auth::CACHING_SHA2_PASSWORD => self.sha2.as_deref(),
            _ => None,
        }
    }

    /// Whether this account can complete `plugin`'s exchange at all.
    pub fn speaks(&self, plugin: &str) -> bool {
        self.verifier(plugin).is_some()
    }

    /// Which plugin to ask a client to switch to, when the one it named is not
    /// one this account can complete.
    ///
    /// `mysql_native_password` first where the account has it: it is the one
    /// every driver can complete, and preferring it keeps a client that named
    /// some third plugin on the path it took before accounts existed.
    pub fn preferred_plugin(&self) -> Option<&'static str> {
        if self.native.is_some() {
            Some(auth::NATIVE_PASSWORD)
        } else if self.sha2.is_some() {
            Some(auth::CACHING_SHA2_PASSWORD)
        } else {
            None
        }
    }

    /// Check a `mysql_native_password` token.
    pub fn verify_native(&self, challenge: &[u8], response: &[u8]) -> bool {
        self.native
            .as_deref()
            .is_some_and(|verifier| auth::verify_native(verifier, challenge, response))
    }

    /// Check a `caching_sha2_password` fast-authentication token.
    pub fn verify_caching_sha2(&self, scramble: &[u8], response: &[u8]) -> bool {
        self.sha2
            .as_deref()
            .is_some_and(|verifier| auth::verify_caching_sha2(verifier, scramble, response))
    }

    /// Check the cleartext password a full authentication sends.
    ///
    /// Dispatches on what is actually stored rather than on what the caller
    /// expects: a strong verifier is checked with PBKDF2 and a scramble
    /// verifier by hashing, and neither can be checked by the other's routine.
    /// A caller that reached here without an encrypted connection has already
    /// been refused — see `Account::requires_tls`.
    pub fn verify_caching_sha2_cleartext(&self, payload: &[u8]) -> bool {
        self.sha2.as_deref().is_some_and(|verifier| {
            if auth::is_strong(verifier) {
                auth::verify_strong_cleartext(verifier, payload)
            } else {
                auth::verify_caching_sha2_cleartext(verifier, payload)
            }
        })
    }

    /// Whether this account can only authenticate over an encrypted link.
    ///
    /// True exactly when its stored verifier is the salted, iterated form: no
    /// scramble can be computed from one, so the only way to check it is
    /// against a cleartext password, and a cleartext password may not cross an
    /// unencrypted connection. The login path turns this into a refusal that
    /// says so, rather than a password failure that does not.
    pub fn requires_tls(&self) -> bool {
        self.sha2.as_deref().is_some_and(auth::is_strong)
    }

    /// An account that exists only to be refused: the stand-in used when a
    /// login names a user this server has never heard of.
    ///
    /// It carries a verifier no password produces (`native_verifier` never
    /// returns a bare `*`), so the exchange runs to exactly the same length as
    /// a real one and fails at exactly the same step. Without it, an unknown
    /// user would be rejected before the token was even read, and the round
    /// trips alone would enumerate accounts.
    pub fn unknown(name: &str) -> Self {
        Self {
            name: name.to_string(),
            native: Some("*".to_string()),
            sha2: Some(String::new()),
            global: Privileges::NONE,
        }
    }
}

// =====================================================================
// the store
// =====================================================================

/// The single credential a server is started with, reduced to verifiers.
///
/// This is what `--user`/`--password` become, and it is the *whole* account
/// model until somebody runs their first `CREATE USER` — see [`install`] for
/// why the store is created then rather than at startup. The plaintext
/// password is hashed in [`Bootstrap::new`] and dropped there: once
/// `Server::bind` has returned, no part of this process holds one.
#[derive(Clone)]
pub struct Bootstrap {
    /// The account name from `--user`.
    pub user: String,
    native: String,
    sha2: String,
}

/// Redacted by hand, like [`Account`]'s: these are verifiers, and a process
/// that prints one has printed what an offline attack starts from.
impl fmt::Debug for Bootstrap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Bootstrap")
            .field("user", &self.user)
            .field("verifiers", &"<redacted>")
            .finish()
    }
}

impl Bootstrap {
    /// Hash `password` for both plugins and forget it.
    pub fn new(user: &str, password: &str) -> Self {
        Self {
            user: user.to_string(),
            native: auth::native_verifier(password),
            sha2: auth::sha2_verifier(password),
        }
    }

    /// The account this credential stands for: a superuser, because that is
    /// exactly what the single `--password` user has always been.
    fn account(&self) -> Account {
        Account {
            name: self.user.clone(),
            native: Some(self.native.clone()),
            sha2: Some(self.sha2.clone()),
            global: Privileges::ALL.with(Privileges::GRANT_OPTION),
        }
    }
}

/// What [`install`] found, and therefore whether `--user`/`--password` mean
/// anything at all on this database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// No account store yet: `--user`/`--password` *are* the account model,
    /// exactly as they were before accounts existed, and nothing has been
    /// written to the database.
    Bootstrap {
        /// The account those flags stand for.
        user: String,
        /// Whether it has an empty password.
        empty_password: bool,
    },
    /// The database has accounts. `--user`/`--password` were **not**
    /// consulted: the file is the authority once it has accounts, or a
    /// forgotten flag on a restart would silently reinstate a rotated
    /// password.
    Existing,
    /// `--reset-superuser` was given on a database that has accounts: the
    /// named one's password was set from the flags and it was made a
    /// superuser, being created if it had been dropped.
    Reset {
        /// The account reset.
        user: String,
        /// Whether it was reset to an empty password.
        empty_password: bool,
    },
}

/// Look at the account store, and reset it if asked to.
///
/// Called once, from `Server::bind`, on a handle nothing else is using.
///
/// # Migration: nothing happens until somebody asks for it
///
/// **A database with no account store is left completely alone.** It keeps
/// behaving exactly as it did before this existed: `--user`/`--password` are
/// the one credential, that credential is a superuser, and not a byte is
/// written. That is not only the least disruptive upgrade — it is the only one
/// that does not change what the database *contains*. The store is two
/// ordinary tables, and every row in this engine draws its row id from one
/// counter shared by every table, so seeding an account at startup would have
/// shifted the first application row of every fresh database from id 1 to id
/// 2. Creating the store on the first `CREATE USER`/`GRANT` confines that to
/// databases whose operator asked for accounts, and `docs/server.md` says so
/// under Divergences.
///
/// **A database that has a store ignores both flags**, because the
/// alternative — a flag silently overwriting a stored password — turns a
/// forgotten line in a service file into a way back into a database whose
/// password was rotated. `--reset-superuser` is the deliberate escape from
/// that, and it needs write access to the file, which is already full access.
pub fn install(
    db: &mut Database,
    user: &str,
    password: &str,
    reset: bool,
    policy: PasswordPolicy,
) -> Result<Installed, MysqlError> {
    if db.catalog().table(USER_TABLE).is_none() {
        // Nothing to reset, and nothing worth creating in order to reset it:
        // with no store, the flags already are the credential.
        return Ok(Installed::Bootstrap {
            user: user.to_string(),
            empty_password: password.is_empty(),
        });
    }
    if !reset {
        return Ok(Installed::Existing);
    }

    let superuser = Privileges::ALL.with(Privileges::GRANT_OPTION);
    if stored_account(db, user)?.is_some() {
        set_password(db, user, password, None, policy)?;
        set_global(db, user, superuser)?;
    } else {
        insert_account(db, user, password, None, superuser, policy)?;
    }
    Ok(Installed::Reset {
        user: user.to_string(),
        empty_password: password.is_empty(),
    })
}

/// Create the account store, and move the bootstrap credential into it.
///
/// Called by every account statement that *writes*, and by nothing else — a
/// `SHOW GRANTS` must not create tables, and on a read-only handle it could
/// not anyway. From the moment this runs the file is the authority and
/// `--user`/`--password` are never consulted again, which is why the bootstrap
/// account is written in first and as a superuser: without it, the operator's
/// own credential would stop working halfway through their first
/// `CREATE USER`.
fn ensure_store(db: &mut Database, bootstrap: &Bootstrap) -> Result<(), MysqlError> {
    if db.catalog().table(USER_TABLE).is_some() {
        return Ok(());
    }
    run_sql(
        db,
        &format!(
            "CREATE TABLE {USER_TABLE} (id INTEGER PRIMARY KEY, name TEXT, \
             native_auth TEXT, sha2_auth TEXT, privileges INTEGER)"
        ),
        &[],
    )?;
    // `USING BTREE` is not decoration: `CREATE INDEX` on a `TEXT` column means
    // the BM25 index in this engine's dialect, and a full-text index can
    // neither answer `WHERE name = ?` nor enforce uniqueness.
    run_sql(
        db,
        &format!("CREATE UNIQUE INDEX {USER_TABLE}_name ON {USER_TABLE} (name) USING BTREE"),
        &[],
    )?;
    run_sql(
        db,
        &format!(
            "CREATE TABLE {GRANT_TABLE} (id INTEGER PRIMARY KEY, account TEXT, \
             target TEXT, privileges INTEGER)"
        ),
        &[],
    )?;
    run_sql(
        db,
        &format!("CREATE INDEX {GRANT_TABLE}_account ON {GRANT_TABLE} (account) USING BTREE"),
        &[],
    )?;
    let seed = bootstrap.account();
    run_sql(
        db,
        &format!(
            "INSERT INTO {USER_TABLE} (name, native_auth, sha2_auth, privileges) \
             VALUES (?, ?, ?, ?)"
        ),
        &[
            text(&seed.name),
            optional_text(seed.native),
            optional_text(seed.sha2),
            Value::Integer(seed.global.bits()),
        ],
    )
}

/// Read one account, or `None` if there is no such name.
///
/// Read afresh on **every** statement rather than cached on the connection.
/// That costs one indexed lookup per statement, and it is what buys the
/// property the docs promise: a `REVOKE` or a `DROP USER` takes effect on the
/// offending session's *next statement*, not at its next reconnection. A cache
/// invalidated by anything less than a re-read would be a window in which a
/// revoked privilege still works.
///
/// With no store, the only account is the bootstrap credential — which is the
/// pre-accounts behaviour exactly: one name, one password, every privilege.
pub fn account(
    db: &mut Database,
    name: &str,
    bootstrap: &Bootstrap,
) -> Result<Option<Account>, MysqlError> {
    // This handle's catalog is whatever it last read, so "no store" is not
    // conclusive on its own — another connection may have created one since.
    // The probe refreshes committed state before it parses, so a second look
    // after it settles the question.
    if db.catalog().table(USER_TABLE).is_none() {
        let refreshed = stored_account(db, name);
        if db.catalog().table(USER_TABLE).is_none() {
            return Ok((name == bootstrap.user).then(|| bootstrap.account()));
        }
        return refreshed;
    }
    stored_account(db, name)
}

/// [`account`], straight out of the store, with no bootstrap fallback.
fn stored_account(db: &mut Database, name: &str) -> Result<Option<Account>, MysqlError> {
    let rows = match query(
        db,
        &format!("SELECT native_auth, sha2_auth, privileges FROM {USER_TABLE} WHERE name = ?"),
        &[text(name)],
    ) {
        Ok(rows) => rows,
        // There is no store to read. Answered as "no such account" rather than
        // as an error — and never as an account, since the only caller that
        // may treat an absent store as the bootstrap credential is [`account`],
        // which checks for that itself.
        Err(_) if db.catalog().table(USER_TABLE).is_none() => return Ok(None),
        Err(error) => return Err(error),
    };
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    Ok(Some(Account {
        name: name.to_string(),
        native: row.first().and_then(as_text),
        sha2: row.get(1).and_then(as_text),
        global: Privileges::from_bits(row.get(2).and_then(as_int).unwrap_or(0)),
    }))
}

/// The privileges `name` holds on `table` specifically.
fn table_grant(db: &mut Database, name: &str, table: &str) -> Result<Privileges, MysqlError> {
    // With no store there are no table grants to hold, and the only account
    // that exists holds everything globally, so it never reaches here.
    if db.catalog().table(GRANT_TABLE).is_none() {
        return Ok(Privileges::NONE);
    }
    let rows = query(
        db,
        &format!("SELECT privileges FROM {GRANT_TABLE} WHERE account = ? AND target = ?"),
        &[text(name), text(&table.to_ascii_lowercase())],
    )?;
    Ok(rows
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(as_int)
        .map(Privileges::from_bits)
        .unwrap_or(Privileges::NONE))
}

/// Every table grant `name` holds, in table order.
fn table_grants(db: &mut Database, name: &str) -> Result<Vec<(String, Privileges)>, MysqlError> {
    // No store means no per-table grants — the bootstrap credential holds
    // everything globally, so there is nothing per-table to list.
    if db.catalog().table(GRANT_TABLE).is_none() {
        return Ok(Vec::new());
    }
    let rows = query(
        db,
        &format!("SELECT target, privileges FROM {GRANT_TABLE} WHERE account = ?"),
        &[text(name)],
    )?;
    let mut out: BTreeMap<String, Privileges> = BTreeMap::new();
    for row in &rows.rows {
        let Some(target) = row.first().and_then(as_text) else {
            continue;
        };
        let bits = Privileges::from_bits(row.get(1).and_then(as_int).unwrap_or(0));
        out.insert(target, bits);
    }
    Ok(out.into_iter().collect())
}

/// Every account name and its global mask, for the "last superuser" check and
/// for `SHOW GRANTS` on somebody else.
fn all_accounts(db: &mut Database) -> Result<Vec<(String, Privileges)>, MysqlError> {
    let rows = query(
        db,
        &format!("SELECT name, privileges FROM {USER_TABLE}"),
        &[],
    )?;
    Ok(rows
        .rows
        .iter()
        .filter_map(|row| {
            let name = row.first().and_then(as_text)?;
            let bits = Privileges::from_bits(row.get(1).and_then(as_int).unwrap_or(0));
            Some((name, bits))
        })
        .collect())
}

fn insert_account(
    db: &mut Database,
    name: &str,
    password: &str,
    plugin: Option<&str>,
    global: Privileges,
    policy: PasswordPolicy,
) -> Result<(), MysqlError> {
    let (native, sha2) = verifiers(password, plugin, policy);
    run_sql(
        db,
        &format!(
            "INSERT INTO {USER_TABLE} (name, native_auth, sha2_auth, privileges) \
             VALUES (?, ?, ?, ?)"
        ),
        &[
            text(name),
            optional_text(native),
            optional_text(sha2),
            Value::Integer(global.bits()),
        ],
    )
}

fn set_password(
    db: &mut Database,
    name: &str,
    password: &str,
    plugin: Option<&str>,
    policy: PasswordPolicy,
) -> Result<(), MysqlError> {
    let (native, sha2) = verifiers(password, plugin, policy);
    run_sql(
        db,
        &format!("UPDATE {USER_TABLE} SET native_auth = ?, sha2_auth = ? WHERE name = ?"),
        &[optional_text(native), optional_text(sha2), text(name)],
    )
}

fn set_global(db: &mut Database, name: &str, global: Privileges) -> Result<(), MysqlError> {
    run_sql(
        db,
        &format!("UPDATE {USER_TABLE} SET privileges = ? WHERE name = ?"),
        &[Value::Integer(global.bits()), text(name)],
    )
}

fn set_table_grant(
    db: &mut Database,
    name: &str,
    target: &str,
    privileges: Privileges,
) -> Result<(), MysqlError> {
    let target = target.to_ascii_lowercase();
    let existing = query(
        db,
        &format!("SELECT id FROM {GRANT_TABLE} WHERE account = ? AND target = ?"),
        &[text(name), text(&target)],
    )?;
    let present = existing.rows.first().and_then(|row| row.first()).cloned();

    if privileges.is_empty() {
        // A row recording nothing is a row `SHOW GRANTS` would print as a
        // grant of `USAGE` on one table, which MySQL does not do either.
        if present.is_some() {
            run_sql(
                db,
                &format!("DELETE FROM {GRANT_TABLE} WHERE account = ? AND target = ?"),
                &[text(name), text(&target)],
            )?;
        }
        return Ok(());
    }
    match present {
        Some(id) => run_sql(
            db,
            &format!("UPDATE {GRANT_TABLE} SET privileges = ? WHERE id = ?"),
            &[Value::Integer(privileges.bits()), id],
        ),
        None => run_sql(
            db,
            &format!("INSERT INTO {GRANT_TABLE} (account, target, privileges) VALUES (?, ?, ?)"),
            &[text(name), text(&target), Value::Integer(privileges.bits())],
        ),
    }
}

/// The verifiers to store for `password` under `plugin`.
///
/// `None` for the plugin means both, which is what keeps every client that
/// worked before an account existed working after one does — see the module
/// docs for the cost of that default and how to opt out of it.
fn verifiers(
    password: &str,
    plugin: Option<&str>,
    policy: PasswordPolicy,
) -> (Option<String>, Option<String>) {
    // A strong account stores one thing and it answers no scramble: keeping a
    // native verifier beside it would leave the weak form on disk as a
    // complete bypass of the strong one, which is worse than not offering the
    // strong one at all. An explicit `IDENTIFIED WITH mysql_native_password`
    // still wins, because that is an operator saying, in as many words, that
    // this account must work with a client that speaks only the old plugin —
    // and a silent downgrade of an explicit request is its own bug.
    if policy == PasswordPolicy::Strong && plugin != Some(auth::NATIVE_PASSWORD) {
        return (None, Some(auth::strong_verifier(password)));
    }
    match plugin {
        Some(auth::NATIVE_PASSWORD) => (Some(auth::native_verifier(password)), None),
        Some(auth::CACHING_SHA2_PASSWORD) => (None, Some(auth::sha2_verifier(password))),
        _ => (
            Some(auth::native_verifier(password)),
            Some(auth::sha2_verifier(password)),
        ),
    }
}

/// How a password is stored when an account is created or rotated.
///
/// This is a *server* policy rather than a per-account one: an operator
/// decides once whether this database's accounts are the fast-scramble kind
/// every MySQL client can log into over any link, or the salted, iterated kind
/// that survives the file being stolen and needs TLS to log in at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasswordPolicy {
    /// Unsalted, two fast hashes deep, per the plugins' own definitions. The
    /// default, because it is what every existing account already is and what
    /// every client can complete without an encrypted connection.
    #[default]
    Scramble,
    /// Salted PBKDF2. Cannot answer a scramble, so a client must complete full
    /// authentication, which this server only allows over TLS.
    Strong,
}

// ------------------------------------------------------------ SQL plumbing

fn query(db: &mut Database, sql: &str, params: &[Value]) -> Result<ResultSet, MysqlError> {
    db.query(sql, params).map_err(|error| from_engine(&error))
}

fn run_sql(db: &mut Database, sql: &str, params: &[Value]) -> Result<(), MysqlError> {
    db.execute(sql, params)
        .map(|_| ())
        .map_err(|error| from_engine(&error))
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string().into())
}

fn optional_text(value: Option<String>) -> Value {
    match value {
        // `NULL` means "this account has no verifier for that plugin", which
        // is a different thing from the empty string, which is
        // `mysql_native_password`'s own spelling for "no password".
        None => Value::Null,
        Some(value) => Value::Text(value.into()),
    }
}

fn as_text(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(n) => Some(*n),
        _ => None,
    }
}

// =====================================================================
// deciding what a statement needs
// =====================================================================

/// What a statement the *shim* answers needs.
///
/// An allow-list by leading keyword, with everything else
/// [`Requirement::Undetermined`]. The list is exactly the set
/// [`crate::shim::handles`] claims, so a statement can never be classified as
/// the shim's here and reach the engine there, or the other way round.
pub fn shim_requirement(sql: &str, session: &Session, catalog: &Catalog) -> Requirement {
    // Normalised first, exactly as `crate::shim::intercept` normalises before
    // it dispatches. **This line is a privilege check, not tidiness.** Both
    // sides have to read the same text, and without it they did not: a
    // statement that opens with `/* a comment */` has no leading keyword at
    // all until the comment is stripped, so `first_word` answered `""`, which
    // is the "empty statement" arm below — `Authenticated`. `intercept` then
    // stripped the comment, found `CREATE USER`, and ran it. Any account with
    // a login could have made itself a superuser by putting a comment in front
    // of the statement. Pinned over the wire by
    // `a_non_superuser_cannot_administer_accounts`, which fails against the
    // version of this function without this line.
    let sql = &crate::sqltext::normalize(sql);
    if let Some(parsed) = parse(sql, session) {
        return match parsed {
            // A malformed or unsupported account statement is refused at the
            // point it runs, with its own message; it needs the same rights as
            // the well-formed one it was trying to be.
            Err(_) => Requirement::Administrative,
            Ok(AclStatement::ShowGrants { user }) => match &user {
                Some(name) if name != &session.user => Requirement::Administrative,
                _ => Requirement::Authenticated,
            },
            // Changing your own password is not an administrative act, and
            // requiring a superuser for it would mean nobody could rotate
            // their own credential. Exactly one named account, and it has to
            // be this session's.
            Ok(AclStatement::AlterUser { users, .. })
                if users.len() == 1 && users[0].name == session.user =>
            {
                Requirement::Authenticated
            }
            Ok(_) => Requirement::Administrative,
        };
    }

    match first_word(sql).as_str() {
        "" => Requirement::Authenticated,
        "SET" | "SHOW" | "USE" | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT"
        | "RELEASE" | "DO" | "DESCRIBE" | "DESC" | "SELECT" => Requirement::Authenticated,
        // `KILL` needs a login and nothing more *here*, because the check it
        // actually needs cannot be written as a privilege: whether one
        // connection may stop another depends on who owns the target, which
        // this function cannot see. It is made in `Registry::kill` instead —
        // your own connections always, anybody's with the superuser, and
        // `ER_KILL_DENIED_ERROR` otherwise — so answering `Authenticated` here
        // is deferral to a stricter check, not an absence of one.
        "KILL" => Requirement::Authenticated,
        // MySQL's own requirement for `OPTIMIZE TABLE`: SELECT and INSERT on
        // every table named. Attributable per table because the statement
        // names them, and parsed by the same function that will run it — a
        // second parser here is how the set of tables checked stops being the
        // set of tables touched.
        "OPTIMIZE" => match crate::shim::parse_optimize(sql, catalog) {
            Ok(tables) => Requirement::Needs(
                tables
                    .into_iter()
                    .flat_map(|table| {
                        [Privileges::SELECT, Privileges::INSERT].map(move |privilege| Need {
                            table: Some(table.to_ascii_lowercase()),
                            privilege,
                        })
                    })
                    .collect(),
            ),
            // Refused at the point it runs, with its own message. Until then
            // it is a statement nothing here could attribute, which is the
            // default-deny case this variant exists for.
            Err(error) => Requirement::Undetermined(error.message),
        },
        other => Requirement::Undetermined(format!(
            "`{other}` is answered by this server rather than by the engine, and there is no \
             privilege defined for it"
        )),
    }
}

/// What a statement the *engine* runs needs, read off its resolved plan.
///
/// The plan, never the text: `SELECT (SELECT secret FROM vault) FROM public`
/// names two tables and only the plan lists both. See
/// [`inlaysql::Statement::table_access`].
pub fn plan_requirement(access: &[(&str, TableAccess)], catalog: &Catalog) -> Requirement {
    let mut needs = Vec::new();
    for (name, what) in access {
        let privilege = match what {
            TableAccess::Read => Privileges::SELECT,
            TableAccess::Insert => Privileges::INSERT,
            TableAccess::Update => Privileges::UPDATE,
            TableAccess::Delete => Privileges::DELETE,
            TableAccess::Create => Privileges::CREATE,
            TableAccess::Drop => Privileges::DROP,
            TableAccess::Alter => Privileges::ALTER,
            // `DROP INDEX` names an index, not a table. Resolving it through
            // the catalog is what makes a per-table `DROP` grant cover it;
            // when the name matches nothing, there is no table to attribute
            // it to, so only a global `DROP` will do. That is the
            // default-deny direction — a table grant cannot satisfy it — and
            // the statement was going to fail with "no such index" anyway.
            TableAccess::DropIndex => {
                let table = catalog
                    .indexes()
                    .find(|index| index.name.eq_ignore_ascii_case(name))
                    .map(|index| index.table.to_ascii_lowercase());
                needs.push(Need {
                    table,
                    privilege: Privileges::DROP,
                });
                continue;
            }
        };
        needs.push(Need {
            table: Some(name.to_ascii_lowercase()),
            privilege,
        });
    }
    Requirement::Needs(needs)
}

/// Decide whether `account` may do what `requirement` asks.
///
/// **The one place a statement is allowed or refused.** Everything above
/// produces a [`Requirement`]; nothing else grants access.
pub fn enforce(
    db: &mut Database,
    account: &Account,
    requirement: &Requirement,
) -> Result<(), MysqlError> {
    // Checked before the superuser shortcut, and so before anything else: this
    // server's own tables are off limits to every account. A superuser
    // administers accounts with the statements below, not by writing rows
    // into the table behind them.
    if let Requirement::Needs(needs) = requirement {
        for need in needs {
            if let Some(table) = &need.table {
                if is_reserved(table) {
                    return Err(reserved_table_denied(&account.name, table));
                }
            }
        }
    }

    if account.is_superuser() {
        return Ok(());
    }

    match requirement {
        Requirement::Authenticated => Ok(()),
        Requirement::Administrative => Err(MysqlError::new(
            1227,
            "42000",
            format!(
                "Access denied; you need GRANT ALL PRIVILEGES ON *.* WITH GRANT OPTION \
                 (this server's superuser) for this operation, and `{}` does not have it",
                account.name
            ),
        )),
        Requirement::Undetermined(why) => Err(MysqlError::new(
            1227,
            "42000",
            format!(
                "Access denied for `{}`: this server could not determine which privilege \
                 this statement needs, so it is refused rather than allowed — {why}",
                account.name
            ),
        )),
        Requirement::Needs(needs) => {
            for need in needs {
                if account.global.contains(need.privilege) {
                    continue;
                }
                let Some(table) = &need.table else {
                    return Err(global_denied(&account.name, need.privilege));
                };
                if table_grant(db, &account.name, table)?.contains(need.privilege) {
                    continue;
                }
                return Err(table_denied(&account.name, need.privilege, table));
            }
            Ok(())
        }
    }
}

/// `ER_TABLEACCESS_DENIED_ERROR`, in MySQL's own wording — a client's error
/// handling recognises the shape.
fn table_denied(user: &str, privilege: Privileges, table: &str) -> MysqlError {
    MysqlError::new(
        1142,
        "42000",
        format!(
            "{} command denied to user '{user}'@'%' for table '{table}'",
            privilege.name()
        ),
    )
}

fn global_denied(user: &str, privilege: Privileges) -> MysqlError {
    MysqlError::new(
        1227,
        "42000",
        format!(
            "Access denied; you need the {} privilege on *.* for this operation, and \
             '{user}'@'%' does not have it",
            privilege.name()
        ),
    )
}

fn reserved_table_denied(user: &str, table: &str) -> MysqlError {
    MysqlError::new(
        1142,
        "42000",
        format!(
            "command denied to user '{user}'@'%' for table '{table}': this is InlaySQL's own \
             account store and no statement may read or write it, superuser included — use \
             CREATE USER / ALTER USER / DROP USER / GRANT / REVOKE / SHOW GRANTS"
        ),
    )
}

/// The error a session gets once its own account has been dropped.
pub fn account_gone(user: &str) -> MysqlError {
    MysqlError::new(
        1045,
        "28000",
        format!("Access denied for user '{user}'@'%': the account no longer exists"),
    )
}

// =====================================================================
// the account statements
// =====================================================================

/// An account named in a `CREATE USER` / `ALTER USER`, with its password
/// already reduced to verifiers.
#[derive(Clone, PartialEq, Eq)]
pub struct NewUser {
    /// The login name.
    pub name: String,
    /// The plugin the operator pinned this account to, if any.
    plugin: Option<&'static str>,
    /// The password, kept only as long as it takes to hash it — see
    /// [`AclStatement`]'s note on why this is not `Debug`.
    password: String,
}

/// Redacted by hand: the password is in here, and the whole point of this
/// module is that it never reaches a log, a panic message or a `{:?}`.
impl fmt::Debug for NewUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewUser")
            .field("name", &self.name)
            .field("plugin", &self.plugin)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Where a grant applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// `ON *.*` — and `ON db.*`, since one file is one schema.
    Global,
    /// `ON [db.]table`, lowercased.
    Table(String),
}

/// A statement this module owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AclStatement {
    /// `CREATE USER [IF NOT EXISTS] ...`.
    CreateUser {
        /// The accounts to create.
        users: Vec<NewUser>,
        /// `IF NOT EXISTS`: an account that already exists is left alone.
        if_not_exists: bool,
    },
    /// `DROP USER [IF EXISTS] ...`.
    DropUser {
        /// The accounts to remove.
        users: Vec<String>,
        /// `IF EXISTS`: a missing account is not an error.
        if_exists: bool,
    },
    /// `ALTER USER [IF EXISTS] ... IDENTIFIED BY ...`.
    AlterUser {
        /// The accounts to change, and their new passwords.
        users: Vec<NewUser>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `GRANT ... ON ... TO ...`.
    Grant {
        /// What to grant.
        privileges: Privileges,
        /// Where it applies.
        scope: Scope,
        /// To whom.
        users: Vec<String>,
        /// `WITH GRANT OPTION`, which is only accepted alongside
        /// `ALL PRIVILEGES ON *.*`.
        grant_option: bool,
    },
    /// `REVOKE ... ON ... FROM ...`, and `REVOKE ALL PRIVILEGES, GRANT OPTION
    /// FROM ...` (which carries no `ON` and takes everything).
    Revoke {
        /// What to take away.
        privileges: Privileges,
        /// Where from; `None` for the `REVOKE ALL PRIVILEGES, GRANT OPTION`
        /// spelling, which clears global and per-table grants alike.
        scope: Option<Scope>,
        /// From whom.
        users: Vec<String>,
    },
    /// `SHOW GRANTS [FOR user]`.
    ShowGrants {
        /// Whose; `None` means the current session's own account.
        user: Option<String>,
    },
}

/// Whether `sql` is one of the statements this module owns.
///
/// By leading keyword only, and shared with [`crate::shim::handles`] so that
/// classification cannot differ between `COM_STMT_PREPARE` and the execution
/// that follows it.
pub fn looks_like(sql: &str) -> bool {
    match first_word(sql).as_str() {
        "GRANT" | "REVOKE" => true,
        // `CREATE TABLE`, `DROP INDEX` and `ALTER TABLE` share their leading
        // word with an account statement and belong to the engine, so the
        // second word is what decides. Every account form — including
        // `CREATE USER IF NOT EXISTS` and `DROP USER IF EXISTS` — has `USER`
        // there, and nothing else does.
        keyword @ ("CREATE" | "DROP" | "ALTER" | "RENAME") => strip_keyword(sql, keyword)
            .map(|rest| starts_with_keyword(rest, "USER"))
            .unwrap_or(false),
        "SHOW" => strip_keyword(sql, "SHOW")
            .map(|rest| starts_with_keyword(rest, "GRANTS"))
            .unwrap_or(false),
        _ => false,
    }
}

/// Parse one of this module's statements.
///
/// `None` when `sql` is not one at all — the caller carries on as before.
/// `Some(Err(..))` when it is one and cannot be honoured, which is always a
/// message naming the part that was not understood, never a silent
/// approximation: a `GRANT` that means less than it says is a security bug.
pub fn parse(sql: &str, session: &Session) -> Option<Result<AclStatement, MysqlError>> {
    if !looks_like(sql) {
        return None;
    }
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let keyword = first_word(trimmed);
    Some(match keyword.as_str() {
        "CREATE" => parse_create_user(trimmed),
        "DROP" => parse_drop_user(trimmed),
        "ALTER" => parse_alter_user(trimmed),
        "RENAME" => Err(MysqlError::unsupported(
            "RENAME USER is not supported: an account's name is its identity here, and \
             renaming it would silently orphan every grant written against the old name — \
             CREATE USER the new name, GRANT it what the old one had, and DROP USER the old",
        )),
        "GRANT" => parse_grant(trimmed, session),
        "REVOKE" => parse_revoke(trimmed, session),
        "SHOW" => parse_show_grants(trimmed),
        // `looks_like` matched, so this is unreachable unless the two drift —
        // and drifting into "allowed" is exactly what must not happen.
        other => Err(MysqlError::unsupported(format!(
            "`{other}` is not an account statement this server implements"
        ))),
    })
}

fn parse_create_user(sql: &str) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "CREATE")
        .and_then(|rest| strip_keyword(rest, "USER"))
        .ok_or_else(|| MysqlError::parse("expected CREATE USER"))?;
    let (rest, if_not_exists) = match strip_keyword(rest, "IF")
        .and_then(|rest| strip_keyword(rest, "NOT"))
        .and_then(|rest| strip_keyword(rest, "EXISTS"))
    {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let users = split_top_level(rest, ',')
        .iter()
        .map(|spec| parse_new_user(spec, true))
        .collect::<Result<Vec<_>, _>>()?;
    if users.is_empty() {
        return Err(MysqlError::parse("CREATE USER names no account"));
    }
    Ok(AclStatement::CreateUser {
        users,
        if_not_exists,
    })
}

fn parse_drop_user(sql: &str) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "DROP")
        .and_then(|rest| strip_keyword(rest, "USER"))
        .ok_or_else(|| MysqlError::parse("expected DROP USER"))?;
    let (rest, if_exists) = match strip_keyword(rest, "IF").and_then(|r| strip_keyword(r, "EXISTS"))
    {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let users = split_top_level(rest, ',')
        .iter()
        .map(|spec| read_user_name(spec))
        .collect::<Result<Vec<_>, _>>()?;
    if users.is_empty() {
        return Err(MysqlError::parse("DROP USER names no account"));
    }
    Ok(AclStatement::DropUser { users, if_exists })
}

fn parse_alter_user(sql: &str) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "ALTER")
        .and_then(|rest| strip_keyword(rest, "USER"))
        .ok_or_else(|| MysqlError::parse("expected ALTER USER"))?;
    let (rest, if_exists) = match strip_keyword(rest, "IF").and_then(|r| strip_keyword(r, "EXISTS"))
    {
        Some(rest) => (rest, true),
        None => (rest, false),
    };
    let users = split_top_level(rest, ',')
        .iter()
        .map(|spec| parse_new_user(spec, false))
        .collect::<Result<Vec<_>, _>>()?;
    if users.is_empty() {
        return Err(MysqlError::parse("ALTER USER names no account"));
    }
    Ok(AclStatement::AlterUser { users, if_exists })
}

/// `user [IDENTIFIED [WITH plugin] BY 'password']`.
///
/// `creating` is true only for `CREATE USER`, where MySQL would
/// default to an empty password. This refuses that instead — see the error
/// text for why an account nobody meant to leave open is worth one extra
/// keystroke.
fn parse_new_user(spec: &str, creating: bool) -> Result<NewUser, MysqlError> {
    let (name, rest) = read_user(spec)?;
    let rest = rest.trim();
    if rest.is_empty() {
        if creating {
            return Err(MysqlError::unsupported(format!(
                "CREATE USER `{name}` without IDENTIFIED BY would create an account with no \
                 password at all; write IDENTIFIED BY '<password>', or IDENTIFIED BY '' if an \
                 open account is really what you want"
            )));
        }
        return Err(MysqlError::unsupported(format!(
            "ALTER USER `{name}` with no IDENTIFIED BY has nothing to change: setting a \
             password is the only account attribute this server has"
        )));
    }
    let rest = strip_keyword(rest, "IDENTIFIED").ok_or_else(|| {
        MysqlError::unsupported(format!(
            "`{rest}` is not something this server can set on account `{name}`; the only \
             supported clause is IDENTIFIED [WITH <plugin>] BY '<password>'"
        ))
    })?;

    let (rest, plugin) = match strip_keyword(rest, "WITH") {
        None => (rest, None),
        Some(rest) => {
            let (named, rest) = read_atom(rest)?;
            let plugin = match named.as_str() {
                name if name.eq_ignore_ascii_case(auth::NATIVE_PASSWORD) => auth::NATIVE_PASSWORD,
                name if name.eq_ignore_ascii_case(auth::CACHING_SHA2_PASSWORD) => {
                    auth::CACHING_SHA2_PASSWORD
                }
                other => {
                    return Err(MysqlError::unsupported(format!(
                        "authentication plugin `{other}` is not implemented; this server speaks \
                         {} and {}",
                        auth::CACHING_SHA2_PASSWORD,
                        auth::NATIVE_PASSWORD
                    )))
                }
            };
            (rest, Some(plugin))
        }
    };

    let rest = strip_keyword(rest, "BY").ok_or_else(|| {
        MysqlError::unsupported(format!(
            "IDENTIFIED on account `{name}` must be followed by BY '<password>'; the AS \
             '<hash>' form is not accepted, because a hash pasted in is a hash this server \
             cannot check the plugin of"
        ))
    })?;
    let rest = rest.trim();
    let password = read_string(rest).ok_or_else(|| {
        MysqlError::parse(format!(
            "expected a quoted password after IDENTIFIED BY for account `{name}`, found `{rest}`"
        ))
    })?;

    Ok(NewUser {
        name,
        plugin,
        password,
    })
}

fn parse_grant(sql: &str, session: &Session) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "GRANT").ok_or_else(|| MysqlError::parse("expected GRANT"))?;
    let on_at = find_clause(rest, "ON").ok_or_else(|| {
        MysqlError::unsupported(
            "GRANT without ON is how a role is granted, and this server has no roles; write \
             GRANT <privileges> ON <scope> TO <user>",
        )
    })?;
    let privileges = parse_privilege_list(&rest[..on_at])?;
    let after_on = &rest[on_at + 2..];
    let to_at = find_clause(after_on, "TO")
        .ok_or_else(|| MysqlError::parse("expected TO in GRANT ... ON ... TO ..."))?;
    let scope = parse_scope(&after_on[..to_at], session)?;

    let mut users_text = &after_on[to_at + 2..];
    let mut grant_option = false;
    if let Some(at) = find_clause(users_text, "WITH") {
        let tail = users_text[at + 4..].trim();
        if !starts_with_keyword(tail, "GRANT")
            || strip_keyword(tail, "GRANT")
                .map(|rest| !starts_with_keyword(rest, "OPTION"))
                .unwrap_or(true)
        {
            return Err(MysqlError::unsupported(format!(
                "`WITH {tail}` is not supported; the only WITH clause this server accepts is \
                 WITH GRANT OPTION"
            )));
        }
        grant_option = true;
        users_text = &users_text[..at];
    }
    if find_clause(users_text, "IDENTIFIED").is_some() {
        return Err(MysqlError::unsupported(
            "GRANT ... IDENTIFIED BY creates an account as a side effect of granting to it, \
             which MySQL itself removed in 8.0; write CREATE USER first",
        ));
    }

    let users = parse_user_list(users_text)?;
    if grant_option && !(privileges.contains(Privileges::ALL) && scope == Scope::Global) {
        return Err(MysqlError::unsupported(
            "WITH GRANT OPTION is only accepted on GRANT ALL PRIVILEGES ON *.*: this server \
             implements one level of delegation — a superuser — and a narrower grant option \
             would promise a partial delegation it does not enforce",
        ));
    }
    Ok(AclStatement::Grant {
        privileges,
        scope,
        users,
        grant_option,
    })
}

fn parse_revoke(sql: &str, session: &Session) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "REVOKE").ok_or_else(|| MysqlError::parse("expected REVOKE"))?;
    let from_at = find_clause(rest, "FROM")
        .ok_or_else(|| MysqlError::parse("expected FROM in REVOKE ... FROM ..."))?;
    let users = parse_user_list(&rest[from_at + 4..])?;
    let head = &rest[..from_at];

    // `REVOKE ALL PRIVILEGES, GRANT OPTION FROM u` carries no `ON` and means
    // "everything, everywhere" — MySQL's own spelling, and the only way to
    // take a superuser's rights back.
    let Some(on_at) = find_clause(head, "ON") else {
        let privileges = parse_privilege_list(head)?;
        if !privileges.contains(Privileges::ALL.with(Privileges::GRANT_OPTION)) {
            return Err(MysqlError::unsupported(format!(
                "REVOKE without ON is only the `REVOKE ALL PRIVILEGES, GRANT OPTION FROM ...` \
                 form; `REVOKE {} FROM ...` needs an ON clause naming what to revoke it on",
                privileges.name()
            )));
        }
        return Ok(AclStatement::Revoke {
            privileges,
            scope: None,
            users,
        });
    };
    let privileges = parse_privilege_list(&head[..on_at])?;
    let scope = parse_scope(&head[on_at + 2..], session)?;
    Ok(AclStatement::Revoke {
        privileges,
        scope: Some(scope),
        users,
    })
}

fn parse_show_grants(sql: &str) -> Result<AclStatement, MysqlError> {
    let rest = strip_keyword(sql, "SHOW")
        .and_then(|rest| strip_keyword(rest, "GRANTS"))
        .ok_or_else(|| MysqlError::parse("expected SHOW GRANTS"))?
        .trim();
    if rest.is_empty() {
        return Ok(AclStatement::ShowGrants { user: None });
    }
    let rest = strip_keyword(rest, "FOR").ok_or_else(|| {
        MysqlError::unsupported(format!(
            "`SHOW GRANTS {rest}` is not supported; the forms are SHOW GRANTS and \
             SHOW GRANTS FOR <user>"
        ))
    })?;
    if find_clause(rest, "USING").is_some() {
        return Err(MysqlError::unsupported(
            "SHOW GRANTS ... USING names roles, and this server has none",
        ));
    }
    // `SHOW GRANTS FOR CURRENT_USER` / `CURRENT_USER()` is how a client asks
    // about itself without knowing its own name.
    let trimmed = rest.trim().trim_end_matches("()").trim();
    if trimmed.eq_ignore_ascii_case("CURRENT_USER") {
        return Ok(AclStatement::ShowGrants { user: None });
    }
    Ok(AclStatement::ShowGrants {
        user: Some(read_user_name(rest)?),
    })
}

// ------------------------------------------------------------ small parsers

/// A comma-separated privilege list.
fn parse_privilege_list(text: &str) -> Result<Privileges, MysqlError> {
    let mut privileges = Privileges::NONE;
    let mut any = false;
    for item in split_top_level(text, ',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        any = true;
        if item.contains('(') {
            return Err(MysqlError::unsupported(format!(
                "`{item}` is a column-level privilege, and this server has none: privileges \
                 here cover a whole table or nothing"
            )));
        }
        let upper = item.to_ascii_uppercase();
        let normalized = upper.split_whitespace().collect::<Vec<_>>().join(" ");
        privileges = privileges.with(match normalized.as_str() {
            "ALL" | "ALL PRIVILEGES" => Privileges::ALL,
            "SELECT" => Privileges::SELECT,
            "INSERT" => Privileges::INSERT,
            "UPDATE" => Privileges::UPDATE,
            "DELETE" => Privileges::DELETE,
            "CREATE" => Privileges::CREATE,
            "DROP" => Privileges::DROP,
            "ALTER" => Privileges::ALTER,
            "GRANT OPTION" => Privileges::GRANT_OPTION,
            "USAGE" => Privileges::NONE,
            // `INDEX` is the one that looks mappable and is not. An index is
            // created and dropped by `CREATE`/`DROP` here, so the nearest
            // translation is `CREATE, DROP` — which at `*.*` would hand out
            // the right to create and drop *tables* as well, and MySQL's
            // `INDEX` grants nothing of the sort. Over-granting is not an
            // approximation, it is a hole.
            "INDEX" => {
                return Err(MysqlError::unsupported(
                    "the INDEX privilege is not implemented: an index is created and dropped \
                     by CREATE and DROP here, and granting those instead would also hand out \
                     the right to create and drop tables, which MySQL's INDEX does not — \
                     write GRANT CREATE, DROP if that is what you mean",
                ))
            }
            other => {
                return Err(MysqlError::unsupported(format!(
                    "privilege `{other}` is not implemented; this server has SELECT, INSERT, \
                     UPDATE, DELETE, CREATE, DROP, ALTER, ALL PRIVILEGES, USAGE and \
                     GRANT OPTION"
                )))
            }
        });
    }
    if !any {
        return Err(MysqlError::parse("no privileges named"));
    }
    Ok(privileges)
}

/// `*.*`, `db.*`, `db.tbl`, `tbl`, optionally behind `TABLE`.
fn parse_scope(text: &str, session: &Session) -> Result<Scope, MysqlError> {
    let text = text.trim();
    for refused in ["FUNCTION", "PROCEDURE"] {
        if starts_with_keyword(text, refused) {
            return Err(MysqlError::unsupported(format!(
                "GRANT ... ON {refused} names a stored routine, and this server has none"
            )));
        }
    }
    let text = strip_keyword(text, "TABLE").unwrap_or(text).trim();
    if text.is_empty() {
        return Err(MysqlError::parse("expected a scope after ON"));
    }

    let (qualifier, object) = match split_qualified(text) {
        Some((qualifier, object)) => (Some(qualifier), object),
        None => (None, unquote(text)),
    };
    if let Some(qualifier) = &qualifier {
        let acceptable = qualifier == "*"
            || qualifier.eq_ignore_ascii_case(DEFAULT_SCHEMA)
            || session
                .database()
                .is_some_and(|current| qualifier.eq_ignore_ascii_case(current));
        if !acceptable {
            return Err(MysqlError::new(
                1044,
                "42000",
                format!(
                    "Access denied for user to database '{qualifier}': one InlaySQL file is one \
                     schema, so a grant can only name `*`, `{DEFAULT_SCHEMA}`, or this \
                     connection's current database"
                ),
            ));
        }
    }
    if object == "*" {
        return Ok(Scope::Global);
    }
    if object.contains('*') {
        return Err(MysqlError::unsupported(format!(
            "`{object}` is not a table name; a grant covers one named table or everything (*.*)"
        )));
    }
    Ok(Scope::Table(object.to_ascii_lowercase()))
}

/// Split `a.b` at a dot that is not inside quotes. `None` when there is none.
fn split_qualified(text: &str) -> Option<(String, String)> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    for (index, &byte) in bytes.iter().enumerate() {
        match quote {
            Some(open) if byte == open => quote = None,
            Some(_) => {}
            None => match byte {
                b'\'' | b'"' | b'`' => quote = Some(byte),
                b'.' => {
                    return Some((
                        unquote(text[..index].trim()),
                        unquote(text[index + 1..].trim()),
                    ))
                }
                _ => {}
            },
        }
    }
    None
}

/// A comma-separated list of user references.
fn parse_user_list(text: &str) -> Result<Vec<String>, MysqlError> {
    let users = split_top_level(text, ',')
        .iter()
        .filter(|item| !item.trim().is_empty())
        .map(|item| read_user_name(item))
        .collect::<Result<Vec<_>, _>>()?;
    if users.is_empty() {
        return Err(MysqlError::parse("no account named"));
    }
    Ok(users)
}

/// One user reference, which must be the whole of `text`.
fn read_user_name(text: &str) -> Result<String, MysqlError> {
    let (name, rest) = read_user(text)?;
    if !rest.trim().is_empty() {
        return Err(MysqlError::parse(format!(
            "`{}` is not a user reference: expected 'name' or 'name'@'%'",
            text.trim()
        )));
    }
    Ok(name)
}

/// `name`, `'name'`, `` `name` ``, `name@host`, `'name'@'host'`.
///
/// The host part is checked rather than discarded. `%` is the only one
/// accepted: it means "from anywhere", which is exactly what this server
/// implements, and accepting `'localhost'` while enforcing nothing would turn
/// a deliberately narrow account into a wide one.
fn read_user(text: &str) -> Result<(String, &str), MysqlError> {
    let (name, rest) = read_atom(text)?;
    if name.is_empty() {
        return Err(MysqlError::parse("an account name may not be empty"));
    }
    let trimmed = rest.trim_start();
    let Some(after_at) = trimmed.strip_prefix('@') else {
        return Ok((name, rest));
    };
    let (host, rest) = read_atom(after_at)?;
    if host != "%" {
        return Err(MysqlError::unsupported(format!(
            "'{name}'@'{host}' names a host, and this server has no host-based access control \
             at all — only '%' is accepted, because accepting '{host}' and then ignoring it \
             would make the account reachable from everywhere it says it is not"
        )));
    }
    Ok((name, rest))
}

/// One quoted or bare token, and what follows it.
fn read_atom(text: &str) -> Result<(String, &str), MysqlError> {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let Some(&first) = bytes.first() else {
        return Err(MysqlError::parse("expected a name"));
    };
    if matches!(first, b'\'' | b'"' | b'`') {
        let mut index = 1;
        while index < bytes.len() {
            if bytes[index] == first {
                // A doubled quote is one literal quote, in both MySQL's string
                // and identifier syntax.
                if bytes.get(index + 1) == Some(&first) {
                    index += 2;
                    continue;
                }
                let inner = &trimmed[1..index];
                let quote = first as char;
                return Ok((
                    inner.replace(&format!("{quote}{quote}"), &quote.to_string()),
                    &trimmed[index + 1..],
                ));
            }
            index += 1;
        }
        return Err(MysqlError::parse(format!("unterminated `{trimmed}`")));
    }
    let end = trimmed
        .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(trimmed.len());
    Ok((trimmed[..end].to_string(), &trimmed[end..]))
}

/// A quoted string literal that is the whole of `text`.
fn read_string(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if !trimmed.starts_with('\'') && !trimmed.starts_with('"') {
        return None;
    }
    let (value, rest) = read_atom(trimmed).ok()?;
    rest.trim().is_empty().then_some(value)
}

/// Strip one layer of quoting, and leave anything unquoted exactly as written.
///
/// Deliberately not [`read_atom`] for the bare case: a scope's parts include
/// `*`, which is not an identifier character, and reading it as one would come
/// back empty and turn `ON *.*` into a grant on a schema called "".
fn unquote(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.starts_with(['\'', '"', '`']) {
        if let Ok((name, _)) = read_atom(trimmed) {
            return name;
        }
    }
    trimmed.to_string()
}

/// The byte offset of a top-level `keyword`, skipping quoted text.
///
/// `crate::sqltext::find_keyword` skips parenthesised text as well, which is
/// wrong here: nothing in these statements parenthesises, and a `GRANT` whose
/// user name contains a bracket must not lose its `TO`.
fn find_clause(text: &str, keyword: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) if byte == open => {
                quote = None;
                index += 1;
                continue;
            }
            Some(_) => {
                index += 1;
                continue;
            }
            None => {}
        }
        if matches!(byte, b'\'' | b'"' | b'`') {
            quote = Some(byte);
            index += 1;
            continue;
        }
        let boundary_before = index == 0 || !is_word_byte(bytes[index - 1]);
        if boundary_before
            && text[index..].len() >= keyword.len()
            && text[index..index + keyword.len()].eq_ignore_ascii_case(keyword)
        {
            let after = bytes.get(index + keyword.len()).copied();
            if after.is_none_or(|byte| !is_word_byte(byte)) {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

// =====================================================================
// running the account statements
// =====================================================================

/// What running an account statement produced.
pub enum Effect {
    /// Reply OK.
    Done,
    /// Reply with these rows (`SHOW GRANTS`).
    Rows(Box<ResultSet>),
}

/// Run one account statement.
///
/// Authorisation has already happened — [`enforce`] against
/// [`shim_requirement`] — so this is only the doing. What it still checks are
/// the invariants no privilege can express: an account cannot be created
/// twice, and the last superuser cannot be removed or demoted, because a
/// database nobody can administer is a database that has to be thrown away.
pub fn execute(
    db: &mut Database,
    session: &Session,
    bootstrap: &Bootstrap,
    statement: &AclStatement,
    policy: PasswordPolicy,
) -> Result<Effect, MysqlError> {
    // Every statement below except `SHOW GRANTS` writes, and a write is what
    // moves this database from "the flags are the credential" to "the file
    // is". `ensure_store` is idempotent, so this is a catalog lookup on every
    // subsequent one.
    if !matches!(statement, AclStatement::ShowGrants { .. }) {
        ensure_store(db, bootstrap)?;
    }
    match statement {
        AclStatement::CreateUser {
            users,
            if_not_exists,
        } => {
            for user in users {
                if account(db, &user.name, bootstrap)?.is_some() {
                    if *if_not_exists {
                        continue;
                    }
                    return Err(MysqlError::new(
                        1396,
                        "HY000",
                        format!("Operation CREATE USER failed for '{}'@'%'", user.name),
                    ));
                }
                insert_account(
                    db,
                    &user.name,
                    &user.password,
                    user.plugin,
                    Privileges::NONE,
                    policy,
                )?;
            }
            Ok(Effect::Done)
        }
        AclStatement::DropUser { users, if_exists } => {
            for name in users {
                if account(db, name, bootstrap)?.is_none() {
                    if *if_exists {
                        continue;
                    }
                    return Err(MysqlError::new(
                        1396,
                        "HY000",
                        format!("Operation DROP USER failed for '{name}'@'%'"),
                    ));
                }
                refuse_if_last_superuser(db, name, "DROP USER")?;
                run_sql(
                    db,
                    &format!("DELETE FROM {USER_TABLE} WHERE name = ?"),
                    &[text(name)],
                )?;
                run_sql(
                    db,
                    &format!("DELETE FROM {GRANT_TABLE} WHERE account = ?"),
                    &[text(name)],
                )?;
            }
            Ok(Effect::Done)
        }
        AclStatement::AlterUser { users, if_exists } => {
            for user in users {
                if account(db, &user.name, bootstrap)?.is_none() {
                    if *if_exists {
                        continue;
                    }
                    return Err(MysqlError::new(
                        1396,
                        "HY000",
                        format!("Operation ALTER USER failed for '{}'@'%'", user.name),
                    ));
                }
                set_password(db, &user.name, &user.password, user.plugin, policy)?;
            }
            Ok(Effect::Done)
        }
        AclStatement::Grant {
            privileges,
            scope,
            users,
            grant_option,
        } => {
            for name in users {
                let Some(existing) = account(db, name, bootstrap)? else {
                    return Err(no_such_account(name));
                };
                match scope {
                    Scope::Global => {
                        let mut granted = existing.global.with(*privileges);
                        if *grant_option {
                            granted = granted.with(Privileges::GRANT_OPTION);
                        }
                        set_global(db, name, granted)?;
                    }
                    Scope::Table(table) => {
                        if is_reserved(table) {
                            return Err(reserved_table_denied(&session.user, table));
                        }
                        let held = table_grant(db, name, table)?;
                        set_table_grant(db, name, table, held.with(*privileges))?;
                    }
                }
            }
            Ok(Effect::Done)
        }
        AclStatement::Revoke {
            privileges,
            scope,
            users,
        } => {
            for name in users {
                let Some(existing) = account(db, name, bootstrap)? else {
                    return Err(no_such_account(name));
                };
                let clears_superuser = match scope {
                    None => true,
                    Some(Scope::Global) => !existing
                        .global
                        .without(*privileges)
                        .contains(Privileges::ALL.with(Privileges::GRANT_OPTION)),
                    Some(Scope::Table(_)) => false,
                };
                if clears_superuser && existing.is_superuser() {
                    refuse_if_last_superuser(db, name, "REVOKE")?;
                }
                match scope {
                    None => {
                        set_global(db, name, Privileges::NONE)?;
                        run_sql(
                            db,
                            &format!("DELETE FROM {GRANT_TABLE} WHERE account = ?"),
                            &[text(name)],
                        )?;
                    }
                    Some(Scope::Global) => {
                        set_global(db, name, existing.global.without(*privileges))?;
                    }
                    Some(Scope::Table(table)) => {
                        let held = table_grant(db, name, table)?;
                        set_table_grant(db, name, table, held.without(*privileges))?;
                    }
                }
            }
            Ok(Effect::Done)
        }
        AclStatement::ShowGrants { user } => {
            let name = user.clone().unwrap_or_else(|| session.user.clone());
            let Some(existing) = account(db, &name, bootstrap)? else {
                return Err(no_such_account(&name));
            };
            let mut lines = Vec::new();
            let global = if existing.is_superuser() {
                format!("GRANT ALL PRIVILEGES ON *.* TO '{name}'@'%' WITH GRANT OPTION")
            } else if existing.global == Privileges::ALL {
                format!("GRANT ALL PRIVILEGES ON *.* TO '{name}'@'%'")
            } else if existing.global.is_empty() {
                // MySQL always emits this line, even for an account with
                // nothing: `USAGE` is its spelling for "connect and no more".
                format!("GRANT USAGE ON *.* TO '{name}'@'%'")
            } else {
                format!(
                    "GRANT {} ON *.* TO '{name}'@'%'",
                    existing.global.without(Privileges::GRANT_OPTION).name()
                )
            };
            lines.push(vec![Value::Text(global.into())]);
            for (table, privileges) in table_grants(db, &name)? {
                lines.push(vec![Value::Text(
                    format!(
                        "GRANT {} ON `{DEFAULT_SCHEMA}`.`{table}` TO '{name}'@'%'",
                        privileges.name()
                    )
                    .into(),
                )]);
            }
            Ok(Effect::Rows(Box::new(ResultSet {
                columns: vec![format!("Grants for {name}@%")],
                rows: lines,
            })))
        }
    }
}

fn no_such_account(name: &str) -> MysqlError {
    MysqlError::new(
        1133,
        "42000",
        format!("Can't find any matching row in the user table for '{name}'@'%'"),
    )
}

/// Refuse an operation that would leave the database with nobody able to
/// administer it.
///
/// The one shape of bricking this feature could cause, so it is checked rather
/// than documented: recovering from it would mean stopping the server and
/// restarting it with `--reset-superuser`, which needs a human with shell
/// access to the machine.
fn refuse_if_last_superuser(
    db: &mut Database,
    name: &str,
    operation: &str,
) -> Result<(), MysqlError> {
    let others = all_accounts(db)?
        .into_iter()
        .filter(|(other, privileges)| {
            other != name && privileges.contains(Privileges::ALL.with(Privileges::GRANT_OPTION))
        })
        .count();
    if others > 0 {
        return Ok(());
    }
    Err(MysqlError::new(
        1227,
        "42000",
        format!(
            "{operation} would leave '{name}'@'%' as the last account that can administer this \
             database with nobody to replace it, and nothing could then create another over \
             the wire — GRANT ALL PRIVILEGES ON *.* ... WITH GRANT OPTION to somebody else \
             first, or restart the server with --reset-superuser"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Limits;

    fn session() -> Session {
        Session::new(
            crate::control::Control::detached(1),
            "root",
            None,
            Limits::default(),
        )
    }

    fn parsed(sql: &str) -> AclStatement {
        parse(sql, &session())
            .unwrap_or_else(|| panic!("{sql} was not recognised as an account statement"))
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"))
    }

    fn refused(sql: &str) -> MysqlError {
        parse(sql, &session())
            .unwrap_or_else(|| panic!("{sql} was not recognised as an account statement"))
            .expect_err(sql)
    }

    #[test]
    fn only_account_statements_are_recognised() {
        for sql in [
            "CREATE USER 'a' IDENTIFIED BY 'b'",
            "create user a identified by 'b'",
            "DROP USER 'a'",
            "ALTER USER 'a' IDENTIFIED BY 'b'",
            "GRANT SELECT ON t TO 'a'",
            "REVOKE SELECT ON t FROM 'a'",
            "SHOW GRANTS",
            "SHOW GRANTS FOR 'a'",
        ] {
            assert!(looks_like(sql), "{sql} should be an account statement");
        }
        // The shapes that must keep reaching the engine untouched.
        for sql in [
            "CREATE TABLE users (id INTEGER PRIMARY KEY)",
            "DROP TABLE user",
            "ALTER TABLE users ADD COLUMN a INT",
            "SELECT * FROM users",
            "SHOW TABLES",
            "SHOW COLUMNS FROM users",
        ] {
            assert!(!looks_like(sql), "{sql} must not be an account statement");
            assert!(parse(sql, &session()).is_none(), "{sql}");
        }
    }

    #[test]
    fn a_password_never_survives_into_a_debug_rendering() {
        let AclStatement::CreateUser { users, .. } =
            parsed("CREATE USER 'app' IDENTIFIED BY 'hunter2'")
        else {
            panic!("expected CREATE USER");
        };
        let rendered = format!("{users:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn a_user_reference_may_name_any_host_and_nothing_else() {
        for sql in [
            "DROP USER 'app'",
            "DROP USER app",
            "DROP USER `app`",
            "DROP USER 'app'@'%'",
            "DROP USER app@'%'",
        ] {
            let AclStatement::DropUser { users, .. } = parsed(sql) else {
                panic!("expected DROP USER");
            };
            assert_eq!(users, vec!["app".to_string()], "{sql}");
        }
        // Accepting a host and then ignoring it is the failure mode this
        // refusal exists for.
        let error = refused("DROP USER 'app'@'localhost'");
        assert!(error.message.contains("host-based"), "{error:?}");
    }

    #[test]
    fn create_user_will_not_quietly_make_an_open_account() {
        let error = refused("CREATE USER 'app'");
        assert!(error.message.contains("no password"), "{error:?}");
        // ...but an empty password can still be asked for in as many words.
        let AclStatement::CreateUser { users, .. } = parsed("CREATE USER 'app' IDENTIFIED BY ''")
        else {
            panic!("expected CREATE USER");
        };
        assert_eq!(users[0].password, "");
    }

    #[test]
    fn a_pinned_plugin_is_recorded_and_an_unknown_one_is_refused() {
        let AclStatement::CreateUser { users, .. } =
            parsed("CREATE USER 'app' IDENTIFIED WITH mysql_native_password BY 'x'")
        else {
            panic!("expected CREATE USER");
        };
        assert_eq!(users[0].plugin, Some(auth::NATIVE_PASSWORD));
        let (native, sha2) = verifiers("x", users[0].plugin, PasswordPolicy::Scramble);
        assert!(native.is_some() && sha2.is_none());

        let error = refused("CREATE USER 'app' IDENTIFIED WITH sha256_password BY 'x'");
        assert!(error.message.contains("sha256_password"), "{error:?}");
    }

    #[test]
    fn the_default_is_both_verifiers_so_no_client_stops_working() {
        let (native, sha2) = verifiers("hunter2", None, PasswordPolicy::Scramble);
        assert!(native.is_some());
        assert!(sha2.is_some());
        assert_ne!(native.unwrap(), "hunter2");
    }

    #[test]
    fn scopes_parse_the_way_mysql_writes_them() {
        let global = |sql: &str| match parsed(sql) {
            AclStatement::Grant { scope, .. } => scope,
            other => panic!("expected GRANT, got {other:?}"),
        };
        assert_eq!(global("GRANT SELECT ON *.* TO 'a'"), Scope::Global);
        assert_eq!(global("GRANT SELECT ON inlaysql.* TO 'a'"), Scope::Global);
        assert_eq!(
            global("GRANT SELECT ON posts TO 'a'"),
            Scope::Table("posts".to_string())
        );
        assert_eq!(
            global("GRANT SELECT ON `inlaysql`.`Posts` TO 'a'"),
            Scope::Table("posts".to_string())
        );
        assert_eq!(
            global("GRANT SELECT ON TABLE posts TO 'a'"),
            Scope::Table("posts".to_string())
        );
        // A schema this file is not is refused rather than treated as this one.
        let error = refused("GRANT SELECT ON otherdb.posts TO 'a'");
        assert_eq!(error.code, 1044, "{error:?}");
    }

    #[test]
    fn everything_left_out_of_the_model_is_refused_by_name() {
        for (sql, expected) in [
            ("GRANT SELECT (email) ON t TO 'a'", "column-level"),
            // The one that looks mappable and is not: `CREATE, DROP` at `*.*`
            // would also grant the right to create and drop tables.
            (
                "GRANT INDEX ON t TO 'a'",
                "INDEX privilege is not implemented",
            ),
            ("GRANT PROXY ON 'b' TO 'a'", "not implemented"),
            ("GRANT SELECT ON FUNCTION f TO 'a'", "stored routine"),
            ("GRANT SELECT ON t TO 'a' WITH GRANT OPTION", "delegation"),
            ("GRANT `role` TO 'a'", "no roles"),
            ("REVOKE SELECT FROM 'a'", "needs an ON clause"),
            ("SHOW GRANTS FOR 'a' USING 'r'", "roles"),
            ("RENAME USER 'a' TO 'b'", "RENAME USER is not supported"),
            (
                "GRANT SELECT ON t TO 'a' IDENTIFIED BY 'x'",
                "CREATE USER first",
            ),
        ] {
            let error = refused(sql);
            assert!(
                error.message.contains(expected),
                "{sql} should name `{expected}`, said: {}",
                error.message
            );
        }
    }

    #[test]
    fn a_superuser_is_exactly_all_privileges_plus_grant_option() {
        let AclStatement::Grant {
            privileges,
            scope,
            grant_option,
            ..
        } = parsed("GRANT ALL PRIVILEGES ON *.* TO 'a' WITH GRANT OPTION")
        else {
            panic!("expected GRANT");
        };
        assert_eq!(privileges, Privileges::ALL);
        assert_eq!(scope, Scope::Global);
        assert!(grant_option);

        let account = Account {
            name: "a".to_string(),
            native: None,
            sha2: None,
            global: Privileges::ALL.with(Privileges::GRANT_OPTION),
        };
        assert!(account.is_superuser());

        // Everything short of it is not a superuser, including all privileges
        // without the grant option and the grant option on its own.
        for global in [
            Privileges::ALL,
            Privileges::GRANT_OPTION,
            Privileges::ALL.without(Privileges::DROP),
        ] {
            let lesser = Account {
                global,
                ..account.clone()
            };
            assert!(!lesser.is_superuser(), "{global:?}");
        }
    }

    #[test]
    fn revoking_everything_has_mysqls_own_spelling() {
        let AclStatement::Revoke {
            privileges,
            scope,
            users,
        } = parsed("REVOKE ALL PRIVILEGES, GRANT OPTION FROM 'a', 'b'")
        else {
            panic!("expected REVOKE");
        };
        assert!(privileges.contains(Privileges::ALL.with(Privileges::GRANT_OPTION)));
        assert_eq!(scope, None);
        assert_eq!(users, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_reserved_table_is_recognised_however_it_is_spelled() {
        assert!(is_reserved("__inlaysql_user"));
        assert!(is_reserved("__INLAYSQL_USER"));
        assert!(is_reserved("__inlaysql_grant"));
        assert!(!is_reserved("__inlaysql_"));
        assert!(!is_reserved("users"));
        assert!(!is_reserved("inlaysql_user"));
    }

    /// The default-deny arm. A statement the shim claims and this cannot
    /// classify has to be refused, not waved through.
    #[test]
    fn an_unclassifiable_shim_statement_is_refused_rather_than_allowed() {
        let requirement = shim_requirement("FLUSH PRIVILEGES", &session(), &Catalog::new());
        assert!(
            matches!(requirement, Requirement::Undetermined(_)),
            "{requirement:?}"
        );
    }

    #[test]
    fn the_shim_statements_every_driver_sends_need_only_an_account() {
        for sql in [
            "SET NAMES utf8mb4",
            "SHOW TABLES",
            "SELECT @@version",
            "BEGIN",
            "COMMIT",
            "USE app",
            "DESCRIBE t",
            // Decoration a real driver sends: a leading comment and a
            // trailing semicolon. Read unnormalised, the first word of these
            // is `/*`, which would land in the default-deny arm and refuse
            // every commented statement a client sends.
            "/* mysql-connector-python */ SET NAMES utf8mb4;",
            "-- a line comment\nSHOW TABLES",
            "/*!40101 SET NAMES utf8 */",
        ] {
            assert_eq!(
                shim_requirement(sql, &session(), &Catalog::new()),
                Requirement::Authenticated,
                "{sql}"
            );
        }
    }

    #[test]
    fn changing_your_own_password_is_not_an_administrative_act() {
        assert_eq!(
            shim_requirement(
                "ALTER USER 'root' IDENTIFIED BY 'x'",
                &session(),
                &Catalog::new()
            ),
            Requirement::Authenticated
        );
        assert_eq!(
            shim_requirement(
                "ALTER USER 'other' IDENTIFIED BY 'x'",
                &session(),
                &Catalog::new()
            ),
            Requirement::Administrative
        );
        // Naming yourself *and* somebody else is still administrative.
        assert_eq!(
            shim_requirement(
                "ALTER USER 'root' IDENTIFIED BY 'x', 'o' IDENTIFIED BY 'y'",
                &session(),
                &Catalog::new()
            ),
            Requirement::Administrative
        );
        assert_eq!(
            shim_requirement("SHOW GRANTS", &session(), &Catalog::new()),
            Requirement::Authenticated
        );
        assert_eq!(
            shim_requirement("SHOW GRANTS FOR 'other'", &session(), &Catalog::new()),
            Requirement::Administrative
        );
    }

    #[test]
    fn an_unknown_bit_in_a_stored_mask_is_dropped_rather_than_honoured() {
        // A file written by a build with more privileges than this one has.
        let stored = Privileges::from_bits(0b1111_1111_1111);
        assert_eq!(stored, Privileges::ALL.with(Privileges::GRANT_OPTION));
    }
}
