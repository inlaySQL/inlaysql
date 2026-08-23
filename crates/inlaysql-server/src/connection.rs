//! One connection: the handshake, then a command loop until the client leaves.
//!
//! Each connection owns its own [`Database`] handle on the same file — decision
//! **D2** in `docs/architecture.md`. The engine is `!Send` by design, so a handle never
//! crosses a thread; several handles on one file already commit concurrently
//! with first-committer-wins, and a handle re-reads committed state at the start
//! of every statement outside an explicit transaction, so one connection sees
//! another's commits without any coordination here.
//!
//! Everything is blocking and synchronous. There is no runtime, no executor and
//! no `Send` bound anywhere in this file, which is what lets the engine be used
//! exactly as it was designed to be.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use inlaysql::{Database, Error, Outcome, ResultSet, Value};

use crate::auth;
use crate::errors::{from_engine, MysqlError};
use crate::packet::{put_lenenc_bytes, put_lenenc_int, Malformed, Reader, Stream};
use crate::protocol::{
    self, auth_more_data, auth_switch_request, eof_packet, err_packet, handshake, ok_packet,
    put_binary_value, text_value, unify_column_type, ColumnDef, Command,
};
use crate::session::{Session, Warning, SERVER_VERSION};
use crate::shim::{self, Intercepted};
use crate::sqltext;

/// How the server was told to authenticate.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The single user this server accepts.
    pub user: String,
    /// Its password. Empty means no password is required.
    pub password: String,
}

/// A statement kept between `COM_STMT_PREPARE` and `COM_STMT_CLOSE`.
struct Prepared {
    /// The statement text, kept so a stale plan can be prepared again and so
    /// shim statements can be re-classified with their bound parameters.
    sql: String,
    /// How many `?` it has.
    param_count: usize,
    /// The engine's plan. `None` for a statement the shim answers.
    plan: Option<inlaysql::Statement>,
    /// Parameter types from the last execute, reused when a client rebinds
    /// without resending them.
    param_types: Vec<(u8, bool)>,
    /// Warnings for the MySQL-only clauses the shim removed at prepare time.
    ///
    /// They are kept here rather than raised then, because MySQL raises a
    /// statement's warnings when it *runs* — a client that prepares once and
    /// executes ten times must see them on every execution, not on none.
    warnings: Vec<Warning>,
}

/// One client connection.
pub struct Connection<S: Read + Write> {
    stream: Stream<S>,
    db: Database,
    session: Session,
    statements: HashMap<u32, Prepared>,
    next_statement_id: u32,
}

impl<S: Read + Write> Connection<S> {
    /// Wrap an accepted connection.
    pub fn new(read_half: S, write_half: S, db: Database, connection_id: u32) -> Self {
        Self {
            stream: Stream::new(read_half, write_half),
            db,
            session: Session::new(connection_id, "", None),
            statements: HashMap::new(),
            next_statement_id: 1,
        }
    }

    /// Authenticate, then serve commands until the client disconnects.
    pub fn serve(&mut self, credentials: &Credentials) -> io::Result<()> {
        if !self.authenticate(credentials)? {
            return Ok(());
        }
        loop {
            let Some(message) = self.stream.read_message()? else {
                return Ok(());
            };
            if !self.dispatch(&message)? {
                return Ok(());
            }
        }
    }

    // ------------------------------------------------------------- auth

    fn authenticate(&mut self, credentials: &Credentials) -> io::Result<bool> {
        let challenge = auth::scramble()?;
        self.stream.write_message(&handshake(
            self.session.connection_id,
            &challenge,
            SERVER_VERSION,
        ))?;
        self.stream.flush()?;

        let Some(response) = self.stream.read_message()? else {
            return Ok(false);
        };
        let response = match parse_handshake_response(&response) {
            Ok(response) => response,
            Err(error) => {
                self.fail(&error)?;
                return Ok(false);
            }
        };

        // TLS is never advertised, so a client cannot have negotiated it. If
        // one asks anyway, say so plainly rather than letting it send a
        // password into a channel it believes is encrypted.
        if response.capabilities & protocol::CLIENT_SSL != 0 {
            self.fail(&MysqlError::new(
                2026,
                "HY000",
                "SSL connection error: this server is plaintext only (v1 has no TLS). \
                 Connect without TLS, over a loopback or trusted link.",
            ))?;
            return Ok(false);
        }

        // A client with no `CLIENT_PLUGIN_AUTH` capability sent no plugin
        // name at all; its token was still computed `mysql_native_password`'s
        // way, the protocol's own default from before plugins existed.
        let plugin = if response.auth_plugin.is_empty() {
            auth::NATIVE_PASSWORD
        } else {
            response.auth_plugin.as_str()
        };

        // `with_password` — whether the client sent anything at all — feeds
        // only the error message's "(using password: YES/NO)", never the
        // comparison itself.
        let (password_ok, with_password) = match plugin {
            auth::CACHING_SHA2_PASSWORD => {
                match self.caching_sha2_authenticate(
                    credentials,
                    &challenge,
                    &response.auth_response,
                )? {
                    Some(outcome) => outcome,
                    // The exchange already ended on its own: the connection
                    // dropped mid-handshake, or the client asked for the RSA
                    // exchange and was refused with its own explicit error.
                    None => return Ok(false),
                }
            }
            auth::NATIVE_PASSWORD => (
                auth::verify(&credentials.password, &challenge, &response.auth_response),
                !response.auth_response.is_empty(),
            ),
            // A plugin this server does not speak directly: ask the client
            // to switch to the one every driver already falls back to.
            _ => {
                self.stream
                    .write_message(&auth_switch_request(&challenge))?;
                self.stream.flush()?;
                let Some(token) = self.stream.read_message()? else {
                    return Ok(false);
                };
                let ok = auth::verify(&credentials.password, &challenge, &token);
                (ok, !token.is_empty())
            }
        };

        let user_ok = response.username == credentials.user;
        if !user_ok || !password_ok {
            // The reply does not distinguish a wrong user from a wrong
            // password, and nothing about the attempt is logged: a rejected
            // login should not tell a guesser which half was right, and a
            // password must never reach a log file.
            self.fail(&MysqlError::access_denied(
                &response.username,
                with_password,
            ))?;
            return Ok(false);
        }

        self.session = Session::new(
            self.session.connection_id,
            &response.username,
            response.database.clone(),
        );
        if let Some(name) = &response.database {
            if let Err(error) = check_database(name) {
                self.fail(&error)?;
                return Ok(false);
            }
        }
        self.ok(0, 0)?;
        Ok(true)
    }

