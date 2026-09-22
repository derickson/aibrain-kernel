//! The background drain: `search_queue` into Elasticsearch, in batches.
//!
//! One bulk request per batch rather than one write per note, because the
//! embedding model runs inside the write. Fifty notes in one call cost one
//! round trip and one model warm-up; fifty calls cost fifty of each.
//!
//! The loop is factored so that `drain_once` is a plain function a test can
//! call. A test that has to start a background task and then guess how long to
//! wait is a test that fails on a slow machine.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::{clip, Es};
use crate::db::{self, QueueItem};

/// How long a claimed row stays claimed. Long enough for a slow bulk, short
/// enough that a worker killed mid-batch does not strand its work.
const LEASE_SECS: i64 = 120;

/// Start the drain loop. Returns immediately.
pub fn spawn(pool: PgPool, es: Arc<Es>) {
    let poll = Duration::from_millis(es.cfg.poll_ms);
    tokio::spawn(async move {
        loop {
            match drain_once(&pool, &es).await {
                Ok(0) => tokio::time::sleep(poll).await,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!("search worker: {err:#}");
                    tokio::time::sleep(poll).await;
                }
            }
        }
    });
}

/// Claim one batch, send it, settle the rows. Returns how many were claimed.
///
/// Zero means the queue is empty (or entirely backed off), which is the
/// caller's cue to sleep.
pub async fn drain_once(pool: &PgPool, es: &Es) -> Result<usize> {
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
        let index = es.index_for(&item.brain_name);
        if item.op == "delete" {
            lines.push(delete_line(&index, &item.rel_path));
            sending.push(item);
            continue;
        }
        match by_path.get(&(item.brain_id.clone(), item.rel_path.clone())) {
            Some(doc) => {
                lines.push(index_lines(&index, doc));
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
    let sent = async {
        let indices: std::collections::BTreeSet<String> =
            sending.iter().map(|i| es.index_for(&i.brain_name)).collect();
        for index in indices {
            es.ensure_index(&index).await?;
        }
        es.bulk(lines.concat()).await
    }
    .await;

    let response = match sent {
        Ok(response) => response,
        Err(err) => {
            tracing::warn!("bulk of {} failed, backing off: {err:#}", sending.len());
            let why = clip(&format!("{err:#}"), 400);
            let failures: Vec<(i64, String)> =
                sending.iter().map(|i| (i.id, why.clone())).collect();
            db::fail_queue(pool, &failures).await?;
            return Ok(sending.len() + vanished.len());
        }
    };

    let (done, failed) = settle(&sending, &response);
    let ok = db::finish_queue(pool, &done).await?;
    if !failed.is_empty() {
        tracing::warn!("{} of {} documents rejected", failed.len(), sending.len());
        db::fail_queue(pool, &failed).await?;
    }
    tracing::debug!("indexed {ok} document(s)");
    Ok(sending.len() + vanished.len())
}

/// Split a bulk response into the items that landed and the ones that did not.
fn settle(sent: &[QueueItem], response: &Value) -> (Vec<QueueItem>, Vec<(i64, String)>) {
    let items = response.get("items").and_then(Value::as_array);
    let Some(items) = items else {
        // A 200 with no items array should not happen; treat it as success
        // rather than looping on rows Elasticsearch has already accepted.
        return (sent.to_vec(), Vec::new());
    };
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for (item, result) in sent.iter().zip(items) {
        let inner = result
            .as_object()
            .and_then(|o| o.values().next())
            .cloned()
            .unwrap_or(Value::Null);
        let status = inner.get("status").and_then(Value::as_i64).unwrap_or(0);
        match inner.get("error") {
            // A delete of a document that is not there is the state we wanted.
            None => done.push(item.clone()),
            Some(_) if status == 404 && item.op == "delete" => done.push(item.clone()),
            Some(error) => {
                let reason = error
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                failed.push((item.id, clip(reason, 400)));
            }
        }
    }
    (done, failed)
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

/// At startup, notice a cluster that is out of step with Postgres.
///
/// The common cases are a database rebuilt while Elasticsearch kept its
/// documents, and an index deleted by hand. Either way the fix is the same:
/// queue the brain and let the worker sort it out.
pub async fn reconcile(pool: &PgPool, es: &Es) -> Result<()> {
    for brain in db::list_brains(pool).await? {
        let index = es.index_for(&brain.name);
        let in_es = match es.count(&index).await {
            Ok(n) => n,
            Err(err) => {
                tracing::warn!("could not count {index}: {err:#}");
                continue;
            }
        };
        let in_pg = db::count_notes_for_brain(pool, &brain.id).await?;
        if in_es == in_pg {
            continue;
        }
        let queued = db::queue_depth_for_brain(pool, &brain.id).await?;
        if queued > 0 {
            tracing::info!(
                "{}: {in_pg} notes vs {in_es} in {index}, {queued} already queued — leaving it",
                brain.name
            );
            continue;
        }
        let n = db::enqueue_all(pool, Some(&brain.id)).await?;
        tracing::info!(
            "{}: {in_pg} notes vs {in_es} in {index} — queued {n} for reindex",
            brain.name
        );
    }
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
            note_id: Some(id),
            rel_path: format!("n{id}.md"),
            op: op.into(),
            attempts: 0,
            enqueued_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn successes_and_failures_are_told_apart() {
        let sent = vec![item(1, "upsert"), item(2, "upsert"), item(3, "delete")];
        let response = json!({ "errors": true, "items": [
            { "index":  { "status": 201 } },
            { "index":  { "status": 400, "error": { "reason": "mapper_parsing_exception: nope" } } },
            { "delete": { "status": 404, "error": { "reason": "not found" } } }
        ]});
        let (done, failed) = settle(&sent, &response);
        assert_eq!(done.iter().map(|i| i.id).collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, 2);
        assert!(failed[0].1.contains("mapper_parsing_exception"));
    }

    #[test]
    fn a_response_without_items_does_not_replay_the_batch() {
        let sent = vec![item(1, "upsert")];
        let (done, failed) = settle(&sent, &json!({ "took": 3 }));
        assert_eq!(done.len(), 1);
        assert!(failed.is_empty());
    }
}
