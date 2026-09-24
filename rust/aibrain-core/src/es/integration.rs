//! End to end against a real cluster: ingest, drain, search, clean up.
//!
//! It lives in-module rather than in `tests/` because the crate is a binary,
//! and it skips rather than fails when the credentials are absent — a suite
//! that goes red on a laptop with no cluster is a suite people stop running.
//!
//!   AIBRAIN_TEST_DATABASE_URL=postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_es \
//!   ELASTICSEARCH_URL=... ELASTICSEARCH_API_KEY=... cargo test es::integration
//!
//! Indices are prefixed `aibrain-test-<pid>-` and deleted at the end, so a
//! real `aibrain-*` index is never touched.

use std::path::PathBuf;
use std::sync::Arc;

use super::{Es, EsConfig};
use crate::config::BrainSpec;
use crate::{db, es, ingest};

/// The vault the test indexes. Only "Sourdough" is about baking, and the word
/// "bread" appears nowhere — that is what makes the semantic assertion mean
/// something.
const NOTES: [(&str, &str); 6] = [
    (
        "Tides.md",
        "# Tides\n\nThe moon pulls the ocean up and down twice a day along the shoreline.\n",
    ),
    (
        "Sourdough.md",
        "# Sourdough\n\nWild yeast ferments flour and water into a tangy loaf over eighteen hours.\n",
    ),
    (
        "Quarterly.md",
        "# Quarterly Review\n\nRevenue grew and the team shipped the new onboarding flow ahead of schedule.\n",
    ),
    (
        "Sailing.md",
        "# Sailing\n\nBowlines, sheet bends and a reef knot are most of the rigging you need.\n",
    ),
    (
        "Compilers.md",
        "# Compilers\n\nA parser builds a tree and the back end walks it emitting machine code.\n",
    ),
    (
        "Garden.md",
        "# Garden\n\nTomatoes want deep mulch, full sun and a great deal more water than you think.\n",
    ),
];

/// Every test here shares one database, and the worker claims queue rows and
/// provisions brains globally — so a test running alongside another would
/// drain, provision or reprovision the other's brains under its own prefix.
/// One at a time. (live.rs creates a database per test instead.)
static SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

struct Harness {
    pool: sqlx::PgPool,
    es: Arc<Es>,
    brain: BrainSpec,
    dir: PathBuf,
}

impl Harness {
    /// The index the worker provisioned for the brain, from Postgres.
    async fn index(&self) -> String {
        index_of(&self.pool, &self.brain.id).await.expect("brain has an index")
    }
}

async fn index_of(pool: &sqlx::PgPool, brain_id: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT index_name FROM brain WHERE id = $1")
        .bind(brain_id)
        .fetch_optional(pool)
        .await
        .unwrap()
        .flatten()
}

