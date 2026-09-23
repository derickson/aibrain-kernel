//! Postgres: the pool, the schema, and every query.
//!
//! All SQL lives here so there is one place to look when the schema moves.
//! Queries are written to return whole composed answers rather than rows the
//! caller then loops over — the Python side reaches this over HTTP, and a
//! round trip per note is exactly the shape that made the previous design
//! unable to grow.

use anyhow::{Context, Result};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use std::time::Duration;

pub mod lifecycle;
pub mod queries;
pub mod todo;

pub use lifecycle::*;
pub use queries::*;

/// Connect, with retries, then bring the schema up to date.
///
/// Retrying rather than failing fast is deliberate: in dev this process starts
/// alongside a Postgres container that takes a second or two to accept
/// connections, and podman-compose's health gating is not reliable enough to
/// depend on.
pub async fn connect(url: &str) -> Result<PgPool> {
    let mut last = None;
    for attempt in 0..30 {
        match PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
        {
            Ok(pool) => {
                migrate(&pool).await?;
                return Ok(pool);
            }
            Err(err) => {
                if attempt == 0 {
                    tracing::info!("waiting for postgres at {}", redact(url));
                }
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    Err(last.unwrap()).context(format!("could not reach postgres at {}", redact(url)))
}

/// Apply the schema. Every statement is idempotent, so this runs on every
/// start rather than tracking versions in a table.
pub async fn migrate(pool: &PgPool) -> Result<()> {
    // Order matters only in that later files may depend on earlier ones.
    let files = [
        include_str!("migrations/0001_corpus.sql"),
        include_str!("migrations/0002_search_queue.sql"),
        include_str!("migrations/0003_todo.sql"),
        include_str!("migrations/0004_todo_folders.sql"),
        include_str!("migrations/0006_index_lifecycle.sql"),
    ];
    // `pg_trgm` needs its own statement boundary and may fail without
    // superuser; the index that depends on it is optional, so a failure there
    // degrades search rather than stopping startup.
    for sql in files {
        for statement in split_statements(sql) {
            if let Err(err) = sqlx::query(&statement).execute(pool).await {
                if statement.contains("pg_trgm") || statement.contains("gin_trgm_ops") {
                    tracing::warn!("skipping trigram index: {err}");
                    continue;
                }
                return Err(err)
                    .context(format!("migration failed on: {}", first_line(&statement)));
            }
        }
    }
    Ok(())
}

fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("--") {
            continue;
        }
        current.push_str(line);
        current.push('\n');
        if line.trim_end().ends_with(';') {
            if !current.trim().is_empty() {
                out.push(current.clone());
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

fn first_line(sql: &str) -> String {
    sql.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(80)
        .collect()
}

/// Hide the password when a connection string goes into a log line.
pub fn redact(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme), Some(at)) if at > scheme + 3 => {
            format!("{}://***{}", &url[..scheme], &url[at..])
        }
        _ => url.to_string(),
    }
}

/// Read a `TEXT[]` column without panicking on NULL.
pub(crate) fn text_array(row: &PgRow, column: &str) -> Vec<String> {
    row.try_get::<Vec<String>, _>(column).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_on_semicolons_and_skip_comments() {
        let sql = "-- a comment\nCREATE TABLE a (x INT);\n\n-- another\nCREATE INDEX i ON a (x);\n";
        let out = split_statements(sql);
        assert_eq!(out.len(), 2);
        assert!(out[0].contains("CREATE TABLE"));
        assert!(!out[0].contains("comment"));
    }

    #[test]
    fn the_real_migration_splits_cleanly() {
        let out = split_statements(include_str!("migrations/0001_corpus.sql"));
        assert!(out.len() > 10, "expected many statements, got {}", out.len());
        for statement in &out {
            assert!(
                statement.trim_end().ends_with(';'),
                "statement does not terminate: {}",
                first_line(statement)
            );
        }
    }

    #[test]
    fn the_queue_migration_splits_cleanly() {
        let out = split_statements(include_str!("migrations/0002_search_queue.sql"));
        assert_eq!(out.len(), 3, "table plus two indexes");
        // The CHECK constraint's parenthesised list must survive intact — a
        // splitter that broke on it would create a table with no constraint.
        assert!(out[0].contains("CHECK (op IN ('upsert', 'delete'))"));
        assert!(out[0].contains("UNIQUE (brain_id, rel_path)"));
        for statement in &out {
            assert!(statement.trim_end().ends_with(';'));
        }
    }

    #[test]
    fn passwords_do_not_reach_the_log() {
        assert_eq!(
            redact("postgres://aibrain:secret@localhost:5433/aibrain"),
            "postgres://***@localhost:5433/aibrain"
        );
        assert_eq!(redact("not a url"), "not a url");
    }
}
