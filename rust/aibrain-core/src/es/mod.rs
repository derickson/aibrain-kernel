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
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DEFAULT_INFERENCE_ID: &str = ".jina-embeddings-v5-omni-small";
pub const DEFAULT_INDEX_PREFIX: &str = "aibrain-";
pub const DEFAULT_BATCH: usize = 50;
pub const DEFAULT_POLL_MS: u64 = 2000;

/// Bumped when `mapping()` changes shape, and stamped into every index's
/// `_meta`, so an index built by an older version can be recognised.
pub const SCHEMA_VERSION: i64 = 1;

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

/// The cluster could not be asked: connection refused, a timeout, a 5xx, or a
/// 429. Says nothing about the request itself, so the worker waits for the
/// cluster rather than charging the failure to whatever it was sending.
#[derive(Debug)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "elasticsearch unavailable: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

/// Whether an error anywhere in the chain is the cluster being unreachable.
pub fn is_unavailable(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<Unavailable>())
}

/// What the worker knows about the cluster, for /status.
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub available: bool,
    /// When `available` last changed.
    pub since: chrono::DateTime<chrono::Utc>,
    pub last_probe: Option<chrono::DateTime<chrono::Utc>>,
    pub last_error: Option<String>,
    pub last_reconcile: Option<chrono::DateTime<chrono::Utc>>,
    /// Indices matching our prefix that carry no `_meta` of ours. Logged and
    /// listed here; never deleted, because we cannot prove we own them.
    pub foreign_indices: Vec<String>,
    /// Indices reconcile found with our `_meta` and no brain, and retired.
    pub orphans_retired: Vec<String>,
}

impl Default for Health {
    fn default() -> Self {
        Health {
            available: true,
            since: chrono::Utc::now(),
            last_probe: None,
            last_error: None,
            last_reconcile: None,
            foreign_indices: Vec::new(),
            orphans_retired: Vec::new(),
        }
    }
}

/// An index this service found under its prefix, with the ownership stamp if
/// it has one.
#[derive(Debug, Clone)]
pub struct Owned {
    pub index: String,
    /// `_meta.aibrain`, when present and written by this prefix.
    pub meta: Option<IndexMeta>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexMeta {
    pub brain_id: String,
    pub uid: String,
    pub schema: i64,
}

pub struct Es {
    pub cfg: EsConfig,
    http: reqwest::Client,
    pub health: Mutex<Health>,
    /// Brains (by uid) whose provisioning the cluster refused, with how many
    /// times and when to try again — so a bad one is not retried every poll.
    pub(crate) provision_retry: Mutex<HashMap<String, (u32, Instant)>>,
}

impl Es {
    pub fn new(cfg: EsConfig) -> Result<Es> {
        let http = reqwest::Client::builder()
            .timeout(BULK_TIMEOUT)
            .build()
            .context("building the Elasticsearch HTTP client")?;
        Ok(Es {
            cfg,
            http,
            health: Mutex::new(Health::default()),
            provision_retry: Mutex::new(HashMap::new()),
        })
    }

    // ── Names ──────────────────────────────────────────────────────────────
    //
    // A new brain's index is `<prefix><uid>`. Aliases carry a `_` after the
    // prefix, which `sanitize_index_name` strips from the front of any vault
    // name, so no index this code has ever created can collide with one.

    /// The index a brain gets: named by its uid, which is never reused.
    pub fn index_for_uid(&self, uid: &str) -> String {
        format!("{}{}", self.cfg.index_prefix, uid)
    }

    /// Where a brain's writes go. Bulk requests set `require_alias`, so once
    /// this alias is gone a late write fails instead of recreating the index.
    pub fn write_alias(&self, uid: &str) -> String {
        format!("{}_w-{}", self.cfg.index_prefix, uid)
    }

    /// Every live index, for an unscoped search. An index joins it when it is
    /// provisioned and leaves it before it is dropped.
    pub fn search_alias(&self) -> String {
        format!("{}_search", self.cfg.index_prefix)
    }

