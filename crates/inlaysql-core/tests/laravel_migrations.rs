//! Laravel's default migrations, statement by statement, and Eloquent's
//! ordinary traffic afterwards.
//!
//! This is the phase's own acceptance criterion made checkable: `docs/architecture.md`'s
//! goal is "a stock framework can use InlaySQL as if it were a normal MySQL
//! server", and the honest way to report progress against that is to run what
//! the framework actually emits and say precisely which statements do not go
//! through yet.
//!
//! So the test asserts **both** lists. Every statement in `ACCEPTED` has to
//! work, and every statement in `REFUSED` has to fail — because a refusal
//! turning into a silent acceptance is exactly the regression this repo cares
//! most about, and a refusal quietly becoming support should be noticed and
//! moved into the other list rather than going unremarked.
//!
//! The SQL is Laravel 11's, as its SQLite grammar emits it (quoted
//! identifiers, `autoincrement`, a separate `create unique index`), plus a
//! sample of its MySQL grammar to show where the Phase 3 shim has to do work.

use inlaysql_core::mem::{LogicalClock, MemIndexFactory, MemStorage};
use inlaysql_core::{Engine, Error};

/// Everything a stock `php artisan migrate` and the Eloquent traffic after it
/// emits that this engine runs today.
const ACCEPTED: &[&str] = &[
    // The migrator's own bookkeeping table, then the default migrations.
    r#"create table "migrations" ("id" integer primary key autoincrement not null, "migration" varchar not null, "batch" integer not null)"#,
    r#"create table "users" ("id" integer primary key autoincrement not null, "name" varchar not null, "email" varchar not null, "email_verified_at" datetime, "password" varchar not null, "remember_token" varchar, "created_at" datetime, "updated_at" datetime)"#,
    r#"create unique index "users_email_unique" on "users" ("email")"#,
    r#"create table "password_reset_tokens" ("email" varchar not null, "token" varchar not null, "created_at" datetime, primary key ("email"))"#,
    r#"create table "sessions" ("id" varchar not null, "user_id" integer, "ip_address" varchar, "user_agent" text, "payload" text not null, "last_activity" integer not null, primary key ("id"))"#,
    r#"create index "sessions_user_id_index" on "sessions" ("user_id")"#,
    r#"create table "cache" ("key" varchar not null, "value" text not null, "expiration" integer not null, primary key ("key"))"#,
    r#"create table "jobs" ("id" integer primary key autoincrement not null, "queue" varchar not null, "payload" text not null, "attempts" integer not null, "reserved_at" integer, "available_at" integer not null, "created_at" integer not null)"#,
    r#"create table "failed_jobs" ("id" integer primary key autoincrement not null, "uuid" varchar not null, "connection" text not null, "queue" text not null, "payload" longtext not null, "exception" longtext not null, "failed_at" datetime not null default CURRENT_TIMESTAMP)"#,
    r#"create unique index "failed_jobs_uuid_unique" on "failed_jobs" ("uuid")"#,
    r#"create table "job_batches" ("id" varchar not null, "name" varchar not null, "total_jobs" integer not null, "pending_jobs" integer not null, "failed_jobs" integer not null, "failed_job_ids" text not null, "options" text, "cancelled_at" integer, "created_at" integer not null, "finished_at" integer, primary key ("id"))"#,
    // The migrator recording what it ran.
    r#"insert into "migrations" ("migration", "batch") values ('2014_10_12_000000_create_users_table', 1)"#,
    r#"select "migration", "batch" from "migrations" where "batch" >= 1 order by "batch" asc, "migration" asc"#,
    // Eloquent create / read / update / upsert / delete.
    r#"insert into "users" ("name", "email", "password", "created_at", "updated_at") values ('a', 'a@example.com', 'x', '2024-01-01 00:00:00', '2024-01-01 00:00:00')"#,
    r#"select * from "users" where "email" = 'a@example.com' limit 1"#,
    r#"update "users" set "name" = 'b', "updated_at" = '2024-01-02 00:00:00' where "id" = 1"#,
    r#"insert into "users" ("id", "name", "email", "password") values (1, 'c', 'c@example.com', 'y') on conflict ("id") do update set "name" = "excluded"."name""#,
    r#"delete from "users" where "id" = 1"#,
    // A later migration changing a table's shape.
    r#"alter table "users" add column "phone" varchar"#,
    r#"alter table "users" rename column "phone" to "mobile""#,
    r#"alter table "users" drop column "mobile""#,
    r#"drop table if exists "job_batches""#,
];

/// What a stock migration emits that this engine does *not* run yet, each with
/// the phase that owns it. None of these is an oversight.
const REFUSED: &[(&str, &str)] = &[
    // Phase 3, decision D1: MySQL's grammar is a shim in the server crate, not
    // a dialect change in core. `auto_increment` is not SQLite syntax and does
    // not parse.
    (
        "create table `posts` (`id` bigint unsigned not null auto_increment primary key, `title` varchar(255) not null, `user_id` bigint unsigned not null)",
        "sql parser error",
    ),
    // Likewise: SQLite has no `ALTER TABLE ... ADD CONSTRAINT`, so a foreign
    // key added after the fact is MySQL-only.
    (
        "alter table `posts` add constraint `posts_user_id_foreign` foreign key (`user_id`) references `users` (`id`) on delete cascade",
        "is not supported",
    ),
    // AHL-475: a qualified column on the left of `SET` — Eloquent writes this
    // on every save of a model with timestamps (`update users set name = ?,
    // users.updated_at = ?`). Verified directly against `sqlite3`: it refuses
    // this even when the qualifier names the statement's own target table, so
    // this stays refused here too — `crates/inlaysql-server`'s shim is where
    // a MySQL client's own qualifier is checked and stripped before the
    // statement ever reaches this parser.
    (
        r#"update "users" set "name" = 'b', "users"."updated_at" = '2024-01-02 00:00:00' where "id" = 1"#,
        "qualified column",
    ),
    // The same shape, same reason, inside `ON CONFLICT DO UPDATE SET`.
    (
        r#"insert into "users" ("id", "name", "email", "password") values (1, 'c', 'c@example.com', 'y') on conflict ("id") do update set "users"."name" = "excluded"."name""#,
        "qualified column",
    ),
];

fn engine() -> Engine {
    Engine::open(
        Box::new(MemStorage::new()),
        Box::new(MemIndexFactory),
        Box::new(LogicalClock::default()),
    )
    .expect("open")
}

#[test]
fn a_stock_migration_runs() {
    let mut engine = engine();
    for sql in ACCEPTED {
        engine.execute(sql, &[]).unwrap_or_else(|e| {
            panic!("a stock Laravel statement was refused:\n  {sql}\n  -> {e}")
        });
    }
}

#[test]
fn what_a_stock_migration_still_hits_is_refused_rather_than_absorbed() {
    let mut engine = engine();
    // Build the tables the refused statements refer to.
    for sql in ACCEPTED
        .iter()
        .take_while(|sql| !sql.contains("insert into"))
    {
        engine.execute(sql, &[]).expect("setup");
    }

    for (sql, expected) in REFUSED {
        let error = engine
            .execute(sql, &[])
            .map(|_| ())
            .expect_err(&format!("`{sql}` is now accepted — move it to ACCEPTED"));
        assert!(
            error.to_string().contains(expected),
            "`{sql}` failed with `{error}`, which does not mention `{expected}`"
        );
        assert!(
            matches!(
                error,
                Error::Type(_) | Error::Unsupported(_) | Error::Parse(_)
            ),
            "`{sql}` failed with {error:?}, which is not a refusal"
        );
    }
}
