//! Every statement the service runs.
//!
//! Composed answers, not rows. `note_page` returns a note with its neighbours
//! and unresolved links in one round trip, because the caller is across a
//! socket and the previous design's habit of one query per wikilink is exactly
//! what made it impossible to grow.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

use super::text_array;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainRow {
    pub id: String,
    pub name: String,
    pub root: String,
    pub seed: i32,
    pub enabled: bool,
    pub note_count: i32,
    pub revision: i64,
    /// Epoch seconds of the last scan, or None if this brain has never been
    /// scanned. Python surfaces it as the "last built" line in the UI.
    pub scanned_at: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoteSummary {
    pub nid: i64,
    pub brain_id: String,
    pub rel_path: String,
    pub name: String,
    pub source: String,
    pub degree: i32,
    pub mtime: f64,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub snippet: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NotePage {
    pub nid: i64,
    pub brain_id: String,
    pub name: String,
    pub source: String,
    pub rel_path: String,
    pub path: String,
    pub mtime: f64,
    pub size: i64,
    pub degree: i32,
    pub words: i64,
    pub excerpt: String,
    pub tags: Vec<String>,
    pub html: String,
    pub linked: Vec<NoteSummary>,
    pub unresolved: Vec<String>,
}

/// Upsert a brain, returning nothing — the caller already knows the id.
pub async fn upsert_brain(
    pool: &PgPool,
    id: &str,
    name: &str,
    root: &str,
    seed: i32,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO brain (id, name, root, seed, enabled)
         VALUES ($1, $2, $3, $4, TRUE)
         ON CONFLICT (id) DO UPDATE
            SET name = EXCLUDED.name,
                root = EXCLUDED.root,
                seed = EXCLUDED.seed,
                enabled = TRUE",
    )
    .bind(id)
    .bind(name)
    .bind(root)
    .bind(seed)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forget every brain that is no longer linked. Cascades to its notes.
pub async fn retain_brains(pool: &PgPool, keep: &[String]) -> Result<u64> {
    let result = sqlx::query("DELETE FROM brain WHERE NOT (id = ANY($1))")
        .bind(keep)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

pub async fn list_brains(pool: &PgPool) -> Result<Vec<BrainRow>> {
    let rows = sqlx::query(
        "SELECT id, name, root, seed, enabled, note_count, revision,
                extract(epoch FROM scanned_at)::double precision AS scanned_at
           FROM brain ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| BrainRow {
            id: r.get("id"),
            name: r.get("name"),
            root: r.get("root"),
            seed: r.get("seed"),
            enabled: r.get("enabled"),
            note_count: r.get("note_count"),
            revision: r.get("revision"),
            scanned_at: r.try_get("scanned_at").ok().flatten(),
        })
        .collect())
}

/// What we already hold for a brain: path -> (id, hash, mtime, size).
///
/// One query, then the scan compares in memory. The alternative — asking
/// Postgres about each file as we walk — is thousands of round trips.
pub async fn existing_notes(
    pool: &PgPool,
    brain_id: &str,
) -> Result<std::collections::HashMap<String, (i64, Vec<u8>, f64, i64)>> {
    let rows = sqlx::query(
        "SELECT id, rel_path, content_hash, mtime, size FROM note WHERE brain_id = $1",
    )
    .bind(brain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("rel_path"),
                (
                    r.get::<i64, _>("id"),
                    r.get::<Vec<u8>, _>("content_hash"),
                    r.get::<f64, _>("mtime"),
                    r.get::<i64, _>("size"),
                ),
            )
        })
        .collect())
}

/// Insert or update one note, returning its id.
#[allow(clippy::too_many_arguments)]
pub async fn upsert_note(
    pool: &PgPool,
    brain_id: &str,
    rel_path: &str,
    title: &str,
    source: &str,
    excerpt: &str,
    body: &str,
    tags: &[String],
    headings: &[String],
    mtime: f64,
    size: i64,
    content_hash: &[u8],
    slot: f64,
) -> Result<i64> {
    let row = sqlx::query(
        "INSERT INTO note (brain_id, rel_path, title, source, excerpt, body, tags,
                           tags_text, headings, mtime, size, content_hash, slot, indexed_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,array_to_string($7,' '),$8,$9,$10,$11,$12, now())
         ON CONFLICT (brain_id, rel_path) DO UPDATE
            SET title = EXCLUDED.title,
                source = EXCLUDED.source,
                excerpt = EXCLUDED.excerpt,
                body = EXCLUDED.body,
                tags = EXCLUDED.tags,
                tags_text = EXCLUDED.tags_text,
                headings = EXCLUDED.headings,
                mtime = EXCLUDED.mtime,
                size = EXCLUDED.size,
                content_hash = EXCLUDED.content_hash,
                slot = EXCLUDED.slot,
                indexed_at = now()
         RETURNING id",
    )
    .bind(brain_id)
    .bind(rel_path)
    .bind(title)
    .bind(source)
    .bind(excerpt)
    .bind(body)
    .bind(tags)
    .bind(headings)
    .bind(mtime)
    .bind(size)
    .bind(content_hash)
    .bind(slot)
    .fetch_one(pool)
    .await?;
    Ok(row.get("id"))
}

