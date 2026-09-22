//! The live-update path, end to end against a real Postgres and a real socket.
//!
//! In-module rather than in `tests/` because the crate is a binary, and it
//! skips with a printed reason rather than failing when the database is
//! absent — a suite that goes red on a laptop with no container is a suite
//! people stop running.
//!
//!   AIBRAIN_TEST_DATABASE_URL=postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_events \
//!     cargo test live::
//!
//! Every brain it creates is named after the process, and is deleted at the
//! end, so it can share a database with anything else.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::api::Ctx;
use crate::config::Config;
use crate::{api, db, events, graph, ingest, watch};

const NOTES: [(&str, &str); 4] = [
    ("Alpha.md", "# Alpha\n\nAlpha points at [[Beta]] and at [[Gamma]].\n"),
    ("Beta.md", "# Beta\n\nBeta is quiet.\n"),
    ("Gamma.md", "# Gamma\n\nGamma links back to [[Alpha]].\n"),
    ("Delta.md", "# Delta\n\nDelta is on its own.\n"),
];

struct Harness {
    ctx: Arc<Ctx>,
    cfg: Config,
    dir: PathBuf,
    base: String,
}

impl Harness {
    fn brain_id(&self) -> &str {
        &self.cfg.brains[0].id
    }

    fn note_path(&self, name: &str) -> PathBuf {
        self.dir.join("vault").join("Notes").join(name)
    }

    async fn teardown(self) {
        sqlx::query("DELETE FROM brain WHERE id = $1")
            .bind(self.brain_id())
            .execute(&self.ctx.pool)
            .await
            .ok();
        sqlx::query("DELETE FROM search_queue WHERE brain_id = $1")
            .bind(self.brain_id())
            .execute(&self.ctx.pool)
            .await
            .ok();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// `None` with a printed reason when the test cannot run here.
async fn setup(tag: &str) -> Option<Harness> {
    let url = std::env::var("AIBRAIN_TEST_DATABASE_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let Some(url) = url else {
        eprintln!("skipping live integration: AIBRAIN_TEST_DATABASE_URL is unset");
        return None;
    };
    let pool = match db::connect(&url).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("skipping live integration: {err:#}");
            return None;
        }
    };

    let unique = format!("{tag}-{}", std::process::id());
    let dir = std::env::temp_dir().join(format!("aibrain-live-{unique}"));
    std::fs::remove_dir_all(&dir).ok();
    let vault = dir.join("vault").join("Notes");
    std::fs::create_dir_all(&vault).ok();
    for (name, body) in NOTES {
        std::fs::write(vault.join(name), body).unwrap();
    }

    let config_path = dir.join("config.json");
    std::fs::write(
        &config_path,
        serde_json::json!({
            "title": "Live",
            "brains": [{
                "id": format!("live-{unique}"),
                "name": format!("Live {unique}"),
                "path": dir.join("vault").to_string_lossy(),
                "enabled": true,
                "seed": 11,
            }],
        })
        .to_string(),
    )
    .unwrap();

    sqlx::query("DELETE FROM brain WHERE id = $1")
        .bind(format!("live-{unique}"))
        .execute(&pool)
        .await
        .ok();

    let cfg = crate::config::load(&config_path).unwrap();
    let ctx = Arc::new(Ctx {
        pool,
        config_path,
        revision: std::sync::atomic::AtomicI64::new(0),
        es: None,
        events: events::Bus::new(),
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = api::router(ctx.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.ok();
    });

    Some(Harness { ctx, cfg, dir, base })
}

/// Read from an open SSE body until a whole frame has arrived, or give up.
///
/// Chunks do not line up with frames, so this accumulates until it sees the
/// blank line that ends one.
async fn next_frame(response: &mut reqwest::Response, buffer: &mut String) -> Option<String> {
    for _ in 0..40 {
        if let Some(cut) = buffer.find("\n\n") {
            let frame = buffer[..cut].to_string();
            buffer.drain(..cut + 2);
            return Some(frame);
        }
        match tokio::time::timeout(Duration::from_secs(5), response.chunk()).await {
            Ok(Ok(Some(chunk))) => buffer.push_str(&String::from_utf8_lossy(&chunk)),
            _ => return None,
        }
    }
    None
}

/// The whole point, in one test: build once, serve a 304, edit a note, and
/// watch the ETag, the revision and the event stream all move together.
#[tokio::test]
async fn an_edit_moves_one_revision_one_etag_and_one_event() {
    let Some(h) = setup("etag").await else { return };

    // ---- ingest
    let stats = ingest::reindex(&h.ctx.pool, &h.cfg.brains, false, |_| {}).await.unwrap();
    assert_eq!(stats.added, NOTES.len(), "every note is new");
    assert_eq!(
        stats.bumped.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec![h.brain_id()],
        "exactly the one vault that changed"
    );

    let client = reqwest::Client::new();

    // ---- /universe twice: the second is a 304
    let first = client.get(format!("{}/universe", h.base)).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let etag = first.headers()["etag"].to_str().unwrap().to_string();
    let payload: serde_json::Value = first.json().await.unwrap();
    assert_eq!(payload["stats"]["notes"], NOTES.len());
    let revision_before = payload["brains"][0]["revision"].as_i64().unwrap();
    assert!(revision_before > 0, "ingest set a real revision");

    let again = client
        .get(format!("{}/universe", h.base))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), 304, "nothing moved, so nothing is resent");
    assert_eq!(again.headers()["etag"].to_str().unwrap(), etag);

    // A second full read is served out of graph_cache and must be identical.
    let cached = client.get(format!("{}/universe", h.base)).send().await.unwrap();
    assert_eq!(cached.headers()["etag"].to_str().unwrap(), etag);
    let from_cache: serde_json::Value = cached.json().await.unwrap();
    assert_eq!(from_cache, payload, "the cached block rebuilds the same payload");

    // ---- open the stream and read the opening ping
    let mut stream = client.get(format!("{}/events", h.base)).send().await.unwrap();
    assert_eq!(stream.status(), 200);
    let mut buffer = String::new();
    let ping = next_frame(&mut stream, &mut buffer).await.expect("an opening ping");
    assert!(ping.starts_with(':'), "a comment, not data: {ping:?}");

    // ---- edit a note the way the watcher would
    std::fs::write(
        h.note_path("Delta.md"),
        "# Delta\n\nDelta now points at [[Beta]] as well.\n",
    )
    .unwrap();
    let touched: HashSet<PathBuf> =
        [std::fs::canonicalize(h.note_path("Delta.md")).unwrap()].into();
    watch::apply(&h.ctx, &h.cfg, touched).await.unwrap();

    let revision_after = db::brain_revision(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap();
    assert_eq!(revision_after, revision_before + 1, "one vault, one bump");

    // ---- one event, naming that brain
    let frame = next_frame(&mut stream, &mut buffer).await.expect("a change event");
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data:"))
        .expect("a data line");
    let change: serde_json::Value = serde_json::from_str(data.trim()).unwrap();
    assert_eq!(change["brain_id"], h.brain_id());
    assert_eq!(change["revision"], revision_after);
    assert!(
        matches!(change["kind"].as_str(), Some("note") | Some("brain")),
        "unexpected kind: {change}"
    );
    drop(stream);

    // ---- and the ETag moved with it
    let third = client
        .get(format!("{}/universe", h.base))
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 200, "the old tag no longer matches");
    let fresh = third.headers()["etag"].to_str().unwrap().to_string();
    assert_ne!(fresh, etag);
    let payload: serde_json::Value = third.json().await.unwrap();
    assert_eq!(payload["brains"][0]["revision"], revision_after);

    h.teardown().await;
}

