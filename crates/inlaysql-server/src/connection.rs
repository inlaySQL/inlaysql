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
use std::sync::Arc;

use inlaysql::{Database, Error, Outcome, ResultSet, Value};

use crate::acl;
use crate::auth;
use crate::control::{Asker, Control, KillScope, Registry};
use crate::errors::{from_engine, MysqlError};
use crate::packet::{put_lenenc_bytes, put_lenenc_int, Malformed, Reader, Stream};
use crate::protocol::{
    self, auth_more_data, auth_switch_request, column_def_for, eof_packet, err_packet, handshake,
    ok_packet, put_binary_value, streamed_column_def, text_value, ColumnDef, Command,
};
use crate::session::{Limits, Session, Warning, SERVER_VERSION};
use crate::shim::{self, Intercepted};
use crate::sqltext;

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
    /// Kept because authentication builds a second [`Session`] once the user
    /// name is known, and both must report the same enforced numbers.
    limits: Limits,
    /// The `--user`/`--password` credential, as verifiers. It is the whole
    /// account model on a database that has never had an account created in
    /// it, and is ignored entirely on one that has — see [`acl::install`].
    bootstrap: acl::Bootstrap,
    /// This connection's cancellation state. Shared with the engine (which
    /// reads it from inside its scan loops) and with the accept loop's
    /// registry (which is how another connection's `KILL` reaches it).
    control: Arc<Control>,
    /// Every live connection, so a `KILL` here can reach one of them.
    registry: Arc<Registry>,
}