async fn uid_of(pool: &sqlx::PgPool, brain_id: &str) -> String {
    sqlx::query_scalar("SELECT uid FROM brain WHERE id = $1")
        .bind(brain_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

async fn test_pool(what: &str) -> Option<sqlx::PgPool> {
    let Some(url) = env("AIBRAIN_TEST_DATABASE_URL") else {
        eprintln!("skipping {what}: AIBRAIN_TEST_DATABASE_URL is unset");
        return None;
    };
    match db::connect(&url).await {
        Ok(pool) => Some(pool),
        Err(err) => {
            eprintln!("skipping {what}: {err:#}");
            None
        }
    }
}

fn write_vault(dir: &std::path::Path) {
    std::fs::remove_dir_all(dir).ok();
    std::fs::create_dir_all(dir.join("Notes")).ok();
    for (name, text) in NOTES {
        std::fs::write(dir.join("Notes").join(name), text).unwrap();
    }
}

/// `None` with a printed reason when the test cannot run here.
///
/// Each test gets its own index prefix (`aibrain-test-<pid>-<tag>-`) so that
/// tests running in parallel, and the teardown that drops everything under a
/// prefix, never reach each other's indices.
async fn setup(tag: &str) -> Option<Harness> {
    let pool = test_pool("es integration").await?;
    if env("ELASTICSEARCH_URL").is_none() || env("ELASTICSEARCH_API_KEY").is_none() {
        eprintln!("skipping es integration: ELASTICSEARCH_URL / _API_KEY are unset");
        return None;
    }
    let pid = std::process::id();
    let mut cfg = EsConfig::from_env()?;
    cfg.index_prefix = format!("aibrain-test-{pid}-{tag}-");
    cfg.batch = 50;

    let dir = std::env::temp_dir().join(format!("aibrain-es-it-{pid}-{tag}"));
    write_vault(&dir);

    let brain = BrainSpec {
        id: format!("estest-{pid}-{tag}"),
        name: format!("EsTest {pid} {tag}"),
        root: dir.to_string_lossy().into_owned(),
        seed: 3,
        excludes: vec![],
        color: "#4db3f0".into(),
        group_by: Default::default(),
    };
    // The worker provisions, drains and reconciles every brain in the
    // database, so anything else left here — an earlier crashed run, or the
    // Python suite's fixtures on the same test database — would be given an
    // index under this test's prefix. A dedicated test database, and SERIAL
    // means no other test here is mid-flight, so clearing it is fair game.
    sqlx::query("DELETE FROM brain").execute(&pool).await.unwrap();

    Some(Harness { pool, es: Arc::new(Es::new(cfg).unwrap()), brain, dir })
}

/// Drop every brain and every index this test made. Everything under the
/// test's own prefix goes, including indices a failed assertion left behind.
async fn teardown(h: &Harness) {
    sqlx::query("DELETE FROM brain WHERE id LIKE $1")
        .bind(format!("{}%", h.brain.id))
        .execute(&h.pool)
        .await
        .ok();
    if let Ok(owned) = h.es.list_owned().await {
        for o in owned {
            h.es.drop_index(&o.index).await.ok();
        }
    }
    sqlx::query("DELETE FROM index_retirement WHERE index_name LIKE $1")
        .bind(format!("{}%", h.es.cfg.index_prefix))
        .execute(&h.pool)
        .await
        .ok();
    std::fs::remove_dir_all(&h.dir).ok();
}

/// Run a test body so that a failed assertion becomes a join error rather
/// than an unwind past the cleanup — a test that leaves an index behind when
/// it fails is a test that poisons the next run.
async fn guarded<F, Fut>(h: Harness, body: F)
where
    F: FnOnce(Arc<Harness>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let h = Arc::new(h);
    let outcome = tokio::spawn(body(h.clone())).await;
    teardown(&h).await;
    outcome.expect("the test body panicked").unwrap();
}

/// Drain until the queue is empty, so a batch boundary cannot fail the test.
async fn drain_all(h: &Harness) {
    drain_with(&h.pool, &h.es).await
}

async fn drain_with(pool: &sqlx::PgPool, es: &Es) {
    for _ in 0..30 {
        if es::worker::drain_once(pool, es).await.unwrap() == 0 {
            return;
        }
    }
    panic!("queue did not drain");
}

async fn titles(h: &Harness, query: &str) -> Vec<String> {
    es::search::run(&h.pool, &h.es, query, std::slice::from_ref(&h.brain.id), 5)
        .await
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect()
}

async fn ingest(h: &Harness) -> anyhow::Result<()> {
    db::upsert_brain(&h.pool, &h.brain.id, &h.brain.name, &h.brain.root, h.brain.seed).await?;
    ingest::ingest_brain(&h.pool, &h.brain, true, &mut |_: &str| {}).await?;
    db::rebuild_links(&h.pool).await?;
    Ok(())
}

/// Coalescing, asserted against Postgres rather than described in a comment.
///
/// Needs only the database, so it runs on a machine with no cluster.
#[tokio::test]
async fn five_edits_before_the_worker_runs_are_one_row() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("queue coalescing").await else { return };
    let brain_id = format!("estest-queue-{}", std::process::id());
    fresh_brain(&pool, &brain_id).await;
    // Claimable only once the brain has an index.
    sqlx::query("UPDATE brain SET index_name = $2, index_ready_at = now() WHERE id = $1")
        .bind(&brain_id)
        .bind(format!("aibrain-test-{brain_id}"))
        .execute(&pool)
        .await
        .unwrap();

    for n in 1..=5i64 {
        db::enqueue_note(&pool, &brain_id, Some(n), "Notes/A.md", "upsert")
            .await
            .unwrap();
    }
    assert_eq!(db::queue_depth_for_brain(&pool, &brain_id).await.unwrap(), 1);

    // And the last operation is the one that survives.
    db::enqueue_note(&pool, &brain_id, Some(9), "Notes/A.md", "delete")
        .await
        .unwrap();
    let row = sqlx::query("SELECT op, note_id, attempts FROM search_queue WHERE brain_id = $1")
        .bind(&brain_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    use sqlx::Row;
    assert_eq!(row.get::<String, _>("op"), "delete");
    assert_eq!(row.get::<i64, _>("note_id"), 9);
    assert_eq!(row.get::<i32, _>("attempts"), 0, "a fresh edit clears the backoff");

    // A claimed row is leased, so a second worker passes it by. The queue is
    // global and oldest-first, so on a shared test database other rows can sit
    // ahead of ours; claim enough to reach it rather than a fixed ten.
    let depth = db::queue_stats(&pool).await.unwrap().pending.max(1);
    let claimed = db::claim_queue(&pool, depth, 120).await.unwrap();
    assert!(claimed.iter().any(|i| i.brain_id == brain_id));
    let again = db::claim_queue(&pool, depth, 120).await.unwrap();
    assert!(!again.iter().any(|i| i.brain_id == brain_id), "already leased");

    drop_brain(&pool, &brain_id).await;
}

#[tokio::test]
async fn a_vault_is_indexed_searched_and_kept_in_step() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("main").await else { return };
    guarded(h, |h| async move { run(&h).await }).await;
}

/// The queue-side coalescing in `five_edits_before_the_worker_runs_are_one_row`
/// is exercised there against raw `enqueue_note` calls, which proves the SQL.
/// This drives it through real file edits and `ingest_one` — the path the
/// watcher actually takes — and follows it all the way to Elasticsearch, so a
/// note rewritten three times before the worker wakes up lands as one document,
/// not three.
#[tokio::test]
async fn three_rapid_edits_coalesce_into_one_document() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("coalesce").await else { return };
    guarded(h, |h| async move { run_coalesce(&h).await }).await;
}

