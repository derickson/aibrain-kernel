//! Elasticsearch: the client, the mapping, and the index naming rules.
//!
//! Elasticsearch is optional. Without `ELASTICSEARCH_URL` the whole subsystem
//! is absent — the queue still fills, but nothing drains it and `/search`
//! answers from Postgres. That is deliberate: a developer who has not been
//! handed a cluster should still get a working service, and turning the
//! cluster on later needs no rebuild of the database, only a resync.

#[cfg(test)]
mod integration;
pub mod search;
pub mod worker;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

pub const DEFAULT_INFERENCE_ID: &str = ".jina-embeddings-v5-omni-small";
pub const DEFAULT_INDEX_PREFIX: &str = "aibrain-";
pub const DEFAULT_BATCH: usize = 50;
pub const DEFAULT_POLL_MS: u64 = 2000;

/// Inference happens inside the bulk call, so the write is as slow as the
/// embedding model. Thirty seconds is not enough; two minutes is.
const BULK_TIMEOUT: Duration = Duration::from_secs(120);
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct EsConfig {
    pub url: String,
    pub api_key: Option<String>,
    pub inference_id: String,
    pub index_prefix: String,
    pub batch: usize,
    pub poll_ms: u64,
}

impl EsConfig {
    /// `None` means "no cluster configured", which is a supported state, not
    /// an error.
    pub fn from_env() -> Option<EsConfig> {
        let url = non_empty("ELASTICSEARCH_URL")?;
        Some(EsConfig {
            url: url.trim_end_matches('/').to_string(),
            api_key: non_empty("ELASTICSEARCH_API_KEY"),
            inference_id: non_empty("AIBRAIN_ES_INFERENCE_ID")
                .unwrap_or_else(|| DEFAULT_INFERENCE_ID.to_string()),
            index_prefix: non_empty("AIBRAIN_ES_INDEX_PREFIX")
                .unwrap_or_else(|| DEFAULT_INDEX_PREFIX.to_string()),
            batch: non_empty("AIBRAIN_ES_BATCH")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_BATCH),
            poll_ms: non_empty("AIBRAIN_ES_POLL_MS")
                .and_then(|v| v.parse().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_POLL_MS),
        })
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

pub struct Es {
    pub cfg: EsConfig,
    http: reqwest::Client,
    /// Indices we have already created or confirmed. Saves a HEAD per bulk.
    known: Mutex<HashSet<String>>,
}

impl Es {
    pub fn new(cfg: EsConfig) -> Result<Es> {
        let http = reqwest::Client::builder()
            .timeout(BULK_TIMEOUT)
            .build()
            .context("building the Elasticsearch HTTP client")?;
        Ok(Es { cfg, http, known: Mutex::new(HashSet::new()) })
    }

    /// The index a brain's notes live in.
    pub fn index_for(&self, brain_name: &str) -> String {
        format!("{}{}", self.cfg.index_prefix, sanitize_index_name(brain_name))
    }

    /// Every index this service owns, for an unscoped search.
    pub fn wildcard(&self) -> String {
        format!("{}*", self.cfg.index_prefix)
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{}", self.cfg.url, path));
        if let Some(key) = &self.cfg.api_key {
            req = req.header("Authorization", format!("ApiKey {key}"));
        }
        req
    }

    /// Create the index if it is not there. Racing creators are fine: a
    /// `resource_already_exists_exception` means someone else won.
    pub async fn ensure_index(&self, index: &str) -> Result<()> {
        if self.known.lock().unwrap().contains(index) {
            return Ok(());
        }
        let head = self
            .request(reqwest::Method::HEAD, &format!("/{index}"))
            .timeout(QUERY_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("HEAD /{index}"))?;
        if head.status().is_success() {
            self.known.lock().unwrap().insert(index.to_string());
            return Ok(());
        }

        let body = json!({ "mappings": mapping(&self.cfg.inference_id) });
        let created = self
            .request(reqwest::Method::PUT, &format!("/{index}"))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PUT /{index}"))?;
        let status = created.status();
        let text = created.text().await.unwrap_or_default();
        if !status.is_success() && !text.contains("resource_already_exists_exception") {
            return Err(anyhow!("could not create index {index}: {status} {}", clip(&text, 400)));
        }
        tracing::info!("elasticsearch index {index} ready");
        self.known.lock().unwrap().insert(index.to_string());
        Ok(())
    }

