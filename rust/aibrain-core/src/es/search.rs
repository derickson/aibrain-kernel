//! `/search`, answered by Elasticsearch.
//!
//! Two retrievers, normalised and summed: a lexical leg that finds the words
//! you typed and a semantic leg that finds the note you meant. The semantic
//! leg carries the larger weight because the lexical one is already what
//! Postgres does well; the value of the cluster is the half that is not
//! keyword matching.
//!
//! Elasticsearch decides the order. Postgres decides the facts: every hit is
//! resolved back to its current row before it is returned, so a note deleted
//! ten seconds ago cannot appear in a result list with a stale id.

use anyhow::Result;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::Es;
use crate::db::{self, NoteSummary};

/// Weights on the two legs. Semantic wins ties on purpose.
const LEXICAL_WEIGHT: f64 = 1.0;
const SEMANTIC_WEIGHT: f64 = 1.5;

/// The snippet markers Postgres already emits, so the UI needs no second case.
const PRE_TAG: &str = "<mark>";
const POST_TAG: &str = "</mark>";

/// The body of the `_search` request.
///
/// Pure and deterministic so a test can assert on it without a cluster.
pub fn build_request(query: &str, limit: i64) -> Value {
    let window = std::cmp::max(limit * 2, 50);
    json!({
        "size": limit,
        "_source": ["note_id", "brain_id", "rel_path", "title", "source",
                    "degree", "mtime", "excerpt"],
        "retriever": {
            "linear": {
                "rank_window_size": window,
                "retrievers": [
                    {
                        "retriever": { "standard": { "query": { "multi_match": {
                            "query": query,
                            // Fuzziness is deliberately absent: on a vault full
                            // of proper nouns it promotes near-misses over the
                            // note you named.
                            "fields": ["title^4", "headings^2", "tags^2",
                                       "source", "excerpt", "body"],
                            "type": "best_fields",
                            "operator": "or",
                            "minimum_should_match": "1"
                        }}}},
                        "weight": LEXICAL_WEIGHT,
                        "normalizer": "minmax"
                    },
                    {
                        "retriever": { "standard": { "query": { "semantic": {
                            "field": "semantic",
                            "query": query
                        }}}},
                        "weight": SEMANTIC_WEIGHT,
                        "normalizer": "minmax"
                    }
                ]
            }
        },
        "highlight": {
            "pre_tags": [PRE_TAG],
            "post_tags": [POST_TAG],
            "fields": {
                "body": { "fragment_size": 160, "number_of_fragments": 1 },
                "semantic": { "number_of_fragments": 1 }
            }
        }
    })
}

/// Which indices a `brains=` parameter points at.
///
/// The parameter carries brain **ids**, as it always has; the index name comes
/// from the brain's display name, so the two have to be mapped through the
/// brain table. An id nobody knows contributes nothing rather than widening
/// the search to everything.
pub fn target_indices(es: &Es, brains: &[String], known: &[(String, String)]) -> String {
    if brains.is_empty() {
        return es.wildcard();
    }
    let mut names: Vec<String> = Vec::new();
    for id in brains {
        if let Some((_, name)) = known.iter().find(|(known_id, _)| known_id == id) {
            let index = es.index_for(name);
            if !names.contains(&index) {
                names.push(index);
            }
        }
    }
    if names.is_empty() {
        // Ask for an index that cannot exist rather than silently searching
        // every brain when the caller asked for one.
        return format!("{}none-", es.cfg.index_prefix);
    }
    names.join(",")
}

/// Run the query and hand back summaries in Elasticsearch's order.
pub async fn run(
    pool: &PgPool,
    es: &Es,
    query: &str,
    brains: &[String],
    limit: i64,
) -> Result<Vec<NoteSummary>> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let known: Vec<(String, String)> = db::list_brains(pool)
        .await?
        .into_iter()
        .map(|b| (b.id, b.name))
        .collect();
    let indices = target_indices(es, brains, &known);
    let response = es.search(&indices, &build_request(query, limit)).await?;

    let hits = response
        .pointer("/hits/hits")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut ordered: Vec<(String, String, String, f32)> = Vec::new();
    for hit in &hits {
        let source = hit.get("_source").cloned().unwrap_or(Value::Null);
        let Some(brain_id) = source.get("brain_id").and_then(Value::as_str) else {
            continue;
        };
        // The _id is the rel_path, but read the field when it is there: it is
        // the one the mapping guarantees.
        let rel_path = source
            .get("rel_path")
            .and_then(Value::as_str)
            .or_else(|| hit.get("_id").and_then(Value::as_str))
            .unwrap_or_default();
        let score = hit.get("_score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        ordered.push((
            brain_id.to_string(),
            rel_path.to_string(),
            snippet_for(hit, &source),
            score,
        ));
    }

    let brain_ids: Vec<String> = ordered.iter().map(|(b, _, _, _)| b.clone()).collect();
    let rel_paths: Vec<String> = ordered.iter().map(|(_, p, _, _)| p.clone()).collect();
    let current = db::summaries_by_path(pool, &brain_ids, &rel_paths).await?;

    Ok(ordered
        .into_iter()
        .filter_map(|(brain_id, rel_path, snippet, score)| {
            let mut summary = current.get(&(brain_id, rel_path))?.clone();
            summary.snippet = snippet;
            summary.score = Some(score);
            Some(summary)
        })
        .collect())
}

