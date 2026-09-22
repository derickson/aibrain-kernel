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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

// `Watcher` must be in scope for `watch()` — it is a trait method.
use notify::{RecursiveMode, Watcher};
use notify_debouncer_full::new_debouncer;

use crate::api::Ctx;
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

fn run(ctx: Arc<Ctx>, runtime: tokio::runtime::Handle) -> anyhow::Result<()> {
    let cfg = ctx.config()?;
    if cfg.brains.is_empty() {
        tracing::info!("nothing linked in obsidian_vaults/ — not watching");
        return Ok(());
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let mut debouncer = new_debouncer(DEBOUNCE, None, tx)?;

    for brain in &cfg.brains {
        let root = Path::new(&brain.root);
        // The vault is reached through a symlink, so watch what it points at —
        // FSEvents reports the real path, and matching against the link would
        // never line up.
        let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        debouncer.watcher().watch(&real, RecursiveMode::Recursive)?;
        tracing::info!("watching {} ({})", brain.name, real.display());
    }

    for result in rx {
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
            let mut changed = false;
            for path in touched {
                let Some((brain, rel)) = locate(&cfg.brains, &path) else {
                    continue;
                };
                match ingest::ingest_one(&ctx.pool, brain, &rel).await {
                    Ok(true) => {
                        tracing::info!("changed {}/{}", brain.name, rel);
                        changed = true;
                    }
                    // A write that did not alter the bytes — an autosave.
                    Ok(false) => {}
                    Err(err) => tracing::warn!("could not ingest {rel}: {err:#}"),
                }
            }
            if changed {
                // Links are global: a new `[[target]]` in one note can resolve
                // against another vault, so the graph is rebuilt rather than
                // patched. It is one pass of SQL and cheap enough to do here.
                if let Err(err) = crate::db::rebuild_links(&ctx.pool).await {
                    tracing::warn!("link rebuild failed: {err:#}");
                }
                ctx.revision.fetch_add(1, Ordering::Relaxed);
            }
        });
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
        }];

        let inside = std::fs::canonicalize(&vault).unwrap().join("Notes/A.md");
        let (brain, rel) = locate(&brains, &inside).unwrap();
        assert_eq!(brain.id, "v");
        assert_eq!(rel, "Notes/A.md");

        assert!(locate(&brains, Path::new("/somewhere/else/B.md")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