/// A rescan that found nothing must not move anything, or the cache and the
/// ETag are both worthless.
#[tokio::test]
async fn a_rescan_that_changed_nothing_leaves_the_revision_alone() {
    let Some(h) = setup("quiet").await else { return };

    ingest::reindex(&h.ctx.pool, &h.cfg.brains, false, |_| {}).await.unwrap();
    let settled = db::brain_revision(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap();

    for _ in 0..3 {
        let stats = ingest::reindex(&h.ctx.pool, &h.cfg.brains, false, |_| {}).await.unwrap();
        assert!(stats.bumped.is_empty(), "nothing changed, nothing bumped");
    }
    assert_eq!(
        db::brain_revision(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap(),
        settled
    );

    // And the cache row is the one the first build wrote.
    let cached = db::graph_cache_get(&h.ctx.pool, h.brain_id()).await.unwrap();
    assert!(cached.is_none(), "nothing has asked for the universe yet");

    let cfg = h.ctx.config().unwrap();
    let revisions = graph::revisions(&h.ctx.pool, &cfg).await.unwrap();
    graph::build(&h.ctx.pool, &cfg, &revisions).await.unwrap();
    let cached = db::graph_cache_get(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap();
    assert_eq!(cached.revision, settled);
    let geometry = graph::unpack(&cached.geometry).unwrap();
    assert_eq!(geometry.nodes(), NOTES.len());
    assert!(!geometry.edges.is_empty(), "Alpha links to Beta and Gamma");

    h.teardown().await;
}

/// A file touched but not changed is an autosave, and must reach nobody.
#[tokio::test]
async fn an_autosave_publishes_nothing() {
    let Some(h) = setup("autosave").await else { return };

    ingest::reindex(&h.ctx.pool, &h.cfg.brains, false, |_| {}).await.unwrap();
    let before = db::brain_revision(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap();
    let mut rx = h.ctx.events.subscribe();

    // Rewritten byte for byte, which is what Obsidian does on every autosave.
    let path = h.note_path("Beta.md");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, &text).unwrap();
    let touched: HashSet<PathBuf> = [std::fs::canonicalize(&path).unwrap()].into();
    watch::apply(&h.ctx, &h.cfg, touched).await.unwrap();

    assert_eq!(
        db::brain_revision(&h.ctx.pool, h.brain_id()).await.unwrap().unwrap(),
        before
    );
    assert!(rx.try_recv().is_err(), "nothing should have been published");

    h.teardown().await;
}

/// Sanity: the packed cache and a fresh layout agree, node for node.
#[tokio::test]
async fn the_cache_and_a_fresh_layout_agree() {
    let Some(h) = setup("agree").await else { return };

    ingest::reindex(&h.ctx.pool, &h.cfg.brains, false, |_| {}).await.unwrap();
    let cfg = h.ctx.config().unwrap();
    let revisions = graph::revisions(&h.ctx.pool, &cfg).await.unwrap();

    let built = graph::build(&h.ctx.pool, &cfg, &revisions).await.unwrap();
    // Second call cannot reach the layout: the cache row is current.
    let served = graph::build(&h.ctx.pool, &cfg, &revisions).await.unwrap();
    assert_eq!(built, served);

    // Throwing the row away must produce the same answer again, which is what
    // makes the cache safe to drop at any time.
    sqlx::query("DELETE FROM graph_cache WHERE brain_id = $1")
        .bind(h.brain_id())
        .execute(&h.ctx.pool)
        .await
        .unwrap();
    let rebuilt = graph::build(&h.ctx.pool, &cfg, &revisions).await.unwrap();
    assert_eq!(built, rebuilt);

    h.teardown().await;
}
