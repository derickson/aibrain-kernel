//! A brain's Elasticsearch index, from provisioning to retirement.
//!
//! Postgres is where the decisions are recorded; `es::worker` carries them out
//! against the cluster. Keeping the decision durable is the point: removing a
//! vault while Elasticsearch is unreachable still removes it, and the index
//! delete happens whenever the cluster comes back.
//!
//! See design_concepts/RECOMMENDATION_ELASTICSEARCH.md.

use anyhow::Result;
use serde::Serialize;
use sqlx::{PgPool, Postgres, Row, Transaction};

/// Finished retirements are kept this long as an audit trail.
const RETIREMENT_KEEP_DAYS: i32 = 30;

/// What the worker needs to know about one brain's index.
#[derive(Debug, Clone)]
pub struct BrainIndex {
    pub id: String,
    pub name: String,
    pub uid: String,
    pub index_name: Option<String>,
    pub ready: bool,
    pub adopt_legacy: bool,
}

fn brain_index(r: &sqlx::postgres::PgRow) -> BrainIndex {
    BrainIndex {
        id: r.get("id"),
        name: r.get("name"),
        uid: r.get("uid"),
        index_name: r.get("index_name"),
        ready: r.get("ready"),
        adopt_legacy: r.get("adopt_legacy"),
    }
}

const BRAIN_INDEX_COLUMNS: &str =
    "id, name, uid, index_name, index_ready_at IS NOT NULL AS ready, adopt_legacy";

pub async fn brain_indices(pool: &PgPool) -> Result<Vec<BrainIndex>> {
    let rows = sqlx::query(&format!("SELECT {BRAIN_INDEX_COLUMNS} FROM brain ORDER BY id"))
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(brain_index).collect())
}

/// Brains whose index has not been provisioned, or has been lost since.
pub async fn unprovisioned(pool: &PgPool) -> Result<Vec<BrainIndex>> {
    let rows = sqlx::query(&format!(
        "SELECT {BRAIN_INDEX_COLUMNS} FROM brain WHERE index_ready_at IS NULL ORDER BY id"
    ))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(brain_index).collect())
}

/// Record the old, name-derived index for every brain that predates uids.
///
/// Done once at startup, before anything can retire such a brain, so that a
/// removal always knows which index to drop. When two old vault names fold to
/// the same index, the first keeps it and the other is given a fresh uid index
/// instead — sharing one index is exactly the bug uids exist to prevent.
pub async fn assign_legacy_index_names(
    pool: &PgPool,
    legacy_name: impl Fn(&str) -> String,
) -> Result<u64> {
    let rows = sqlx::query(
        "SELECT id, name FROM brain
          WHERE adopt_legacy AND index_name IS NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    let mut assigned = 0;
    for row in rows {
        let id: String = row.get("id");
        let name: String = row.get("name");
        let index = legacy_name(&name);
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM brain WHERE index_name = $1)
                 OR EXISTS (SELECT 1 FROM index_retirement
                             WHERE index_name = $1 AND done_at IS NULL)",
        )
        .bind(&index)
        .fetch_one(pool)
        .await?;
        if taken {
            tracing::warn!("{name}: legacy index {index} is already claimed; giving it a new one");
            sqlx::query("UPDATE brain SET adopt_legacy = FALSE WHERE id = $1")
                .bind(&id)
                .execute(pool)
                .await?;
            continue;
        }
        sqlx::query("UPDATE brain SET index_name = $2 WHERE id = $1 AND index_name IS NULL")
            .bind(&id)
            .bind(&index)
            .execute(pool)
            .await?;
        assigned += 1;
    }
    Ok(assigned)
}

/// Record the index a brain will use, before it is created.
///
/// Scoped to the uid, so a brain retired (and perhaps re-added under the same
/// id) while provisioning was in flight is left alone. `false` means the brain
/// generation this was for is gone and the caller must not create anything.
pub async fn claim_index_name(pool: &PgPool, id: &str, uid: &str, index: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE brain SET index_name = $3, index_ready_at = NULL
          WHERE id = $1 AND uid = $2",
    )
    .bind(id)
    .bind(uid)
    .bind(index)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The index exists with its aliases; the worker may now write to it.