    /// The `caching_sha2_password` half of [`Self::authenticate`].
    ///
    /// `Ok(Some((password_ok, with_password)))` when the exchange completed
    /// and the caller should still apply the shared username/password check.
    /// `Ok(None)` when it already ended on its own: the connection dropped
    /// mid-exchange, or the client asked for the RSA public-key exchange and
    /// was refused with its own explicit error — this server has no RSA
    /// implementation and v1's plaintext-localhost posture is what makes the
    /// full-authentication fallback below acceptable instead (see
    /// `docs/server.md` and the module docs on [`auth`]).
    fn caching_sha2_authenticate(
        &mut self,
        credentials: &Credentials,
        challenge: &[u8],
        initial_response: &[u8],
    ) -> io::Result<Option<(bool, bool)>> {
        // A 32-byte response is the plugin's fast-authentication attempt —
        // every real client sends one optimistically, hoping the server has
        // something to check it against. This one always does: it already
        // holds the plaintext password (v1's single user/password), so there
        // is no "cache miss" case to fall back from the way real MySQL's
        // in-memory hash cache has one.
        if initial_response.len() == 32 {
            let ok = auth::caching_sha2_verify(&credentials.password, challenge, initial_response);
            if ok {
                self.stream
                    .write_message(&auth_more_data(&[auth::CACHING_SHA2_FAST_AUTH_SUCCESS]))?;
                self.stream.flush()?;
            }
            return Ok(Some((ok, true)));
        }

        // No fast-authentication attempt (an empty response, or one of any
        // other length): ask the client to complete full authentication.
        self.stream.write_message(&auth_more_data(&[
            auth::CACHING_SHA2_PERFORM_FULL_AUTHENTICATION,
        ]))?;
        self.stream.flush()?;
        let Some(payload) = self.stream.read_message()? else {
            return Ok(None);
        };

        if payload == [auth::CACHING_SHA2_REQUEST_PUBLIC_KEY] {
            self.fail(&MysqlError::unsupported(
                "the RSA public-key exchange for caching_sha2_password is not implemented; \
                 this server is plaintext-localhost only (see docs/server.md) and accepts the \
                 cleartext password directly during full authentication instead of RSA — \
                 reconnect, or tell the client to use mysql_native_password",
            ))?;
            return Ok(None);
        }

        let ok = auth::caching_sha2_full_auth_verify(&credentials.password, &payload);
        Ok(Some((ok, !payload.is_empty())))
    }

    // -------------------------------------------------------- dispatch

    /// Handle one command. Returns whether the connection should stay open.
    fn dispatch(&mut self, message: &[u8]) -> io::Result<bool> {
        let Some((&head, body)) = message.split_first() else {
            self.fail(&MysqlError::unknown("empty command packet"))?;
            return Ok(true);
        };

        match Command::from_byte(head) {
            Command::Quit => return Ok(false),
            Command::Ping => self.ok(0, 0)?,
            Command::InitDb => {
                let name = String::from_utf8_lossy(body).to_string();
                match check_database(&name) {
                    Ok(()) => {
                        self.session.database = Some(name);
                        self.ok(0, 0)?;
                    }
                    Err(error) => self.fail(&error)?,
                }
            }
            Command::Query => {
                let sql = String::from_utf8_lossy(body).to_string();
                self.run_text_query(&sql)?;
            }
            Command::StmtPrepare => {
                let sql = String::from_utf8_lossy(body).to_string();
                self.prepare(&sql)?;
            }
            Command::StmtExecute => self.execute_prepared(body)?,
            Command::StmtClose => {
                if let Ok(id) = Reader::new(body).u32() {
                    self.statements.remove(&id);
                }
                // COM_STMT_CLOSE is the one command with no reply at all.
                return Ok(true);
            }
            Command::StmtReset => match Reader::new(body).u32() {
                Ok(id) if self.statements.contains_key(&id) => {
                    if let Some(prepared) = self.statements.get_mut(&id) {
                        prepared.param_types.clear();
                    }
                    self.ok(0, 0)?;
                }
                _ => self.fail(&MysqlError::new(
                    1243,
                    "HY000",
                    "Unknown prepared statement handler given to mysqld_stmt_reset",
                ))?,
            },
            // Superseded by `SHOW COLUMNS` two decades ago, and its reply is a
            // result set with no header, which nothing here would gain from.
            Command::FieldList => self.fail(&MysqlError::unsupported(
                "COM_FIELD_LIST is not supported; use SHOW COLUMNS FROM <table>",
            ))?,
            Command::Unknown(byte) => self.fail(&MysqlError::unknown_command(byte))?,
        }
        Ok(true)
    }

