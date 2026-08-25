//! The `inlaysql` command.
//!
//! ```sh
//! inlaysql serve --mcp app.inlay [--allow-writes] [--max-rows N] [--max-bytes N]
//! inlaysql serve --mysql app.inlay [--port N] [--bind ADDR] [--user U]
//! inlaysql changes app.inlay [--from N]
//! inlaysql backup app.inlay app-2026-08-25.inlay
//! inlaysql vacuum app.inlay
//! ```

use std::io::{self, BufReader};
use std::process::ExitCode;

use inlaysql::{Database, SourceAccess};
use inlaysql_mcp::{Limits, Server};
use inlaysql_server::{Server as MysqlServer, ServerOptions};

const USAGE: &str = "\
inlaysql — an embedded database with first-class hybrid retrieval

USAGE:
    inlaysql serve --mcp <database> [OPTIONS]
    inlaysql serve --mysql <database> [OPTIONS]
    inlaysql changes <database> [--from <version>]
    inlaysql backup <database> <destination>
    inlaysql vacuum <database>

SERVE --mcp OPTIONS:
    --allow-writes     Expose the `execute` tool. Off by default: the client is
                       a language model and the database is somebody's data.
    --max-rows <n>     Most rows a tool call returns (default 200).
    --max-bytes <n>    Most bytes a tool call returns (default 65536).