    /// The ownership stamp written into an index's mapping `_meta`.
    pub fn meta_for(&self, brain_id: &str, uid: &str) -> Value {
        json!({
            "aibrain": {
                "brain_id": brain_id,
                "uid": uid,
                "prefix": self.cfg.index_prefix,
                "schema": SCHEMA_VERSION,
                "created_at": chrono::Utc::now().to_rfc3339(),
            }
        })
    }

    /// Read `_meta.aibrain` back, but only when this prefix wrote it. The live
    /// prefix `aibrain-` also matches the test suite's `aibrain-test-<pid>-`
    /// indices; checking the stamped prefix keeps one from claiming the other.
    pub fn parse_meta(&self, meta: &Value) -> Option<IndexMeta> {
        let ours = meta.get("aibrain")?;
        if ours.get("prefix").and_then(Value::as_str) != Some(self.cfg.index_prefix.as_str()) {
            return None;
        }
        Some(IndexMeta {
            brain_id: ours.get("brain_id")?.as_str()?.to_string(),
            uid: ours.get("uid")?.as_str()?.to_string(),
            schema: ours.get("schema").and_then(Value::as_i64).unwrap_or(0),
        })
    }

    // ── Transport ──────────────────────────────────────────────────────────

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{}", self.cfg.url, path));
        if let Some(key) = &self.cfg.api_key {
            req = req.header("Authorization", format!("ApiKey {key}"));
        }
        req
    }

