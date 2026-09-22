//! Reading the same `~/.aibrain/config.json` the Python side writes.
//!
//! One config file, two readers. Rust never writes it — the UI owns that — so
//! there is no question of the two disagreeing about who is authoritative.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct BrainSpec {
    pub id: String,
    pub name: String,
    pub root: String,
    pub seed: i32,
    pub excludes: Vec<String>,
    /// The UI owns assigning this (see `reconcile_brains` in aibrain/config.py);
    /// the fallback below only covers a config Python has not touched yet.
    pub color: String,
}

#[derive(Debug, Deserialize)]
struct RawBrain {
    id: String,
    name: String,
    path: String,
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    seed: Option<i32>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    color: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawView {
    #[serde(default)]
    ribbon_twist: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawTodo {
    #[serde(default)]
    day_start_hour: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    brains: Vec<RawBrain>,
    #[serde(default)]
    view: Option<RawView>,
    #[serde(default)]
    todo: Option<RawTodo>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub brains: Vec<BrainSpec>,
    pub ribbon_twist: f64,
    pub title: String,
    /// The hour a day begins. Work finished at 01:00 belongs to the evening
    /// before, not to a new day's empty list.
    pub todo_day_start_hour: u32,
}

/// 04:00 unless the config says otherwise: late enough that a night owl's last
/// task lands on the day they were working, early enough that nobody is up
/// before it.
pub const DEFAULT_DAY_START_HOUR: u32 = 4;

/// Where the config lives, honouring `AIBRAIN_HOME` the way Python does.
pub fn default_path() -> PathBuf {
    match std::env::var("AIBRAIN_HOME") {
        Ok(home) => Path::new(&home).join("config.json"),
        Err(_) => dirs_home().join(".aibrain").join("config.json"),
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let raw: RawConfig = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let brains = raw
        .brains
        .into_iter()
        .filter(|b| b.enabled)
        .enumerate()
        .map(|(i, b)| BrainSpec {
            seed: b.seed.unwrap_or(7 + i as i32 * 13),
            color: b.color.filter(|c| !c.is_empty()).unwrap_or_else(|| {
                crate::layout::SOURCE_COLORS[i % crate::layout::SOURCE_COLORS.len()].to_string()
            }),
            id: b.id,
            name: b.name,
            root: expand(&b.path),
            excludes: b.exclude,
        })
        // A vault whose link is broken simply is not there; the UI reports it.
        .filter(|b| Path::new(&b.root).is_dir())
        .collect();

    Ok(Config {
        brains,
        ribbon_twist: raw.view.and_then(|v| v.ribbon_twist).unwrap_or(0.25),
        title: raw.title.unwrap_or_else(|| "AI Brains".to_string()),
        todo_day_start_hour: raw
            .todo
            .and_then(|t| t.day_start_hour)
            // A nonsense hour is a typo, not an instruction; fall back rather
            // than refuse to start.
            .filter(|h| (0..24).contains(h))
            .map(|h| h as u32)
            .unwrap_or(DEFAULT_DAY_START_HOUR),
    })
}

fn expand(path: &str) -> String {
    match path.strip_prefix("~/") {
        Some(rest) => dirs_home().join(rest).to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_brains_are_left_out() {
        let dir = std::env::temp_dir().join(format!("aibrain-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let vault = dir.join("V");
        std::fs::create_dir_all(&vault).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"brains":[
                     {{"id":"a","name":"A","path":"{}","enabled":true}},
                     {{"id":"b","name":"B","path":"{}","enabled":false}}
                   ]}}"#,
                vault.to_string_lossy(),
                vault.to_string_lossy()
            ),
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.brains.len(), 1);
        assert_eq!(cfg.brains[0].id, "a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_brain_whose_path_is_gone_is_left_out() {
        let dir = std::env::temp_dir().join(format!("aibrain-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"brains":[{"id":"x","name":"X","path":"/nope/nothing/here"}]}"#,
        )
        .unwrap();
        assert!(load(&path).unwrap().brains.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_color_falls_back_to_the_palette_but_an_explicit_one_wins() {
        let dir = std::env::temp_dir().join(format!("aibrain-cfg5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("A");
        let b = dir.join("B");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            format!(
                r##"{{"brains":[
                     {{"id":"a","name":"A","path":"{}"}},
                     {{"id":"b","name":"B","path":"{}","color":"#123456"}}
                   ]}}"##,
                a.to_string_lossy(),
                b.to_string_lossy()
            ),
        )
        .unwrap();
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.brains[0].color, crate::layout::SOURCE_COLORS[0]);
        assert_eq!(cfg.brains[1].color, "#123456");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_fields_fall_back_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("aibrain-cfg3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{}"#).unwrap();
        let cfg = load(&path).unwrap();
        assert!(cfg.brains.is_empty());
        assert_eq!(cfg.ribbon_twist, 0.25);
        assert_eq!(cfg.todo_day_start_hour, DEFAULT_DAY_START_HOUR);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_day_start_hour_is_read_and_a_silly_one_is_ignored() {
        let dir = std::env::temp_dir().join(format!("aibrain-cfg4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"todo":{"day_start_hour":6}}"#).unwrap();
        assert_eq!(load(&path).unwrap().todo_day_start_hour, 6);
        std::fs::write(&path, r#"{"todo":{"day_start_hour":99}}"#).unwrap();
        assert_eq!(load(&path).unwrap().todo_day_start_hour, DEFAULT_DAY_START_HOUR);
        std::fs::remove_dir_all(&dir).ok();
    }
}