impl<S: Read + Write> Connection<S> {
    /// Wrap an accepted connection.
    pub fn new(
        read_half: S,
        write_half: S,
        db: Database,
        control: Arc<Control>,
        limits: Limits,
        bootstrap: acl::Bootstrap,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            stream: Stream::new(read_half, write_half),
            db,
            session: Session::new(Arc::clone(&control), "", None, limits),
            statements: HashMap::new(),
            next_statement_id: 1,
            limits,
            bootstrap,
            control,
            registry,
        }
    }

    /// Authenticate, then serve commands until the client disconnects.
    pub fn serve(&mut self) -> io::Result<()> {
        if !self.authenticate()? {
            return Ok(());
        }
        loop {
            let Some(message) = self.stream.read_message()? else {
                return Ok(());
            };
            if !self.dispatch(&message)? {
                return Ok(());
            }
            // A `KILL CONNECTION` that landed while this connection was running
            // a statement has been answered — the client got its error packet —
            // and now the connection itself goes. Checked after the reply
            // rather than before it so the client is told why, instead of
            // seeing a socket close with nothing on it. (A `KILL` that lands
            // while this connection is *idle* does not wait for this: it shuts
            // the socket down, and the `read_message` above returns.)
            if self.control.is_closing() {
                return Ok(());
            }
        }
    }

    // ------------------------------------------------------------- auth

    fn authenticate(&mut self) -> io::Result<bool> {
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

        // The account is read from the file before a byte of the exchange is
        // checked. A name with no account gets a stand-in that no password
        // satisfies ([`acl::Account::unknown`]) rather than an early refusal,
        // so the exchange runs to the same length and fails at the same step:
        // a guesser must not be able to enumerate accounts by watching how far
        // a login gets before it is turned away.
        let account = match acl::account(&mut self.db, &response.username, &self.bootstrap) {
            Ok(Some(account)) => account,
            Ok(None) => acl::Account::unknown(&response.username),
            // The store itself could not be read. That is a broken database,
            // not a bad password, and saying "access denied" would send an
            // operator looking for the wrong thing.
            Err(error) => {
                self.fail(&error)?;
                return Ok(false);
            }
        };

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
            auth::CACHING_SHA2_PASSWORD if account.speaks(auth::CACHING_SHA2_PASSWORD) => {
                match self.caching_sha2_authenticate(
                    &account,
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
            auth::NATIVE_PASSWORD if account.speaks(auth::NATIVE_PASSWORD) => (
                account.verify_native(&challenge, &response.auth_response),
                !response.auth_response.is_empty(),
            ),
            // Either a plugin this server does not speak at all, or one this
            // account has no verifier for because `IDENTIFIED WITH` pinned it
            // to the other. Both are answered the way MySQL answers them:
            // `AuthSwitchRequest` naming a plugin the account can complete.
            _ => {
                let Some(switch_to) = account.preferred_plugin() else {
                    // An account with no verifier at all cannot authenticate
                    // under anything. Refused as a wrong password, not as a
                    // different condition, so it says nothing new about the
                    // account.
                    self.fail(&MysqlError::access_denied(&response.username, false))?;
                    return Ok(false);
                };
                self.stream
                    .write_message(&auth_switch_request(switch_to, &challenge))?;
                self.stream.flush()?;
                let Some(token) = self.stream.read_message()? else {
                    return Ok(false);
                };
                match switch_to {
                    auth::CACHING_SHA2_PASSWORD => {
                        match self.caching_sha2_authenticate(&account, &challenge, &token)? {
                            Some(outcome) => outcome,
                            None => return Ok(false),
                        }
                    }
                    _ => (account.verify_native(&challenge, &token), !token.is_empty()),
                }
            }
        };

        if !password_ok {
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
            Arc::clone(&self.control),
            &response.username,
            response.database.clone(),
            self.limits,
        );
        // Recorded on the shared control, not only in the session: `KILL` runs
        // on somebody else's thread and has to be able to ask whose connection
        // this is without touching state this thread owns.
        self.control.set_user(&response.username);
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
        account: &acl::Account,
        challenge: &[u8],
        initial_response: &[u8],
    ) -> io::Result<Option<(bool, bool)>> {
        // A 32-byte response is the plugin's fast-authentication attempt —
        // every real client sends one optimistically, hoping the server has
        // something to check it against. This one always does: the account
        // carries exactly the digest real MySQL's in-memory cache would hold,
        // on disk from the moment it was created, so there is no "cache miss"
        // case to fall back from.
        if initial_response.len() == 32 {
            let ok = account.verify_caching_sha2(challenge, initial_response);
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

        let ok = account.verify_caching_sha2_cleartext(&payload);
        Ok(Some((ok, !payload.is_empty())))
    }

    // --------------------------------------------------- authorisation

    /// This connection's account, as it stands **now**.
    ///
    /// Read from the file on every statement rather than captured at login.
    /// That is what makes a `REVOKE` or a `DROP USER` take effect on an
    /// already-connected session's *next statement*: a copy taken at
    /// authentication would keep working for as long as the client held the
    /// socket, which for a pooled connection is indefinitely.
    ///
    /// The one window left is an explicit transaction: a handle inside one
    /// reads its pinned snapshot, so a grant changed by another connection
    /// mid-transaction is not visible until it ends. That is the engine's
    /// isolation working as designed, and it is documented rather than worked
    /// around — see `docs/server.md`.
    fn account(&mut self) -> Result<acl::Account, MysqlError> {
        let user = self.session.user.clone();
        let bootstrap = self.bootstrap.clone();
        match acl::account(&mut self.db, &user, &bootstrap)? {
            Some(account) => Ok(account),
            // The account was dropped while this session was connected. MySQL
            // lets such a session run on; this refuses its next statement,
            // which is the only reading of `DROP USER` that does not leave a
            // removed account with an open door for as long as it keeps its
            // socket.
            None => Err(acl::account_gone(&user)),
        }
    }

    /// Authorise a statement **this server** answers.
    ///
    /// Statements that reach the engine return `Ok` here and are authorised
    /// from their plan in [`Self::authorize_plan`] instead — the plan is the
    /// only thing that knows every table a subquery reaches, and authorising
    /// from the text would be a bypass rather than an approximation.
    fn authorize_statement(&mut self, sql: &str) -> Result<(), MysqlError> {
        if !shim::handles(sql) {
            return Ok(());
        }
        let requirement = acl::shim_requirement(sql, &self.session);
        let account = self.account()?;
        acl::enforce(&mut self.db, &account, &requirement)
    }

    /// Authorise a planned statement, from what the plan says it touches.
    fn authorize_plan(&mut self, plan: &inlaysql::Statement) -> Result<(), MysqlError> {
        let requirement = acl::plan_requirement(&plan.table_access(), self.db.catalog());
        let account = self.account()?;
        acl::enforce(&mut self.db, &account, &requirement)
    }

    /// Plan `sql` **and authorise it**.
    ///
    /// The only place in this file that turns statement text into a plan the
    /// engine will run. Everything that executes goes through here or through
    /// [`Self::authorize_plan`] on a plan that was kept from a previous
    /// `COM_STMT_PREPARE` — so there is no path to the engine that skipped the
    /// check, which matters more than where the check is written: the call
    /// site nobody remembers is the vulnerability.
    ///
    /// `fresh` picks between [`Database::prepare_fresh`] and
    /// [`Database::prepare`] for the same reason the callers always did — see
    /// [`Self::run_on_engine`].
    fn prepare_authorized(
        &mut self,
        sql: &str,
        fresh: bool,
    ) -> Result<inlaysql::Statement, MysqlError> {
        let plan = if fresh {
            self.db.prepare_fresh(sql).map_err(|e| from_engine(&e))?
        } else {
            self.db.prepare(sql).map_err(|e| from_engine(&e))?
        };
        self.authorize_plan(&plan)?;
        Ok(plan)
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
            // `mysqladmin kill`, and older drivers. Same operation as the
            // `KILL` statement, same authorisation, same registry — only the
            // spelling differs, so it goes through the same function.
            Command::ProcessKill => {
                let target = Reader::new(body).u32();
                match target {
                    Ok(target) => match self.kill(target, KillScope::Connection) {
                        Ok(()) => self.ok(0, 0)?,
                        Err(error) => self.fail(&error)?,
                    },
                    Err(_) => self.fail(&MysqlError::unknown(
                        "malformed COM_PROCESS_KILL packet: it carries a four-byte connection id",
                    ))?,
                }
            }
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
            Ok(Answer::Rows { rows, plan }) => self.write_result_set(&rows, plan.as_ref(), false),
            Ok(Answer::Streamed(plan)) => self.stream_answer(&plan, sql, &[], false, None),
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

        // Before the shim is allowed to *do* anything with it: `handle_set`
        // mutates the session, `USE` changes the schema, and an account
        // statement writes to the store. A check after that point would be a
        // check after the effect.
        self.authorize_statement(sql)?;

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
            // No plan behind it, so no declared column types either: a `SHOW`
            // or an `information_schema` answer is described by its values
            // alone, exactly as it was before streaming existed. It is also
            // bounded by construction — the catalog, not a table — which is
            // why it is not worth streaming.
            Intercepted::Rows(rows) => Ok(Answer::Rows {
                rows: *rows,
                plan: None,
            }),
            Intercepted::Acl(statement) => {
                // MySQL commits an open transaction before an account
                // statement, and so does this — for a sharper reason than
                // parity: a `REVOKE` that a later `ROLLBACK` could undo is a
                // `REVOKE` that did not happen, after the client was told it
                // had.
                self.commit()?;
                match acl::execute(&mut self.db, &self.session, &self.bootstrap, &statement)? {
                    acl::Effect::Done => Ok(Answer::ok()),
                    acl::Effect::Rows(rows) => Ok(Answer::Rows {
                        rows: *rows,
                        plan: None,
                    }),
                }
            }
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
            Intercepted::Kill {
                connection_id,
                scope,
            } => {
                self.kill(connection_id, scope)?;
                Ok(Answer::ok())
            }
            Intercepted::PassThrough => self.run_on_engine(sql, params),
        }
    }

    /// `KILL`, from either the statement or `COM_PROCESS_KILL`.
    ///
    /// The privilege check lives in [`Registry::kill`] so both spellings get
    /// the same one; what this adds is *who is asking*, which is read fresh
    /// from the account store rather than from anything cached at login — so a
    /// superuser whose grant was revoked mid-session cannot still kill other
    /// people's connections, for the same reason every other statement
    /// re-reads it.
    ///
    /// Killing your own connection is allowed, and a `KILL CONNECTION` on
    /// yourself takes the socket out from under the OK packet this would have
    /// answered with — the shutdown happens inside the registry, before there
    /// is a reply to write. The client sees a closed connection, which is what
    /// it asked for and what MySQL gives it; the failed write ends this
    /// connection's loop. `KILL QUERY` on yourself is the harmless case: there
    /// is no statement running but this one, so it stops nothing and the OK
    /// goes out normally.
    fn kill(&mut self, target: u32, scope: KillScope) -> Result<(), MysqlError> {
        let account = self.account()?;
        let asker = Asker {
            connection_id: self.session.connection_id,
            user: self.session.user.clone(),
            superuser: account.is_superuser(),
        };
        self.registry.kill(target, scope, &asker)
    }

    /// Run one statement the engine owns.
    ///
    /// Planned before it runs, where the old shape ran and planned in one call.
    /// The plan is the only thing that can answer "which columns, of what
    /// type" *before* the first row exists, and the MySQL protocol demands
    /// that answer up front: the column-definition packets go out before any
    /// row does. So it decides which of the two shapes below this statement
    /// gets — the answer written to the socket as the engine produces it, or
    /// the whole answer built in memory and written afterwards.
    ///
    /// [`Database::prepare_fresh`] and not `prepare`: planning has to see DDL
    /// another connection committed, which is what `Database::execute` did by
    /// refreshing before it parsed.
    fn run_on_engine(&mut self, sql: &str, params: &[Value]) -> Result<Answer, MysqlError> {
        self.begin_implicit()?;
        let plan = self.prepare_authorized(sql, true)?;
        if streamed_column_defs(&plan).is_some() {
            return Ok(Answer::Streamed(plan));
        }

        let before = self.db.last_insert_row_id();
        let outcome = match self.db.execute_prepared(&plan, params) {
            // The schema moved between planning and running, which is only
            // possible when another connection committed DDL in that window.
            // Planning again is always the right answer and the client never
            // sees the condition — the same recovery `run_plan` makes for a
            // statement whose plan is *kept* across executions, where the
            // window is the whole life of the prepared statement rather than
            // two calls.
            Err(Error::Stale(_)) => {
                let fresh = self.prepare_authorized(sql, true)?;
                let outcome = self
                    .db
                    .execute_prepared(&fresh, params)
                    .map_err(|e| from_engine(&e))?;
                return Ok(self.finish(outcome, before, Some(&fresh)));
            }
            other => other.map_err(|e| from_engine(&e))?,
        };
        Ok(self.finish(outcome, before, Some(&plan)))
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
        //
        // It is still authorised, because a statement that reaches OK with no
        // privilege check is exactly the hole this design exists to close, and
        // "it happens to do nothing" is a property of today's translation
        // rather than a rule. There is no plan to attribute it to a table, so
        // the requirement is the global `ALTER` the statement asked for —
        // default-deny, since a per-table grant cannot satisfy it.
        if statements.is_empty() {
            let account = self.account()?;
            acl::enforce(
                &mut self.db,
                &account,
                &acl::Requirement::Needs(vec![acl::Need {
                    table: None,
                    privilege: acl::Privileges::ALTER,
                }]),
            )?;
            return Ok(Answer::ok());
        }

        // One statement is the overwhelmingly common shape here — a `SELECT`
        // whose only translation was renaming `LENGTH(...)` to
        // `octet_length(...)`, say — and it is the only shape that can be
        // streamed, since the loop below has to run each of several statements
        // to completion before it knows what to answer with. Handing it to
        // `run_on_engine` is what stops a query losing the streamed path over a
        // function name: the rows are the same rows either way, and building
        // them all in memory first is exactly what this change exists to stop.
        if let [only] = statements {
            let count = sqltext::count_placeholders(only);
            if count > params.len() {
                return Err(too_few_parameters(only, count, params.len()));
            }
            return self.run_on_engine(only, &params[..count]);
        }

        self.begin_implicit()?;
        let mut remaining = params;
        let mut answer = Answer::ok();
        for statement in statements {
            let count = sqltext::count_placeholders(statement);
            if count > remaining.len() {
                return Err(too_few_parameters(statement, count, remaining.len()));
            }
            let (this_statement, rest) = remaining.split_at(count);
            remaining = rest;

            let before = self.db.last_insert_row_id();
            // Planned and authorised one at a time rather than handed to
            // `Database::execute`, which would plan internally and leave this
            // sequence as the one path to the engine with no privilege check
            // on it. Each statement is planned *fresh* because the one before
            // it may have created the table this one indexes.
            let plan = self.prepare_authorized(statement, true)?;
            let outcome = match self.db.execute_prepared(&plan, this_statement) {
                Err(Error::Stale(_)) => {
                    let fresh = self.prepare_authorized(statement, true)?;
                    self.db
                        .execute_prepared(&fresh, this_statement)
                        .map_err(|error| from_engine(&error))?
                }
                other => other.map_err(|error| from_engine(&error))?,
            };
            answer = self.finish(outcome, before, None);
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
    ///
    /// `plan` is the statement that produced `outcome`, where the caller has
    /// one. It is carried into the reply for its *column types*: a column that
    /// came back empty, or all `NULL`, has no value to infer a wire type from,
    /// and the plan's declared type is the only honest answer left. See
    /// [`crate::protocol::column_def_for`].
    fn finish(
        &mut self,
        outcome: Outcome,
        before: Option<u64>,
        plan: Option<&inlaysql::Statement>,
    ) -> Answer {
        match outcome {
            Outcome::Rows(rows) => Answer::Rows {
                rows,
                plan: plan.cloned(),
            },
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

        // Authorised here as well as at execution, which is MySQL's own
        // behaviour and is only ever *earlier* than the check that matters:
        // `run`/`run_plan` check again when the statement runs, so a privilege
        // revoked in between is still caught. Preparing something you may not
        // run should say so at the prepare, not at the first execute.
        if let Err(error) = self.authorize_statement(&normalized) {
            return self.fail(&error);
        }

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
                    match self.prepare_authorized(only, false) {
                        Ok(plan) => Prepared {
                            param_count: plan.parameter_count(),
                            sql: only.clone(),
                            plan: Some(plan),
                            param_types: Vec::new(),
                            warnings,
                        },
                        Err(error) => return self.fail(&error),
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
            Ok(Answer::Rows { rows, plan }) => self.write_result_set(&rows, plan.as_ref(), true),
            Ok(Answer::Streamed(plan)) => self.stream_answer(&plan, &sql, &params, true, Some(id)),
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
        // Re-authorised on every execution, not once when it was prepared. A
        // prepared statement outlives the grant that allowed it: a client that
        // prepares `SELECT * FROM salaries`, has its SELECT revoked, and then
        // executes must be refused, and a check at prepare time alone would
        // let it through for the life of the connection.
        self.authorize_plan(&plan)?;

        // A read whose columns the plan can describe up front is answered from
        // the socket outwards; staleness is discovered by the streaming call
        // instead of here, before a byte is written, and repaired there.
        if streamed_column_defs(&plan).is_some() {
            return Ok(Answer::Streamed(plan));
        }

        let outcome = match self.db.execute_prepared(&plan, params) {
            Err(Error::Stale(_)) => {
                let fresh = self.prepare_authorized(sql, false)?;
                if let Some(prepared) = self.statements.get_mut(&id) {
                    prepared.plan = Some(fresh.clone());
                }
                let outcome = self
                    .db
                    .execute_prepared(&fresh, params)
                    .map_err(|e| from_engine(&e))?;
                return Ok(self.finish(outcome, before, Some(&fresh)));
            }
            other => other.map_err(|e| from_engine(&e))?,
        };
        Ok(self.finish(outcome, before, Some(&plan)))
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

    /// Write a result set that is already entirely in memory.
    ///
    /// `plan` is the statement that produced it, where there is one; see
    /// [`Self::finish`] and [`column_def_for`] for what it decides.
    fn write_result_set(
        &mut self,
        rows: &ResultSet,
        plan: Option<&inlaysql::Statement>,
        binary: bool,
    ) -> io::Result<()> {
        // A result set with no columns is not representable: the leading count
        // would be zero, which a client reads as an OK packet.
        if rows.columns.is_empty() {
            return self.ok(0, 0);
        }

        let schema = shim::schema_name(&self.session);
        let declared = plan.map(|plan| plan.columns()).unwrap_or_default();
        let defs: Vec<ColumnDef> = rows
            .columns
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let ty = declared.get(index).and_then(|column| column.ty);
                column_def_for(name.clone(), ty, &rows.rows, index)
            })
            .collect();

        let status = self.session.status_flags();
        let warnings = self.session.warning_count();
        write_result_set_header(&mut self.stream, &defs, &schema, status, warnings)?;

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

    /// Answer a read by writing its rows to the socket as the engine produces
    /// them, re-planning once if the schema moved under the plan.
    ///
    /// `cached_as` is the prepared-statement id the plan is kept under, when
    /// it is kept at all. Refreshing it on a re-plan is what stops every
    /// execution after a `CREATE INDEX` paying for the same re-plan.
    fn stream_answer(
        &mut self,
        plan: &inlaysql::Statement,
        sql: &str,
        params: &[Value],
        binary: bool,
        cached_as: Option<u32>,
    ) -> io::Result<()> {
        match self.write_streamed_result_set(plan, params, binary)? {
            Streamed::Done => return Ok(()),
            // Nothing reached the wire, so every recovery a materialised
            // statement has is still open — and a plan the schema moved under
            // has exactly one: plan it again, below.
            Streamed::NothingWritten(Error::Stale(_)) => {}
            Streamed::NothingWritten(error) => return self.fail(&from_engine(&error)),
        }

        let fresh = match self.prepare_authorized(sql, true) {
            Ok(fresh) => fresh,
            Err(error) => return self.fail(&error),
        };
        if let Some(id) = cached_as {
            if let Some(prepared) = self.statements.get_mut(&id) {
                prepared.plan = Some(fresh.clone());
            }
        }
        // The re-planned statement need not still be streamable — a column the
        // old plan knew the type of may have been dropped or redeclared — so
        // the fallback is the materialising path, not a second stream.
        if streamed_column_defs(&fresh).is_none() {
            return match self.db.query_prepared(&fresh, params) {
                Ok(rows) => self.write_result_set(&rows, Some(&fresh), binary),
                Err(error) => self.fail(&from_engine(&error)),
            };
        }
        match self.write_streamed_result_set(&fresh, params, binary)? {
            Streamed::Done => Ok(()),
            Streamed::NothingWritten(error) => self.fail(&from_engine(&error)),
        }
    }

    /// One pass of [`Self::stream_answer`]: the column definitions, then every
    /// row as the engine hands it over, then the terminating EOF.
    ///
    /// Nothing here retains a row. The engine's row callback lends one row at
    /// a time — reusing a single projected-row allocation for a non-blocking
    /// query — and it is encoded straight into a packet and handed to the
    /// socket's buffered writer, so the memory this holds is the widest single
    /// row plus one write buffer, whether the answer is ten rows or ten
    /// million. That is the whole point: `SELECT * FROM big_table` used to
    /// build every row of the answer in memory before the client could read
    /// the first one, which is one query taking the server down.
    fn write_streamed_result_set(
        &mut self,
        plan: &inlaysql::Statement,
        params: &[Value],
        binary: bool,
    ) -> io::Result<Streamed> {
        let Some(defs) = streamed_column_defs(plan) else {
            // Only ever built from a plan this already answered `Some` for, so
            // reaching here is a bug above rather than anything a client did.
            return Ok(Streamed::NothingWritten(Error::Unsupported(
                "this statement's columns cannot be described before it runs".to_string(),
            )));
        };
        let schema = shim::schema_name(&self.session);
        let status = self.session.status_flags();
        let warnings = self.session.warning_count();

        // Split borrow: the row callback writes to the socket while the engine
        // holds the database handle. They are different fields, and saying so
        // here is what lets one closure use both.
        let Connection { stream, db, .. } = self;

        // The header is written from *inside* the callback, not before the
        // call. Everything the engine can fail at before its first row —
        // validating a plan the schema moved under, resolving a bound `LIMIT`,
        // a read error from storage — then still has nothing on the wire, so
        // it can be answered, or retried, like any other failure. After one
        // row is out none of that is available: the protocol has no way to
        // un-send a packet.
        let mut header_written = false;
        let mut io_error: Option<io::Error> = None;

        let outcome = db.query_prepared_each(plan, params, |row| {
            let mut write = || -> io::Result<()> {
                if !header_written {
                    write_result_set_header(stream, &defs, &schema, status, warnings)?;
                    header_written = true;
                }
                let packet = if binary {
                    binary_row(row, &defs)
                } else {
                    text_row(row)
                };
                stream.write_message(&packet)
            };
            match write() {
                Ok(()) => Ok(()),
                // The client stopped reading, or the connection died. The
                // engine only needs to be told to stop; the real error is
                // carried out in `io_error`, because an `io::Error` has no
                // home in the engine's own error type and inventing one would
                // report a client disconnection as a database failure.
                Err(error) => {
                    io_error = Some(error);
                    Err(Error::Storage("the client stopped reading".to_string()))
                }
            }
        });

        // Checked before the engine's own error, which in this case is only the
        // stand-in that stopped the scan. Nothing can be reported to a client
        // whose socket is what failed, so this leaves the connection to end.
        if let Some(error) = io_error {
            return Err(error);
        }

        match outcome {
            Ok(_) => {
                if !header_written {
                    write_result_set_header(stream, &defs, &schema, status, warnings)?;
                }
                stream.write_message(&eof_packet(status, warnings))?;
                stream.flush()?;
                Ok(Streamed::Done)
            }
            Err(error) if !header_written => Ok(Streamed::NothingWritten(error)),
            // Rows are already on the wire and the protocol cannot recall
            // them, so the result set is terminated by an ERR packet where its
            // final EOF would have gone. That is MySQL's own answer to the
            // same problem — a `SELECT` killed or failing part-way through
            // ends its row stream with an ERR packet — and every client
            // decodes it, because it is the same packet it already watches for
            // in place of the *first* one.
            Err(error) => {
                let error = from_engine(&error);
                stream.write_message(&err_packet(error.code, error.sqlstate, &error.message))?;
                stream.flush()?;
                Ok(Streamed::Done)
            }
        }
    }
}

/// `ER_WRONG_ARGUMENTS`: a translated statement wants more bound values than
/// the client's original statement carried.
fn too_few_parameters(statement: &str, needed: usize, remaining: usize) -> MysqlError {
    MysqlError::new(
        1210,
        "HY000",
        format!(
            "Incorrect arguments to EXECUTE: `{statement}` needs {needed} \
             parameter(s), only {remaining} remain"
        ),
    )
}

/// The column count, the column definitions and the EOF that ends them — the
/// metadata every result set opens with, in either protocol.
fn write_result_set_header<S: Read + Write>(
    stream: &mut Stream<S>,
    defs: &[ColumnDef],
    schema: &str,
    status: u16,
    warnings: u16,
) -> io::Result<()> {
    let mut count = Vec::new();
    put_lenenc_int(&mut count, defs.len() as u64);
    stream.write_message(&count)?;
    for def in defs {
        stream.write_message(&def.encode(schema))?;
    }
    stream.write_message(&eof_packet(status, warnings))
}

/// The wire description of every column a statement projects, or `None` if any
/// one of them cannot be described before the statement runs.
///
/// This is the whole streaming decision, and it is made from the plan alone —
/// never from the table's size, which is not knowable and would not matter if
/// it were. See [`streamed_column_def`] for what makes a column describable.
fn streamed_column_defs(plan: &inlaysql::Statement) -> Option<Vec<ColumnDef>> {
    // A write is excluded twice over: the engine refuses a row callback on one
    // (a callback may fail part-way, and a consumer's error must not look like
    // a failed statement after a mutation already committed), and `affected
    // rows` is not a result set anyway.
    if !plan.is_read_only() {
        return None;
    }
    let columns = plan.columns();
    if columns.is_empty() {
        return None;
    }
    columns
        .iter()
        .map(|column| streamed_column_def(column.name.clone(), column.ty))
        .collect()
}

/// What running a statement produced.
enum Answer {
    Ok {
        affected: u64,
        insert_id: u64,
    },
    /// Rows already built in memory, with the plan that produced them when
    /// the engine was the one that did.
    Rows {
        rows: ResultSet,
        plan: Option<inlaysql::Statement>,
    },
    /// A read whose rows go to the socket as the engine produces them. The
    /// statement has been planned and not yet run.
    Streamed(inlaysql::Statement),
}

impl Answer {
    fn ok() -> Self {
        Answer::Ok {
            affected: 0,
            insert_id: 0,
        }
    }
}

/// How far a streamed result set got before it stopped.
enum Streamed {
    /// The whole result set reached the socket, terminating packet included —
    /// EOF if it ended, ERR if it failed after rows had already been sent.
    Done,
    /// The engine failed before a single packet was written, so the caller
    /// still has every option an unstreamed failure has.
    NothingWritten(Error),
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