pub async fn delete_notes(pool: &PgPool, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM note WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replace the link targets recorded for one note.
pub async fn set_link_targets(pool: &PgPool, note_id: i64, targets: &[String]) -> Result<()> {
    sqlx::query("DELETE FROM link_target WHERE src_id = $1")
        .bind(note_id)
        .execute(pool)
        .await?;
    if targets.is_empty() {
        return Ok(());
    }
    // One statement with an array beats one per target; a hub note can carry
    // a hundred links and this runs for every note in the vault.
    sqlx::query(
        "INSERT INTO link_target (src_id, target)
         SELECT $1, unnest($2::text[])",
    )
    .bind(note_id)
    .bind(targets)
    .execute(pool)
    .await?;
    Ok(())
}

/// Resolve every recorded target into an edge, and recompute degrees.
///
/// Done in SQL in one pass rather than by pulling the corpus into memory.
/// Resolution order matches Obsidian: same vault by basename, then same vault
/// by path, then a unique match anywhere — that last case is what produces the
/// arcs between galaxies.
pub async fn rebuild_links(pool: &PgPool) -> Result<i64> {
    let mut tx = pool.begin().await?;

    sqlx::query("DELETE FROM link").execute(&mut *tx).await?;
    sqlx::query("UPDATE link_target SET resolved = FALSE")
        .execute(&mut *tx)
        .await?;

    // A lookup table of every way a note can be addressed, folded the way
    // `normalize` folds: lowercased, separators to spaces.
    sqlx::query(
        "CREATE TEMP TABLE note_key ON COMMIT DROP AS
         SELECT id, brain_id, key, priority FROM (
             SELECT id, brain_id,
                    btrim(regexp_replace(lower(regexp_replace(rel_path, '^.*/', '')),
                                         '\\.md$', '')) AS key,
                    1 AS priority
               FROM note
             UNION ALL
             SELECT id, brain_id, btrim(lower(title)), 2 FROM note
             UNION ALL
             SELECT id, brain_id,
                    btrim(regexp_replace(lower(rel_path), '\\.md$', '')), 3
               FROM note
         ) k",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE note_key SET key = btrim(regexp_replace(
             replace(replace(key, '_', ' '), '-', ' '), '\\s+', ' ', 'g'))",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("CREATE INDEX ON note_key (key)")
        .execute(&mut *tx)
        .await?;

    // Same-vault matches win; a cross-vault match only counts when it is the
    // only one, otherwise a title shared by two vaults would link arbitrarily.
    sqlx::query(
        "CREATE TEMP TABLE resolved ON COMMIT DROP AS
         WITH folded AS (
             SELECT lt.id AS target_id, lt.src_id, n.brain_id AS src_brain,
                    btrim(regexp_replace(
                        replace(replace(lower(
                            regexp_replace(lt.target, '\\.md$', '')
                        ), '_', ' '), '-', ' '), '\\s+', ' ', 'g')) AS key
               FROM link_target lt
               JOIN note n ON n.id = lt.src_id
         ),
         same AS (
             SELECT DISTINCT ON (f.target_id) f.target_id, f.src_id, k.id AS dst_id
               FROM folded f
               JOIN note_key k ON k.key = f.key AND k.brain_id = f.src_brain
              ORDER BY f.target_id, k.priority, k.id
         ),
         anywhere AS (
             SELECT f.target_id, f.src_id, min(k.id) AS dst_id
               FROM folded f
               JOIN note_key k ON k.key = f.key
              WHERE f.target_id NOT IN (SELECT target_id FROM same)
              GROUP BY f.target_id, f.src_id
             HAVING count(DISTINCT k.id) = 1
         )
         SELECT * FROM same UNION ALL SELECT * FROM anywhere",
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE link_target SET resolved = TRUE
           WHERE id IN (SELECT target_id FROM resolved)",
    )
    .execute(&mut *tx)
    .await?;

    let inserted = sqlx::query(
        "INSERT INTO link (src_id, dst_id)
         SELECT DISTINCT src_id, dst_id FROM resolved WHERE src_id <> dst_id
         ON CONFLICT DO NOTHING",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    sqlx::query(
        "UPDATE note n SET degree = COALESCE(d.c, 0)
           FROM (SELECT id, (SELECT count(*) FROM link
                              WHERE src_id = note.id OR dst_id = note.id) AS c
                   FROM note) d
          WHERE FALSE",
    )
    .execute(&mut *tx)
    .await
    .ok();

    // Degree in one pass over the edge list, rather than a correlated subquery
    // per note — the difference between milliseconds and minutes at scale.
    sqlx::query(
        "WITH deg AS (
             SELECT id, count(l.*) AS c
               FROM note
               LEFT JOIN link l ON l.src_id = note.id OR l.dst_id = note.id
              GROUP BY id
         )
         UPDATE note SET degree = deg.c FROM deg WHERE note.id = deg.id",
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE brain b SET note_count = c.n, revision = b.revision + 1, scanned_at = now()
           FROM (SELECT brain_id, count(*) AS n FROM note GROUP BY brain_id) c
          WHERE b.id = c.brain_id",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE brain SET note_count = 0
          WHERE id NOT IN (SELECT DISTINCT brain_id FROM note)",
    )
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(inserted as i64)
}

