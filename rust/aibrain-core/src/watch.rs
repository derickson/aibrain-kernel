//! Watching the vaults for changes.
//!
//! Runs on the host, not in a container: macOS delivers filesystem events
//! through FSEvents, and a bind mount into a podman or Docker VM does not
//! forward them. A watcher inside the container would sit silent while you
//! edited notes, which is why `docker-compose.yml` only containerises Postgres.
//!
//! Events are debounced because a single save in Obsidian produces a burst —
//! the editor writes a temp file, renames it over the original, and touches the
//! directory. Reacting to each one would reparse the same note several times.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// `Watcher` must be in scope for `watch()` — it is a trait method.
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::new_debouncer;

use crate::api::Ctx;
use crate::events::Change;
use crate::ingest;

/// How long to wait for a burst of writes to settle before reacting.
const DEBOUNCE: Duration = Duration::from_millis(400);

pub fn spawn(ctx: Arc<Ctx>) {
    // The notify channel is blocking, so the watch loop owns an OS thread
    // rather than a task. That thread has no runtime of its own, so the handle
    // is captured here — while we are still inside the runtime — and carried
    // across; asking for it on the other side panics.
    let runtime = tokio::runtime::Handle::current();
    std::thread::Builder::new()
        .name("vault-watch".into())
        .spawn(move || {
            if let Err(err) = run(ctx, runtime) {
                tracing::error!("watcher stopped: {err:#}");
            }
        })
        .expect("spawn watcher thread");
}

/// How often the set of watched vaults is compared with the config, so a
/// vault linked or unlinked while we run is picked up without a restart.
const RESYNC: Duration = Duration::from_secs(5);

fn run(ctx: Arc<Ctx>, runtime: tokio::runtime::Handle) -> anyhow::Result<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(DEBOUNCE, None, tx)?;
    // Real directory -> brain name, for what is being watched right now.
    let mut watched: BTreeMap<PathBuf, String> = BTreeMap::new();
    sync_watches(&ctx, debouncer.watcher(), &mut watched);
    if watched.is_empty() {
        tracing::info!("nothing linked in obsidian_vaults/ yet — watching for vaults to appear");
    }

    loop {
        let result = match rx.recv_timeout(RESYNC) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                sync_watches(&ctx, debouncer.watcher(), &mut watched);
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let events = match result {
            Ok(events) => events,
            Err(errors) => {
                for err in errors {
                    tracing::warn!("watch error: {err}");
                }
                continue;
            }
        };

        let mut touched: HashSet<PathBuf> = HashSet::new();
        for event in events {
            for path in &event.paths {
                if is_markdown(path) {
                    touched.insert(path.clone());
                }
            }
        }
        if touched.is_empty() {
            continue;
        }

        // Re-read the config each round: a vault linked or unlinked while we
        // run should take effect without a restart.
        let cfg = match ctx.config() {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::warn!("config unreadable: {err:#}");
                continue;
            }
        };

        let ctx = ctx.clone();
        runtime.spawn(async move {
            if let Err(err) = apply(&ctx, &cfg, touched).await {
                tracing::warn!("watch round failed: {err:#}");
            }
        });
    }
    Ok(())
}

/// Watch every linked vault and stop watching any that was unlinked.
///
/// Watching is by real path, because the vault is reached through a symlink
/// and FSEvents reports the real path — matching against the link would never
/// line up. A vault whose link is gone no longer canonicalises, so it drops
/// out here, and its events stop arriving rather than being filtered later.
fn sync_watches(
    ctx: &Ctx,
    watcher: &mut impl Watcher,
    watched: &mut BTreeMap<PathBuf, String>,
) {
    let cfg = match ctx.config() {
        Ok(cfg) => cfg,
        Err(err) => {
            tracing::warn!("config unreadable, keeping the current watches: {err:#}");
            return;
        }
    };
    let wanted: BTreeMap<PathBuf, String> = cfg
        .brains
        .iter()
        .filter_map(|b| {
            let real = std::fs::canonicalize(&b.root).ok()?;
            real.is_dir().then(|| (real, b.name.clone()))
        })
        .collect();

    let gone: Vec<PathBuf> = watched.keys().filter(|p| !wanted.contains_key(*p)).cloned().collect();
    for path in gone {
        if let Err(err) = watcher.unwatch(&path) {
            tracing::debug!("unwatch {}: {err}", path.display());
        }
        if let Some(name) = watched.remove(&path) {
            tracing::info!("stopped watching {name} ({})", path.display());
        }
    }
    for (path, name) in wanted {
        if watched.contains_key(&path) {
            continue;
        }
        match watcher.watch(&path, RecursiveMode::Recursive) {
            Ok(()) => {
                tracing::info!("watching {name} ({})", path.display());
                watched.insert(path, name);
            }
            Err(err) => tracing::warn!("could not watch {name} ({}): {err}", path.display()),
        }
    }
}