    /// Send, and sort failures into "the cluster is unavailable" (an
    /// `Unavailable` in the chain) and everything else. The caller sees the
    /// status and body of any answer that was not a 5xx or 429.
    async fn call(&self, req: reqwest::RequestBuilder, what: &str) -> Result<(StatusCode, String)> {
        let response = req
            .send()
            .await
            .map_err(|e| anyhow::Error::new(Unavailable(format!("{what}: {e}"))))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| anyhow::Error::new(Unavailable(format!("{what}: {e}"))))?;
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            return Err(anyhow::Error::new(Unavailable(format!(
                "{what}: {status} {}",
                clip(&text, 400)
            ))));
        }
        Ok((status, text))
    }

    fn json(text: &str, what: &str) -> Result<Value> {
        serde_json::from_str(text).with_context(|| format!("{what} did not return JSON"))
    }

    /// Is the cluster there and does it accept our key? Any failure counts as
    /// unavailable: a rejected key stops every write just as surely as a
    /// refused connection.
    pub async fn ping(&self) -> Result<()> {
        let (status, text) =
            self.call(self.request(reqwest::Method::GET, "/").timeout(QUERY_TIMEOUT), "GET /").await?;
        if !status.is_success() {
            return Err(anyhow::Error::new(Unavailable(format!(
                "GET /: {status} {}",
                clip(&text, 200)
            ))));
        }
        Ok(())
    }

    // ── Index lifecycle ────────────────────────────────────────────────────

    pub async fn exists(&self, index: &str) -> Result<bool> {
        let (status, _) = self
            .call(
                self.request(reqwest::Method::HEAD, &format!("/{index}")).timeout(QUERY_TIMEOUT),
                "HEAD index",
            )
            .await?;
        match status {
            s if s.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => Err(anyhow!("HEAD /{index}: {s}")),
        }
    }

    /// Create the index with the mapping and ownership stamp. `false` when it
    /// already existed. This is the only place an index is created: writes go
    /// through an alias with `require_alias`, so nothing else can.
    pub async fn create_index(&self, index: &str, meta: &Value) -> Result<bool> {
        let body = json!({ "mappings": mapping(&self.cfg.inference_id, meta) });
        let (status, text) = self
            .call(
                self.request(reqwest::Method::PUT, &format!("/{index}")).json(&body),
                "PUT index",
            )
            .await?;
        if status.is_success() {
            tracing::info!("elasticsearch index {index} created");
            return Ok(true);
        }
        if text.contains("resource_already_exists_exception") {
            return Ok(false);
        }
        Err(anyhow!("could not create index {index}: {status} {}", clip(&text, 400)))
    }

    /// Stamp `_meta` onto an index that already exists (an adopted one).
    pub async fn put_meta(&self, index: &str, meta: &Value) -> Result<()> {
        let (status, text) = self
            .call(
                self.request(reqwest::Method::PUT, &format!("/{index}/_mapping"))
                    .timeout(QUERY_TIMEOUT)
                    .json(&json!({ "_meta": meta })),
                "PUT _mapping",
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("could not stamp {index}: {status} {}", clip(&text, 400)));
        }
        Ok(())
    }

    /// Every concrete index under our prefix, with its stamp if it has one.
    pub async fn list_owned(&self) -> Result<Vec<Owned>> {
        let path = format!(
            "/{}*/_mapping?allow_no_indices=true&ignore_unavailable=true",
            self.cfg.index_prefix
        );
        let (status, text) = self
            .call(self.request(reqwest::Method::GET, &path).timeout(QUERY_TIMEOUT), "GET _mapping")
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(anyhow!("listing indices failed: {status} {}", clip(&text, 400)));
        }
        let value = Self::json(&text, "GET _mapping")?;
        let mut out: Vec<Owned> = value
            .as_object()
            .map(|m| {
                m.iter()
                    .map(|(index, body)| Owned {
                        index: index.clone(),
                        meta: body
                            .pointer("/mappings/_meta")
                            .and_then(|meta| self.parse_meta(meta)),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.index.cmp(&b.index));
        Ok(out)
    }

    /// The aliases on an index, or `None` when the index does not exist.
    async fn aliases_of(&self, index: &str) -> Result<Option<Vec<String>>> {
        let (status, text) = self
            .call(
                self.request(reqwest::Method::GET, &format!("/{index}/_alias"))
                    .timeout(QUERY_TIMEOUT),
                "GET _alias",
            )
            .await?;
        if status == StatusCode::NOT_FOUND && text.contains("index_not_found_exception") {
            return Ok(None);
        }
        // A 404 without that type means "index exists, no aliases".
        if status == StatusCode::NOT_FOUND {
            return Ok(Some(Vec::new()));
        }
        if !status.is_success() {
            return Err(anyhow!("GET /{index}/_alias: {status} {}", clip(&text, 400)));
        }
        let value = Self::json(&text, "GET _alias")?;
        Ok(Some(
            value
                .pointer(&format!("/{}/aliases", index.replace('~', "~0").replace('/', "~1")))
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        ))
    }

    async fn update_aliases(&self, actions: Vec<Value>) -> Result<()> {
        if actions.is_empty() {
            return Ok(());
        }
        let (status, text) = self
            .call(
                self.request(reqwest::Method::POST, "/_aliases")
                    .timeout(QUERY_TIMEOUT)
                    .json(&json!({ "actions": actions })),
                "POST _aliases",
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("alias update failed: {status} {}", clip(&text, 400)));
        }
        Ok(())
    }

    /// Point the brain's write alias and the shared search alias at `index`,
    /// in one atomic call. Any other generation's write alias on the index —
    /// left by a database rebuilt around it — is removed in the same call.
    pub async fn attach(&self, index: &str, uid: &str) -> Result<()> {
        let current = self
            .aliases_of(index)
            .await?
            .ok_or_else(|| anyhow!("cannot alias {index}: it does not exist"))?;
        let write = self.write_alias(uid);
        let stale_prefix = format!("{}_w-", self.cfg.index_prefix);
        let mut actions: Vec<Value> = current
            .iter()
            .filter(|a| a.starts_with(&stale_prefix) && **a != write)
            .map(|a| json!({ "remove": { "index": index, "alias": a } }))
            .collect();
        actions.push(json!({ "add": { "index": index, "alias": write, "is_write_index": true } }));
        actions.push(json!({ "add": { "index": index, "alias": self.search_alias() } }));
        self.update_aliases(actions).await
    }

    /// Take an index out of service: remove its write alias and its place in
    /// the search alias. From here a late write fails `require_alias` and a
    /// search no longer sees it. `false` when the index is already gone.
    pub async fn detach(&self, index: &str) -> Result<bool> {
        let Some(current) = self.aliases_of(index).await? else {
            return Ok(false);
        };
        let write_prefix = format!("{}_w-", self.cfg.index_prefix);
        let search = self.search_alias();
        let actions: Vec<Value> = current
            .iter()
            .filter(|a| a.starts_with(&write_prefix) || **a == search)
            .map(|a| json!({ "remove": { "index": index, "alias": a } }))
            .collect();
        self.update_aliases(actions).await?;
        Ok(true)
    }

    /// Delete an index. One that is already gone is the state we wanted.
    pub async fn drop_index(&self, index: &str) -> Result<()> {
        let (status, text) = self
            .call(
                self.request(reqwest::Method::DELETE, &format!("/{index}")).timeout(QUERY_TIMEOUT),
                "DELETE index",
            )
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        Err(anyhow!("could not delete {index}: {status} {}", clip(&text, 400)))
    }

    /// Remove documents from other brains out of an adopted index. The old
    /// naming let two vaults whose names folded alike share one index.
    pub async fn delete_foreign_docs(&self, index: &str, brain_id: &str) -> Result<i64> {
        let body = json!({
            "query": { "bool": { "must_not": { "term": { "brain_id": brain_id } } } }
        });
        let (status, text) = self
            .call(
                self.request(
                    reqwest::Method::POST,
                    &format!("/{index}/_delete_by_query?conflicts=proceed&refresh=true"),
                )
                .json(&body),
                "POST _delete_by_query",
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("delete_by_query on {index}: {status} {}", clip(&text, 400)));
        }
        let value = Self::json(&text, "_delete_by_query")?;
        Ok(value.get("deleted").and_then(Value::as_i64).unwrap_or(0))
    }

    // ── Documents ──────────────────────────────────────────────────────────

    /// Send one NDJSON bulk body. Returns the parsed response.
    ///
    /// `require_alias=true`: every action must name an alias. When a brain's
    /// write alias has been removed, its writes are refused rather than
    /// auto-creating an index with a dynamic mapping and no embeddings.
    pub async fn bulk(&self, body: String) -> Result<Value> {
        let (status, text) = self
            .call(
                self.request(reqwest::Method::POST, "/_bulk?require_alias=true")
                    .header("Content-Type", "application/x-ndjson")
                    .body(body),
                "POST /_bulk",
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("bulk rejected: {status} {}", clip(&text, 400)));
        }
        Self::json(&text, "bulk")
    }

    pub async fn search(&self, indices: &str, body: &Value) -> Result<Value> {
        let (status, text) = self
            .call(
                self.request(
                    reqwest::Method::POST,
                    &format!("/{indices}/_search?ignore_unavailable=true&allow_no_indices=true"),
                )
                .timeout(QUERY_TIMEOUT)
                .json(body),
                "POST _search",
            )
            .await?;
        if !status.is_success() {
            return Err(anyhow!("search rejected: {status} {}", clip(&text, 400)));
        }
        Self::json(&text, "search")
    }

    /// Documents in an index; a missing index counts as zero.
    pub async fn count(&self, index: &str) -> Result<i64> {
        let (status, text) = self
            .call(
                self.request(reqwest::Method::GET, &format!("/{index}/_count"))
                    .timeout(QUERY_TIMEOUT),
                "GET _count",
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(0);
        }
        if !status.is_success() {
            return Err(anyhow!("count failed: {status} {}", clip(&text, 200)));
        }
        let value = Self::json(&text, "count")?;
        Ok(value.get("count").and_then(Value::as_i64).unwrap_or(0))
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn refresh(&self, indices: &str) -> Result<()> {
        self.call(
            self.request(
                reqwest::Method::POST,
                &format!("/{indices}/_refresh?ignore_unavailable=true"),
            )
            .timeout(QUERY_TIMEOUT),
            "POST _refresh",
        )
        .await?;
        Ok(())
    }
}

