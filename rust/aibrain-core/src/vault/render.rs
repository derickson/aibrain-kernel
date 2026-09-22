//! Markdown to HTML.
//!
//! Replaces the hand-written Python renderer. Wikilinks are the reason this
//! exists rather than a plain pulldown-cmark call: `[[Target]]` has to become
//! a link the UI can act on, and the ids come from the link table the ingest
//! already built.

use pulldown_cmark::{html, Event, Options, Parser};
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(!?)\[\[([^\[\]]+?)\]\]").unwrap())
}

/// Render markdown, leaving wikilinks as placeholders the caller resolves.
pub fn to_html(body: &str, _brain_id: &str) -> String {
    to_html_with(body, &HashMap::new())
}

/// Render markdown, turning `[[Target]]` into a link when the target resolved.
pub fn to_html_with(body: &str, resolved: &HashMap<String, i64>) -> String {
    // Wikilinks are swapped for sentinels first so the markdown parser cannot
    // reinterpret their contents, then swapped back after escaping.
    let mut stash: Vec<String> = Vec::new();
    let staged = wikilink_re().replace_all(body, |caps: &regex::Captures| {
        let embed = !caps[1].is_empty();
        let inner = &caps[2];
        let (target, alias) = match inner.split_once('|') {
            Some((t, a)) => (t, Some(a)),
            None => (inner, None),
        };
        let clean = target.split('#').next().unwrap_or(target).trim();
        let label = alias.unwrap_or(clean).trim();

        let html = if embed {
            format!("<span class=\"embed\">⧉ {}</span>", escape(label))
        } else {
            match resolved.get(&crate::vault::normalize(clean)) {
                Some(id) => format!(
                    "<a class=\"wiki\" data-note=\"{id}\" href=\"#note-{id}\">{}</a>",
                    escape(label)
                ),
                None => format!(
                    "<span class=\"wiki wiki-missing\" title=\"not in the index\">{}</span>",
                    escape(label)
                ),
            }
        };
        stash.push(html);
        format!("\u{0}{}\u{0}", stash.len() - 1)
    });

    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    // pulldown-cmark passes raw HTML through untouched. A note is not a
    // trusted document — it can be anything you pasted — so HTML in the source
    // is shown as text rather than rendered into the app's own DOM.
    let events = Parser::new_ext(&staged, options).map(|event| match event {
        Event::Html(raw) => Event::Text(raw),
        Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });

    let mut out = String::with_capacity(staged.len() * 2);
    html::push_html(&mut out, events);

    // Put the wikilink HTML back. The sentinel survives the parser as text,
    // which is why it is a control character rather than anything markdown-ish.
    let restore = Regex::new("\u{0}(\\d+)\u{0}").unwrap();
    restore
        .replace_all(&out, |caps: &regex::Captures| {
            stash
                .get(caps[1].parse::<usize>().unwrap_or(usize::MAX))
                .cloned()
                .unwrap_or_default()
        })
        .into_owned()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_markdown_renders() {
        let html = to_html("# Title\n\nSome **bold** and `code`.\n", "b");
        assert!(html.contains("<h1>Title</h1>"));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn tables_and_tasks_render() {
        let html = to_html("| A | B |\n|---|---|\n| 1 | 2 |\n", "b");
        assert!(html.contains("<table>"), "{html}");
        let tasks = to_html("- [x] done\n- [ ] open\n", "b");
        assert!(tasks.contains("type=\"checkbox\""), "{tasks}");
    }

    #[test]
    fn a_resolved_wikilink_becomes_a_clickable_link() {
        let mut resolved = HashMap::new();
        resolved.insert("darkwater".to_string(), 42i64);
        let html = to_html_with("See [[Darkwater]].", &resolved);
        assert!(html.contains("data-note=\"42\""), "{html}");
        assert!(html.contains(">Darkwater</a>"), "{html}");
    }

    #[test]
    fn an_unresolved_wikilink_is_marked_not_linked() {
        let html = to_html_with("See [[Nowhere]].", &HashMap::new());
        assert!(html.contains("wiki-missing"), "{html}");
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn aliases_and_headings_are_handled() {
        let mut resolved = HashMap::new();
        resolved.insert("target".to_string(), 7i64);
        let html = to_html_with("[[Target|shown]] and [[Target#Section]]", &resolved);
        assert!(html.contains(">shown</a>"), "{html}");
        assert!(html.contains("data-note=\"7\""), "{html}");
    }

    #[test]
    fn embeds_are_not_links() {
        let html = to_html_with("![[Diagram]]", &HashMap::new());
        assert!(html.contains("embed"), "{html}");
        assert!(!html.contains("<a "), "{html}");
    }

    #[test]
    fn raw_html_in_a_note_cannot_inject() {
        // The payload text survives as *content* — that is fine and expected.
        // What must not survive is a real tag, so assert on the markup, not on
        // whether the scary-looking string appears anywhere in the output.
        for source in [
            "<img src=x onerror=alert(1)>",
            "<script>alert(1)</script>",
            "Inline <b onclick=\"steal()\">text</b> here",
            "<div onmouseover=x>block</div>",
            "<iframe src=//evil></iframe>",
        ] {
            let html = to_html(source, "b");
            for tag in ["<img", "<script", "<iframe", "<b ", "<div "] {
                assert!(!html.contains(tag), "{source} produced a live {tag}: {html}");
            }
            assert!(html.contains("&lt;"), "{source} was not escaped: {html}");
        }
    }

    #[test]
    fn a_wikilink_label_cannot_inject() {
        let html = to_html_with("[[<script>alert(1)</script>]]", &HashMap::new());
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }
}
