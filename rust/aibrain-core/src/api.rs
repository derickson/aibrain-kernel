//! The HTTP surface Python talks to.
//!
//! Coarse on purpose. The previous design exposed fourteen fine-grained
//! queries that Python looped over — one search per wikilink to render a note,
//! twenty-five round trips to build the universe. In-process that was free.
//! Across a socket it would be fatal, so each route here answers a whole
//! question: give me this note *and* its neighbours *and* its rendered HTML.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::sync::Arc;

use crate::config::Config;
use crate::db;
use crate::es;
use crate::graph;

pub struct Ctx {
    pub pool: PgPool,
    pub config_path: std::path::PathBuf,
    /// Bumped whenever ingest changes anything, so clients can poll cheaply.
    pub revision: std::sync::atomic::AtomicI64,
    /// `None` when no cluster is configured; search then answers from Postgres.
    pub es: Option<Arc<es::Es>>,
}

impl Ctx {
    pub fn config(&self) -> anyhow::Result<Config> {
        crate::config::load(&self.config_path)
    }
}

type Shared = Arc<Ctx>;

/// Anything that goes wrong becomes JSON, never an empty 500.
pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!("{:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{:#}", self.0) })),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(err: E) -> Self {
        ApiError(err.into())
    }
}

type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn router(ctx: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/universe", get(universe))
        .route("/search", get(search))
        .route("/note/:id", get(note))
        .route("/notes/by-path", get(note_by_path))
        .route("/notes/recent", get(recent))
        .route("/reindex", post(reindex))
        .route("/search/resync", post(resync_search))
        .with_state(ctx)
}

async fn health(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    // Touch the database rather than just reporting that the process is alive;
    // "up but cannot reach Postgres" is the failure worth catching.
    let notes = db::count_notes(&ctx.pool).await?;
    Ok(Json(json!({ "ok": true, "notes": notes })))
}

async fn status(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let cfg = ctx.config()?;
    let brains = db::list_brains(&ctx.pool).await?;
    let search = match &ctx.es {
        None => json!({ "engine": "postgres" }),
        Some(es) => {
            let indices: Vec<String> =
                brains.iter().map(|b| es.index_for(&b.name)).collect();
            json!({
                "engine": "elasticsearch",
                "inference_id": es.cfg.inference_id,
                "queue": db::queue_stats(&ctx.pool).await?,
                "indices": indices,
            })
        }
    };
    Ok(Json(json!({
        "title": cfg.title,
        "notes": db::count_notes(&ctx.pool).await?,
        "links": db::count_links(&ctx.pool).await?,
        "revision": ctx.revision.load(std::sync::atomic::Ordering::Relaxed),
        "brains": brains,
        "search": search,
    })))
}

async fn universe(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let cfg = ctx.config()?;
    Ok(Json(graph::build(&ctx.pool, &cfg).await?))
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    #[serde(default)]
    brains: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    /// `any=1` ORs the terms. A question needs this; a search box does not.
    #[serde(default)]
    any: Option<String>,
}

async fn search(
    State(ctx): State<Shared>,
    Query(params): Query<SearchParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let query = params.q.unwrap_or_default();
    let brains: Vec<String> = params
        .brains
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let limit = params.limit.unwrap_or(60).clamp(1, 500);
    let any = matches!(params.any.as_deref(), Some("1") | Some("true"));

    // Elasticsearch when it is there, Postgres when it is not — and Postgres
    // again when Elasticsearch answers with an error, because a search box
    // that goes blank because a cluster hiccuped is worse than a lexical one.
    let mut engine = "postgres";
    let mut results = None;
    if let Some(es) = &ctx.es {
        match es::search::run(&ctx.pool, es, &query, &brains, limit).await {
            // The semantic leg matches every indexed note, so an empty answer
            // means the notes are not in the index yet — a vault scanned
            // moments ago whose queue is still draining. Postgres has them.
            Ok(hits) if hits.is_empty() && !query.trim().is_empty() => {
                tracing::debug!("elasticsearch returned nothing, using postgres");
            }
            Ok(hits) => {
                engine = "elasticsearch";
                results = Some(hits);
            }
            Err(err) => tracing::warn!("elasticsearch search failed, using postgres: {err:#}"),
        }
    }

    let results = match results {
        Some(hits) => hits,
        None => {
            let mut hits = db::search(&ctx.pool, &query, &brains, limit, any).await?;
            // A question whose every term must match usually matches nothing.
            // Widen once rather than returning an empty answer that looks like
            // "no such note".
            if hits.is_empty() && !any && query.split_whitespace().count() > 1 {
                hits = db::search(&ctx.pool, &query, &brains, limit, true).await?;
            }
            hits
        }
    };

    Ok(Json(json!({
        "query": query,
        "count": results.len(),
        "engine": engine,
        "results": results,
    })))
}

/// Queue every note for reindexing.
///
/// The backfill path: a database whose notes were ingested before any of this
/// existed has no queue rows, and nothing else would ever create them.
async fn resync_search(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let enqueued = db::enqueue_all(&ctx.pool, None).await?;
    tracing::info!("resync queued {enqueued} note(s) for search indexing");
    Ok(Json(json!({
        "enqueued": enqueued,
        "engine": if ctx.es.is_some() { "elasticsearch" } else { "postgres" },
    })))
}

async fn note(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    match db::note_page(&ctx.pool, id).await? {
        Some(page) => Ok(Json(page).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such note" })),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct ByPathParams {
    brain: String,
    path: String,
}

/// A note looked up by where it lives. The ACP agent only learns which files
/// a subprocess opened, so a path is all it has to turn a read into a citation.
async fn note_by_path(
    State(ctx): State<Shared>,
    Query(params): Query<ByPathParams>,
) -> ApiResult<Response> {
    match db::note_by_path(&ctx.pool, &params.brain, &params.path).await? {
        Some(summary) => Ok(Json(summary).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such note" })),
        )
            .into_response()),
    }
}

#[derive(Deserialize)]
struct RecentParams {
    limit: Option<i64>,
}

async fn recent(
    State(ctx): State<Shared>,
    Query(params): Query<RecentParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = params.limit.unwrap_or(20).clamp(1, 200);
    Ok(Json(json!({ "results": db::recent(&ctx.pool, limit).await? })))
}

#[derive(Deserialize)]
struct ReindexBody {
    #[serde(default)]
    force: bool,
}

async fn reindex(
    State(ctx): State<Shared>,
    Json(body): Json<ReindexBody>,
) -> ApiResult<Json<serde_json::Value>> {
    let cfg = ctx.config()?;
    let stats =
        crate::ingest::reindex(&ctx.pool, &cfg.brains, body.force, |line| tracing::info!("{line}"))
            .await?;
    if stats.changed() {
        ctx.revision.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(Json(json!({
        "scanned": stats.scanned,
        "added": stats.added,
        "updated": stats.updated,
        "removed": stats.removed,
        "unchanged": stats.unchanged,
        "links": stats.links,
    })))
}
