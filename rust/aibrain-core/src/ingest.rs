//! Getting the vaults into Postgres, and keeping them there.
//!
//! A scan compares what is on disk against what the database already holds and
//! touches only the difference. The comparison is by content hash rather than
//! by timestamp alone, so an editor that rewrites a file byte-for-byte — which
//! Obsidian does on every autosave — costs a read and a hash, not a reparse
//! and a reindex.

use anyhow::Result;
use sqlx::PgPool;
use std::path::Path;

use crate::config::BrainSpec;
use crate::db;
use crate::layout;
use crate::vault::{self, scan};

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub links: i64,
}

impl Stats {
    pub fn changed(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

/// Rescan every brain and rebuild the link graph.
pub async fn reindex(
    pool: &PgPool,
    brains: &[BrainSpec],
    force: bool,
    mut progress: impl FnMut(&str),
) -> Result<Stats> {
    let mut total = Stats::default();

    let keep: Vec<String> = brains.iter().map(|b| b.id.clone()).collect();
    let dropped = db::retain_brains(pool, &keep).await?;
    if dropped > 0 {
        progress(&format!("dropped {dropped} brain(s) no longer linked"));
    }

    for brain in brains {
        db::upsert_brain(pool, &brain.id, &brain.name, &brain.root, brain.seed).await?;
        let stats = ingest_brain(pool, brain, force, &mut progress).await?;
        total.scanned += stats.scanned;
        total.added += stats.added;
        total.updated += stats.updated;
        total.removed += stats.removed;
        total.unchanged += stats.unchanged;
    }

    progress("resolving links…");
    total.links = db::rebuild_links(pool).await?;
    Ok(total)
}

/// Write one parsed note and its link targets.
///
/// Both the full scan and the watcher go through here. They used to each build
/// their own upsert, and promptly drifted — one stored the raw markdown while
/// the other stored stripped text, so a note edited live silently lost its
/// formatting and its wikilinks.
async fn write_note(
    pool: &PgPool,
    brain: &BrainSpec,
    parsed: &vault::ParsedNote,
    mtime: f64,
    size: i64,
) -> Result<i64> {
    let note_id = db::upsert_note(
        pool,
        &brain.id,
        &parsed.rel_path,
        &parsed.title,
        &parsed.source,
        &parsed.excerpt,
        // The raw markdown: the reader renders from this, and to_tsvector
        // tokenises the markup away on its own.
        &parsed.body,
        &parsed.tags,
        &parsed.headings,
        mtime,
        size,
        &parsed.content_hash,
        layout::slot_for(&brain.id, &parsed.rel_path),
    )
    .await?;
    db::set_link_targets(pool, note_id, &parsed.targets).await?;
    // Queue the note for Elasticsearch even when no cluster is configured.
    // An insert is cheap, and a queue kept while the feature was off is what
    // lets it be switched on later without rescanning the vaults.
    db::enqueue_note(
        pool,
        &brain.id,
        &brain.name,
        Some(note_id),
        &parsed.rel_path,
        "upsert",
    )
    .await?;
    Ok(note_id)
}

/// Scan one vault and apply the difference.
pub async fn ingest_brain(
    pool: &PgPool,
    brain: &BrainSpec,
    force: bool,
    progress: &mut impl FnMut(&str),
) -> Result<Stats> {
    let root = Path::new(&brain.root);
    if !root.is_dir() {
        progress(&format!("skipping {}: {} is not a directory", brain.name, brain.root));
        return Ok(Stats::default());
    }
    progress(&format!("scanning {} ({})", brain.name, brain.root));

    let found = scan::walk(root, &brain.excludes);
    let known = db::existing_notes(pool, &brain.id).await?;
    let mut stats = Stats { scanned: found.len(), ..Default::default() };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for entry in &found {
        seen.insert(entry.rel_path.clone());
        let prior = known.get(&entry.rel_path);

        // Cheap rejection first: same size and timestamp means untouched.
        if !force {
            if let Some((_, _, mtime, size)) = prior {
                if (*mtime - entry.mtime).abs() < 0.001 && *size == entry.size {
                    stats.unchanged += 1;
                    continue;
                }
            }
        }

        let Ok(text) = tokio::fs::read_to_string(&entry.path).await else {
            continue;
        };
        let parsed = vault::parse(&entry.rel_path, &text);

        // The timestamp moved but the bytes did not — an autosave. Nothing to do.
        if let Some((_, hash, _, _)) = prior {
            if !force && hash.as_slice() == parsed.content_hash.as_slice() {
                stats.unchanged += 1;
                continue;
            }
        }

        write_note(pool, brain, &parsed, entry.mtime, entry.size).await?;

        if prior.is_some() {
            stats.updated += 1;
        } else {
            stats.added += 1;
        }
        if (stats.added + stats.updated) % 500 == 0 {
            progress(&format!("  {}: {} written", brain.name, stats.added + stats.updated));
        }
    }

    let stale: Vec<i64> = known
        .iter()
        .filter(|(path, _)| !seen.contains(*path))
        .map(|(_, (id, _, _, _))| *id)
        .collect();
    stats.removed = stale.len();
    db::delete_notes(pool, &stale).await?;

    progress(&format!(
        "  {}: {} on disk (+{} ~{} -{} ={})",
        brain.name, stats.scanned, stats.added, stats.updated, stats.removed, stats.unchanged
    ));
    Ok(stats)
}

/// Re-read one file after the watcher saw it change.
///
/// Returns whether anything actually changed, so the caller only bumps the
/// revision — and only invalidates the layout cache — when it needs to.
pub async fn ingest_one(pool: &PgPool, brain: &BrainSpec, rel_path: &str) -> Result<bool> {
    let path = Path::new(&brain.root).join(rel_path);
    let known = db::existing_notes(pool, &brain.id).await?;

    if !path.is_file() {
        if let Some((id, _, _, _)) = known.get(rel_path) {
            db::delete_notes(pool, &[*id]).await?;
            return Ok(true);
        }
        return Ok(false);
    }

    let text = tokio::fs::read_to_string(&path).await?;
    let parsed = vault::parse(rel_path, &text);
    if let Some((_, hash, _, _)) = known.get(rel_path) {
        if hash.as_slice() == parsed.content_hash.as_slice() {
            return Ok(false);
        }
    }

    let meta = tokio::fs::metadata(&path).await?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    write_note(pool, brain, &parsed, mtime, meta.len() as i64).await?;
    Ok(true)
}