pub async fn count_notes(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query("SELECT count(*) AS c FROM note")
        .fetch_one(pool)
        .await?;
    Ok(row.get("c"))
}

pub async fn count_links(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query("SELECT count(*) AS c FROM link")
        .fetch_one(pool)
        .await?;
    Ok(row.get("c"))
}

/// Notes for one brain, everything it has — no cap.
pub async fn notes_for_layout(pool: &PgPool, brain_id: &str) -> Result<Vec<crate::layout::LayoutNote>> {
    let rows = sqlx::query(
        "SELECT id, rel_path, title, source, degree, mtime
           FROM note WHERE brain_id = $1 ORDER BY id",
    )
    .bind(brain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| crate::layout::LayoutNote {
            id: r.get("id"),
            rel_path: r.get("rel_path"),
            title: r.get("title"),
            source: r.get("source"),
            degree: r.get("degree"),
            mtime: r.get("mtime"),
        })
        .collect())
}

/// Titles for a list of note ids, returned in exactly that order.
///
/// `unnest ... WITH ORDINALITY` preserves the caller's ordering, which matters
/// because the browser indexes names by global id — a set-returning join would
/// hand them back in whatever order the planner liked.
pub async fn titles_for(pool: &PgPool, ids: &[i64]) -> Result<Vec<String>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT n.title
           FROM unnest($1::bigint[]) WITH ORDINALITY AS w(id, ord)
           JOIN note n ON n.id = w.id
          ORDER BY w.ord",
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.get("title")).collect())
}

/// Edges with both ends inside one brain.
pub async fn edges_within(pool: &PgPool, brain_id: &str) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query(
        "SELECT l.src_id, l.dst_id FROM link l
           JOIN note a ON a.id = l.src_id AND a.brain_id = $1
           JOIN note b ON b.id = l.dst_id AND b.brain_id = $1",
    )
    .bind(brain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("src_id"), r.get("dst_id")))
        .collect())
}

/// Edges whose ends are in different brains — the arcs between galaxies.
pub async fn edges_across(pool: &PgPool, limit: i64) -> Result<Vec<(i64, i64)>> {
    let rows = sqlx::query(
        "SELECT DISTINCT least(l.src_id, l.dst_id) AS a, greatest(l.src_id, l.dst_id) AS b
           FROM link l
           JOIN note s ON s.id = l.src_id
           JOIN note d ON d.id = l.dst_id
          WHERE s.brain_id <> d.brain_id
          LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| (r.get("a"), r.get("b"))).collect())
}

