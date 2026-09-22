//! Every statement the to-do list runs.
//!
//! Separate from `queries.rs` because the corpus and the list have opposite
//! natures: the corpus is derived from markdown and can be thrown away, the
//! list is a source of truth and is therefore append-only. Nothing here ever
//! deletes a row — `complete` and `cancel` stamp a timestamp and write an
//! event, and that history is what `/todos/history` reads back.
//!
//! Timestamps cross this boundary as instants, never as dates: which day an
//! instant belongs to depends on the configured start hour and the machine's
//! timezone, and that decision lives in `todo.rs`, once.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row, Transaction};

#[derive(Debug, Clone, Serialize)]
pub struct TodoRef {
    pub brain_id: String,
    pub rel_path: String,
    /// Filled when the note exists right now. A rename empties it without
    /// losing the path, which is what lets the link be repaired later.
    pub note_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TodoEvent {
    pub at: f64,
    pub kind: String,
    pub from_day: Option<String>,
    pub to_day: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Todo {
    pub id: i64,
    pub body: String,
    pub created_at: f64,
    pub first_scheduled_on: String,
    pub scheduled_on: String,
    pub completed_at: Option<f64>,
    pub cancelled_at: Option<f64>,
    pub sort_order: f64,
    /// "open", "completed" or "cancelled" — the browser strikes through the
    /// last two rather than working it out from two nullable timestamps.
    pub state: &'static str,
    pub refs: Vec<TodoRef>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<TodoEvent>,
    /// Set once an item has been filed into a folder — see todo_folder.rs
    /// doc-comment on the migration for what that exempts it from.
    pub folder_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TodoFolder {
    pub id: i64,
    pub name: String,
    pub collapsed: bool,
    pub sort_order: f64,
    pub todos: Vec<Todo>,
}

const COLUMNS: &str = "id, body, created_at, first_scheduled_on, scheduled_on, \
                       completed_at, cancelled_at, sort_order, folder_id";

fn epoch(at: DateTime<Utc>) -> f64 {
    at.timestamp() as f64 + at.timestamp_subsec_micros() as f64 / 1_000_000.0
}

fn row_to_todo(row: &sqlx::postgres::PgRow) -> Todo {
    let completed: Option<DateTime<Utc>> = row.get("completed_at");
    let cancelled: Option<DateTime<Utc>> = row.get("cancelled_at");
    Todo {
        id: row.get("id"),
        body: row.get("body"),
        created_at: epoch(row.get::<DateTime<Utc>, _>("created_at")),
        first_scheduled_on: row.get::<NaiveDate, _>("first_scheduled_on").to_string(),
        scheduled_on: row.get::<NaiveDate, _>("scheduled_on").to_string(),
        completed_at: completed.map(epoch),
        cancelled_at: cancelled.map(epoch),
        sort_order: row.get("sort_order"),
        state: match (cancelled.is_some(), completed.is_some()) {
            (true, _) => "cancelled",
            (_, true) => "completed",
            _ => "open",
        },
        refs: Vec::new(),
        events: Vec::new(),
        folder_id: row.get("folder_id"),
    }
}

/// Write one line of history. Every mutation calls this; nothing else does.
pub async fn log_event(
    tx: &mut Transaction<'_, Postgres>,
    todo_id: i64,
    at: DateTime<Utc>,
    kind: &str,
    from_day: Option<NaiveDate>,
    to_day: Option<NaiveDate>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO todo_event (todo_id, at, kind, from_day, to_day)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(todo_id)
    .bind(at)
    .bind(kind)
    .bind(from_day)
    .bind(to_day)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Move everything still open from before `day` onto `day`, logging each move.
///
/// Returns how many moved. Done on read rather than on a timer because a
/// missed midnight — laptop asleep, app not running — must not lose a day.
pub async fn roll_over(pool: &PgPool, day: NaiveDate, at: DateTime<Utc>) -> Result<usize> {
    let mut tx = pool.begin().await?;
    // FOR UPDATE, because two browser tabs opening the same day at once would
    // otherwise each log a move for the same item.
    let stale = sqlx::query(
        "SELECT id, scheduled_on FROM todo
          WHERE scheduled_on < $1 AND completed_at IS NULL AND cancelled_at IS NULL
            AND folder_id IS NULL
          ORDER BY id
            FOR UPDATE",
    )
    .bind(day)
    .fetch_all(&mut *tx)
    .await?;

    for row in &stale {
        let id: i64 = row.get("id");
        let from: NaiveDate = row.get("scheduled_on");
        sqlx::query("UPDATE todo SET scheduled_on = $1 WHERE id = $2")
            .bind(day)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        log_event(&mut tx, id, at, "rolled", Some(from), Some(day)).await?;
    }
    tx.commit().await?;
    Ok(stale.len())
}

/// The list for one day: what is open on it, plus what was finished or
/// abandoned during it. The finished ones are matched on an instant range
/// because "during it" depends on the start hour, which SQL does not know.
pub async fn for_day(
    pool: &PgPool,
    day: NaiveDate,
    from: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Result<Vec<Todo>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM todo
          WHERE folder_id IS NULL AND (
                (completed_at IS NULL AND cancelled_at IS NULL AND scheduled_on = $1)
             OR (completed_at >= $2 AND completed_at < $3)
             OR (cancelled_at >= $2 AND cancelled_at < $3)
          )
          ORDER BY sort_order, id"
    ))
    .bind(day)
    .bind(from)
    .bind(until)
    .fetch_all(pool)
    .await?;
    let mut todos: Vec<Todo> = rows.iter().map(row_to_todo).collect();
    attach_refs(pool, &mut todos).await?;
    Ok(todos)
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<Todo>> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM todo WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    let Some(row) = row else { return Ok(None) };
    let mut todos = vec![row_to_todo(&row)];
    attach_refs(pool, &mut todos).await?;
    Ok(todos.pop())
}

/// Search the history by body text, newest first, with every event attached.
pub async fn history(pool: &PgPool, query: &str, limit: i64) -> Result<Vec<Todo>> {
    let tsquery = super::to_tsquery(query, false);
    let rows = if tsquery.is_empty() {
        sqlx::query(&format!(
            "SELECT {COLUMNS} FROM todo ORDER BY created_at DESC LIMIT $1"
        ))
        .bind(limit)
        .fetch_all(pool)
        .await?
    } else {
        // A bad tsquery is someone typing, not a bug; fall back to listing.
        match sqlx::query(&format!(
            "SELECT {COLUMNS} FROM todo, to_tsquery('english', $1) q
              WHERE tsv @@ q
              ORDER BY ts_rank_cd(tsv, q) DESC, created_at DESC
              LIMIT $2"
        ))
        .bind(&tsquery)
        .bind(limit)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows,
            Err(err) => {
                tracing::debug!("todo history search failed for {tsquery:?}: {err}");
                Vec::new()
            }
        }
    };

    let mut todos: Vec<Todo> = rows.iter().map(row_to_todo).collect();
    attach_refs(pool, &mut todos).await?;
    attach_events(pool, &mut todos).await?;
    Ok(todos)
}

async fn attach_refs(pool: &PgPool, todos: &mut [Todo]) -> Result<()> {
    if todos.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = todos.iter().map(|t| t.id).collect();
    // The stored note_id can be stale — a reindex that deleted and re-created
    // a note nulls it. The path is the durable half, so resolve through it on
    // read and the link repairs itself without anyone running a repair.
    let rows = sqlx::query(
        "SELECT r.todo_id, r.brain_id, r.rel_path,
                COALESCE(r.note_id,
                         (SELECT n.id FROM note n
                           WHERE n.brain_id = r.brain_id AND n.rel_path = r.rel_path)
                        ) AS note_id
           FROM todo_ref r
          WHERE r.todo_id = ANY($1) ORDER BY r.id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let todo_id: i64 = row.get("todo_id");
        if let Some(todo) = todos.iter_mut().find(|t| t.id == todo_id) {
            todo.refs.push(TodoRef {
                brain_id: row.get("brain_id"),
                rel_path: row.get("rel_path"),
                note_id: row.get("note_id"),
            });
        }
    }
    Ok(())
}

async fn attach_events(pool: &PgPool, todos: &mut [Todo]) -> Result<()> {
    if todos.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = todos.iter().map(|t| t.id).collect();
    let rows = sqlx::query(
        "SELECT todo_id, at, kind, from_day, to_day FROM todo_event
          WHERE todo_id = ANY($1) ORDER BY at, id",
    )
    .bind(&ids)
    .fetch_all(pool)
    .await?;
    for row in rows {
        let todo_id: i64 = row.get("todo_id");
        if let Some(todo) = todos.iter_mut().find(|t| t.id == todo_id) {
            todo.events.push(TodoEvent {
                at: epoch(row.get::<DateTime<Utc>, _>("at")),
                kind: row.get("kind"),
                from_day: row.get::<Option<NaiveDate>, _>("from_day").map(|d| d.to_string()),
                to_day: row.get::<Option<NaiveDate>, _>("to_day").map(|d| d.to_string()),
            });
        }
    }
    Ok(())
}

/// Add an item to a day, at the end of it.
pub async fn create(
    pool: &PgPool,
    body: &str,
    day: NaiveDate,
    at: DateTime<Utc>,
    refs: &[(String, String)],
) -> Result<i64> {
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query(
        "INSERT INTO todo (body, created_at, first_scheduled_on, scheduled_on, sort_order)
         VALUES ($1, $2, $3, $3,
                 COALESCE((SELECT MAX(sort_order) FROM todo WHERE scheduled_on = $3), 0) + 1)
         RETURNING id",
    )
    .bind(body)
    .bind(at)
    .bind(day)
    .fetch_one(&mut *tx)
    .await?
    .get("id");

    log_event(&mut tx, id, at, "created", None, Some(day)).await?;
    for (brain_id, rel_path) in refs {
        insert_ref(&mut tx, id, brain_id, rel_path).await?;
        log_event(&mut tx, id, at, "linked", None, None).await?;
    }
    tx.commit().await?;
    Ok(id)
}

async fn insert_ref(
    tx: &mut Transaction<'_, Postgres>,
    todo_id: i64,
    brain_id: &str,
    rel_path: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO todo_ref (todo_id, brain_id, rel_path, note_id)
         VALUES ($1, $2, $3,
                 (SELECT id FROM note WHERE brain_id = $2 AND rel_path = $3))
         ON CONFLICT (todo_id, brain_id, rel_path) DO UPDATE
            SET note_id = EXCLUDED.note_id",
    )
    .bind(todo_id)
    .bind(brain_id)
    .bind(rel_path)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Attach a note. Both the path and the id are kept; see the migration.
pub async fn link(
    pool: &PgPool,
    id: i64,
    brain_id: &str,
    rel_path: &str,
    at: DateTime<Utc>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let exists: Option<i64> = sqlx::query("SELECT id FROM todo WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .map(|r| r.get("id"));
    if exists.is_none() {
        return Ok(false);
    }
    insert_ref(&mut tx, id, brain_id, rel_path).await?;
    log_event(&mut tx, id, at, "linked", None, None).await?;
    tx.commit().await?;
    Ok(true)
}

/// Mark done, undone, or abandoned. `kind` is the event that gets logged.
pub async fn set_state(
    pool: &PgPool,
    id: i64,
    kind: &str,
    at: DateTime<Utc>,
) -> Result<bool> {
    // `uncompleted` binds no instant, so the three cannot share one statement
    // — Postgres rejects a parameter the query never mentions.
    let sql = match kind {
        "completed" => "UPDATE todo SET completed_at = $2, cancelled_at = NULL WHERE id = $1",
        "uncompleted" => "UPDATE todo SET completed_at = NULL, cancelled_at = NULL WHERE id = $1",
        "cancelled" => "UPDATE todo SET cancelled_at = $2 WHERE id = $1",
        other => anyhow::bail!("unknown state {other}"),
    };
    let mut tx = pool.begin().await?;
    let statement = sqlx::query(sql).bind(id);
    let statement = if kind == "uncompleted" { statement } else { statement.bind(at) };
    let changed = statement.execute(&mut *tx).await?.rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    log_event(&mut tx, id, at, kind, None, None).await?;
    tx.commit().await?;
    Ok(true)
}

/// Move an item to another day. Uncompletes nothing: a finished item that is
/// rescheduled is a mistake being corrected, and the event says so.
pub async fn reschedule(
    pool: &PgPool,
    id: i64,
    to_day: NaiveDate,
    at: DateTime<Utc>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query("SELECT scheduled_on FROM todo WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(false);
    };
    let from: NaiveDate = row.get("scheduled_on");
    sqlx::query("UPDATE todo SET scheduled_on = $1 WHERE id = $2")
        .bind(to_day)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    log_event(&mut tx, id, at, "rescheduled", Some(from), Some(to_day)).await?;
    tx.commit().await?;
    Ok(true)
}

/// Edit the text or the position in the list.
pub async fn patch(
    pool: &PgPool,
    id: i64,
    body: Option<&str>,
    sort_order: Option<f64>,
    at: DateTime<Utc>,
) -> Result<bool> {
    if body.is_none() && sort_order.is_none() {
        return get(pool, id).await.map(|t| t.is_some());
    }
    let mut tx = pool.begin().await?;
    let changed = sqlx::query(
        "UPDATE todo
            SET body = COALESCE($2::text, body),
                sort_order = COALESCE($3::double precision, sort_order)
          WHERE id = $1",
    )
    .bind(id)
    .bind(body)
    .bind(sort_order)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    log_event(&mut tx, id, at, "edited", None, None).await?;
    tx.commit().await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// folders — a persistent backlog, exempt from for_day and roll_over above
// ---------------------------------------------------------------------------

/// Every folder, each with its member todos attached, ordered the way the
/// shelf renders them.
pub async fn list_folders(pool: &PgPool) -> Result<Vec<TodoFolder>> {
    let folder_rows = sqlx::query(
        "SELECT id, name, collapsed, sort_order FROM todo_folder ORDER BY sort_order, id",
    )
    .fetch_all(pool)
    .await?;
    let mut folders: Vec<TodoFolder> = folder_rows
        .iter()
        .map(|row| TodoFolder {
            id: row.get("id"),
            name: row.get("name"),
            collapsed: row.get("collapsed"),
            sort_order: row.get("sort_order"),
            todos: Vec::new(),
        })
        .collect();
    if folders.is_empty() {
        return Ok(folders);
    }

    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM todo WHERE folder_id IS NOT NULL ORDER BY folder_id, sort_order, id"
    ))
    .fetch_all(pool)
    .await?;
    let mut todos: Vec<Todo> = rows.iter().map(row_to_todo).collect();
    attach_refs(pool, &mut todos).await?;
    for todo in todos {
        if let Some(folder) = folders.iter_mut().find(|f| Some(f.id) == todo.folder_id) {
            folder.todos.push(todo);
        }
    }
    Ok(folders)
}

pub async fn create_folder(pool: &PgPool, name: &str) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO todo_folder (name, sort_order)
         VALUES ($1, COALESCE((SELECT MAX(sort_order) FROM todo_folder), 0) + 1)
         RETURNING id",
    )
    .bind(name)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

