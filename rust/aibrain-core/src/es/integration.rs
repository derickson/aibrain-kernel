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

struct Harness {
    pool: sqlx::PgPool,
    es: Arc<Es>,
    brain: BrainSpec,
    dir: PathBuf,
}

impl Harness {
    fn index(&self) -> String {
        self.es.index_for(&self.brain.name)
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// `None` with a printed reason when the test cannot run here.
async fn setup() -> Option<Harness> {
    let Some(database_url) = env("AIBRAIN_TEST_DATABASE_URL") else {
        eprintln!("skipping es integration: AIBRAIN_TEST_DATABASE_URL is unset");
        return None;
    };
    if env("ELASTICSEARCH_URL").is_none() || env("ELASTICSEARCH_API_KEY").is_none() {
        eprintln!("skipping es integration: ELASTICSEARCH_URL / _API_KEY are unset");
        return None;
    }
    let mut cfg = EsConfig::from_env()?;
    cfg.index_prefix = format!("aibrain-test-{}-", std::process::id());
    cfg.batch = 50;

    let pool = match db::connect(&database_url).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("skipping es integration: {err:#}");
            return None;
        }
    };

    let dir = std::env::temp_dir().join(format!("aibrain-es-it-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("Notes")).ok();
    for (name, text) in NOTES {
        std::fs::write(dir.join("Notes").join(name), text).unwrap();
    }

    let brain = BrainSpec {
        id: format!("estest-{}", std::process::id()),
        name: format!("EsTest {}", std::process::id()),
        root: dir.to_string_lossy().into_owned(),
        seed: 3,
        excludes: vec![],
    };

    // A dedicated test database, so starting from a clean queue is fair game
    // and keeps a leftover row from an earlier run out of this batch.
    sqlx::query("DELETE FROM search_queue").execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM brain WHERE id LIKE 'estest-%'")
        .execute(&pool)
        .await
        .unwrap();

    Some(Harness { pool, es: Arc::new(Es::new(cfg).unwrap()), brain, dir })
}

async fn teardown(h: &Harness) {
    h.es.delete_index(&h.index()).await.ok();
    sqlx::query("DELETE FROM brain WHERE id = $1")
        .bind(&h.brain.id)
        .execute(&h.pool)
        .await
        .ok();
    sqlx::query("DELETE FROM search_queue WHERE brain_id = $1")
        .bind(&h.brain.id)
        .execute(&h.pool)
        .await
        .ok();
    std::fs::remove_dir_all(&h.dir).ok();
}

/// Drain until the queue is empty, so a batch boundary cannot fail the test.
async fn drain_all(h: &Harness) {
    for _ in 0..20 {
        if es::worker::drain_once(&h.pool, &h.es).await.unwrap() == 0 {
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

/// Coalescing, asserted against Postgres rather than described in a comment.
///
/// Needs only the database, so it runs on a machine with no cluster.
#[tokio::test]
async fn five_edits_before_the_worker_runs_are_one_row() {
    let Some(url) = env("AIBRAIN_TEST_DATABASE_URL") else {
        eprintln!("skipping queue coalescing: AIBRAIN_TEST_DATABASE_URL is unset");
        return;
    };
    let Ok(pool) = db::connect(&url).await else {
        eprintln!("skipping queue coalescing: cannot reach {}", db::redact(&url));
        return;
    };
    let brain_id = format!("estest-queue-{}", std::process::id());
    sqlx::query("DELETE FROM search_queue WHERE brain_id = $1")
        .bind(&brain_id)
        .execute(&pool)
        .await
        .unwrap();

    for n in 1..=5i64 {
        db::enqueue_note(&pool, &brain_id, "Q", Some(n), "Notes/A.md", "upsert")
            .await
            .unwrap();
    }
    assert_eq!(db::queue_depth_for_brain(&pool, &brain_id).await.unwrap(), 1);

    // And the last operation is the one that survives.
    db::enqueue_note(&pool, &brain_id, "Q", Some(9), "Notes/A.md", "delete")
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

    // A claimed row is leased, so a second worker passes it by.
    let claimed = db::claim_queue(&pool, 10, 120).await.unwrap();
    assert!(claimed.iter().any(|i| i.brain_id == brain_id));
    let again = db::claim_queue(&pool, 10, 120).await.unwrap();
    assert!(!again.iter().any(|i| i.brain_id == brain_id), "already leased");

    sqlx::query("DELETE FROM search_queue WHERE brain_id = $1")
        .bind(&brain_id)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_vault_is_indexed_searched_and_kept_in_step() {
    let Some(h) = setup().await else { return };
    // The body runs in its own task so that a failed assertion becomes a join
    // error rather than an unwind past the cleanup — a test that leaves an
    // index behind when it fails is a test that poisons the next run.
    let h = Arc::new(h);
    let outcome = tokio::spawn({
        let h = h.clone();
        async move { run(&h).await }
    })
    .await;
    teardown(&h).await;
    outcome.expect("the test body panicked").unwrap();
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
    h.es.refresh(&h.index()).await?;
    assert_eq!(h.es.count(&h.index()).await?, NOTES.len() as i64);

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
    h.es.refresh(&h.index()).await?;
    let edited = titles(h, "zucchini glut").await;
    assert_eq!(edited.first().map(String::as_str), Some("Garden"), "{edited:?}");

    // A deletion leaves Elasticsearch too.
    std::fs::remove_file(h.dir.join("Notes").join("Sailing.md"))?;
    assert!(ingest::ingest_one(&h.pool, &h.brain, "Notes/Sailing.md").await?.is_some());
    drain_all(h).await;
    h.es.refresh(&h.index()).await?;
    assert_eq!(
        h.es.count(&h.index()).await?,
        NOTES.len() as i64 - 1,
        "the removed note left the index"
    );

    // A hit whose note is gone from Postgres is never returned, even if
    // Elasticsearch is momentarily behind.
    let gone = titles(h, "reef knot rigging").await;
    assert!(!gone.iter().any(|t| t == "Sailing"), "{gone:?}");

    Ok(())
}