/// Full-text search.
///
/// `any` decides how terms combine. A search box wants every term (you type
/// more to narrow); a question wants any of them, because one absent word —
/// "describe", "summarise" — otherwise takes the whole query to zero results
/// and leaves an agent with no context at all.
pub async fn search(
    pool: &PgPool,
    query: &str,
    brains: &[String],
    limit: i64,
    any: bool,
) -> Result<Vec<NoteSummary>> {
    let tsquery = to_tsquery(query, any);
    if tsquery.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT n.id, n.brain_id, n.rel_path, n.title, n.source, n.degree, n.mtime,
                ts_headline('english', n.body, q,
                    'StartSel=<mark>, StopSel=</mark>, MaxWords=26, MinWords=8, \
                     ShortWord=3, MaxFragments=1, FragmentDelimiter= … ') AS snippet,
                ts_rank_cd('{0.1, 0.3, 0.6, 1.0}', n.tsv, q) AS score
           FROM note n, to_tsquery('english', $1) q
          WHERE n.tsv @@ q
            AND ($2::text[] IS NULL OR cardinality($2::text[]) = 0
                 OR n.brain_id = ANY($2::text[]))
          ORDER BY (ts_rank_cd('{0.1, 0.3, 0.6, 1.0}', n.tsv, q)
                    + least(n.degree, 20) * 0.001) DESC
          LIMIT $3",
    )
    .bind(&tsquery)
    .bind(brains)
    .bind(limit)
    .fetch_all(pool)
    .await;

    let rows = match rows {
        Ok(rows) => rows,
        // A malformed tsquery is a user typing, not a bug. Return nothing
        // rather than a 500.
        Err(err) => {
            tracing::debug!("search failed for {tsquery:?}: {err}");
            return Ok(Vec::new());
        }
    };

    Ok(rows
        .into_iter()
        .map(|r| NoteSummary {
            nid: r.get("id"),
            brain_id: r.get("brain_id"),
            rel_path: r.get("rel_path"),
            name: r.get("title"),
            source: r.get("source"),
            degree: r.get("degree"),
            mtime: r.get("mtime"),
            snippet: r.try_get::<String, _>("snippet").unwrap_or_default(),
            score: r.try_get::<f32, _>("score").ok(),
        })
        .collect())
}

/// Build a `to_tsquery` expression from whatever a person typed.
///
/// Everything is escaped and rebuilt from scratch rather than passed through,
/// so no input can be a syntax error or an injection.
pub fn to_tsquery(text: &str, any: bool) -> String {
    let mut terms: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '\'') {
        let word: String = raw.chars().filter(|c| c.is_alphanumeric()).collect();
        if word.len() < 2 {
            continue;
        }
        // Prefix match, so search narrows as you type.
        terms.push(format!("{}:*", word.to_lowercase()));
        if terms.len() >= 12 {
            break;
        }
    }
    terms.join(if any { " | " } else { " & " })
}

pub async fn recent(pool: &PgPool, limit: i64) -> Result<Vec<NoteSummary>> {
    let rows = sqlx::query(
        "SELECT id, brain_id, rel_path, title, source, degree, mtime, excerpt
           FROM note ORDER BY mtime DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| NoteSummary {
            nid: r.get("id"),
            brain_id: r.get("brain_id"),
            rel_path: r.get("rel_path"),
            name: r.get("title"),
            source: r.get("source"),
            degree: r.get("degree"),
            mtime: r.get("mtime"),
            snippet: r.get::<String, _>("excerpt").chars().take(180).collect(),
            score: None,
        })
        .collect())
}

/// One note, found by the path it lives at rather than by its id.
///
/// The ACP agent watches which files a subprocess opens, and a path is all it
/// has to go on; without this it cannot tell a note it read from a scratch file.
pub async fn note_by_path(
    pool: &PgPool,
    brain_id: &str,
    rel_path: &str,
) -> Result<Option<NoteSummary>> {
    let row = sqlx::query(
        "SELECT id, brain_id, rel_path, title, source, degree, mtime, excerpt
           FROM note WHERE brain_id = $1 AND rel_path = $2",
    )
    .bind(brain_id)
    .bind(rel_path)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| NoteSummary {
        nid: r.get("id"),
        brain_id: r.get("brain_id"),
        rel_path: r.get("rel_path"),
        name: r.get("title"),
        source: r.get("source"),
        degree: r.get("degree"),
        mtime: r.get("mtime"),
        snippet: r.get::<String, _>("excerpt").chars().take(180).collect(),
        score: None,
    }))
}