/// Re-read one debounced burst of files and tell anyone listening.
///
/// The revision is per brain now, so the events this publishes carry the
/// number a client can compare against the one it is holding. Note events go
/// out first and the brain event last, so a UI that only handles the coarse
/// one still sees the right revision arrive after the notes it explains.
pub(crate) async fn apply(
    ctx: &Arc<Ctx>,
    cfg: &crate::config::Config,
    touched: HashSet<PathBuf>,
) -> anyhow::Result<()> {
    let before = crate::db::brain_fingerprints(&ctx.pool).await?;
    // brain id -> the notes in it this round rewrote.
    let mut written: BTreeMap<String, Vec<i64>> = BTreeMap::new();

    for path in touched {
        let Some((brain, rel)) = locate(&cfg.brains, &path) else {
            continue;
        };
        match ingest::ingest_one(&ctx.pool, brain, &rel).await {
            Ok(Some(note_id)) => {
                tracing::info!("changed {}/{}", brain.name, rel);
                written.entry(brain.id.clone()).or_default().push(note_id);
            }
            // A write that did not alter the bytes — an autosave.
            Ok(None) => {}
            Err(err) => tracing::warn!("could not ingest {rel}: {err:#}"),
        }
    }
    if written.is_empty() {
        return Ok(());
    }

    // Links are global: a new `[[target]]` in one note can resolve against
    // another vault, so the graph is rebuilt rather than patched. It is one
    // pass of SQL and cheap enough to do here.
    crate::db::rebuild_links(&ctx.pool).await?;

    let after = crate::db::brain_fingerprints(&ctx.pool).await?;
    let mut bumping: BTreeSet<String> = after
        .iter()
        .filter(|(id, fingerprint)| before.get(*id) != Some(fingerprint))
        .map(|(id, _)| id.clone())
        .collect();
    bumping.extend(written.keys().filter(|id| after.contains_key(*id)).cloned());
    let bumped = crate::db::bump_revisions(&ctx.pool, &bumping.into_iter().collect::<Vec<_>>())
        .await?;
    ctx.revision.fetch_add(1, Ordering::Relaxed);

    let revision_of: BTreeMap<&str, i64> =
        bumped.iter().map(|(id, rev)| (id.as_str(), *rev)).collect();
    for (brain_id, note_ids) in &written {
        let revision = revision_of.get(brain_id.as_str()).copied().unwrap_or(0);
        for note_id in note_ids {
            ctx.events.publish(Change::note(brain_id, revision, *note_id));
        }
    }
    for (brain_id, revision) in &bumped {
        ctx.events.publish(Change::brain(brain_id, *revision));
    }
    Ok(())
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("md") | Some("markdown")
    )
}

/// Which brain owns this path, and where it sits inside it.
///
/// Compares against the canonical root because the watcher is given real paths
/// while the config holds symlinks.
fn locate<'a>(
    brains: &'a [crate::config::BrainSpec],
    path: &Path,
) -> Option<(&'a crate::config::BrainSpec, String)> {
    for brain in brains {
        let root = std::fs::canonicalize(&brain.root).ok()?;
        if let Ok(rel) = path.strip_prefix(&root) {
            let rel_path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if !rel_path.is_empty() {
                return Some((brain, rel_path));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_markdown_counts() {
        assert!(is_markdown(Path::new("/v/Note.md")));
        assert!(is_markdown(Path::new("/v/Note.MARKDOWN")));
        assert!(!is_markdown(Path::new("/v/image.png")));
        assert!(!is_markdown(Path::new("/v/.DS_Store")));
    }

    #[test]
    fn a_path_is_matched_to_its_vault() {
        let dir = std::env::temp_dir().join(format!("aibrain-watch-{}", std::process::id()));
        let vault = dir.join("V");
        std::fs::create_dir_all(vault.join("Notes")).unwrap();
        let brains = vec![crate::config::BrainSpec {
            id: "v".into(),
            name: "V".into(),
            root: vault.to_string_lossy().into_owned(),
            seed: 1,
            excludes: vec![],
            color: "#4db3f0".into(),
            group_by: Default::default(),
        }];

        let inside = std::fs::canonicalize(&vault).unwrap().join("Notes/A.md");
        let (brain, rel) = locate(&brains, &inside).unwrap();
        assert_eq!(brain.id, "v");
        assert_eq!(rel, "Notes/A.md");

        assert!(locate(&brains, Path::new("/somewhere/else/B.md")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
