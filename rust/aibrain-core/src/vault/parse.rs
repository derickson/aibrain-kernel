//! Reading one markdown file into a record.
//!
//! Deliberately tolerant, for the same reason the Python version it replaces
//! was: a vault is somebody's pile of markdown, not a schema. One note in the
//! test corpus has `{}` as its entire frontmatter, plenty have none, and
//! several have keys that no YAML parser would accept. Losing a note is worse
//! than losing its metadata, so every failure here degrades to "we still have
//! the title and the body".

use std::collections::BTreeSet;
use std::path::Path;

use regex::Regex;
use std::sync::OnceLock;

/// One markdown file, parsed.
#[derive(Debug, Clone)]
pub struct ParsedNote {
    pub rel_path: String,
    pub title: String,
    /// Top-level folder — the ribbon this note sits in. Root files get "Root".
    pub source: String,
    pub body: String,
    pub excerpt: String,
    pub tags: Vec<String>,
    pub headings: Vec<String>,
    /// Raw wikilink and relative-markdown-link targets, in document order.
    pub targets: Vec<String>,
    pub content_hash: [u8; 32],
}

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"!?\[\[([^\[\]]+?)\]\]").unwrap())
}

fn md_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]]*\]\(([^)\s]+\.md)\)").unwrap())
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|\s)#([A-Za-z0-9][\w/-]*)").unwrap())
}

fn heading_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^#{1,6}\s+(.*)$").unwrap())
}

fn code_fence_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```").unwrap())
}

/// Fold a link target or title into a comparable key.
///
/// Mirrors how Obsidian treats `[[Agent-Context_Protocol]]` and
/// `[[agent context protocol]]` as the same target.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    for ch in text.chars() {
        // Strip combining marks so "Café" and "Cafe" match, the same fold the
        // Python version did with NFKD + combining-character removal.
        if is_combining(ch) {
            continue;
        }
        let ch = if ch == '_' || ch == '-' { ' ' } else { ch };
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        }
    }
    out.trim().to_string()
}

fn is_combining(ch: char) -> bool {
    matches!(ch as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF)
}

/// Split YAML-ish frontmatter off the top, returning (keys, body).
///
/// Only flat scalars and simple lists — the values that matter (title, tags,
/// aliases, ids) are flat, and a real YAML parser would reject files this
/// corpus actually contains.
pub fn split_frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let rest = match text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        Some(r) => r,
        None => return (Vec::new(), text),
    };
    let Some(end) = find_closing_fence(rest) else {
        return (Vec::new(), text);
    };
    let (block, after) = rest.split_at(end);
    let body = after
        .trim_start_matches("---")
        .trim_start_matches('\r')
        .trim_start_matches('\n');

    let mut fields: Vec<(String, String)> = Vec::new();
    let mut key: Option<String> = None;
    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "{}" || trimmed == "---" {
            continue;
        }
        if let (Some(item), Some(k)) = (trimmed.strip_prefix("- "), key.as_ref()) {
            let value = item.trim().trim_matches(['"', '\'']).to_string();
            if let Some(slot) = fields.iter_mut().find(|(existing, _)| existing == k) {
                if slot.1.is_empty() {
                    slot.1 = value;
                } else {
                    slot.1.push_str(", ");
                    slot.1.push_str(&value);
                }
            }
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('\t') {
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_string();
                let v = v.trim().trim_matches(['"', '\'']).to_string();
                key = Some(k.clone());
                fields.push((k, v));
            }
        }
    }
    (fields, body)
}

fn find_closing_fence(rest: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// A readable plain-text rendering, for excerpts and for the search column.
pub fn strip_markup(text: &str) -> String {
    let no_code = code_fence_re().replace_all(text, " ");
    let unlinked = wikilink_re().replace_all(&no_code, |caps: &regex::Captures| {
        let inner = &caps[1];
        inner
            .rsplit('|')
            .next()
            .unwrap_or(inner)
            .split('#')
            .next()
            .unwrap_or(inner)
            .to_string()
    });
    let mut out = String::with_capacity(unlinked.len());
    let mut last_space = false;
    for ch in unlinked.chars() {
        let ch = if matches!(ch, '*' | '_' | '`' | '>' | '#' | '[' | ']') { ' ' } else { ch };
        if ch == ' ' || ch == '\t' {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    out
}