/// One note with everything the reader needs, in a single round trip.
pub async fn note_page(pool: &PgPool, note_id: i64) -> Result<Option<NotePage>> {
    let Some(row) = sqlx::query(
        "SELECT n.id, n.brain_id, n.rel_path, n.title, n.source, n.body, n.tags,
                n.excerpt, n.mtime, n.size, n.degree, b.root
           FROM note n JOIN brain b ON b.id = n.brain_id
          WHERE n.id = $1",
    )
    .bind(note_id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };

    let brain_id: String = row.get("brain_id");
    let body: String = row.get("body");
    let root: String = row.get("root");
    let rel_path: String = row.get("rel_path");

    let linked = sqlx::query(
        "SELECT n.id, n.brain_id, n.rel_path, n.title, n.source, n.degree, n.mtime, n.excerpt
           FROM note n
          WHERE n.id IN (SELECT dst_id FROM link WHERE src_id = $1
                         UNION SELECT src_id FROM link WHERE dst_id = $1)
          ORDER BY n.degree DESC LIMIT 24",
    )
    .bind(note_id)
    .fetch_all(pool)
    .await?;

    let unresolved = sqlx::query(
        "SELECT DISTINCT target FROM link_target
          WHERE src_id = $1 AND NOT resolved LIMIT 12",
    )
    .bind(note_id)
    .fetch_all(pool)
    .await?;

    // Resolve this note's wikilinks in one query, then render. The Python
    // version ran a full-text search per link — thirty queries for a hub note —
    // which is exactly the shape that does not survive a network hop.
    let targets = crate::vault::link_targets(&body);
    let resolved = resolve_targets(pool, &brain_id, &targets).await?;
    let html = crate::vault::render::to_html_with(&body, &resolved);

    Ok(Some(NotePage {
        nid: row.get("id"),
        brain_id,
        name: row.get("title"),
        source: row.get("source"),
        path: format!("{}/{}", root.trim_end_matches('/'), rel_path),
        rel_path,
        mtime: row.get("mtime"),
        size: row.get("size"),
        degree: row.get("degree"),
        words: body.split_whitespace().count() as i64,
        excerpt: row.get("excerpt"),
        tags: text_array(&row, "tags"),
        html,
        linked: linked
            .into_iter()
            .map(|r| NoteSummary {
                nid: r.get("id"),
                brain_id: r.get("brain_id"),
                rel_path: r.get("rel_path"),
                name: r.get("title"),
                source: r.get("source"),
                degree: r.get("degree"),
                mtime: r.get("mtime"),
                snippet: String::new(),
                score: None,
            })
            .collect(),
        unresolved: unresolved.into_iter().map(|r| r.get("target")).collect(),
    }))
}

/// Resolve `[[targets]]` to note ids in one query, for the renderer.
pub async fn resolve_targets(
    pool: &PgPool,
    brain_id: &str,
    targets: &[String],
) -> Result<std::collections::HashMap<String, i64>> {
    if targets.is_empty() {
        return Ok(Default::default());
    }
    let folded: Vec<String> = targets.iter().map(|t| crate::vault::normalize(t)).collect();
    let rows = sqlx::query(
        "WITH wanted AS (SELECT unnest($1::text[]) AS key)
         SELECT DISTINCT ON (w.key) w.key, n.id
           FROM wanted w
           JOIN note n ON btrim(regexp_replace(replace(replace(lower(
                    regexp_replace(n.rel_path, '^.*/', '')), '_', ' '), '-', ' '),
                    '\\s+', ' ', 'g')) = w.key
                     OR btrim(regexp_replace(replace(replace(lower(n.title),
                    '_', ' '), '-', ' '), '\\s+', ' ', 'g')) = w.key
          ORDER BY w.key, (n.brain_id = $2) DESC, n.id",
    )
    .bind(&folded)
    .bind(brain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<String, _>("key"), r.get::<i64, _>("id")))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_box_narrows_and_a_question_widens() {
        assert_eq!(to_tsquery("agent protocol", false), "agent:* & protocol:*");
        assert_eq!(to_tsquery("agent protocol", true), "agent:* | protocol:*");
    }

    #[test]
    fn punctuation_cannot_become_syntax() {
        // Everything is rebuilt from alphanumerics, so none of this reaches
        // Postgres as operators.
        for nasty in ["'; DROP TABLE note; --", "a & b | !c", "((()))", "*:*"] {
            let q = to_tsquery(nasty, false);
            assert!(!q.contains('!'), "{nasty} -> {q}");
            assert!(!q.contains('('), "{nasty} -> {q}");
            assert!(!q.contains(';'), "{nasty} -> {q}");
        }
    }

    #[test]
    fn empty_and_tiny_input_yields_no_query() {
        assert_eq!(to_tsquery("", false), "");
        assert_eq!(to_tsquery("   ", false), "");
        assert_eq!(to_tsquery("a", false), "", "single letters are noise");
    }

    #[test]
    fn term_count_is_bounded() {
        let long = (0..100).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        assert_eq!(to_tsquery(&long, false).split(" & ").count(), 12);
    }
}