    // ------------------------------------------------------- statements

    fn run_text_query(&mut self, sql: &str) -> io::Result<()> {
        match self.run(sql, &[]) {
            Ok(Answer::Ok {
                affected,
                insert_id,
            }) => self.ok(affected, insert_id),
            Ok(Answer::Rows(rows)) => self.write_result_set(&rows, false),
            Err(error) => self.fail(&error),
        }
    }

    /// Run one statement, through the shim first and the engine otherwise.
    fn run(&mut self, sql: &str, params: &[Value]) -> Result<Answer, MysqlError> {
        // Resolved once, up front, so every path below — shim classification,
        // a shim-rewritten DDL statement, and a plain pass-through — reads
        // the same corrected text. See `rewrite_backslash_escapes`: a client
        // that escapes literal values with a backslash (most that do not use
        // a true binary-protocol prepared statement) means something specific
        // by it, and the engine's SQLite dialect does not understand that
        // syntax on its own.
        let sql = &sqltext::rewrite_backslash_escapes(sql);

        // MySQL's rule: every statement starts with an empty warning list,
        // except the ones whose purpose is to read it.
        if !shim::reads_warnings(sql) {
            self.session.set_warnings(Vec::new());
        }
        let decision = shim::intercept(sql, params, self.db.catalog(), &mut self.session);

        match decision {
            Intercepted::Rewritten {
                statements,
                warnings,
            } => {
                // The warnings are recorded before the statement runs, so a
                // `SHOW WARNINGS` after a *failed* translation-and-refusal by
                // the engine still lists what the shim took off it.
                self.session.set_warnings(warnings);
                self.run_statements_on_engine(&statements, params)
            }
            Intercepted::Ok => Ok(Answer::ok()),
            Intercepted::Rows(rows) => Ok(Answer::Rows(*rows)),
            Intercepted::Failed(error) => Err(error),
            Intercepted::UseDatabase(name) => {
                self.session.database = Some(name);
                Ok(Answer::ok())
            }
            Intercepted::Begin => {
                // MySQL commits an open transaction when a new one begins.
                if self.session.in_transaction {
                    self.commit()?;
                }
                self.db.begin().map_err(|e| from_engine(&e))?;
                self.session.in_transaction = true;
                Ok(Answer::ok())
            }
            Intercepted::Commit => {
                self.commit()?;
                Ok(Answer::ok())
            }
            Intercepted::Rollback => {
                // Rolling back outside a transaction is a no-op in MySQL, not
                // an error, and drivers rely on that during cleanup.
                if self.session.in_transaction {
                    self.db.rollback().map_err(|e| from_engine(&e))?;
                    self.session.in_transaction = false;
                }
                Ok(Answer::ok())
            }
            Intercepted::SetAutocommit(on) => {
                if on && self.session.in_transaction {
                    // Turning autocommit back on commits the open transaction.
                    self.commit()?;
                }
                self.session.autocommit = on;
                Ok(Answer::ok())
            }
            Intercepted::PassThrough => self.run_on_engine(sql, params),
        }
    }

    fn run_on_engine(&mut self, sql: &str, params: &[Value]) -> Result<Answer, MysqlError> {
        self.begin_implicit()?;
        let before = self.db.last_insert_row_id();
        let outcome = self
            .db
            .execute(sql, params)
            .map_err(|error| from_engine(&error))?;
        Ok(self.finish(outcome, before))
    }

    /// Run every statement a MySQL DDL translation expanded to, in order —
    /// `crate::mysqlddl`'s multi-operation `ALTER TABLE` split, or an
    /// operation that became its own free-standing statement.
    ///
    /// **This is not atomic the way MySQL's single statement is.** Each
    /// statement is its own call to the engine; if the third of five fails,
    /// the first two already committed and the last two never run. The error
    /// reported is the failing statement's own, naming what went wrong, and
    /// nothing here undoes what already happened — the client sees exactly as
    /// much as if it had sent the statements one at a time itself, because
    /// that is exactly what this is doing.
    ///
    /// `params` are the bound values of the *original* client statement,
    /// which is not one-to-one with the statements below once one MySQL
    /// statement has become several: each gets only the slice of `params` its
    /// own `?` placeholders account for, in the order both appear. DDL of the
    /// kind this exists for essentially never binds a parameter, but nothing
    /// here assumes that.
    fn run_statements_on_engine(
        &mut self,
        statements: &[String],
        params: &[Value],
    ) -> Result<Answer, MysqlError> {
        // Every operation the translation carried turned into a warning and
        // nothing else (an `ADD CONSTRAINT ... FOREIGN KEY` on its own) — a
        // plain OK, and the engine is not touched at all.
        if statements.is_empty() {
            return Ok(Answer::ok());
        }

        self.begin_implicit()?;
        let mut remaining = params;
        let mut answer = Answer::ok();
        for statement in statements {
            let count = sqltext::count_placeholders(statement);
            if count > remaining.len() {
                return Err(MysqlError::new(
                    1210,
                    "HY000",
                    format!(
                        "Incorrect arguments to EXECUTE: `{statement}` needs {count} \
                         parameter(s), only {} remain",
                        remaining.len()
                    ),
                ));
            }
            let (this_statement, rest) = remaining.split_at(count);
            remaining = rest;

            let before = self.db.last_insert_row_id();
            let outcome = self
                .db
                .execute(statement, this_statement)
                .map_err(|error| from_engine(&error))?;
            answer = self.finish(outcome, before);
        }
        Ok(answer)
    }