/// Wikilink and relative-markdown-link targets, in document order, deduplicated.
pub fn link_targets(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for caps in wikilink_re().captures_iter(text) {
        let raw = caps[1]
            .split('|')
            .next()
            .unwrap_or("")
            .split('#')
            .next()
            .unwrap_or("")
            .trim();
        if !raw.is_empty() && seen.insert(raw.to_string()) {
            out.push(raw.to_string());
        }
    }
    for caps in md_link_re().captures_iter(text) {
        let raw = &caps[1];
        if raw.starts_with("http://") || raw.starts_with("https://") {
            continue;
        }
        let name = raw.rsplit('/').next().unwrap_or(raw);
        let name = name.strip_suffix(".md").unwrap_or(name).trim();
        if !name.is_empty() && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }
    out
}

fn excerpt(body: &str, limit: usize) -> String {
    let plain = strip_markup(body);
    let plain = plain.trim();
    if plain.chars().count() <= limit {
        return plain.to_string();
    }
    let cut: String = plain.chars().take(limit).collect();
    match cut.rfind(' ') {
        Some(at) if at > limit * 6 / 10 => format!("{}…", &cut[..at]),
        _ => format!("{}…", cut.trim_end()),
    }
}

/// Parse one file's text into a record. `rel_path` is posix-style, relative to
/// the vault root.
pub fn parse(rel_path: &str, text: &str) -> ParsedNote {
    // Postgres refuses a NUL byte in any text column, valid UTF-8 or not —
    // a note that has picked one up (a paste from a binary source, a
    // truncated write) would otherwise take the whole scan down with it.
    let owned;
    let text = if text.contains('\0') {
        owned = text.replace('\0', "");
        owned.as_str()
    } else {
        text
    };
    let (fields, body) = split_frontmatter(text);

    let stem = Path::new(rel_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(rel_path)
        .to_string();
    let title = fields
        .iter()
        .find(|(k, _)| k == "title")
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or(stem);

    let source = match rel_path.split_once('/') {
        Some((folder, _)) => folder.to_string(),
        None => "Root".to_string(),
    };

    let mut tags: BTreeSet<String> = fields
        .iter()
        .find(|(k, _)| k == "tags")
        .map(|(_, v)| {
            v.split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let scrubbed = code_fence_re().replace_all(body, " ");
    for caps in tag_re().captures_iter(&scrubbed) {
        tags.insert(caps[1].to_string());
    }

    let headings: Vec<String> = heading_re()
        .captures_iter(body)
        .take(12)
        .map(|c| c[1].trim().to_string())
        .collect();

    ParsedNote {
        rel_path: rel_path.to_string(),
        title,
        source,
        excerpt: excerpt(body, 400),
        tags: tags.into_iter().collect(),
        headings,
        targets: link_targets(body),
        content_hash: *blake3::hash(text.as_bytes()).as_bytes(),
        body: body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_split_off() {
        let (fields, body) = split_frontmatter("---\ntitle: Index\ntags: root\n---\n\n# Index\n");
        assert_eq!(fields.iter().find(|(k, _)| k == "title").unwrap().1, "Index");
        assert!(body.starts_with("# Index"));
    }

    #[test]
    fn empty_object_frontmatter_survives() {
        // A real note in the corpus opens exactly like this.
        let (fields, body) = split_frontmatter("---\n{}\n---\n\n# Hermes Agent\n\nText.\n");
        assert!(fields.is_empty());
        assert!(body.starts_with("# Hermes Agent"));
    }

    #[test]
    fn a_file_without_frontmatter_is_untouched() {
        let text = "# Plain\n\nNo frontmatter here.\n";
        let (fields, body) = split_frontmatter(text);
        assert!(fields.is_empty());
        assert_eq!(body, text);
    }

    #[test]
    fn an_unterminated_fence_is_not_frontmatter() {
        let text = "---\ntitle: never closed\n\n# Body\n";
        let (fields, body) = split_frontmatter(text);
        assert!(fields.is_empty());
        assert_eq!(body, text, "the whole file is body when the fence never closes");
    }

    #[test]
    fn list_values_are_joined() {
        let (fields, _) = split_frontmatter("---\naliases:\n  - One\n  - Two\n---\nbody\n");
        assert_eq!(fields.iter().find(|(k, _)| k == "aliases").unwrap().1, "One, Two");
    }

    #[test]
    fn link_targets_are_found_in_order_without_duplicates() {
        let body = "See [[Protocols]], [[Recipes/Sourdough]], [[Protocols|again]] and \
                    [a file](./Notes/Other.md) plus [web](https://example.com/x.md).";
        assert_eq!(
            link_targets(body),
            vec!["Protocols", "Recipes/Sourdough", "Other"]
        );
    }

    #[test]
    fn embeds_and_headings_are_stripped_from_targets() {
        assert_eq!(link_targets("![[Diagram]] and [[Note#Heading]]"), vec!["Diagram", "Note"]);
    }

    #[test]
    fn normalize_folds_separators_and_accents() {
        assert_eq!(normalize("Agent-Context_Protocol"), "agent context protocol");
        assert_eq!(normalize("Cafe\u{0301}"), "cafe");
        assert_eq!(normalize("  Spaced   Out  "), "spaced out");
    }

    #[test]
    fn source_is_the_top_level_folder() {
        assert_eq!(parse("Journal/2026-01-01.md", "# Hi").source, "Journal");
        assert_eq!(parse("Welcome.md", "# Hi").source, "Root");
        assert_eq!(parse("a/b/c.md", "# Hi").source, "a");
    }

    #[test]
    fn title_prefers_frontmatter_then_filename() {
        assert_eq!(parse("x.md", "---\ntitle: Real\n---\nbody").title, "Real");
        assert_eq!(parse("Some Note.md", "body").title, "Some Note");
        // An empty title key must not win over the filename.
        assert_eq!(parse("Fallback.md", "---\ntitle:\n---\nbody").title, "Fallback");
    }

    #[test]
    fn tags_come_from_both_frontmatter_and_body() {
        let note = parse("x.md", "---\ntags: alpha\n---\nText #beta and #gamma/deep\n");
        assert!(note.tags.contains(&"alpha".to_string()));
        assert!(note.tags.contains(&"beta".to_string()));
        assert!(note.tags.contains(&"gamma/deep".to_string()));
    }

    #[test]
    fn tags_inside_code_fences_are_ignored() {
        let note = parse("x.md", "```\n#notatag\n```\nreal #tag\n");
        assert!(note.tags.contains(&"tag".to_string()));
        assert!(!note.tags.contains(&"notatag".to_string()));
    }

    #[test]
    fn the_hash_tracks_content_not_metadata() {
        let a = parse("x.md", "same body");
        let b = parse("different-name.md", "same body");
        assert_eq!(a.content_hash, b.content_hash);
        assert_ne!(a.content_hash, parse("x.md", "other body").content_hash);
    }

    #[test]
    fn nul_bytes_are_stripped_rather_than_carried_into_postgres() {
        // Postgres rejects a NUL byte in a text column outright, so a note
        // that has one anywhere would otherwise fail on write and take the
        // whole scan down with it.
        let note = parse("x.md", "---\ntitle: Tit\0le\n---\n\nBody with a stray\0byte.\n");
        assert_eq!(note.title, "Title");
        assert!(!note.body.contains('\0'));
        assert_eq!(
            note.content_hash,
            parse("x.md", "---\ntitle: Title\n---\n\nBody with a straybyte.\n").content_hash
        );
    }
}