async fn run_coalesce(h: &Harness) -> anyhow::Result<()> {
    ingest(h).await?;
    drain_all(h).await;
    h.es.refresh(&h.index().await).await?;
    assert_eq!(h.es.count(&h.index().await).await?, NOTES.len() as i64);

    // Edit the same note three times in a row, before the worker gets a
    // chance to drain any of them — the way three fast saves in Obsidian
    // would arrive at the watcher.
    for body in [
        "# Garden\n\nFirst rewrite: courgettes everywhere.\n",
        "# Garden\n\nSecond rewrite: tomatoes ripening fast.\n",
        "# Garden\n\nThird rewrite: the zucchini glut wins.\n",
    ] {
        std::fs::write(h.dir.join("Notes").join("Garden.md"), body)?;
        assert!(ingest::ingest_one(&h.pool, &h.brain, "Notes/Garden.md").await?.is_some());
    }
    assert_eq!(
        db::queue_depth_for_brain(&h.pool, &h.brain.id).await?,
        1,
        "three edits to the same note must coalesce into one queue row"
    );

    drain_all(h).await;
    h.es.refresh(&h.index().await).await?;
    assert_eq!(
        h.es.count(&h.index().await).await?,
        NOTES.len() as i64,
        "still one document per note, not one per edit"
    );
    let found = titles(h, "zucchini glut wins").await;
    assert_eq!(
        found.first().map(String::as_str),
        Some("Garden"),
        "the surviving document is the last edit, not an earlier one: {found:?}"
    );

    Ok(())
}