    /// With autocommit off, MySQL opens a transaction at the first statement
    /// and keeps it open until the client ends it. Doing that here is what
    /// makes `SET autocommit=0` mean something rather than being recorded and
    /// ignored.
    fn begin_implicit(&mut self) -> Result<(), MysqlError> {
        if !self.session.autocommit && !self.session.in_transaction {
            self.db.begin().map_err(|e| from_engine(&e))?;
            self.session.in_transaction = true;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<(), MysqlError> {
        if self.session.in_transaction {
            let result = self.db.commit();
            // A failed commit still ends the transaction — the engine rolled
            // it back — so the session must not be left believing one is open.
            self.session.in_transaction = false;
            result.map_err(|e| from_engine(&e))?;
        }
        Ok(())
    }

    /// Turn an engine outcome into a reply, tracking the insert id.
    fn finish(&mut self, outcome: Outcome, before: Option<u64>) -> Answer {
        match outcome {
            Outcome::Rows(rows) => Answer::Rows(rows),
            Outcome::Ddl => Answer::ok(),
            Outcome::Written(count) => {
                let after = self.db.last_insert_row_id();
                // Only a statement that *generated* an id reports one, which
                // is MySQL's rule: an INSERT supplying its own key leaves
                // LAST_INSERT_ID() alone.
                let insert_id = if after != before {
                    after.unwrap_or(0)
                } else {
                    0
                };
                if insert_id != 0 {
                    self.session.last_insert_id = insert_id;
                }
                Answer::Ok {
                    affected: count as u64,
                    insert_id,
                }
            }
        }
    }

    fn prepare(&mut self, sql: &str) -> io::Result<()> {
        // A bound parameter never carries a client-side escape (it arrives as
        // a typed binary value, not text), but a literal written directly
        // into the prepared statement's own shape can — the same rewrite
        // `run` applies to a text-protocol statement, applied here so a
        // statement cannot mean one thing prepared and another sent plain.
        let sql = &sqltext::rewrite_backslash_escapes(sql);
        let normalized = sqltext::normalize(sql);

        let prepared = if shim::handles(&normalized) {
            Prepared {
                param_count: sqltext::count_placeholders(&normalized),
                sql: normalized,
                plan: None,
                param_types: Vec::new(),
                warnings: Vec::new(),
            }
        } else {
            // The same translation the text path applies, through the same
            // function, so a statement cannot mean one thing when it is sent
            // and another when it is prepared.
            let translation = match shim::translate(&normalized, self.db.catalog()) {
                Ok(translation) => translation,
                Err(error) => return self.fail(&error),
            };
            match translation.statements.as_slice() {
                // The ordinary case, and the only one AHL-466's real column
                // metadata below can come from: exactly one statement, planned
                // once so `columns()` can describe what it actually projects.
                [only] => {
                    let warnings = shim::translation_warnings(&translation);
                    match self.db.prepare(only) {
                        Ok(plan) => Prepared {
                            param_count: plan.parameter_count(),
                            sql: only.clone(),
                            plan: Some(plan),
                            param_types: Vec::new(),
                            warnings,
                        },
                        Err(error) => return self.fail(&from_engine(&error)),
                    }
                }
                // A MySQL statement that expanded to zero or several engine
                // statements (a multi-operation `ALTER TABLE`, or an
                // operation that became its own `CREATE INDEX` /
                // `DROP INDEX` / nothing at all): there is no single plan to
                // hold, and none of these return rows anyway, so this is run
                // the same way a shim-answered statement is — re-translated
                // and re-run at execute time through `Connection::run`, which
                // already knows how to run a `Rewritten` translation's
                // statements in sequence. `warnings` is reported there too,
                // for the same reason: it depends on the translation, which
                // is redone at execute time.
                _ => Prepared {
                    param_count: sqltext::count_placeholders(&normalized),
                    sql: normalized.clone(),
                    plan: None,
                    param_types: Vec::new(),
                    warnings: Vec::new(),
                },
            }
        };

        let id = self.next_statement_id;
        self.next_statement_id = self.next_statement_id.wrapping_add(1).max(1);
        let param_count = prepared.param_count;
        // Real column metadata (AHL-466) when the engine planned this
        // statement: `inlaysql::Statement::columns()` knows the projection
        // without running it. A shim-answered statement (`plan` is `None` —
        // `SHOW`, `information_schema`, a session `SET`) still reports zero,
        // because its shape is not known until the shim itself runs it, and
        // this server has no way to ask the shim "what would this return"
        // without doing exactly that.
        let columns: Vec<ColumnDef> = prepared
            .plan
            .as_ref()
            .map(|plan| {
                plan.columns()
                    .iter()
                    .map(|column| protocol::column_def_from_type(column.name.clone(), column.ty))
                    .collect()
            })
            .unwrap_or_default();
        self.statements.insert(id, prepared);

        let mut packet = vec![0x00];
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&(columns.len() as u16).to_le_bytes());
        packet.extend_from_slice(&(param_count as u16).to_le_bytes());
        packet.push(0);
        packet.extend_from_slice(&0u16.to_le_bytes());
        self.stream.write_message(&packet)?;

        let schema = shim::schema_name(&self.session);
        if param_count > 0 {
            for index in 0..param_count {
                let def = ColumnDef::text(format!("?{}", index + 1));
                self.stream.write_message(&def.encode(&schema))?;
            }
            let status = self.session.status_flags();
            self.stream
                .write_message(&eof_packet(status, self.session.warning_count()))?;
        }
        // The `COM_STMT_PREPARE_OK` reply carries the parameter definitions
        // first and the column definitions second — MySQL's own packet
        // order, not this server's choice.
        if !columns.is_empty() {
            for def in &columns {
                self.stream.write_message(&def.encode(&schema))?;
            }
            let status = self.session.status_flags();
            self.stream
                .write_message(&eof_packet(status, self.session.warning_count()))?;
        }
        self.stream.flush()
    }

    fn execute_prepared(&mut self, body: &[u8]) -> io::Result<()> {
        let (id, params) = match self.decode_execute(body) {
            Ok(decoded) => decoded,
            Err(error) => return self.fail(&error),
        };

        let Some(prepared) = self.statements.get(&id) else {
            return self.fail(&MysqlError::new(
                1243,
                "HY000",
                "Unknown prepared statement handler given to mysqld_stmt_execute",
            ));
        };
        let sql = prepared.sql.clone();
        let has_plan = prepared.plan.is_some();

        let answer = if has_plan {
            self.run_plan(id, &sql, &params)
        } else {
            self.run(&sql, &params)
        };

        match answer {
            Ok(Answer::Ok {
                affected,
                insert_id,
            }) => self.ok(affected, insert_id),
            // Binary result sets: a statement executed through the prepared
            // path answers in the binary protocol, as the client expects.
            Ok(Answer::Rows(rows)) => self.write_result_set(&rows, true),
            Err(error) => self.fail(&error),
        }
    }

    /// Run a statement the engine planned, re-planning once if the schema moved
    /// under it. A stale plan is the engine telling us the ordinals it resolved
    /// are no longer trustworthy; preparing again is always the right answer,
    /// and doing it here means a client never sees the condition.
    fn run_plan(&mut self, id: u32, sql: &str, params: &[Value]) -> Result<Answer, MysqlError> {
        // A prepared statement raises its translation warnings on every
        // execution, as MySQL does — the clause was still ignored this time.
        let warnings = self
            .statements
            .get(&id)
            .map(|prepared| prepared.warnings.clone())
            .unwrap_or_default();
        self.session.set_warnings(warnings);

        self.begin_implicit()?;
        let before = self.db.last_insert_row_id();

        let plan = match self.statements.get(&id).and_then(|p| p.plan.clone()) {
            Some(plan) => plan,
            None => return Err(MysqlError::unknown("prepared statement vanished")),
        };

        let outcome = match self.db.execute_prepared(&plan, params) {
            Err(Error::Stale(_)) => {
                let fresh = self.db.prepare(sql).map_err(|e| from_engine(&e))?;
                if let Some(prepared) = self.statements.get_mut(&id) {
                    prepared.plan = Some(fresh.clone());
                }
                self.db
                    .execute_prepared(&fresh, params)
                    .map_err(|e| from_engine(&e))?
            }
            other => other.map_err(|e| from_engine(&e))?,
        };
        Ok(self.finish(outcome, before))
    }

    /// Decode `COM_STMT_EXECUTE`: the statement id, then the bound values.
    fn decode_execute(&mut self, body: &[u8]) -> Result<(u32, Vec<Value>), MysqlError> {
        let malformed = || MysqlError::unknown("malformed COM_STMT_EXECUTE packet");
        let mut reader = Reader::new(body);
        let id = reader.u32().map_err(|_| malformed())?;
        reader.u8().map_err(|_| malformed())?; // flags
        reader.u32().map_err(|_| malformed())?; // iteration count

        let Some(prepared) = self.statements.get(&id) else {
            return Err(MysqlError::new(
                1243,
                "HY000",
                "Unknown prepared statement handler given to mysqld_stmt_execute",
            ));
        };
        let count = prepared.param_count;
        if count == 0 {
            return Ok((id, Vec::new()));
        }

        let null_bitmap = reader
            .take(count.div_ceil(8))
            .map_err(|_| malformed())?
            .to_vec();
        let rebound = reader.u8().map_err(|_| malformed())? == 1;

        let types = if rebound {
            let mut types = Vec::with_capacity(count);
            for _ in 0..count {
                let ty = reader.u8().map_err(|_| malformed())?;
                let flags = reader.u8().map_err(|_| malformed())?;
                types.push((ty, flags & 0x80 != 0));
            }
            types
        } else {
            // The client is reusing the types it sent last time. If it never
            // sent any, there is nothing to decode the values with.
            let types = prepared.param_types.clone();
            if types.len() != count {
                return Err(MysqlError::new(
                    1210,
                    "HY000",
                    "Incorrect arguments to EXECUTE: parameter types were never sent",
                ));
            }
            types
        };

        let mut params = Vec::with_capacity(count);
        for (index, (ty, unsigned)) in types.iter().enumerate() {
            let is_null = null_bitmap
                .get(index / 8)
                .is_some_and(|byte| byte & (1 << (index % 8)) != 0);
            if is_null {
                params.push(Value::Null);
                continue;
            }
            params.push(decode_binary_param(&mut reader, *ty, *unsigned).map_err(|_| malformed())?);
        }

        if let Some(prepared) = self.statements.get_mut(&id) {
            prepared.param_types = types;
        }
        Ok((id, params))
    }

    // ----------------------------------------------------------- replies

    fn ok(&mut self, affected: u64, insert_id: u64) -> io::Result<()> {
        let status = self.session.status_flags();
        let warnings = self.session.warning_count();
        self.stream
            .write_message(&ok_packet(affected, insert_id, status, warnings, ""))?;
        self.stream.flush()
    }

    fn fail(&mut self, error: &MysqlError) -> io::Result<()> {
        self.stream
            .write_message(&err_packet(error.code, error.sqlstate, &error.message))?;
        self.stream.flush()
    }

    fn write_result_set(&mut self, rows: &ResultSet, binary: bool) -> io::Result<()> {
        // A result set with no columns is not representable: the leading count
        // would be zero, which a client reads as an OK packet.
        if rows.columns.is_empty() {
            return self.ok(0, 0);
        }

        let schema = shim::schema_name(&self.session);
        let defs: Vec<ColumnDef> = rows
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let mut def = unify_column_type(&rows.rows, index);
                def.name = name.clone();
                def
            })
            .collect();

