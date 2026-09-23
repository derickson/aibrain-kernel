//! The background worker: index lifecycle and the `search_queue` drain.
//!
//! One bulk request per batch rather than one write per note, because the
//! embedding model runs inside the write. Fifty notes in one call cost one
//! round trip and one model warm-up; fifty calls cost fifty of each.
//!
//! Each pass does three things, in this order:
//!
//! 1. **Retire** indices whose brain was removed: detach the aliases, then
//!    delete the index. Recorded in `index_retirement`, so a removal made while
//!    the cluster was down still happens once it is back.
//! 2. **Provision** brains that have no index yet (or lost it): create it with
//!    the mapping and ownership stamp, point the aliases at it. This is the only
//!    place an index is ever created.
//! 3. **Drain** one batch of queued documents through each brain's write alias
//!    with `require_alias`, so a write can never recreate a dropped index.
//!
//! Around that sits one breaker for the whole cluster. When Elasticsearch is
//! unreachable nothing is charged to the rows being sent; the worker stops
//! claiming, probes with backoff, and picks up within a probe of recovery.
//!
//! `drain_once` is a plain function a test can call. A test that has to start
//! a background task and then guess how long to wait is a test that fails on a
//! slow machine.
//!
//! See design_concepts/RECOMMENDATION_ELASTICSEARCH.md.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{clip, is_unavailable, Es, Owned, Unavailable};
use crate::db::{self, BrainIndex, QueueItem};

/// How long a claimed row stays claimed. Long enough for a slow bulk, short
/// enough that a worker killed mid-batch does not strand its work.
const LEASE_SECS: i64 = 120;

/// How often the cluster is compared against Postgres, besides at startup and
/// whenever it comes back from an outage.
const RECONCILE_EVERY: Duration = Duration::from_secs(15 * 60);

/// The breaker's probe interval doubles from the first to the last.
const PROBE_FIRST: Duration = Duration::from_secs(1);
const PROBE_MAX: Duration = Duration::from_secs(60);

/// Retirements handled per pass. They are one alias call and one delete each.
const RETIRE_BATCH: i64 = 20;

