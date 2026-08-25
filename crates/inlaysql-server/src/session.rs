//! Per-connection state, and the system variables the shim answers from.
//!
//! None of this reaches the engine. A `SET` that arrives here is recorded and
//! forgotten on disconnect, which is the truthful behaviour for a server that
//! has no session subsystem: the alternative is either refusing statements
//! every driver sends on connect, or pretending a setting took effect. What is
//! *not* a no-op is `autocommit`, which really does change when work is
//! committed, so it lives in its own field rather than the variable map.

use std::collections::BTreeMap;

/// The version string this server reports.
///
/// It has to begin with a number a client will accept as "modern MySQL" —
/// drivers gate features on it, and anything below 5.x makes them fall back to
/// a protocol this server does not speak. The suffix says what is really
/// answering, so nobody reading a log concludes they are talking to MySQL.
pub const SERVER_VERSION: &str = "8.0.35-inlaysql";

/// The limits this server actually applies, in the units the MySQL system
/// variables that report them use.
///
/// Every number here is enforced by something concrete — `max_connections` by
/// the accept loop in [`crate::Server::run`], the two timeouts by the socket
/// options [`crate::serve_connection`] sets — and [`Session::variable`] reports
/// exactly these values and no others. That pairing is the whole point of the
/// type: a reported timeout nothing honours is worse than reporting none,
/// because a pool sizes itself and a driver decides how long to keep a
/// connection warm against these numbers. Before this existed the server said
/// `max_connections=0` while refusing the 65th connection, and named two
/// timeouts it never enforced.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The most connections served at once, after [`crate::Server::bind`] has
    /// clamped it. Reported as `max_connections`.
    pub max_connections: usize,
    /// The socket read timeout, in seconds. Reported as `wait_timeout`,
    /// `interactive_timeout` **and** `net_read_timeout` — one number for all
    /// three because one `SO_RCVTIMEO` is what enforces them: the same timer
    /// covers waiting for the next command and reading the rest of one
    /// already begun, and this server does not distinguish those two states.
    /// Reporting MySQL's much shorter conventional `net_read_timeout` here
    /// would name a limit nothing applies.
    pub read_timeout_secs: u64,
    /// The socket write timeout, in seconds. Reported as `net_write_timeout`.
    pub write_timeout_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: crate::DEFAULT_MAX_CONNECTIONS,
            read_timeout_secs: crate::DEFAULT_WAIT_TIMEOUT_SECS,
            write_timeout_secs: crate::NET_WRITE_TIMEOUT_SECS,
        }
    }
}

/// One warning raised by the statement that just ran.
///
/// The shim raises these for every MySQL-only clause it removed from a
/// statement (see [`crate::mysqlddl`]). Recording them is what keeps a dropped
/// clause from being a *silent* drop: the OK packet carries the count and
/// `SHOW WARNINGS` lists them, so a client that runs a migration can ask what
/// happened to it and get an itemised answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    /// The MySQL warning code.
    pub code: u16,
    /// What was ignored, and why.
    pub message: String,
}

/// One connection's session.
pub struct Session {
    /// The connection id reported to the client and in `CONNECTION_ID()`.
    pub connection_id: u32,
    /// The authenticated user name.
    pub user: String,
    /// The current default schema, if one has been selected.
    pub database: Option<String>,
    /// Whether each statement commits on its own.
    pub autocommit: bool,
    /// Whether the engine currently has a transaction open.
    pub in_transaction: bool,
    /// The row id of the last insert that generated one.
    pub last_insert_id: u64,
    /// Session variables, recorded but inert.
    variables: BTreeMap<String, String>,
    /// User variables (`@name`), likewise.
    user_variables: BTreeMap<String, String>,
    /// Warnings from the last statement that raised any.
    warnings: Vec<Warning>,
    /// What this server enforces, and therefore what it reports.
    limits: Limits,
}

impl Session {
    /// A fresh session for `connection_id`.
    pub fn new(connection_id: u32, user: &str, database: Option<String>, limits: Limits) -> Self {
        Self {
            connection_id,
            user: user.to_string(),
            database,
            autocommit: true,
            in_transaction: false,
            last_insert_id: 0,
            variables: BTreeMap::new(),
            user_variables: BTreeMap::new(),
            warnings: Vec::new(),
            limits,
        }
    }

    /// Replace the warnings a client would see for the statement just run.
    pub fn set_warnings(&mut self, warnings: Vec<Warning>) {
        self.warnings = warnings;
    }

    /// The warnings from the last statement that raised any.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// The count an OK or EOF packet reports.
    ///
    /// Saturating rather than truncating: a wrapped count that came out as `0`
    /// would tell a client there was nothing to see.
    pub fn warning_count(&self) -> u16 {
        u16::try_from(self.warnings.len()).unwrap_or(u16::MAX)
    }