        let mut count = Vec::new();
        put_lenenc_int(&mut count, defs.len() as u64);
        self.stream.write_message(&count)?;
        for def in &defs {
            self.stream.write_message(&def.encode(&schema))?;
        }
        let status = self.session.status_flags();
        let warnings = self.session.warning_count();
        self.stream.write_message(&eof_packet(status, warnings))?;

        for row in &rows.rows {
            let packet = if binary {
                binary_row(row, &defs)
            } else {
                text_row(row)
            };
            self.stream.write_message(&packet)?;
        }
        self.stream.write_message(&eof_packet(status, warnings))?;
        self.stream.flush()
    }
}

/// What running a statement produced.
enum Answer {
    Ok { affected: u64, insert_id: u64 },
    Rows(ResultSet),
}

impl Answer {
    fn ok() -> Self {
        Answer::Ok {
            affected: 0,
            insert_id: 0,
        }
    }
}

/// One row in the text protocol: every value a length-encoded string.
fn text_row(row: &[Value]) -> Vec<u8> {
    let mut packet = Vec::new();
    for value in row {
        match text_value(value) {
            Some(bytes) => put_lenenc_bytes(&mut packet, &bytes),
            None => packet.push(0xfb),
        }
    }
    packet
}

/// One row in the binary protocol: a NULL bitmap, then typed values.
///
/// The bitmap is offset by two bits, a quirk of the protocol rather than of
/// this implementation: the first two bits are reserved.
fn binary_row(row: &[Value], defs: &[ColumnDef]) -> Vec<u8> {
    let mut packet = vec![0x00];
    let bitmap_len = (row.len() + 7 + 2) / 8;
    let mut bitmap = vec![0u8; bitmap_len];
    for (index, value) in row.iter().enumerate() {
        if matches!(value, Value::Null) {
            bitmap[(index + 2) / 8] |= 1 << ((index + 2) % 8);
        }
    }
    packet.extend_from_slice(&bitmap);
    for (index, value) in row.iter().enumerate() {
        let ty = defs
            .get(index)
            .map(|d| d.ty)
            .unwrap_or(protocol::TYPE_VAR_STRING);
        put_binary_value(&mut packet, ty, value);
    }
    packet
}