/// Start the worker loop. Returns immediately.
pub fn spawn(pool: PgPool, es: Arc<Es>) {
    let poll = Duration::from_millis(es.cfg.poll_ms);
    tokio::spawn(async move {
        let mut probe = PROBE_FIRST;
        let mut last_reconcile: Option<Instant> = None;
        loop {
            if !es.health.lock().unwrap().available {
                tokio::time::sleep(jitter(probe)).await;
                match es.ping().await {
                    Ok(()) => {
                        set_available(&es, true, None);
                        tracing::info!("elasticsearch is reachable again — resuming");
                        probe = PROBE_FIRST;
                        // An outage is exactly when drift builds up.
                        last_reconcile = None;
                    }
                    Err(err) => {
                        record_probe(&es, &err);
                        probe = (probe * 2).min(PROBE_MAX);
                        continue;
                    }
                }
            }

            if last_reconcile.is_none_or(|t| t.elapsed() >= RECONCILE_EVERY) {
                match reconcile(&pool, &es).await {
                    Ok(()) => last_reconcile = Some(Instant::now()),
                    Err(err) if is_unavailable(&err) => {
                        trip(&es, &err);
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!("reconcile failed: {err:#}");
                        last_reconcile = Some(Instant::now());
                    }
                }
            }

            match drain_once(&pool, &es).await {
                Ok(0) => tokio::time::sleep(poll).await,
                Ok(_) => {}
                Err(err) if is_unavailable(&err) => trip(&es, &err),
                Err(err) => {
                    tracing::warn!("search worker: {err:#}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

/// Open the breaker.
fn trip(es: &Es, err: &anyhow::Error) {
    tracing::warn!("elasticsearch unavailable, pausing the worker: {err:#}");
    set_available(es, false, Some(format!("{err:#}")));
}

fn set_available(es: &Es, available: bool, error: Option<String>) {
    let mut health = es.health.lock().unwrap();
    if health.available != available {
        health.since = chrono::Utc::now();
    }
    health.available = available;
    health.last_probe = Some(chrono::Utc::now());
    if error.is_some() || available {
        health.last_error = error.map(|e| clip(&e, 400));
    }
}

fn record_probe(es: &Es, err: &anyhow::Error) {
    tracing::debug!("elasticsearch still unavailable: {err:#}");
    let mut health = es.health.lock().unwrap();
    health.last_probe = Some(chrono::Utc::now());
    health.last_error = Some(clip(&format!("{err:#}"), 400));
}

/// Up to a quarter again, so several processes do not probe in lockstep.
fn jitter(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    base + base.mul_f64((nanos % 1000) as f64 / 4000.0)
}

/// One pass: retire, provision, then one batch of documents. Returns how much
/// work it found; zero is the caller's cue to sleep.
///
/// An `Unavailable` error means the cluster could not be reached; nothing was
/// charged to any row, and the caller should open the breaker.
pub async fn drain_once(pool: &PgPool, es: &Es) -> Result<usize> {
    let mut work = retire_due(pool, es).await?;
    work += provision_pending(pool, es).await?;
    work += drain_documents(pool, es).await?;
    Ok(work)
}

// ── 1. Retire ──────────────────────────────────────────────────────────────

async fn retire_due(pool: &PgPool, es: &Es) -> Result<usize> {
    let due = db::due_retirements(pool, RETIRE_BATCH).await?;
    for r in &due {
        // Belt and braces: never drop an index a live brain points at.
        if let Some(owner) = db::index_owner(pool, &r.index_name).await? {
            let why = format!("{} is in use by brain {owner}; not dropping it", r.index_name);
            tracing::error!("{why}");
            db::fail_retirement(pool, &r.index_name, &why).await?;
            continue;
        }
        let result = async {
            // Detach first: if the delete is refused, the index is at least
            // out of service — unsearchable and unwritable.
            es.detach(&r.index_name).await?;
            es.drop_index(&r.index_name).await
        }
        .await;
        match result {
            Ok(()) => {
                db::finish_retirement(pool, &r.index_name).await?;
                tracing::info!("dropped index {} (brain {} removed)", r.index_name, r.brain_id);
            }
            Err(err) if is_unavailable(&err) => return Err(err),
            Err(err) => {
                tracing::warn!("could not drop {}: {err:#}", r.index_name);
                db::fail_retirement(pool, &r.index_name, &format!("{err:#}")).await?;
            }
        }
    }
    Ok(due.len())
}

// ── 2. Provision ───────────────────────────────────────────────────────────

async fn provision_pending(pool: &PgPool, es: &Es) -> Result<usize> {
    let pending = db::unprovisioned(pool).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let everyone = db::brain_indices(pool).await?;
    let retired = db::retired_index_names(pool).await?;
    let mut owned: Option<Vec<Owned>> = None;
    let mut done = 0;
    for brain in &pending {
        if waiting(es, &brain.uid) {
            continue;
        }
        match provision(pool, es, brain, &everyone, &retired, &mut owned).await {
            Ok(true) => {
                es.provision_retry.lock().unwrap().remove(&brain.uid);
                done += 1;
            }
            // The brain was retired while we worked; its retirement handles it.
            Ok(false) => {}
            Err(err) if is_unavailable(&err) => return Err(err),
            Err(err) => {
                let delay = back_off(es, &brain.uid);
                tracing::warn!(
                    "could not provision an index for {}: {err:#} — retrying in {}s",
                    brain.name,
                    delay.as_secs()
                );
            }
        }
    }
    Ok(done)
}

fn waiting(es: &Es, uid: &str) -> bool {
    es.provision_retry
        .lock()
        .unwrap()
        .get(uid)
        .is_some_and(|(_, at)| Instant::now() < *at)
}

fn back_off(es: &Es, uid: &str) -> Duration {
    let mut retry = es.provision_retry.lock().unwrap();
    let entry = retry.entry(uid.to_string()).or_insert((0, Instant::now()));
    entry.0 += 1;
    let delay = Duration::from_secs(2u64.pow(entry.0.min(11))).min(Duration::from_secs(3600));
    entry.1 = Instant::now() + delay;
    delay
}

/// Give one brain a working index. `false` when the brain generation this
/// was for has been retired in the meantime.
async fn provision(
    pool: &PgPool,
    es: &Es,
    brain: &BrainIndex,
    everyone: &[BrainIndex],
    retired: &HashMap<String, bool>,
    owned: &mut Option<Vec<Owned>>,
) -> Result<bool> {
    let claimed_by_other = |name: &str| {
        everyone
            .iter()
            .any(|o| o.id != brain.id && o.index_name.as_deref() == Some(name))
    };

    let mut adopting = false;
    let target = match &brain.index_name {
        // Predates uids: take over the old name-derived index if it is there.
        Some(name) if brain.adopt_legacy => {
            if es.exists(name).await? {
                adopting = true;
                name.clone()
            } else {
                db::abandon_legacy(pool, &brain.id, &brain.uid).await?;
                es.index_for_uid(&brain.uid)
            }
        }
        // Provisioned before and lost since (or a crash mid-provision).
        Some(name) => name.clone(),
        None => {
            // A database rebuilt around an index already stamped for this
            // brain: adopt it rather than paying to embed every note again.
            if owned.is_none() {
                *owned = Some(es.list_owned().await?);
            }
            let found = owned.as_deref().unwrap_or_default().iter().find(|o| {
                o.meta.as_ref().is_some_and(|m| m.brain_id == brain.id)
                    && !retired.contains_key(&o.index)
                    && !claimed_by_other(&o.index)
            });
            match found {
                Some(o) => {
                    adopting = true;
                    o.index.clone()
                }
                None => es.index_for_uid(&brain.uid),
            }
        }
    };

    // Record the name before creating anything, so a retirement racing this
    // knows which index to drop.
    if !db::claim_index_name(pool, &brain.id, &brain.uid, &target).await? {
        return Ok(false);
    }
    let meta = es.meta_for(&brain.id, &brain.uid);
    let created = if es.exists(&target).await? {
        es.put_meta(&target, &meta).await?;
        false
    } else {
        es.create_index(&target, &meta).await?
    };
    if adopting && !created {
        let n = es.delete_foreign_docs(&target, &brain.id).await?;
        if n > 0 {
            tracing::info!("{target}: removed {n} document(s) belonging to other vaults");
        }
        tracing::info!("{}: adopted existing index {target}", brain.name);
    }
    es.attach(&target, &brain.uid).await?;
    if created {
        // A fresh index is empty. A new brain's notes are already queued by
        // ingest; one whose index was lost needs them all again.
        let n = db::enqueue_all(pool, Some(&brain.id)).await?;
        tracing::info!("{}: index {target} ready, {n} note(s) queued", brain.name);
    }
    db::mark_index_ready(pool, &brain.id, &brain.uid).await
}

// ── 3. Drain ───────────────────────────────────────────────────────────────

async fn drain_documents(pool: &PgPool, es: &Es) -> Result<usize> {
    let claimed = db::claim_queue(pool, es.cfg.batch as i64, LEASE_SECS).await?;
    if claimed.is_empty() {
        return Ok(0);
    }

    // The upserts need their current content; the deletes need only a path.
    let wanted: Vec<&QueueItem> = claimed.iter().filter(|i| i.op == "upsert").collect();
    let brain_ids: Vec<String> = wanted.iter().map(|i| i.brain_id.clone()).collect();
    let rel_paths: Vec<String> = wanted.iter().map(|i| i.rel_path.clone()).collect();
    let docs = db::notes_by_path(pool, &brain_ids, &rel_paths).await?;
    let by_path: HashMap<(String, String), db::IndexDoc> = docs
        .into_iter()
        .map(|d| ((d.brain_id.clone(), d.rel_path.clone()), d))
        .collect();

    // A note that vanished between enqueue and claim has nothing to index. Its
    // removal was queued separately by delete_notes, so dropping the row here
    // loses nothing.
    let mut vanished: Vec<QueueItem> = Vec::new();
    let mut sending: Vec<QueueItem> = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    for item in claimed {
        let target = es.write_alias(&item.uid);
        if item.op == "delete" {
            lines.push(delete_line(&target, &item.rel_path));
            sending.push(item);
            continue;
        }
        match by_path.get(&(item.brain_id.clone(), item.rel_path.clone())) {
            Some(doc) => {
                lines.push(index_lines(&target, doc));
                sending.push(item);
            }
            None => vanished.push(item),
        }
    }

    db::finish_queue(pool, &vanished).await?;
    if sending.is_empty() {
        return Ok(vanished.len());
    }

    // One request for the whole batch, so a cluster that is down produces one
    // warn line rather than one per document.
    let response = match es.bulk(lines.concat()).await {
        Ok(response) => response,
        Err(err) if is_unavailable(&err) => {
            // Not the documents' fault: hand them back uncharged.
            let ids: Vec<i64> = sending.iter().map(|i| i.id).collect();
            db::release_queue(pool, &ids).await?;
            return Err(err);
        }
        Err(err) => {
            tracing::warn!("bulk of {} failed, backing off: {err:#}", sending.len());
            let why = clip(&format!("{err:#}"), 400);
            let failures: Vec<(i64, String)> =
                sending.iter().map(|i| (i.id, why.clone())).collect();
            db::fail_queue(pool, &failures).await?;
            return Ok(sending.len() + vanished.len());
        }
    };

    let settled = settle(&sending, &response);
    let ok = db::finish_queue(pool, &settled.done).await?;
    if !settled.failed.is_empty() {
        tracing::warn!("{} of {} documents rejected", settled.failed.len(), sending.len());
        db::fail_queue(pool, &settled.failed).await?;
    }
    if !settled.lost.is_empty() {
        // The write alias is gone. For a brain that still exists that means
        // its index was deleted behind our back: stop writing, reprovision.
        // For one being retired the rows are already gone and this is moot.
        let brains: BTreeSet<(String, String)> = settled
            .lost
            .iter()
            .map(|i| (i.brain_id.clone(), i.uid.clone()))
            .collect();
        for (brain_id, uid) in &brains {
            tracing::warn!("{brain_id}: write alias missing — reprovisioning its index");
            db::mark_index_lost(pool, brain_id, uid).await?;
        }
        let ids: Vec<i64> = settled.lost.iter().map(|i| i.id).collect();
        db::release_queue(pool, &ids).await?;
    }
    if !settled.throttled.is_empty() {
        let ids: Vec<i64> = settled.throttled.iter().map(|i| i.id).collect();
        db::release_queue(pool, &ids).await?;
        return Err(anyhow!(Unavailable(format!(
            "{} document(s) refused with 429",
            ids.len()
        ))));
    }
    tracing::debug!("indexed {ok} document(s)");
    Ok(sending.len() + vanished.len())
}

/// A bulk response sorted by what should happen to each row.
#[derive(Debug, Default)]
struct Settled {
    /// Landed (or a delete of something already gone). Remove from the queue.
    done: Vec<QueueItem>,
    /// Refused for a reason about the document. Charge an attempt.
    failed: Vec<(i64, String)>,
    /// The target alias does not exist. Release, and reprovision the brain.
    lost: Vec<QueueItem>,
    /// Refused with 429. Release uncharged and back off the whole worker.
    throttled: Vec<QueueItem>,
}

/// Split a bulk response into what landed, what was refused, and what could
/// not be attempted.
fn settle(sent: &[QueueItem], response: &Value) -> Settled {
    let mut out = Settled::default();
    let Some(items) = response.get("items").and_then(Value::as_array) else {
        // A 200 with no items array should not happen; treat it as success
        // rather than looping on rows Elasticsearch has already accepted.
        out.done = sent.to_vec();
        return out;
    };
    for (item, result) in sent.iter().zip(items) {
        let inner = result
            .as_object()
            .and_then(|o| o.values().next())
            .cloned()
            .unwrap_or(Value::Null);
        let status = inner.get("status").and_then(Value::as_i64).unwrap_or(0);
        let Some(error) = inner.get("error") else {
            out.done.push(item.clone());
            continue;
        };
        let kind = error.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "index_not_found_exception" {
            out.lost.push(item.clone());
        } else if status == 429 {
            out.throttled.push(item.clone());
        } else if status == 404 && item.op == "delete" {
            // A delete of a document that is not there is the state we wanted.
            out.done.push(item.clone());
        } else {
            let reason = error
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            out.failed.push((item.id, clip(reason, 400)));
        }
    }
    out
}

/// Two NDJSON lines: the action, then the document.
pub fn index_lines(index: &str, doc: &db::IndexDoc) -> String {
    let action = json!({ "index": { "_index": index, "_id": doc.rel_path } });
    // The semantic field is composed here rather than with copy_to: copy_to
    // into a semantic_text field is not supported, and doing it in the writer
    // keeps what gets embedded visible in one place.
    let body = json!({
        "note_id": doc.note_id,
        "brain_id": doc.brain_id,
        "brain_name": doc.brain_name,
        "rel_path": doc.rel_path,
        "title": doc.title,
        "source": doc.source,
        "tags": doc.tags,
        "headings": doc.headings,
        "excerpt": doc.excerpt,
        "body": doc.body,
        "mtime": doc.mtime,
        "degree": doc.degree,
        "size": doc.size,
        "content_hash": doc.content_hash,
        "indexed_at": chrono::Utc::now().to_rfc3339(),
        "semantic": semantic_text(&doc.title, &doc.body),
    });
    format!("{action}\n{body}\n")
}

pub fn delete_line(index: &str, rel_path: &str) -> String {
    format!("{}\n", json!({ "delete": { "_index": index, "_id": rel_path } }))
}

/// What actually gets embedded.
pub fn semantic_text(title: &str, body: &str) -> String {
    format!("{title}\n\n{body}")
}

// ── Reconcile ──────────────────────────────────────────────────────────────

/// What reconcile decided, before it acts. A pure function of the three
/// inputs, so the rules are tested without a cluster.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    /// Ready brains whose index is missing: reprovision (which requeues).
    pub lost: Vec<(String, String)>,
    /// Ready brains whose index is there: compare document counts.
    pub check: Vec<(String, String, String)>,
    /// Indices stamped as ours with no brain behind them: retire.
    pub orphans: Vec<(String, String, String)>,
    /// Indices under our prefix with no stamp of ours: report only.
    pub foreign: Vec<String>,
}

pub(crate) fn plan(
    brains: &[BrainIndex],
    owned: &[Owned],
    retired: &HashMap<String, bool>,
) -> Plan {
    let present: HashSet<&str> = owned.iter().map(|o| o.index.as_str()).collect();
    let mut out = Plan::default();

    for b in brains.iter().filter(|b| b.ready) {
        let Some(index) = &b.index_name else { continue };
        if present.contains(index.as_str()) {
            out.check.push((b.id.clone(), b.name.clone(), index.clone()));
        } else {
            out.lost.push((b.id.clone(), b.uid.clone()));
        }
    }

    for o in owned {
        if brains.iter().any(|b| b.index_name.as_deref() == Some(o.index.as_str())) {
            continue;
        }
        let Some(meta) = &o.meta else {
            out.foreign.push(o.index.clone());
            continue;
        };
        // A retirement still pending will drop it; nothing to add.
        if retired.get(&o.index) == Some(&false) {
            continue;
        }
        // Its brain exists but has no index yet: provisioning may adopt it.
        if brains.iter().any(|b| b.id == meta.brain_id && !b.ready) {
            continue;
        }
        out.orphans.push((o.index.clone(), meta.brain_id.clone(), meta.uid.clone()));
    }
    out
}

/// Compare the cluster with Postgres, both ways, and queue whatever fixes the
/// difference. Runs at startup, every `RECONCILE_EVERY`, and after an outage.
///
/// The cases: a database rebuilt while Elasticsearch kept its documents, an
/// index deleted by hand, a count gone out of step, and an index left behind
/// by a brain that no longer exists. Only indices carrying our own `_meta`
/// are ever retired; anything else under the prefix is reported, not touched.
pub async fn reconcile(pool: &PgPool, es: &Es) -> Result<()> {
    let owned = es.list_owned().await?;
    let brains = db::brain_indices(pool).await?;
    let retired = db::retired_index_names(pool).await?;
    let plan = plan(&brains, &owned, &retired);

    for (brain_id, uid) in &plan.lost {
        tracing::warn!("{brain_id}: its index is missing — reprovisioning");
        db::mark_index_lost(pool, brain_id, uid).await?;
    }
    for (index, brain_id, uid) in &plan.orphans {
        tracing::warn!("{index}: stamped for brain {brain_id}, which is gone — retiring it");
        db::retire_orphan(pool, index, brain_id, uid).await?;
    }
    if !plan.foreign.is_empty() {
        tracing::info!(
            "not ours, left alone: {} (no aibrain _meta for prefix {})",
            plan.foreign.join(", "),
            es.cfg.index_prefix
        );
    }

    for (brain_id, name, index) in &plan.check {
        let in_es = es.count(index).await?;
        let in_pg = db::count_notes_for_brain(pool, brain_id).await?;
        if in_es == in_pg {
            continue;
        }
        let queued = db::queue_depth_for_brain(pool, brain_id).await?;
        if queued > 0 {
            tracing::info!(
                "{name}: {in_pg} notes vs {in_es} in {index}, {queued} already queued — leaving it"
            );
            continue;
        }
        let n = db::enqueue_all(pool, Some(brain_id)).await?;
        tracing::info!("{name}: {in_pg} notes vs {in_es} in {index} — queued {n} for reindex");
    }

    let mut health = es.health.lock().unwrap();
    health.last_reconcile = Some(chrono::Utc::now());
    health.foreign_indices = plan.foreign;
    for (index, _, _) in plan.orphans {
        if !health.orphans_retired.contains(&index) {
            health.orphans_retired.push(index);
        }
    }
    // Enough to read on a status page, not a growing list.
    let excess = health.orphans_retired.len().saturating_sub(20);
    health.orphans_retired.drain(..excess);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> db::IndexDoc {
        db::IndexDoc {
            note_id: 12,
            brain_id: "b1".into(),
            brain_name: "Grognard".into(),
            rel_path: "Notes/Tides.md".into(),
            title: "Tides".into(),
            source: "Notes".into(),
            tags: vec!["sea".into()],
            headings: vec!["Moon".into()],
            excerpt: "about tides".into(),
            body: "The moon pulls the ocean.".into(),
            mtime: 1.5,
            degree: 3,
            size: 25,
            content_hash: "deadbeef".into(),
        }
    }

    #[test]
    fn an_upsert_is_two_ndjson_lines_keyed_by_path() {
        let text = index_lines("aibrain-grognard", &doc());
        let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
        assert_eq!(lines.len(), 2, "action line then document line");
        assert!(text.ends_with('\n'), "bulk bodies must end in a newline");

        let action: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(action["index"]["_index"], "aibrain-grognard");
        // The id is the path, not the Postgres serial: rebuild the database
        // and the ids move, the paths do not.
        assert_eq!(action["index"]["_id"], "Notes/Tides.md");

        let body: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(body["note_id"], 12);
        assert_eq!(body["tags"][0], "sea");
        assert_eq!(body["semantic"], "Tides\n\nThe moon pulls the ocean.");
        assert!(body["indexed_at"].is_string());
    }

    #[test]
    fn a_delete_is_one_line() {
        let text = delete_line("aibrain-grognard", "Notes/Gone.md");
        assert_eq!(text.matches('\n').count(), 1);
        let action: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(action["delete"]["_id"], "Notes/Gone.md");
    }

    #[test]
    fn a_batch_frames_as_one_body() {
        let body = format!("{}{}", index_lines("i", &doc()), delete_line("i", "x.md"));
        // Three lines: two for the upsert, one for the delete.
        assert_eq!(body.lines().count(), 3);
        for line in body.lines() {
            serde_json::from_str::<Value>(line).expect("every line is a JSON object");
        }
    }

    fn item(id: i64, op: &str) -> QueueItem {
        QueueItem {
            id,
            brain_id: "b1".into(),
            brain_name: "Grognard".into(),
            uid: "u1".into(),
            index_name: "aibrain-u1".into(),
            note_id: Some(id),
            rel_path: format!("n{id}.md"),
            op: op.into(),
            attempts: 0,
            enqueued_at: chrono::Utc::now(),
        }
    }

    fn ids(items: &[QueueItem]) -> Vec<i64> {
        items.iter().map(|i| i.id).collect()
    }

    #[test]
    fn successes_and_failures_are_told_apart() {
        let sent = vec![item(1, "upsert"), item(2, "upsert"), item(3, "delete")];
        let response = json!({ "errors": true, "items": [
            { "index":  { "status": 201 } },
            { "index":  { "status": 400, "error": { "type": "mapper_parsing_exception",
                                                    "reason": "mapper_parsing_exception: nope" } } },
            { "delete": { "status": 404, "error": { "type": "not_found", "reason": "not found" } } }
        ]});
        let settled = settle(&sent, &response);
        assert_eq!(ids(&settled.done), vec![1, 3]);
        assert_eq!(settled.failed.len(), 1);
        assert_eq!(settled.failed[0].0, 2);
        assert!(settled.failed[0].1.contains("mapper_parsing_exception"));
        assert!(settled.lost.is_empty() && settled.throttled.is_empty());
    }

    #[test]
    fn a_missing_write_alias_is_lost_not_failed() {
        // What `require_alias=true` answers once an index has been dropped.
        // Not the document's fault, and not "already deleted" either — even
        // for a delete, which is otherwise done on a 404.
        let sent = vec![item(1, "upsert"), item(2, "delete")];
        let missing = json!({
            "type": "index_not_found_exception",
            "reason": "no such index [aibrain-_w-u1] and [require_alias] request flag is [true]"
        });
        let response = json!({ "errors": true, "items": [
            { "index":  { "status": 404, "error": missing } },
            { "delete": { "status": 404, "error": missing } }
        ]});
        let settled = settle(&sent, &response);
        assert_eq!(ids(&settled.lost), vec![1, 2]);
        assert!(settled.done.is_empty() && settled.failed.is_empty());
    }

    #[test]
    fn a_429_is_throttled_not_charged() {
        let sent = vec![item(1, "upsert"), item(2, "upsert")];
        let response = json!({ "errors": true, "items": [
            { "index": { "status": 201 } },
            { "index": { "status": 429, "error": { "type": "es_rejected_execution_exception",
                                                   "reason": "queue full" } } }
        ]});
        let settled = settle(&sent, &response);
        assert_eq!(ids(&settled.done), vec![1]);
        assert_eq!(ids(&settled.throttled), vec![2]);
        assert!(settled.failed.is_empty());
    }

    #[test]
    fn a_response_without_items_does_not_replay_the_batch() {
        let sent = vec![item(1, "upsert")];
        let settled = settle(&sent, &json!({ "took": 3 }));
        assert_eq!(settled.done.len(), 1);
        assert!(settled.failed.is_empty());
    }

    // ── reconcile's rules ──────────────────────────────────────────────────

    fn brain(id: &str, index: Option<&str>, ready: bool) -> BrainIndex {
        BrainIndex {
            id: id.into(),
            name: id.to_uppercase(),
            uid: format!("uid-{id}"),
            index_name: index.map(str::to_string),
            ready,
            adopt_legacy: false,
        }
    }

    fn owned(index: &str, brain_id: Option<&str>) -> Owned {
        Owned {
            index: index.into(),
            meta: brain_id.map(|b| super::super::IndexMeta {
                brain_id: b.into(),
                uid: format!("uid-{b}"),
                schema: 1,
            }),
        }
    }

    #[test]
    fn a_ready_brain_is_checked_when_its_index_is_there_and_lost_when_not() {
        let brains = [brain("a", Some("p-a"), true), brain("b", Some("p-b"), true)];
        let p = plan(&brains, &[owned("p-a", Some("a"))], &HashMap::new());
        assert_eq!(p.check, vec![("a".into(), "A".into(), "p-a".into())]);
        assert_eq!(p.lost, vec![("b".into(), "uid-b".into())]);
        assert!(p.orphans.is_empty() && p.foreign.is_empty());
    }

    #[test]
    fn a_stamped_index_with_no_brain_is_an_orphan() {
        let p = plan(&[], &[owned("p-gone", Some("gone"))], &HashMap::new());
        assert_eq!(p.orphans, vec![("p-gone".into(), "gone".into(), "uid-gone".into())]);
    }

    #[test]
    fn an_unstamped_index_is_reported_never_retired() {
        // `aibrain-test`, `aibrain-other`, or anything another tool made.
        let p = plan(&[], &[owned("p-stray", None)], &HashMap::new());
        assert_eq!(p.foreign, vec!["p-stray".to_string()]);
        assert!(p.orphans.is_empty());
    }

    #[test]
    fn a_pending_retirement_is_left_to_the_worker() {
        let pending = HashMap::from([("p-gone".to_string(), false)]);
        let p = plan(&[], &[owned("p-gone", Some("gone"))], &pending);
        assert!(p.orphans.is_empty());
        // But one already carried out, whose index is somehow back, goes again.
        let done = HashMap::from([("p-gone".to_string(), true)]);
        let p = plan(&[], &[owned("p-gone", Some("gone"))], &done);
        assert_eq!(p.orphans.len(), 1);
    }

    #[test]
    fn an_index_its_unprovisioned_brain_may_adopt_is_not_an_orphan() {
        // A database rebuilt around an index: the brain is back, has no index
        // name yet, and provisioning will find this one by its stamp.
        let brains = [brain("a", None, false)];
        let p = plan(&brains, &[owned("p-old", Some("a"))], &HashMap::new());
        assert!(p.orphans.is_empty());
    }

    #[test]
    fn an_index_a_brain_records_is_never_an_orphan_or_foreign() {
        // A legacy index adopted but not yet stamped has no _meta.
        let brains = [brain("a", Some("p-legacy"), false)];
        let p = plan(&brains, &[owned("p-legacy", None)], &HashMap::new());
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        let base = Duration::from_secs(4);
        let j = jitter(base);
        assert!(j >= base && j <= base + Duration::from_secs(1));
    }
}
