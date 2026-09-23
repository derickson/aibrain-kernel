//! Getting the vaults into Postgres, and keeping them there.
//!
//! A scan compares what is on disk against what the database already holds and
//! touches only the difference. The comparison is by content hash rather than
//! by timestamp alone, so an editor that rewrites a file byte-for-byte — which
//! Obsidian does on every autosave — costs a read and a hash, not a reparse
//! and a reindex.

use anyhow::Result;
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::BrainSpec;
use crate::db;
use crate::layout;
use crate::vault::{self, scan};

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub links: i64,
    /// Every brain whose revision this run moved, and what it moved to.
    /// Empty when the scan found nothing to do, which is the usual case.
    pub bumped: Vec<(String, i64)>,
    /// Brains this run retired because the config no longer lists them.
    pub retired: Vec<String>,
}

impl Stats {
    pub fn changed(&self) -> bool {
        self.added > 0 || self.updated > 0 || self.removed > 0
    }
}

/// Which brains a run has to bump.
///
/// The ones the caller saw change directly, plus any whose note count or total
/// degree moved across the run. That second half is what catches a vault
/// changed from outside itself: a new `[[target]]` in vault A resolving into
/// vault B raises B's degrees, and so B's star sizes, without any file in B
/// being touched.
fn brains_to_bump(
    before: &BTreeMap<String, (i64, i64)>,
    after: &BTreeMap<String, (i64, i64)>,
    touched: &BTreeSet<String>,
) -> Vec<String> {
    let mut out: BTreeSet<String> = after
        .iter()
        .filter(|(id, fingerprint)| before.get(*id) != Some(fingerprint))
        .map(|(id, _)| id.clone())
        .collect();
    // A brain the caller wrote to still counts even when the counts came out
    // the same — an edited note changes its text, not its degree.
    out.extend(touched.iter().filter(|id| after.contains_key(*id)).cloned());
    out.into_iter().collect()
}

/// Rescan every brain and rebuild the link graph.
pub async fn reindex(
    pool: &PgPool,
    brains: &[BrainSpec],
    force: bool,
    mut progress: impl FnMut(&str),
) -> Result<Stats> {
    let mut total = Stats::default();
    let before = db::brain_fingerprints(pool).await?;

    // A brain the config no longer lists is retired the same way the Remove
    // button retires one: rows, queue and index together. This is the
    // fallback for a removal the service did not hear about directly.
    let keep: Vec<String> = brains.iter().map(|b| b.id.clone()).collect();
    for id in db::brains_not_in(pool, &keep).await? {
        if db::retire_brain(pool, &id).await?.is_some() {
            total.retired.push(id);
        }
    }
    if !total.retired.is_empty() {
        progress(&format!("retired {} brain(s) no longer linked", total.retired.len()));
    }

    let mut touched: BTreeSet<String> = BTreeSet::new();
    for brain in brains {
        // Held across the scan so a removal cannot interleave with it. Taken
        // before looking at the disk: the caller's config may be stale, and a
        // vault unlinked since then must not be written back in.
        let lock = db::BrainLock::shared(pool, &brain.id).await?;
        if !Path::new(&brain.root).is_dir() {
            progress(&format!("skipping {}: {} is no longer linked", brain.name, brain.root));
            lock.release().await?;
            continue;
        }
        db::upsert_brain(pool, &brain.id, &brain.name, &brain.root, brain.seed).await?;
        let stats = ingest_brain(pool, brain, force, &mut progress).await;
        lock.release().await?;
        let stats = stats?;
        if stats.changed() {
            touched.insert(brain.id.clone());
        }
        total.scanned += stats.scanned;
        total.added += stats.added;
        total.updated += stats.updated;
        total.removed += stats.removed;
        total.unchanged += stats.unchanged;
    }

    progress("resolving links…");
    total.links = db::rebuild_links(pool).await?;

    let after = db::brain_fingerprints(pool).await?;
    total.bumped = db::bump_revisions(pool, &brains_to_bump(&before, &after, &touched)).await?;
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
/// `None` when nothing actually changed, so the caller only bumps the
/// revision — and only invalidates the layout cache — when it needs to. `Some`
/// carries the note's id, which is what the change event names; for a delete
/// that is the id the note had, which is still what a client needs to forget.
pub async fn ingest_one(pool: &PgPool, brain: &BrainSpec, rel_path: &str) -> Result<Option<i64>> {
    let path = Path::new(&brain.root).join(rel_path);
    let known = db::existing_notes(pool, &brain.id).await?;

    if !path.is_file() {
        if let Some((id, _, _, _)) = known.get(rel_path) {
            db::delete_notes(pool, &[*id]).await?;
            return Ok(Some(*id));
        }
        return Ok(None);
    }

    let text = tokio::fs::read_to_string(&path).await?;
    let parsed = vault::parse(rel_path, &text);
    if let Some((_, hash, _, _)) = known.get(rel_path) {
        if hash.as_slice() == parsed.content_hash.as_slice() {
            return Ok(None);
        }
    }

    let meta = tokio::fs::metadata(&path).await?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);

    let id = write_note(pool, brain, &parsed, mtime, meta.len() as i64).await?;
    Ok(Some(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, i64, i64)]) -> BTreeMap<String, (i64, i64)> {
        pairs.iter().map(|(id, n, d)| (id.to_string(), (*n, *d))).collect()
    }

    fn set(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_scan_that_changed_nothing_bumps_nothing() {
        let same = map(&[("a", 10, 4), ("b", 3, 0)]);
        assert!(brains_to_bump(&same, &same, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn only_the_vault_that_gained_a_note_is_bumped() {
        let before = map(&[("a", 10, 4), ("b", 3, 0)]);
        let after = map(&[("a", 11, 4), ("b", 3, 0)]);
        assert_eq!(brains_to_bump(&before, &after, &BTreeSet::new()), vec!["a"]);
    }

    #[test]
    fn an_edit_that_moved_no_counts_still_bumps_its_own_vault() {
        // Rewriting a note's prose changes neither the count nor the degree,
        // but the browser is holding the old title and excerpt.
        let same = map(&[("a", 10, 4), ("b", 3, 0)]);
        assert_eq!(brains_to_bump(&same, &same, &set(&["b"])), vec!["b"]);
    }

    #[test]
    fn a_link_resolving_across_vaults_bumps_both() {
        // A new [[target]] written in `a` resolved into `b`, so both ends
        // gained a degree and both galaxies have to be redrawn.
        let before = map(&[("a", 10, 4), ("b", 3, 0)]);
        let after = map(&[("a", 10, 5), ("b", 3, 1)]);
        assert_eq!(brains_to_bump(&before, &after, &set(&["a"])), vec!["a", "b"]);
    }

    #[test]
    fn a_new_vault_is_bumped_and_a_dropped_one_is_not() {
        let before = map(&[("a", 10, 4), ("gone", 5, 2)]);
        let after = map(&[("a", 10, 4), ("fresh", 0, 0)]);
        assert_eq!(brains_to_bump(&before, &after, &BTreeSet::new()), vec!["fresh"]);
    }

    #[test]
    fn a_vault_touched_but_since_dropped_is_not_bumped() {
        // Nothing to update, and naming it would make the UPDATE claim a row
        // count it did not have.
        let before = map(&[("a", 10, 4)]);
        let after = map(&[("a", 10, 4)]);
        assert!(brains_to_bump(&before, &after, &set(&["gone"])).is_empty());
    }
}