/// Decode one bound parameter.
fn decode_binary_param(
    reader: &mut Reader<'_>,
    ty: u8,
    unsigned: bool,
) -> Result<Value, Malformed> {
    Ok(match ty {
        // NULL
        0x06 => Value::Null,
        // TINY
        0x01 => {
            let byte = reader.u8()?;
            Value::Integer(if unsigned {
                byte as i64
            } else {
                byte as i8 as i64
            })
        }
        // SHORT, YEAR
        0x02 | 0x0d => {
            let value = reader.u16()?;
            Value::Integer(if unsigned {
                value as i64
            } else {
                value as i16 as i64
            })
        }
        // LONG, INT24
        0x03 | 0x09 => {
            let value = reader.u32()?;
            Value::Integer(if unsigned {
                value as i64
            } else {
                value as i32 as i64
            })
        }
        // LONGLONG
        0x08 => Value::Integer(reader.u64()? as i64),
        // FLOAT
        0x04 => Value::Real(
            f32::from_le_bytes(reader.take(4)?.try_into().map_err(|_| Malformed)?) as f64,
        ),
        // DOUBLE
        0x05 => Value::Real(f64::from_le_bytes(
            reader.take(8)?.try_into().map_err(|_| Malformed)?,
        )),
        // DATE, DATETIME, TIMESTAMP. The engine has no temporal type, so these
        // are decoded to keep the packet in step and handed on as the text a
        // client would have sent for a string column.
        0x0a | 0x0c | 0x07 => Value::Text(decode_datetime(reader)?.into()),
        // TIME
        0x0b => Value::Text(decode_time(reader)?.into()),
        // The blob family stays bytes; everything else is text if it is valid
        // UTF-8, and bytes if it is not.
        // TINY_BLOB, MEDIUM_BLOB, LONG_BLOB, BLOB.
        0xf9..=0xfc => Value::Blob(reader.lenenc_bytes()?.unwrap_or_default().to_vec()),
        _ => {
            let bytes = reader.lenenc_bytes()?.unwrap_or_default();
            match std::str::from_utf8(bytes) {
                Ok(text) => Value::Text(text.to_string().into()),
                Err(_) => Value::Blob(bytes.to_vec()),
            }
        }
    })
}