async fn run(h: &Harness) -> anyhow::Result<()> {
    db::upsert_brain(&h.pool, &h.brain.id, &h.brain.name, &h.brain.root, h.brain.seed).await?;
    let stats = ingest::ingest_brain(&h.pool, &h.brain, true, &mut |_: &str| {}).await?;
    assert_eq!(stats.added, NOTES.len(), "every note ingested");
    db::rebuild_links(&h.pool).await?;

    assert_eq!(
        db::queue_depth_for_brain(&h.pool, &h.brain.id).await?,
        NOTES.len() as i64,
        "ingest fills the queue"
    );
    // What /status reports. Decoding this is easy to get wrong — extract()
    // is NUMERIC, not double — so it is asserted rather than assumed.
    let stats = db::queue_stats(&h.pool).await?;
    assert_eq!(stats.pending, NOTES.len() as i64);
    assert_eq!(stats.failing, 0);
    assert!(stats.oldest_age_s >= 0.0);

    drain_all(h).await;
    assert_eq!(
        db::queue_depth_for_brain(&h.pool, &h.brain.id).await?,
        0,
        "the worker empties it"
    );
    h.es.refresh(&h.index().await).await?;
    assert_eq!(h.es.count(&h.index().await).await?, NOTES.len() as i64);

    // Semantic: neither word is in the note.
    let semantic = titles(h, "bread baking").await;
    assert_eq!(
        semantic.first().map(String::as_str),
        Some("Sourdough"),
        "semantic leg should find the loaf: {semantic:?}"
    );

    // Lexical: the word is in the note and nowhere else.
    let lexical = titles(h, "onboarding").await;
    assert_eq!(
        lexical.first().map(String::as_str),
        // The title comes from the filename, as vault::parse decides.
        Some("Quarterly"),
        "lexical leg should find the review: {lexical:?}"
    );

    // An edit re-indexes.
    std::fs::write(
        h.dir.join("Notes").join("Garden.md"),
        "# Garden\n\nThe zucchini glut arrives in August and nobody is ready for it.\n",
    )?;
    assert!(ingest::ingest_one(&h.pool, &h.brain, "Notes/Garden.md").await?.is_some());
    drain_all(h).await;
    h.es.refresh(&h.index().await).await?;
    let edited = titles(h, "zucchini glut").await;
    assert_eq!(edited.first().map(String::as_str), Some("Garden"), "{edited:?}");

    // A deletion leaves Elasticsearch too.
    std::fs::remove_file(h.dir.join("Notes").join("Sailing.md"))?;
    assert!(ingest::ingest_one(&h.pool, &h.brain, "Notes/Sailing.md").await?.is_some());
    drain_all(h).await;
    h.es.refresh(&h.index().await).await?;
    assert_eq!(
        h.es.count(&h.index().await).await?,
        NOTES.len() as i64 - 1,
        "the removed note left the index"
    );

    // A hit whose note is gone from Postgres is never returned, even if
    // Elasticsearch is momentarily behind.
    let gone = titles(h, "reef knot rigging").await;
    assert!(!gone.iter().any(|t| t == "Sailing"), "{gone:?}");

    Ok(())
}

// ── Index lifecycle ────────────────────────────────────────────────────────
//
// design_concepts/RECOMMENDATION_ELASTICSEARCH.md, §3–§6. The first group
// needs only Postgres; the second needs a cluster.

async fn fresh_brain(pool: &sqlx::PgPool, id: &str) {
    drop_brain(pool, id).await;
    db::upsert_brain(pool, id, &format!("Brain {id}"), "/nonexistent", 1).await.unwrap();
}