    /// Send one NDJSON bulk body. Returns the parsed response.
    pub async fn bulk(&self, body: String) -> Result<Value> {
        let response = self
            .request(reqwest::Method::POST, "/_bulk")
            .header("Content-Type", "application/x-ndjson")
            .body(body)
            .send()
            .await
            .context("POST /_bulk")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("bulk rejected: {status} {}", clip(&text, 400)));
        }
        serde_json::from_str(&text).context("bulk response was not JSON")
    }

    pub async fn search(&self, indices: &str, body: &Value) -> Result<Value> {
        let response = self
            .request(
                reqwest::Method::POST,
                &format!("/{indices}/_search?ignore_unavailable=true"),
            )
            .timeout(QUERY_TIMEOUT)
            .json(body)
            .send()
            .await
            .context("POST _search")?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("search rejected: {status} {}", clip(&text, 400)));
        }
        serde_json::from_str(&text).context("search response was not JSON")
    }

    /// Documents in an index; a missing index counts as zero.
    pub async fn count(&self, index: &str) -> Result<i64> {
        let response = self
            .request(reqwest::Method::GET, &format!("/{index}/_count"))
            .timeout(QUERY_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("GET /{index}/_count"))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(0);
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("count failed: {status} {}", clip(&text, 200)));
        }
        let value: Value = serde_json::from_str(&text).context("count response was not JSON")?;
        Ok(value.get("count").and_then(Value::as_i64).unwrap_or(0))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn refresh(&self, indices: &str) -> Result<()> {
        self.request(
            reqwest::Method::POST,
            &format!("/{indices}/_refresh?ignore_unavailable=true"),
        )
        .timeout(QUERY_TIMEOUT)
        .send()
        .await
        .context("POST _refresh")?;
        Ok(())
    }

    /// Only used by tests, which clean up after themselves.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn delete_index(&self, index: &str) -> Result<()> {
        self.request(reqwest::Method::DELETE, &format!("/{index}"))
            .timeout(QUERY_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("DELETE /{index}"))?;
        self.known.lock().unwrap().remove(index);
        Ok(())
    }
}

/// A vault name folded into something Elasticsearch will accept as part of an
/// index name: lowercase, only `[a-z0-9._-]`, no leading `-`, `_`, `+` or `.`.
pub fn sanitize_index_name(name: &str) -> String {
    let folded: String = name
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = folded.trim_start_matches(['-', '_', '+', '.']);
    let capped: String = trimmed.chars().take(200).collect();
    if capped.is_empty() {
        // A vault called "???" still needs somewhere to live.
        "brain".to_string()
    } else {
        capped
    }
}

/// The mapping every index gets. `dynamic: false` so a stray field in a
/// future document cannot quietly add a mapping we did not intend.
pub fn mapping(inference_id: &str) -> Value {
    json!({
        "dynamic": false,
        "properties": {
            "note_id":      { "type": "long" },
            "brain_id":     { "type": "keyword" },
            "brain_name":   { "type": "keyword" },
            "rel_path":     { "type": "keyword" },
            "title":        { "type": "text",
                              "fields": { "keyword": { "type": "keyword", "ignore_above": 256 } } },
            "source":       { "type": "keyword" },
            "tags":         { "type": "keyword" },
            "headings":     { "type": "text" },
            "excerpt":      { "type": "text" },
            "body":         { "type": "text" },
            "mtime":        { "type": "double" },
            "degree":       { "type": "integer" },
            "size":         { "type": "long" },
            "content_hash": { "type": "keyword" },
            "indexed_at":   { "type": "date" },
            "semantic":     { "type": "semantic_text", "inference_id": inference_id }
        }
    })
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_names_become_legal_index_names() {
        assert_eq!(sanitize_index_name("Grognard"), "grognard");
        assert_eq!(sanitize_index_name("Obsidian Cloud Home"), "obsidian-cloud-home");
        assert_eq!(sanitize_index_name("Dave's Brain!"), "dave-s-brain-");
        assert_eq!(sanitize_index_name("agent.knowledge_v2-x"), "agent.knowledge_v2-x");
    }

    #[test]
    fn leading_characters_elasticsearch_rejects_are_stripped() {
        for name in ["-Alpha", "_Alpha", "+Alpha", ".Alpha", "-_+.Alpha"] {
            assert_eq!(sanitize_index_name(name), "alpha", "{name}");
        }
        // A name that folds to nothing but separators still needs a name.
        assert_eq!(sanitize_index_name("???"), "brain");
        assert_eq!(sanitize_index_name(""), "brain");
    }

    #[test]
    fn index_names_are_capped() {
        let long = "A".repeat(400);
        assert_eq!(sanitize_index_name(&long).len(), 200);
    }

    #[test]
    fn the_mapping_carries_the_configured_inference_endpoint() {
        let m = mapping(".my-endpoint");
        assert_eq!(m["dynamic"], false);
        assert_eq!(m["properties"]["semantic"]["type"], "semantic_text");
        assert_eq!(m["properties"]["semantic"]["inference_id"], ".my-endpoint");
        assert_eq!(m["properties"]["title"]["fields"]["keyword"]["type"], "keyword");
    }

    #[test]
    fn no_cluster_configured_is_not_an_error() {
        // The env is shared across tests, so only the shape is asserted here:
        // from_env returns an Option rather than failing.
        let _: Option<EsConfig> = EsConfig::from_env();
    }
}