fn decode_datetime(reader: &mut Reader<'_>) -> Result<String, Malformed> {
    let length = reader.u8()?;
    if length == 0 {
        return Ok("0000-00-00 00:00:00".to_string());
    }
    let year = reader.u16()?;
    let month = reader.u8()?;
    let day = reader.u8()?;
    if length == 4 {
        return Ok(format!("{year:04}-{month:02}-{day:02}"));
    }
    let hour = reader.u8()?;
    let minute = reader.u8()?;
    let second = reader.u8()?;
    if length == 11 {
        let micros = reader.u32()?;
        return Ok(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}.{micros:06}"
        ));
    }
    Ok(format!(
        "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
    ))
}

fn decode_time(reader: &mut Reader<'_>) -> Result<String, Malformed> {
    let length = reader.u8()?;
    if length == 0 {
        return Ok("00:00:00".to_string());
    }
    let negative = reader.u8()? == 1;
    let days = reader.u32()?;
    let hours = reader.u8()? as u32 + days * 24;
    let minutes = reader.u8()?;
    let seconds = reader.u8()?;
    let sign = if negative { "-" } else { "" };
    if length == 12 {
        let micros = reader.u32()?;
        return Ok(format!(
            "{sign}{hours:02}:{minutes:02}:{seconds:02}.{micros:06}"
        ));
    }
    Ok(format!("{sign}{hours:02}:{minutes:02}:{seconds:02}"))
}

/// One database file is one schema, so the only names accepted are this
/// server's own. Silently accepting any name would let a client believe it had
/// switched to a different database and write into this one.
fn check_database(name: &str) -> Result<(), MysqlError> {
    if name.is_empty() || name.eq_ignore_ascii_case(shim::DEFAULT_SCHEMA) {
        return Ok(());
    }
    if ["information_schema", "mysql", "performance_schema", "sys"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(MysqlError::new(
            1044,
            "42000",
            format!("Access denied for user to database '{name}'"),
        ));
    }
    // Any other name is accepted and becomes this connection's label: one file
    // is one schema, and the name a client picked is only ever cosmetic.
    Ok(())
}

/// The fields of a `HandshakeResponse41`.
struct HandshakeResponse {
    capabilities: u32,
    username: String,
    auth_response: Vec<u8>,
    database: Option<String>,
    auth_plugin: String,
}

/// Written by hand rather than derived, so the authentication token cannot be
/// printed by accident. It is not the password, but it is the material a
/// replay would need, and it has no business in a log line or a panic message.
impl std::fmt::Debug for HandshakeResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandshakeResponse")
            .field("capabilities", &format_args!("{:#010x}", self.capabilities))
            .field("username", &self.username)
            .field(
                "auth_response",
                &format_args!("<{} bytes, redacted>", self.auth_response.len()),
            )
            .field("database", &self.database)
            .field("auth_plugin", &self.auth_plugin)
            .finish()
    }
}