async fn drop_brain(pool: &sqlx::PgPool, id: &str) {
    sqlx::query("DELETE FROM brain WHERE id = $1").bind(id).execute(pool).await.unwrap();
    sqlx::query("DELETE FROM index_retirement WHERE brain_id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn retiring_a_brain_purges_its_queue_and_records_the_drop() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("retirement").await else { return };
    let id = format!("estest-retire-{}", std::process::id());
    fresh_brain(&pool, &id).await;
    let uid = uid_of(&pool, &id).await;
    let index = format!("aibrain-test-{uid}");
    assert!(db::claim_index_name(&pool, &id, &uid, &index).await.unwrap());
    for n in 0..3 {
        db::enqueue_note(&pool, &id, Some(n), &format!("N{n}.md"), "upsert").await.unwrap();
    }
    assert_eq!(db::queue_depth_for_brain(&pool, &id).await.unwrap(), 3);

    let retired = db::retire_brain(&pool, &id).await.unwrap().expect("it existed");
    assert_eq!(retired.index_name.as_deref(), Some(index.as_str()));
    assert_eq!(retired.uid, uid);
    assert_eq!(db::queue_depth_for_brain(&pool, &id).await.unwrap(), 0, "queue purged");
    let pending = db::pending_retirements(&pool).await.unwrap();
    assert!(pending.iter().any(|r| r.index_name == index && r.attempts == 0));

    // Idempotent, and a late enqueue for the gone brain is a no-op, not an error.
    assert!(db::retire_brain(&pool, &id).await.unwrap().is_none());
    db::enqueue_note(&pool, &id, Some(9), "Late.md", "upsert").await.unwrap();
    db::delete_notes(&pool, &[]).await.unwrap();
    assert_eq!(db::queue_depth_for_brain(&pool, &id).await.unwrap(), 0);

    drop_brain(&pool, &id).await;
}

#[tokio::test]
async fn a_brain_readded_under_the_same_id_gets_a_new_uid() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("uid reuse").await else { return };
    let id = format!("estest-readd-{}", std::process::id());
    fresh_brain(&pool, &id).await;
    let first = uid_of(&pool, &id).await;
    // A rescan keeps it.
    db::upsert_brain(&pool, &id, "Renamed", "/nonexistent", 1).await.unwrap();
    assert_eq!(uid_of(&pool, &id).await, first);
    // A removal and re-add does not: a queued drop of the old generation's
    // index must never be able to name the new one's.
    db::retire_brain(&pool, &id).await.unwrap();
    db::upsert_brain(&pool, &id, "Again", "/nonexistent", 1).await.unwrap();
    assert_ne!(uid_of(&pool, &id).await, first);
    drop_brain(&pool, &id).await;
}

#[tokio::test]
async fn an_unprovisioned_brain_is_not_claimed() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("claim gating").await else { return };
    let id = format!("estest-gate-{}", std::process::id());
    fresh_brain(&pool, &id).await;
    db::enqueue_note(&pool, &id, Some(1), "A.md", "upsert").await.unwrap();
    let depth = db::queue_stats(&pool).await.unwrap().pending.max(1);
    let claimed = db::claim_queue(&pool, depth, 1).await.unwrap();
    assert!(!claimed.iter().any(|i| i.brain_id == id), "no index yet, so no writes");
    db::release_queue(&pool, &claimed.iter().map(|i| i.id).collect::<Vec<_>>()).await.unwrap();
    drop_brain(&pool, &id).await;
}

#[tokio::test]
async fn an_outage_costs_no_attempts_and_a_bad_document_is_set_aside() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("dead letter").await else { return };
    let id = format!("estest-dead-{}", std::process::id());
    fresh_brain(&pool, &id).await;
    db::enqueue_note(&pool, &id, Some(1), "Bad.md", "upsert").await.unwrap();
    let row: i64 = sqlx::query_scalar("SELECT id FROM search_queue WHERE brain_id = $1")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let attempts = || async {
        sqlx::query_scalar::<_, i32>("SELECT attempts FROM search_queue WHERE id = $1")
            .bind(row)
            .fetch_optional(&pool)
            .await
            .unwrap()
    };

    db::release_queue(&pool, &[row]).await.unwrap();
    assert_eq!(attempts().await, Some(0), "released, not charged");

    for n in 1..db::MAX_ATTEMPTS {
        db::fail_queue(&pool, &[(row, format!("refused {n}"))]).await.unwrap();
    }
    assert_eq!(attempts().await, Some(db::MAX_ATTEMPTS - 1));
    db::fail_queue(&pool, &[(row, "refused for good".into())]).await.unwrap();
    assert_eq!(attempts().await, None, "moved out of the queue");
    let dead: String = sqlx::query_scalar("SELECT last_error FROM search_dead_letter WHERE id = $1")
        .bind(row)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(dead, "refused for good");

    sqlx::query("DELETE FROM search_dead_letter WHERE id = $1")
        .bind(row)
        .execute(&pool)
        .await
        .unwrap();
    drop_brain(&pool, &id).await;
}

