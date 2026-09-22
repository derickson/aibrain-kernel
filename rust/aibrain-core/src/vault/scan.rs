//! Walking a vault.
//!
//! Read-only, always: this process never writes into somebody's notes. Folders
//! that hold attachments rather than prose are skipped, because indexing a
//! thousand images as empty notes helps nobody.

use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Folders skipped by default. Matches the Python exporter's list so the two
/// agree on what a vault contains.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".obsidian", ".trash", ".git", "ZZ-Attachments", "ZZ-Attachements",
    "assets", "scans", "Excalidraw", "node_modules",
];

/// One markdown file found on disk.
pub struct Found {
    pub path: PathBuf,
    pub rel_path: String,
    pub mtime: f64,
    pub size: i64,
}

fn is_markdown(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
        Some("md") | Some("markdown")
    )
}

/// Every markdown file under `root`, skipping excluded and hidden folders.
///
/// Symlinks are followed at the root (the vault itself is one) but not below
/// it, so a link inside a vault cannot walk us out of the vault or into a loop.
pub fn walk(root: &Path, excludes: &[String]) -> Vec<Found> {
    let skip: Vec<&str> = if excludes.is_empty() {
        DEFAULT_EXCLUDES.to_vec()
    } else {
        excludes.iter().map(String::as_str).collect()
    };

    WalkDir::new(root)
        .follow_root_links(true)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            !(entry.file_type().is_dir() && skip.iter().any(|s| *s == name))
        })
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && is_markdown(entry.path()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            let rel = entry.path().strip_prefix(root).ok()?;
            let rel_path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Some(Found {
                mtime: meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0),
                size: meta.len() as i64,
                rel_path,
                path: entry.path().to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aibrain-scan-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_markdown_and_skips_the_rest() {
        let dir = scratch("basic");
        fs::write(dir.join("One.md"), "a").unwrap();
        fs::write(dir.join("Two.markdown"), "b").unwrap();
        fs::write(dir.join("image.png"), "c").unwrap();
        fs::create_dir_all(dir.join("Notes")).unwrap();
        fs::write(dir.join("Notes/Three.md"), "d").unwrap();

        let mut found: Vec<String> = walk(&dir, &[]).into_iter().map(|f| f.rel_path).collect();
        found.sort();
        assert_eq!(found, ["Notes/Three.md", "One.md", "Two.markdown"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hidden_and_excluded_folders_are_skipped() {
        let dir = scratch("excludes");
        fs::create_dir_all(dir.join(".obsidian")).unwrap();
        fs::write(dir.join(".obsidian/conf.md"), "x").unwrap();
        fs::create_dir_all(dir.join("Excalidraw")).unwrap();
        fs::write(dir.join("Excalidraw/d.md"), "x").unwrap();
        fs::create_dir_all(dir.join("Keep")).unwrap();
        fs::write(dir.join("Keep/y.md"), "x").unwrap();

        let found: Vec<String> = walk(&dir, &[]).into_iter().map(|f| f.rel_path).collect();
        assert_eq!(found, ["Keep/y.md"]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn relative_paths_are_posix_style() {
        let dir = scratch("paths");
        fs::create_dir_all(dir.join("a/b/c")).unwrap();
        fs::write(dir.join("a/b/c/Deep.md"), "x").unwrap();
        let found = walk(&dir, &[]);
        assert_eq!(found[0].rel_path, "a/b/c/Deep.md");
        assert!(found[0].size > 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_symlinked_vault_root_is_followed() {
        // This is how every vault reaches us: obsidian_vaults/X is a link.
        let real = scratch("real");
        fs::write(real.join("Note.md"), "x").unwrap();
        let links = scratch("links");
        let link = links.join("Vault");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let found: Vec<String> = walk(&link, &[]).into_iter().map(|f| f.rel_path).collect();
        assert_eq!(found, ["Note.md"]);
        fs::remove_dir_all(&real).ok();
        fs::remove_dir_all(&links).ok();
    }
}