pub async fn mark_index_ready(pool: &PgPool, id: &str, uid: &str) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE brain SET index_ready_at = now(), adopt_legacy = FALSE
          WHERE id = $1 AND uid = $2",
    )
    .bind(id)
    .bind(uid)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// The index or its alias went missing. Stop claiming this brain's rows until
/// provisioning has put it back.
pub async fn mark_index_lost(pool: &PgPool, id: &str, uid: &str) -> Result<()> {
    sqlx::query("UPDATE brain SET index_ready_at = NULL WHERE id = $1 AND uid = $2")
        .bind(id)
        .bind(uid)
        .execute(pool)
        .await?;
    Ok(())
}

/// Give up on adopting the old index (it was not there) and use a uid one.
pub async fn abandon_legacy(pool: &PgPool, id: &str, uid: &str) -> Result<()> {
    sqlx::query(
        "UPDATE brain SET adopt_legacy = FALSE, index_name = NULL, index_ready_at = NULL
          WHERE id = $1 AND uid = $2",
    )
    .bind(id)
    .bind(uid)
    .execute(pool)
    .await?;
    Ok(())
}

// ── Serialising lifecycle changes ──────────────────────────────────────────
//
// A reindex that read the config before a vault was removed would otherwise
// re-insert the brain row after the removal committed, and queue every note
// in it for embedding again. Reindex holds a shared lock per brain while it
// scans; retirement takes the same key exclusively, so the two cannot overlap.
// Transaction-scoped, so an error path that drops the guard releases it.

const LOCK_KEY: &str = "hashtext('aibrain:brain:' || $1)";

/// Held while one brain is scanned. Drop or `release` it when done.
pub struct BrainLock(Transaction<'static, Postgres>);

impl BrainLock {
    pub async fn shared(pool: &PgPool, brain_id: &str) -> Result<BrainLock> {
        let mut tx = pool.begin().await?;
        sqlx::query(&format!("SELECT pg_advisory_xact_lock_shared({LOCK_KEY})"))
            .bind(brain_id)
            .execute(&mut *tx)
            .await?;
        Ok(BrainLock(tx))
    }

    pub async fn release(self) -> Result<()> {
        self.0.commit().await?;
        Ok(())
    }
}

// ── Retirement ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Retired {
    pub brain_id: String,
    pub uid: String,
    /// `None` when the brain never had an index — nothing to drop.
    pub index_name: Option<String>,
}

/// Remove a brain and record that its index must go. One transaction.
///
/// Deleting the row cascades to its notes, links, layout cache and — through
/// `search_queue_brain_fk` — every queued write for it, so nothing is left for
/// the worker to send. The index delete itself is written to
/// `index_retirement` and carried out by the worker, whether or not the
/// cluster is reachable right now. Idempotent: a brain already gone is `None`.
pub async fn retire_brain(pool: &PgPool, brain_id: &str) -> Result<Option<Retired>> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("SELECT pg_advisory_xact_lock({LOCK_KEY})"))
        .bind(brain_id)
        .execute(&mut *tx)
        .await?;
    let Some(row) = sqlx::query("SELECT uid, index_name FROM brain WHERE id = $1 FOR UPDATE")
        .bind(brain_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        tx.commit().await?;
        return Ok(None);
    };
    let uid: String = row.get("uid");
    let index_name: Option<String> = row.get("index_name");

    if let Some(index) = &index_name {
        request_retirement(&mut tx, index, brain_id, &uid).await?;
    }
    sqlx::query("DELETE FROM brain WHERE id = $1")
        .bind(brain_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(Retired { brain_id: brain_id.to_string(), uid, index_name }))
}