fn parse_handshake_response(payload: &[u8]) -> Result<HandshakeResponse, MysqlError> {
    let malformed = || MysqlError::unknown("malformed handshake response");
    let mut reader = Reader::new(payload);
    let capabilities = reader.u32().map_err(|_| malformed())?;

    // A pre-4.1 client cannot be served: everything below assumes the 4.1
    // layout, so guessing would misread the packet rather than fail cleanly.
    if capabilities & protocol::CLIENT_PROTOCOL_41 == 0 {
        return Err(MysqlError::new(
            1043,
            "08S01",
            "Bad handshake: this server requires a client speaking the 4.1 protocol or newer",
        ));
    }
    // An SSL request packet is only the first 32 bytes and stops there.
    if capabilities & protocol::CLIENT_SSL != 0 {
        return Ok(HandshakeResponse {
            capabilities,
            username: String::new(),
            auth_response: Vec::new(),
            database: None,
            auth_plugin: String::new(),
        });
    }

    reader.u32().map_err(|_| malformed())?; // max packet size
    reader.u8().map_err(|_| malformed())?; // charset
    reader.take(23).map_err(|_| malformed())?; // reserved
    let username = reader.nul_str().map_err(|_| malformed())?.to_string();

    let auth_response = if capabilities & protocol::CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
        reader
            .lenenc_bytes()
            .map_err(|_| malformed())?
            .unwrap_or_default()
            .to_vec()
    } else if capabilities & protocol::CLIENT_SECURE_CONNECTION != 0 {
        let length = reader.u8().map_err(|_| malformed())? as usize;
        reader.take(length).map_err(|_| malformed())?.to_vec()
    } else {
        reader
            .nul_str()
            .map_err(|_| malformed())?
            .as_bytes()
            .to_vec()
    };

    let database = if capabilities & protocol::CLIENT_CONNECT_WITH_DB != 0 {
        Some(reader.nul_str().map_err(|_| malformed())?.to_string())
    } else {
        None
    };

    let auth_plugin = if capabilities & protocol::CLIENT_PLUGIN_AUTH != 0 {
        reader.nul_str().unwrap_or("").to_string()
    } else {
        String::new()
    };

    Ok(HandshakeResponse {
        capabilities,
        username,
        auth_response,
        database,
        auth_plugin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_row_marks_nulls_with_the_reserved_byte() {
        let packet = text_row(&[Value::Null, Value::Integer(7)]);
        assert_eq!(packet, vec![0xfb, 1, b'7']);
    }

    #[test]
    fn a_binary_row_offsets_its_null_bitmap_by_two_bits() {
        let defs = vec![ColumnDef::integer("a"), ColumnDef::integer("b")];
        let packet = binary_row(&[Value::Null, Value::Integer(1)], &defs);
        assert_eq!(packet[0], 0x00, "the row header");
        // Column 0 is NULL, so bit 2 is set; column 1 is not, so bit 3 is clear.
        assert_eq!(packet[1], 0b0000_0100);
        assert_eq!(&packet[2..], &1i64.to_le_bytes());
    }

    #[test]
    fn binary_parameters_decode_at_every_width() {
        let cases: Vec<(u8, Vec<u8>, Value)> = vec![
            (0x01, vec![0xff], Value::Integer(-1)),
            (0x02, 300u16.to_le_bytes().to_vec(), Value::Integer(300)),
            (0x03, 70000u32.to_le_bytes().to_vec(), Value::Integer(70000)),
            (0x08, 5i64.to_le_bytes().to_vec(), Value::Integer(5)),
            (0x05, 1.5f64.to_le_bytes().to_vec(), Value::Real(1.5)),
        ];
        for (ty, bytes, expected) in cases {
            let mut reader = Reader::new(&bytes);
            assert_eq!(
                decode_binary_param(&mut reader, ty, false).unwrap(),
                expected,
                "type {ty:#x}"
            );
        }
    }

    #[test]
    fn an_unsigned_tiny_is_not_sign_extended() {
        let bytes = vec![0xff];
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            decode_binary_param(&mut reader, 0x01, true).unwrap(),
            Value::Integer(255)
        );
    }

    #[test]
    fn a_string_parameter_becomes_text_and_a_blob_stays_bytes() {
        let bytes = vec![3, b'a', b'b', b'c'];
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            decode_binary_param(&mut reader, 0xfe, false).unwrap(),
            Value::Text("abc".to_string().into())
        );
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            decode_binary_param(&mut reader, 0xfc, false).unwrap(),
            Value::Blob(b"abc".to_vec())
        );
    }

    /// A temporal parameter has no engine type behind it, but it must still be
    /// consumed exactly, or every parameter after it is read from the wrong
    /// offset.
    #[test]
    fn a_datetime_parameter_is_consumed_exactly() {
        let mut bytes = vec![7];
        bytes.extend_from_slice(&2026u16.to_le_bytes());
        bytes.extend_from_slice(&[8, 17, 14, 30, 5]);
        bytes.push(42); // a byte that must still be there afterwards
        let mut reader = Reader::new(&bytes);
        assert_eq!(
            decode_binary_param(&mut reader, 0x0c, false).unwrap(),
            Value::Text("2026-08-17 14:30:05".to_string().into())
        );
        assert_eq!(reader.u8().unwrap(), 42);
    }

    #[test]
    fn a_short_parameter_packet_is_an_error_not_a_panic() {
        let bytes = vec![1, 2];
        let mut reader = Reader::new(&bytes);
        assert!(decode_binary_param(&mut reader, 0x08, false).is_err());
    }

    #[test]
    fn a_pre_41_client_is_refused_clearly() {
        let payload = 0u32.to_le_bytes().to_vec();
        let error = parse_handshake_response(&payload).unwrap_err();
        assert_eq!(error.code, 1043);
    }

    #[test]
    fn a_handshake_response_round_trips() {
        let capabilities = protocol::CLIENT_PROTOCOL_41
            | protocol::CLIENT_SECURE_CONNECTION
            | protocol::CLIENT_CONNECT_WITH_DB
            | protocol::CLIENT_PLUGIN_AUTH;
        let mut payload = capabilities.to_le_bytes().to_vec();
        payload.extend_from_slice(&16_777_216u32.to_le_bytes());
        payload.push(45);
        payload.extend_from_slice(&[0u8; 23]);
        payload.extend_from_slice(b"root\0");
        payload.push(3);
        payload.extend_from_slice(b"abc");
        payload.extend_from_slice(b"app\0");
        payload.extend_from_slice(b"mysql_native_password\0");

        let response = parse_handshake_response(&payload).unwrap();
        assert_eq!(response.username, "root");
        assert_eq!(response.auth_response, b"abc");
        assert_eq!(response.database.as_deref(), Some("app"));
        assert_eq!(response.auth_plugin, "mysql_native_password");
    }

    #[test]
    fn mysqls_own_schemas_are_not_pretended_to_exist() {
        assert!(check_database("mysql").is_err());
        assert!(check_database("information_schema").is_err());
        assert!(check_database("app").is_ok());
        assert!(check_database("").is_ok());
    }
}