#[tokio::test]
async fn a_stale_reindex_cannot_resurrect_a_removed_vault() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("stale reindex").await else { return };
    let pid = std::process::id();
    let id = format!("estest-stale-{pid}");
    drop_brain(&pool, &id).await;
    let real = std::env::temp_dir().join(format!("aibrain-stale-real-{pid}"));
    let links = std::env::temp_dir().join(format!("aibrain-stale-links-{pid}"));
    write_vault(&real);
    std::fs::remove_dir_all(&links).ok();
    std::fs::create_dir_all(&links).unwrap();
    let link = links.join("Stale");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let spec = BrainSpec {
        id: id.clone(),
        name: "Stale".into(),
        root: link.to_string_lossy().into_owned(),
        seed: 1,
        excludes: vec![],
        color: "#fff".into(),
        group_by: Default::default(),
    };

    ingest::reindex(&pool, std::slice::from_ref(&spec), false, |_| {}).await.unwrap();
    assert_eq!(db::count_notes_for_brain(&pool, &id).await.unwrap(), NOTES.len() as i64);

    // Python unlinks, then retires. A reindex still holding the old config
    // must not write the brain back.
    std::fs::remove_file(&link).unwrap();
    db::retire_brain(&pool, &id).await.unwrap();
    let stats = ingest::reindex(&pool, std::slice::from_ref(&spec), false, |_| {}).await.unwrap();
    assert_eq!(stats.added, 0);
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM brain WHERE id = $1)")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!exists, "the removed vault stayed removed");

    // And a reindex whose config no longer lists a brain retires it.
    std::os::unix::fs::symlink(&real, &link).unwrap();
    ingest::reindex(&pool, std::slice::from_ref(&spec), false, |_| {}).await.unwrap();
    let stats = ingest::reindex(&pool, &[], false, |_| {}).await.unwrap();
    assert!(stats.retired.contains(&id));

    drop_brain(&pool, &id).await;
    std::fs::remove_dir_all(&real).ok();
    std::fs::remove_dir_all(&links).ok();
}