SERVE --mysql OPTIONS:
    --bind <addr>      Address to listen on (default 127.0.0.1, loopback only).
    --port <n>         Port to listen on (default 3306). 0 asks the OS for a
                       free one and prints what it got.
    --user <name>      The single accepted user name (default `root`).
    --password <pw>    Its password. Visible to `ps` — prefer --password-env.
    --password-env <VAR>
                       Read the password from this environment variable.
    --max-connections <n>
                       Most connections served at once (default 64).
    --wait-timeout <n> Seconds a connection may be silent before the server
                       closes it (default 28800, MySQL's own). Reported as
                       wait_timeout, and now actually enforced — without it,
                       --max-connections silent sockets hold every slot until
                       the process is restarted. Must be at least 1; for
                       effectively none, ask for a large one (31536000).
    --page-reuse       Reclaim pages a commit stopped using, instead of only
                       ever growing the file. Off by default.
                       READ THIS FIRST: a reclaimed page is overwritten in
                       place, so NOTHING may open this file read-only while
                       the server runs with it on — including `inlaysql serve
                       --mcp`, which opens read-only by default. A lock-free
                       read-only handle takes no lock and cannot be seen, so
                       it cannot be waited for; this is why the engine
                       defaults the option off. Without the flag, a database
                       under steady-state churn grows for ever and the only
                       way back is to stop the server and run `inlaysql
                       vacuum`, which needs the lock the server holds.
    --query-memory <bytes>
                       Most memory one statement may hold in an ORDER BY,
                       GROUP BY, DISTINCT or window step (default 536870912).
                       Those cannot answer before they have read every input
                       row, so without a ceiling one query is bounded only by
                       the machine — and the out-of-memory killer ends the
                       process, taking every other connection with it. Past
                       this, that one statement is refused and the server
                       keeps serving. PER STATEMENT: --max-connections
                       clients can each hold this much. 0 removes the
                       ceiling. Not a spill threshold; there is no spilling.

    SECURITY: the MySQL protocol is served in PLAINTEXT. This version has no
    TLS, so every statement, every result and every credential crosses the
    connection in the clear. It listens on 127.0.0.1 unless --bind says
    otherwise; binding anywhere else exposes an unencrypted database to the
    network and should only be done across a link you already trust.

    There is one user and one password, given here or in the environment —
    there is no user table, no grants and no per-table permissions.

    The SQL surface is a subset: a stock ORM's migrations will NOT run yet.
    `docs/server.md` lists exactly what works and what does not.

CHANGES OPTIONS:
    --from <version>   Start after this version. 0 (the default) means the whole
                       retained log.

BACKUP:
    Copies a consistent snapshot of <database> to <destination>, while
    whatever is writing to it keeps writing. The destination is an ordinary
    database file — open it, or move it back over the original to restore;
    there is no restore command because there is nothing for one to do.

    Refuses to overwrite an existing <destination>. A failure leaves no file
    behind at all, so a backup that exists is a backup that finished.

    Opens the source read-write when it can, and read-only when a server
    already holds it — it says which. Those are not equally strong: a
    read-write backup pins its snapshot against page reclamation and is sound
    even under `serve --mysql --page-reuse`, while a read-only one takes no
    lock and cannot be seen by the writer that would do the reclaiming. It
    refuses outright if it finds the source recording reclaimable pages, but
    do not take a read-only backup of a file a writer has --page-reuse on for.

    Not a compaction: page numbers are preserved, so a file that grew large
    from deletes produces a copy with holes in it (sparse — its live size on
    disk, its old size on paper). Use vacuum for that.

VACUUM:
    Compacts the database file in place: copies every table, constraint and
    index into a fresh file, then atomically replaces the original with it.
    For shrinking a file after a large one-time DELETE — day-to-day growth
    from ordinary churn is what page reuse is for (`serve --mysql
    --page-reuse`, or EngineOptions::page_reuse when embedding), not this.
    Needs an exclusive lock (refuses if another handle has the file open for
    writing) and free disk space for a second full copy while it runs.

The MCP server speaks JSON-RPC over stdin/stdout, so it is wired into a client
by pointing that client's command at it. Nothing is written to stdout except
protocol messages; diagnostics go to stderr.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("inlaysql: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("changes") => changes(&args[1..]),
        Some("backup") => backup(&args[1..]),
        Some("vacuum") => vacuum(&args[1..]),
        Some("--help") | Some("-h") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

/// `serve` picks a protocol from its first flag, so the two modes never share
/// an option list and a typo cannot silently start the wrong server.
fn serve(args: &[String]) -> Result<(), String> {
    match args
        .iter()
        .find(|arg| arg.starts_with("--"))
        .map(String::as_str)
    {
        Some("--mysql") => serve_mysql(args),
        Some("--mcp") => serve_mcp(args),
        _ => Err(format!(
            "serve needs --mcp <database> or --mysql <database>\n\n{USAGE}"
        )),
    }
}

fn serve_mysql(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut options = ServerOptions::default();
    let mut password_given = false;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--mysql" => path = rest.next().cloned(),
            "--bind" => {
                options.bind = rest
                    .next()
                    .cloned()
                    .ok_or_else(|| "--bind needs an address".to_string())?
            }
            "--port" => options.port = number(rest.next(), "--port")? as u16,
            "--user" => {
                options.user = rest
                    .next()
                    .cloned()
                    .ok_or_else(|| "--user needs a name".to_string())?
            }
            "--password" => {
                options.password = rest
                    .next()
                    .cloned()
                    .ok_or_else(|| "--password needs a value".to_string())?;
                password_given = true;
            }
            "--password-env" => {
                let variable = rest
                    .next()
                    .ok_or_else(|| "--password-env needs a variable name".to_string())?;
                options.password = std::env::var(variable).map_err(|_| {
                    format!("--password-env: `{variable}` is not set in the environment")
                })?;
                password_given = true;
            }
            "--max-connections" => {
                options.max_connections = number(rest.next(), "--max-connections")?
            }
            "--wait-timeout" => {
                options.wait_timeout_secs = number(rest.next(), "--wait-timeout")? as u64
            }
            "--page-reuse" => options.page_reuse = true,
            "--query-memory" => options.query_memory_bytes = number(rest.next(), "--query-memory")?,
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    let path = path.ok_or_else(|| format!("serve needs --mysql <database>\n\n{USAGE}"))?;
    let server = MysqlServer::bind(&path, &options).map_err(|error| error.to_string())?;
    let address = server.local_addr().map_err(|error| error.to_string())?;

    // Everything below goes to stderr: it is diagnostics, and a caller
    // redirecting stdout should not have to filter it out.
    let mut log = io::stderr();
    inlaysql_server::print_exposure_warning(&options, &mut log)
        .map_err(|error| error.to_string())?;
    if password_given && args.iter().any(|arg| arg == "--password") {
        eprintln!(
            "inlaysql: WARNING: --password puts the password in this machine's process list; \n\
             inlaysql:          --password-env keeps it out of it."
        );
    }
    eprintln!(
        "inlaysql: serving {path} over the MySQL protocol on {address} as user `{}`",
        options.user
    );
    eprintln!(
        "inlaysql: the SQL surface is a subset — see docs/server.md for what does not work yet"
    );

    server.run().map_err(|error| error.to_string())
}

fn serve_mcp(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut allow_writes = false;
    let mut limits = Limits::default();

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--mcp" => path = rest.next().cloned(),
            "--allow-writes" => allow_writes = true,
            "--max-rows" => limits.max_rows = number(rest.next(), "--max-rows")?,
            "--max-bytes" => limits.max_bytes = number(rest.next(), "--max-bytes")?,
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    let path = path.ok_or_else(|| format!("serve needs --mcp <database>\n\n{USAGE}"))?;
    let mut server = Server::open(&path, allow_writes, limits).map_err(|e| e.to_string())?;

    // stderr, so it cannot be mistaken for a protocol message.
    eprintln!(
        "inlaysql: serving {path} over MCP ({}), {} rows / {} bytes per call",
        if allow_writes {
            "reads and writes"
        } else {
            "read-only"
        },
        limits.max_rows,
        limits.max_bytes
    );

    server
        .serve(BufReader::new(io::stdin()), io::stdout())
        .map_err(|error| error.to_string())
}

fn backup(args: &[String]) -> Result<(), String> {
    let mut positional = Vec::new();

    for arg in args {
        match arg.as_str() {
            other if !other.starts_with("--") => positional.push(other.to_string()),
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    let [source, destination] = positional.as_slice() else {
        return Err(format!(
            "backup needs a database and a destination\n\n{USAGE}"
        ));
    };

    let outcome = inlaysql::backup(source, destination).map_err(|e| e.to_string())?;

    // stderr, so `inlaysql backup` composes in a pipeline the same way every
    // other diagnostic in this file does.
    //
    // The access mode is reported rather than left implicit because it is the
    // one thing that decides how much the copy is worth, and the operator is
    // the only one who can act on it — see BACKUP in the usage text.
    match outcome.access {
        SourceAccess::Exclusive => eprintln!(
            "inlaysql: backed up {source} to {destination} \
             (snapshot {}, {} pages, exclusive)",
            outcome.summary.seq, outcome.summary.pages,
        ),
        SourceAccess::LockFree => eprintln!(
            "inlaysql: backed up {source} to {destination} \
             (snapshot {}, {} pages, read-only — another process holds the file \
             for writing; sound unless that writer has page reuse on)",
            outcome.summary.seq, outcome.summary.pages,
        ),
    }
    Ok(())
}

fn vacuum(args: &[String]) -> Result<(), String> {
    let mut path = None;

    for arg in args {
        match arg.as_str() {
            other if path.is_none() && !other.starts_with("--") => path = Some(other.to_string()),
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    let path = path.ok_or_else(|| format!("vacuum needs a database\n\n{USAGE}"))?;
    inlaysql::vacuum(&path).map_err(|e| e.to_string())?;
    eprintln!("inlaysql: {path} vacuumed");
    Ok(())
}

fn changes(args: &[String]) -> Result<(), String> {
    let mut path = None;
    let mut from = 0u64;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--from" => from = number(rest.next(), "--from")? as u64,
            other if path.is_none() && !other.starts_with("--") => path = Some(other.to_string()),
            other => return Err(format!("unknown option `{other}`\n\n{USAGE}")),
        }
    }

    let path = path.ok_or_else(|| format!("changes needs a database\n\n{USAGE}"))?;
    let db = Database::open(&path).map_err(|e| e.to_string())?;
    let changes = db.changes(from).map_err(|e| e.to_string())?;

    if changes.lost(from) {
        eprintln!(
            "inlaysql: warning — changes before version {} were dropped from the log; \
             resynchronise with a full read",
            changes.floor
        );
    }
    for change in &changes.changes {
        println!(
            "{}\t{}\t{}\t{}",
            change.version,
            change.kind.as_str(),
            change.table,
            change.id
        );
    }
    eprintln!("inlaysql: at version {}", changes.version);
    Ok(())
}

fn number(value: Option<&String>, flag: &str) -> Result<usize, String> {
    value
        .ok_or_else(|| format!("{flag} needs a value"))?
        .parse()
        .map_err(|_| format!("{flag} needs a number"))
}