/// The name the old code derived from a vault's display name. Only used to
/// adopt indices built before uids existed.
pub fn legacy_index_name(prefix: &str, brain_name: &str) -> String {
    format!("{prefix}{}", sanitize_index_name(brain_name))
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
pub fn mapping(inference_id: &str, meta: &Value) -> Value {
    json!({
        "dynamic": false,
        "_meta": meta,
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
        let m = mapping(".my-endpoint", &json!({ "aibrain": { "uid": "u1" } }));
        assert_eq!(m["_meta"]["aibrain"]["uid"], "u1");
        assert_eq!(m["dynamic"], false);
        assert_eq!(m["properties"]["semantic"]["type"], "semantic_text");
        assert_eq!(m["properties"]["semantic"]["inference_id"], ".my-endpoint");
        assert_eq!(m["properties"]["title"]["fields"]["keyword"]["type"], "keyword");
    }

    fn es(prefix: &str) -> Es {
        Es::new(EsConfig {
            url: "http://127.0.0.1:9".into(),
            api_key: None,
            inference_id: DEFAULT_INFERENCE_ID.into(),
            index_prefix: prefix.into(),
            batch: 50,
            poll_ms: 2000,
        })
        .unwrap()
    }

    #[test]
    fn new_indices_are_named_by_uid_and_aliases_cannot_collide_with_them() {
        let es = es("aibrain-");
        assert_eq!(es.index_for_uid("3f9c"), "aibrain-3f9c");
        assert_eq!(es.write_alias("3f9c"), "aibrain-_w-3f9c");
        assert_eq!(es.search_alias(), "aibrain-_search");
        // A vault can never be named into an alias: the leading `_` is stripped.
        assert_eq!(legacy_index_name("aibrain-", "_search"), "aibrain-search");
        assert_eq!(legacy_index_name("aibrain-", "Obsidian Cloud Home"), "aibrain-obsidian-cloud-home");
    }

    #[test]
    fn the_ownership_stamp_is_only_ours_under_our_own_prefix() {
        let live = es("aibrain-");
        let test = es("aibrain-test-42-");
        let stamp = test.meta_for("estest-42", "u1");
        assert_eq!(stamp["aibrain"]["schema"], SCHEMA_VERSION);
        // The live prefix also matches the test suite's indices by wildcard;
        // the stamp keeps the live service from treating them as its own.
        assert_eq!(live.parse_meta(&stamp), None);
        assert_eq!(
            test.parse_meta(&stamp),
            Some(IndexMeta { brain_id: "estest-42".into(), uid: "u1".into(), schema: SCHEMA_VERSION })
        );
        assert_eq!(live.parse_meta(&json!({ "other": {} })), None);
    }

    #[test]
    fn unavailability_is_found_anywhere_in_the_chain() {
        let err = anyhow::Error::new(Unavailable("connection refused".into()));
        assert!(is_unavailable(&err));
        assert!(is_unavailable(&err.context("draining the queue")));
        assert!(!is_unavailable(&anyhow!("mapper_parsing_exception")));
    }

    #[tokio::test]
    async fn a_closed_port_is_unavailable_not_a_document_error() {
        // Port 9 (discard) is closed on any sane machine.
        let es = es("aibrain-test-closed-");
        let err = es.bulk("{}\n".into()).await.unwrap_err();
        assert!(is_unavailable(&err), "{err:#}");
        assert!(is_unavailable(&es.ping().await.unwrap_err()));
    }

    #[test]
    fn no_cluster_configured_is_not_an_error() {
        // The env is shared across tests, so only the shape is asserted here:
        // from_env returns an Option rather than failing.
        let _: Option<EsConfig> = EsConfig::from_env();
    }
}