/// Edit a folder's name, its collapsed state, or its position among the
/// other folders. `None` leaves a field as it was.
pub async fn update_folder(
    pool: &PgPool,
    id: i64,
    name: Option<&str>,
    collapsed: Option<bool>,
    sort_order: Option<f64>,
) -> Result<bool> {
    let changed = sqlx::query(
        "UPDATE todo_folder
            SET name = COALESCE($2::text, name),
                collapsed = COALESCE($3::boolean, collapsed),
                sort_order = COALESCE($4::double precision, sort_order)
          WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(collapsed)
    .bind(sort_order)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(changed > 0)
}

/// Un-files every member (ON DELETE SET NULL) and drops the folder. A member
/// with a stale scheduled_on reappears on today's list via the next
/// roll_over — nothing here needs to touch todo rows itself.
pub async fn delete_folder(pool: &PgPool, id: i64) -> Result<bool> {
    let changed = sqlx::query("DELETE FROM todo_folder WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(changed > 0)
}

/// File an item into a folder, or (`folder_id: None`) take it back out.
/// Unfiling deliberately leaves `scheduled_on` alone — see the migration's
/// doc-comment for why that is enough to put it back in the daily rotation.
pub async fn file_todo(
    pool: &PgPool,
    id: i64,
    folder_id: Option<i64>,
    at: DateTime<Utc>,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    let sort_order: f64 = match folder_id {
        Some(fid) => {
            sqlx::query(
                "SELECT COALESCE(MAX(sort_order), 0) + 1 AS n FROM todo WHERE folder_id = $1",
            )
            .bind(fid)
            .fetch_one(&mut *tx)
            .await?
            .get("n")
        }
        None => 0.0,
    };
    let changed = if folder_id.is_some() {
        sqlx::query("UPDATE todo SET folder_id = $2, sort_order = $3 WHERE id = $1")
            .bind(id)
            .bind(folder_id)
            .bind(sort_order)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    } else {
        sqlx::query("UPDATE todo SET folder_id = NULL WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
    };
    if changed == 0 {
        return Ok(false);
    }
    let kind = if folder_id.is_some() { "filed" } else { "unfiled" };
    log_event(&mut tx, id, at, kind, None, None).await?;
    tx.commit().await?;
    Ok(true)
}