#[tokio::test]
async fn a_removal_waits_for_a_scan_in_progress() {
    let _serial = SERIAL.lock().await;
    let Some(pool) = test_pool("lifecycle lock").await else { return };
    let id = format!("estest-lock-{}", std::process::id());
    fresh_brain(&pool, &id).await;

    let scan = db::BrainLock::shared(&pool, &id).await.unwrap();
    let retire = tokio::spawn({
        let pool = pool.clone();
        let id = id.clone();
        async move { db::retire_brain(&pool, &id).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!retire.is_finished(), "retirement must wait for the scan");
    scan.release().await.unwrap();
    let retired = tokio::time::timeout(std::time::Duration::from_secs(5), retire)
        .await
        .expect("retirement proceeds once the scan is done")
        .unwrap()
        .unwrap();
    assert!(retired.is_some());
    drop_brain(&pool, &id).await;
}

// ── With a cluster ─────────────────────────────────────────────────────────

/// An `Es` with the same prefix pointed at a closed port: the cluster "down".
fn unreachable(h: &Harness) -> Es {
    let mut cfg = h.es.cfg.clone();
    cfg.url = "http://127.0.0.1:9".into();
    Es::new(cfg).unwrap()
}

#[tokio::test]
async fn removing_a_vault_drops_its_index_and_a_late_write_cannot_recreate_it() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("retire").await else { return };
    guarded(h, |h| async move {
        ingest(&h).await?;
        drain_all(&h).await;
        let index = h.index().await;
        let uid = uid_of(&h.pool, &h.brain.id).await;
        assert!(h.es.exists(&index).await?);
        assert!(index.ends_with(&uid), "a new brain's index is named by its uid");

        // A write the worker claimed just before the removal.
        std::fs::write(h.dir.join("Notes").join("Garden.md"), "# Garden\n\nLate edit.\n")?;
        ingest::ingest_one(&h.pool, &h.brain, "Notes/Garden.md").await?;
        let depth = db::queue_stats(&h.pool).await?.pending.max(1);
        let in_flight: Vec<_> = db::claim_queue(&h.pool, depth, 120)
            .await?
            .into_iter()
            .filter(|i| i.brain_id == h.brain.id)
            .collect();
        assert_eq!(in_flight.len(), 1);
        let late_body = format!(
            "{}\n",
            serde_json::json!({ "index": { "_index": h.es.write_alias(&uid), "_id": "Late.md" } })
        ) + "{\"title\":\"late\"}\n";

        db::retire_brain(&h.pool, &h.brain.id).await?.expect("retired");
        assert_eq!(db::queue_depth_for_brain(&h.pool, &h.brain.id).await?, 0);
        es::worker::drain_once(&h.pool, &h.es).await?;
        assert!(!h.es.exists(&index).await?, "the index was dropped");
        assert!(db::pending_retirements(&h.pool).await?.iter().all(|r| r.index_name != index));

        // The in-flight batch lands now. require_alias refuses it rather than
        // auto-creating an index with a dynamic mapping.
        let response = h.es.bulk(late_body).await?;
        assert_eq!(response["errors"], true);
        assert_eq!(response["items"][0]["index"]["error"]["type"], "index_not_found_exception");
        assert!(!h.es.exists(&index).await?, "and nothing recreated it");
        assert!(h.es.list_owned().await?.is_empty());
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn a_removal_made_while_the_cluster_is_down_happens_when_it_returns() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("outage").await else { return };
    guarded(h, |h| async move {
        ingest(&h).await?;
        drain_all(&h).await;
        let index = h.index().await;

        db::retire_brain(&h.pool, &h.brain.id).await?.expect("retired");
        let down = unreachable(&h);
        let err = es::worker::drain_once(&h.pool, &down).await.unwrap_err();
        assert!(es::is_unavailable(&err), "{err:#}");
        let pending = db::pending_retirements(&h.pool).await?;
        let mine = pending.iter().find(|r| r.index_name == index).expect("still pending");
        assert_eq!(mine.attempts, 0, "an outage is not charged to the retirement");
        assert!(h.es.exists(&index).await?);

        es::worker::drain_once(&h.pool, &h.es).await?;
        assert!(!h.es.exists(&index).await?, "dropped once the cluster was back");
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn names_that_fold_alike_get_separate_indices() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("fold").await else { return };
    guarded(h, |h| async move {
        let a = format!("{}-a", h.brain.id);
        let b = format!("{}-b", h.brain.id);
        db::upsert_brain(&h.pool, &a, "My Notes", "/nonexistent", 1).await?;
        db::upsert_brain(&h.pool, &b, "my-notes", "/nonexistent", 1).await?;
        drain_all(&h).await;
        let (ia, ib) = (index_of(&h.pool, &a).await.unwrap(), index_of(&h.pool, &b).await.unwrap());
        assert_ne!(ia, ib);
        assert!(h.es.exists(&ia).await? && h.es.exists(&ib).await?);
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn a_vault_readded_before_the_drop_keeps_its_new_index() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("readd").await else { return };
    guarded(h, |h| async move {
        ingest(&h).await?;
        drain_all(&h).await;
        let old = h.index().await;

        db::retire_brain(&h.pool, &h.brain.id).await?;
        // Re-added under the same id before the worker ran the retirement.
        ingest(&h).await?;
        drain_all(&h).await;
        let new = h.index().await;
        assert_ne!(old, new);
        assert!(!h.es.exists(&old).await?, "the old generation's index is gone");
        assert!(h.es.exists(&new).await?, "the new one survived its predecessor's drop");
        h.es.refresh(&new).await?;
        assert_eq!(h.es.count(&new).await?, NOTES.len() as i64);
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn an_index_deleted_behind_our_back_is_reprovisioned_not_autocreated() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("lost").await else { return };
    guarded(h, |h| async move {
        ingest(&h).await?;
        drain_all(&h).await;
        let index = h.index().await;

        // Someone deletes it by hand. The next write finds no alias…
        h.es.drop_index(&index).await?;
        std::fs::write(h.dir.join("Notes").join("Garden.md"), "# Garden\n\nAfter the loss.\n")?;
        ingest::ingest_one(&h.pool, &h.brain, "Notes/Garden.md").await?;
        drain_all(&h).await;

        // …and the brain is reprovisioned with the real mapping and every
        // note requeued, rather than one document in a dynamic-mapped index.
        assert!(h.es.exists(&index).await?);
        h.es.refresh(&index).await?;
        assert_eq!(h.es.count(&index).await?, NOTES.len() as i64);
        let owned = h.es.list_owned().await?;
        let mine = owned.iter().find(|o| o.index == index).expect("listed");
        assert_eq!(mine.meta.as_ref().map(|m| m.brain_id.as_str()), Some(h.brain.id.as_str()));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn reconcile_retires_a_stamped_orphan_and_leaves_a_stranger_alone() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("orphan").await else { return };
    guarded(h, |h| async move {
        let orphan = format!("{}orphan", h.es.cfg.index_prefix);
        let stranger = format!("{}stranger", h.es.cfg.index_prefix);
        h.es.create_index(&orphan, &h.es.meta_for("estest-nobody", "u-nobody")).await?;
        h.es.create_index(&stranger, &serde_json::json!({ "someone": "else" })).await?;

        es::worker::reconcile(&h.pool, &h.es).await?;
        es::worker::drain_once(&h.pool, &h.es).await?;
        assert!(!h.es.exists(&orphan).await?, "our orphan was retired");
        assert!(h.es.exists(&stranger).await?, "an index we cannot prove is ours stays");
        assert!(h.es.health.lock().unwrap().foreign_indices.contains(&stranger));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn an_index_from_before_uids_is_adopted_in_place() {
    let _serial = SERIAL.lock().await;
    let Some(h) = setup("adopt").await else { return };
    guarded(h, |h| async move {
        ingest(&h).await?;
        // The state an upgrade starts from: the brain predates uids, and the
        // old code already indexed it into a name-derived index with no stamp
        // — which, under the old naming, another vault could share.
        sqlx::query("UPDATE brain SET adopt_legacy = TRUE WHERE id = $1")
            .bind(&h.brain.id)
            .execute(&h.pool)
            .await?;
        sqlx::query("DELETE FROM search_queue WHERE brain_id = $1")
            .bind(&h.brain.id)
            .execute(&h.pool)
            .await?;
        let legacy = es::legacy_index_name(&h.es.cfg.index_prefix, &h.brain.name);
        h.es.create_index(&legacy, &serde_json::json!({})).await?;
        h.es.attach(&legacy, "seed").await?;
        let mine = db::notes_by_path(&h.pool, &[h.brain.id.clone()], &["Notes/Tides.md".into()])
            .await?
            .remove(0);
        let mut theirs = mine.clone();
        theirs.brain_id = "some-other-vault".into();
        theirs.rel_path = "Theirs.md".into();
        let seed = h.es.write_alias("seed");
        let body = es::worker::index_lines(&seed, &mine) + &es::worker::index_lines(&seed, &theirs);
        assert_eq!(h.es.bulk(body).await?["errors"], false);
        h.es.refresh(&legacy).await?;
        assert_eq!(h.es.count(&legacy).await?, 2);

        let prefix = h.es.cfg.index_prefix.clone();
        db::assign_legacy_index_names(&h.pool, |n| es::legacy_index_name(&prefix, n)).await?;
        es::worker::drain_once(&h.pool, &h.es).await?;

        assert_eq!(h.index().await, legacy, "kept its old name, no new index");
        assert_eq!(db::queue_depth_for_brain(&h.pool, &h.brain.id).await?, 0, "nothing re-embedded");
        h.es.refresh(&legacy).await?;
        assert_eq!(h.es.count(&legacy).await?, 1, "the other vault's document was removed");
        let owned = h.es.list_owned().await?;
        assert_eq!(owned.len(), 1, "{owned:?}");
        assert_eq!(owned[0].meta.as_ref().map(|m| m.uid.clone()), Some(uid_of(&h.pool, &h.brain.id).await));
        // Searchable through the alias, and the stray write alias is gone.
        let found = es::search::run(&h.pool, &h.es, "moon ocean", &[], 5).await?;
        assert!(found.iter().any(|s| s.name == "Tides"), "{found:?}");
        let late = es::worker::index_lines(&seed, &theirs);
        assert_eq!(h.es.bulk(late).await?["errors"], true, "only the brain's own alias writes");
        Ok(())
    })
    .await;
}