async fn request_retirement(
    tx: &mut Transaction<'_, Postgres>,
    index: &str,
    brain_id: &str,
    uid: &str,
) -> Result<()> {
    // An index name retired before and somehow back (an orphan found again)
    // is simply retired again.
    sqlx::query(
        "INSERT INTO index_retirement (index_name, brain_id, uid)
         VALUES ($1, $2, $3)
         ON CONFLICT (index_name) DO UPDATE
            SET brain_id = EXCLUDED.brain_id, uid = EXCLUDED.uid,
                requested_at = now(), attempts = 0, last_error = NULL,
                not_before = now(), done_at = NULL",
    )
    .bind(index)
    .bind(brain_id)
    .bind(uid)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Retire an index that no brain owns (found by reconcile).
pub async fn retire_orphan(pool: &PgPool, index: &str, brain_id: &str, uid: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    request_retirement(&mut tx, index, brain_id, uid).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Retirement {
    pub index_name: String,
    pub brain_id: String,
    pub uid: String,
    pub attempts: i32,
    pub last_error: Option<String>,
    pub requested_at: chrono::DateTime<chrono::Utc>,
}

fn retirement(r: &sqlx::postgres::PgRow) -> Retirement {
    Retirement {
        index_name: r.get("index_name"),
        brain_id: r.get("brain_id"),
        uid: r.get("uid"),
        attempts: r.get("attempts"),
        last_error: r.get("last_error"),
        requested_at: r.get("requested_at"),
    }
}

/// Retirements whose backoff has elapsed, oldest first.
pub async fn due_retirements(pool: &PgPool, limit: i64) -> Result<Vec<Retirement>> {
    let rows = sqlx::query(
        "SELECT index_name, brain_id, uid, attempts, last_error, requested_at
           FROM index_retirement
          WHERE done_at IS NULL AND not_before <= now()
          ORDER BY requested_at LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(retirement).collect())
}

/// Every retirement not yet carried out, for /status.
pub async fn pending_retirements(pool: &PgPool) -> Result<Vec<Retirement>> {
    let rows = sqlx::query(
        "SELECT index_name, brain_id, uid, attempts, last_error, requested_at
           FROM index_retirement WHERE done_at IS NULL ORDER BY requested_at",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(retirement).collect())
}

/// Index names with a retirement on file, and whether it has been carried
/// out. Reconcile and adoption must never claim one of these.
pub async fn retired_index_names(
    pool: &PgPool,
) -> Result<std::collections::HashMap<String, bool>> {
    let rows = sqlx::query("SELECT index_name, done_at IS NOT NULL AS done FROM index_retirement")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| (r.get("index_name"), r.get("done"))).collect())
}

/// The brain whose recorded index this is, if any.
pub async fn index_owner(pool: &PgPool, index: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT id FROM brain WHERE index_name = $1")
        .bind(index)
        .fetch_optional(pool)
        .await?)
}

pub async fn finish_retirement(pool: &PgPool, index: &str) -> Result<()> {
    sqlx::query(
        "UPDATE index_retirement SET done_at = now(), last_error = NULL WHERE index_name = $1",
    )
    .bind(index)
    .execute(pool)
    .await?;
    // Housekeeping rides along: nobody needs a year of finished rows.
    sqlx::query(
        "DELETE FROM index_retirement
          WHERE done_at < now() - make_interval(days => $1)",
    )
    .bind(RETIREMENT_KEEP_DAYS)
    .execute(pool)
    .await?;
    Ok(())
}

/// A retirement the cluster refused (not merely could not be reached). Same
/// backoff shape as the queue: 2^attempts seconds, capped at an hour.
pub async fn fail_retirement(pool: &PgPool, index: &str, why: &str) -> Result<()> {
    sqlx::query(
        "UPDATE index_retirement
            SET attempts = attempts + 1, last_error = $2,
                not_before = now() + make_interval(
                    secs => least(3600.0, power(2.0, least(attempts + 1, 12))::double precision))
          WHERE index_name = $1",
    )
    .bind(index)
    .bind(why.chars().take(500).collect::<String>())
    .execute(pool)
    .await?;
    Ok(())
}

// ── What /status reports ───────────────────────────────────────────────────

pub async fn dead_letter_count(pool: &PgPool) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT count(*) FROM search_dead_letter")
        .fetch_one(pool)
        .await?)
}

#[derive(Debug, Clone, Serialize)]
pub struct BrainQueue {
    pub brain_id: String,
    pub index_name: Option<String>,
    pub ready: bool,
    pub queued: i64,
}

pub async fn queue_by_brain(pool: &PgPool) -> Result<Vec<BrainQueue>> {
    let rows = sqlx::query(
        "SELECT b.id, b.index_name, b.index_ready_at IS NOT NULL AS ready,
                count(q.id) AS queued
           FROM brain b LEFT JOIN search_queue q ON q.brain_id = b.id
          GROUP BY b.id ORDER BY b.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| BrainQueue {
            brain_id: r.get("id"),
            index_name: r.get("index_name"),
            ready: r.get("ready"),
            queued: r.get("queued"),
        })
        .collect())
}