/// Body highlight, else the semantic chunk, else the stored excerpt.
fn snippet_for(hit: &Value, source: &Value) -> String {
    let highlight = |field: &str| {
        hit.pointer(&format!("/highlight/{field}/0"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let text = highlight("body")
        .or_else(|| highlight("semantic"))
        .or_else(|| {
            source
                .get("excerpt")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    // A semantic highlight is a whole chunk, which can be the whole note.
    // Postgres returns about twenty-six words; match that order of size.
    super::clip(text.trim(), 240)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::es::{EsConfig, Es};

    fn es() -> Es {
        Es::new(EsConfig {
            url: "http://example.invalid".into(),
            api_key: None,
            inference_id: ".jina-embeddings-v5-omni-small".into(),
            index_prefix: "aibrain-".into(),
            batch: 50,
            poll_ms: 2000,
        })
        .unwrap()
    }

    #[test]
    fn the_request_has_both_legs_with_their_weights() {
        let body = build_request("tide tables", 10);
        let legs = body["retriever"]["linear"]["retrievers"]
            .as_array()
            .expect("two retrievers");
        assert_eq!(legs.len(), 2);

        assert_eq!(legs[0]["weight"], 1.0);
        assert_eq!(legs[0]["normalizer"], "minmax");
        let lexical = &legs[0]["retriever"]["standard"]["query"]["multi_match"];
        assert_eq!(lexical["query"], "tide tables");
        assert_eq!(lexical["type"], "best_fields");
        assert_eq!(lexical["operator"], "or");
        assert_eq!(lexical["minimum_should_match"], "1");
        assert_eq!(
            lexical["fields"],
            json!(["title^4", "headings^2", "tags^2", "source", "excerpt", "body"])
        );
        // The semantic field must not appear in the lexical leg; a multi_match
        // over a semantic_text field is an error, not a bonus.
        assert!(!lexical["fields"].to_string().contains("semantic"));

        assert_eq!(legs[1]["weight"], 1.5);
        assert_eq!(legs[1]["normalizer"], "minmax");
        let semantic = &legs[1]["retriever"]["standard"]["query"]["semantic"];
        assert_eq!(semantic["field"], "semantic");
        assert_eq!(semantic["query"], "tide tables");
    }

    #[test]
    fn the_rank_window_never_falls_below_fifty() {
        assert_eq!(build_request("q", 1)["retriever"]["linear"]["rank_window_size"], 50);
        assert_eq!(build_request("q", 25)["retriever"]["linear"]["rank_window_size"], 50);
        assert_eq!(build_request("q", 60)["retriever"]["linear"]["rank_window_size"], 120);
    }

    #[test]
    fn highlights_use_the_markers_postgres_already_emits() {
        let h = &build_request("q", 10)["highlight"];
        assert_eq!(h["pre_tags"], json!(["<mark>"]));
        assert_eq!(h["post_tags"], json!(["</mark>"]));
        assert_eq!(h["fields"]["body"]["fragment_size"], 160);
        assert_eq!(h["fields"]["body"]["number_of_fragments"], 1);
        assert_eq!(h["fields"]["semantic"]["number_of_fragments"], 1);
    }

    #[test]
    fn brain_ids_become_index_names() {
        let es = es();
        let known = vec![
            ("v1".to_string(), "Grognard".to_string()),
            ("v2".to_string(), "Obsidian Cloud Home".to_string()),
        ];
        assert_eq!(target_indices(&es, &[], &known), "aibrain-*");
        assert_eq!(
            target_indices(&es, &["v1".into()], &known),
            "aibrain-grognard"
        );
        assert_eq!(
            target_indices(&es, &["v1".into(), "v2".into()], &known),
            "aibrain-grognard,aibrain-obsidian-cloud-home"
        );
        // An unknown id narrows to nothing rather than widening to everything.
        assert_eq!(target_indices(&es, &["nope".into()], &known), "aibrain-none-");
    }

    #[test]
    fn the_snippet_prefers_the_body_highlight() {
        let source = json!({ "excerpt": "stored excerpt" });
        let both = json!({ "highlight": { "body": ["a <mark>hit</mark>"], "semantic": ["chunk"] } });
        assert_eq!(snippet_for(&both, &source), "a <mark>hit</mark>");

        let semantic_only = json!({ "highlight": { "semantic": ["chunk"] } });
        assert_eq!(snippet_for(&semantic_only, &source), "chunk");

        assert_eq!(snippet_for(&json!({}), &source), "stored excerpt");
        assert_eq!(snippet_for(&json!({}), &json!({})), "");
    }

    #[test]
    fn a_whole_note_chunk_is_not_returned_as_a_snippet() {
        let long = "word ".repeat(200);
        let hit = json!({ "highlight": { "semantic": [long] } });
        let snippet = snippet_for(&hit, &json!({}));
        assert!(snippet.chars().count() <= 241, "{}", snippet.chars().count());
        assert!(snippet.ends_with('…'));
    }
}