    /// The status flags to report in an OK or EOF packet.
    pub fn status_flags(&self) -> u16 {
        let mut flags = 0;
        if self.autocommit {
            flags |= crate::protocol::SERVER_STATUS_AUTOCOMMIT;
        }
        if self.in_transaction {
            flags |= crate::protocol::SERVER_STATUS_IN_TRANS;
        }
        flags
    }

    /// Record a session variable.
    pub fn set_variable(&mut self, name: &str, value: &str) {
        self.variables
            .insert(name.to_ascii_lowercase(), value.to_string());
    }

    /// Record a user variable (`@name`).
    pub fn set_user_variable(&mut self, name: &str, value: &str) {
        self.user_variables
            .insert(name.to_ascii_lowercase(), value.to_string());
    }

    /// Read a user variable. Unset ones are SQL `NULL`, as in MySQL.
    pub fn user_variable(&self, name: &str) -> Option<&str> {
        self.user_variables
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    /// Read a system variable.
    ///
    /// A value that was `SET` earlier wins, so a driver that writes a variable
    /// and reads it back is not contradicted. Otherwise the defaults below
    /// describe what this server actually does — `have_ssl` really is
    /// `DISABLED`, and the isolation level really is repeatable read, because
    /// an explicit transaction pins its snapshot until it ends.
    pub fn variable(&self, name: &str) -> Option<String> {
        let name = name.to_ascii_lowercase();
        if let Some(value) = self.variables.get(&name) {
            return Some(value.clone());
        }
        let value = match name.as_str() {
            "version" => SERVER_VERSION,
            "version_comment" => "InlaySQL — embedded SQL with hybrid retrieval",
            "version_compile_os" => std::env::consts::OS,
            "version_compile_machine" => std::env::consts::ARCH,
            "protocol_version" => "10",
            "autocommit" => return Some(if self.autocommit { "1" } else { "0" }.to_string()),
            "character_set_client"
            | "character_set_connection"
            | "character_set_results"
            | "character_set_server"
            | "character_set_database" => "utf8mb4",
            "character_set_system" => "utf8mb3",
            "collation_connection" | "collation_server" | "collation_database" => {
                "utf8mb4_general_ci"
            }
            "sql_mode" => "STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION",
            "max_allowed_packet" => "67108864",
            "transaction_isolation" | "tx_isolation" => "REPEATABLE-READ",
            "transaction_read_only" | "tx_read_only" => "0",
            // Table names are compared case-insensitively but stored with the
            // spelling that created them: that is exactly what 2 means.
            "lower_case_table_names" => "2",
            "time_zone" => "SYSTEM",
            "system_time_zone" => "UTC",
            // The three variables below are read off [`Limits`] rather than
            // written out here, because each one is a number this server
            // really applies — see that type for why a reported-but-unenforced
            // timeout is worse than no answer at all.
            "wait_timeout" | "interactive_timeout" | "net_read_timeout" => {
                return Some(self.limits.read_timeout_secs.to_string())
            }
            "net_write_timeout" => return Some(self.limits.write_timeout_secs.to_string()),
            // Said plainly, because a client may be deciding whether to trust
            // this link with a password.
            "have_ssl" | "have_openssl" => "DISABLED",
            "ssl_cipher" => "",
            // MySQL answers `GPL` for its community build and `Commercial`
            // for the licensed one, and a client that inspects this is asking
            // which of those it is talking to. This engine is dual licensed
            // under the AGPL or a commercial licence, so it says so rather
            // than borrowing either of MySQL's two answers.
            "license" => "AGPL",
            "init_connect" => "",
            "sql_auto_is_null" => "0",
            "default_storage_engine" => "InlaySQL",
            // There is no foreign-key enforcement, so claiming otherwise would
            // be a lie a migration tool could act on.
            "foreign_key_checks" => "0",
            "unique_checks" => "0",
            "performance_schema" => "0",
            "event_scheduler" => "OFF",
            "hostname" => "localhost",
            "socket" => "",
            "max_connections" => return Some(self.limits.max_connections.to_string()),
            "group_concat_max_len" => "1024",
            "sql_select_limit" => "18446744073709551615",
            "sql_quote_show_create" => "1",
            "auto_increment_increment" | "auto_increment_offset" => "1",
            _ => return None,
        };
        Some(value.to_string())
    }

    /// Every system variable that has an answer, for `SHOW VARIABLES`.
    pub fn all_variables(&self) -> Vec<(String, String)> {
        const KNOWN: &[&str] = &[
            "auto_increment_increment",
            "auto_increment_offset",
            "autocommit",
            "character_set_client",
            "character_set_connection",
            "character_set_database",
            "character_set_results",
            "character_set_server",
            "character_set_system",
            "collation_connection",
            "collation_database",
            "collation_server",
            "default_storage_engine",
            "event_scheduler",
            "foreign_key_checks",
            "group_concat_max_len",
            "have_openssl",
            "have_ssl",
            "hostname",
            "init_connect",
            "interactive_timeout",
            "license",
            "lower_case_table_names",
            "max_allowed_packet",
            "max_connections",
            "net_read_timeout",
            "net_write_timeout",
            "performance_schema",
            "protocol_version",
            "socket",
            "sql_auto_is_null",
            "sql_mode",
            "sql_quote_show_create",
            "sql_select_limit",
            "ssl_cipher",
            "system_time_zone",
            "time_zone",
            "transaction_isolation",
            "transaction_read_only",
            "tx_isolation",
            "unique_checks",
            "version",
            "version_comment",
            "version_compile_machine",
            "version_compile_os",
            "wait_timeout",
        ];

        let mut out: BTreeMap<String, String> = KNOWN
            .iter()
            .filter_map(|name| self.variable(name).map(|value| (name.to_string(), value)))
            .collect();
        // Anything the client set that is not in the list above still shows up,
        // so `SET x = 1; SHOW VARIABLES LIKE 'x'` is not a contradiction.
        for (name, value) in &self.variables {
            out.insert(name.clone(), value.clone());
        }
        out.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_variable_wins_over_the_default() {
        let mut session = Session::new(1, "root", None, Limits::default());
        assert_eq!(
            session.variable("sql_mode").as_deref(),
            Some("STRICT_TRANS_TABLES,NO_ENGINE_SUBSTITUTION")
        );
        session.set_variable("SQL_MODE", "ANSI");
        assert_eq!(session.variable("sql_mode").as_deref(), Some("ANSI"));
    }

    #[test]
    fn autocommit_reads_back_the_live_value_not_a_recorded_one() {
        let mut session = Session::new(1, "root", None, Limits::default());
        assert_eq!(session.variable("autocommit").as_deref(), Some("1"));
        session.autocommit = false;
        assert_eq!(session.variable("autocommit").as_deref(), Some("0"));
    }

    #[test]
    fn an_unknown_variable_has_no_answer() {
        assert_eq!(
            Session::new(1, "root", None, Limits::default()).variable("wibble"),
            None
        );
    }

    /// Every limit a client can read back is the one this server was built
    /// with — not a constant written into the variable table beside it.
    ///
    /// This used to be `max_connections = 0` (no cap) against a real cap of
    /// 64, and two timeouts nothing applied. The numbers below are deliberately
    /// none of the defaults, so a hard-coded answer cannot pass.
    #[test]
    fn the_reported_limits_are_the_ones_the_server_was_given() {
        let session = Session::new(
            1,
            "root",
            None,
            Limits {
                max_connections: 7,
                read_timeout_secs: 11,
                write_timeout_secs: 13,
            },
        );
        assert_eq!(session.variable("max_connections").as_deref(), Some("7"));
        for name in ["wait_timeout", "interactive_timeout", "net_read_timeout"] {
            assert_eq!(
                session.variable(name).as_deref(),
                Some("11"),
                "{name} must report the socket read timeout that is actually set"
            );
        }
        assert_eq!(session.variable("net_write_timeout").as_deref(), Some("13"));

        // And `SHOW VARIABLES` says the same thing as `@@name`, which is where
        // a client that lists everything would otherwise see the stale answer.
        let all = session.all_variables();
        for (name, value) in [
            ("max_connections", "7"),
            ("wait_timeout", "11"),
            ("net_write_timeout", "13"),
        ] {
            assert!(
                all.iter().any(|(n, v)| n == name && v == value),
                "SHOW VARIABLES should list {name}={value}, got {all:?}"
            );
        }
    }

    /// The two claims a security-minded client might actually read.
    #[test]
    fn the_server_does_not_claim_to_have_tls_or_foreign_keys() {
        let session = Session::new(1, "root", None, Limits::default());
        assert_eq!(session.variable("have_ssl").as_deref(), Some("DISABLED"));
        assert_eq!(session.variable("foreign_key_checks").as_deref(), Some("0"));
    }

    #[test]
    fn status_flags_track_autocommit_and_transactions() {
        let mut session = Session::new(1, "root", None, Limits::default());
        assert_eq!(
            session.status_flags(),
            crate::protocol::SERVER_STATUS_AUTOCOMMIT
        );
        session.in_transaction = true;
        assert_eq!(
            session.status_flags(),
            crate::protocol::SERVER_STATUS_AUTOCOMMIT | crate::protocol::SERVER_STATUS_IN_TRANS
        );
        session.autocommit = false;
        assert_eq!(
            session.status_flags(),
            crate::protocol::SERVER_STATUS_IN_TRANS
        );
    }

    #[test]
    fn show_variables_includes_client_set_ones() {
        let mut session = Session::new(1, "root", None, Limits::default());
        session.set_variable("wibble", "7");
        let all = session.all_variables();
        assert!(all.iter().any(|(n, v)| n == "wibble" && v == "7"));
        assert!(all.iter().any(|(n, _)| n == "version"));
    }

    /// The version string is load-bearing: a client that reads a number below
    /// 5 falls back to a protocol this server does not implement.
    #[test]
    fn the_version_looks_modern_and_names_itself() {
        assert!(SERVER_VERSION.starts_with('8'));
        assert!(SERVER_VERSION.contains("inlaysql"));
    }
}
