//! The HTTP surface Python talks to.
//!
//! Coarse on purpose. The previous design exposed fourteen fine-grained
//! queries that Python looped over — one search per wikilink to render a note,
//! twenty-five round trips to build the universe. In-process that was free.
//! Across a socket it would be fatal, so each route here answers a whole
//! question: give me this note *and* its neighbours *and* its rendered HTML.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sqlx::PgPool;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tokio_stream::{Stream, StreamExt};

use crate::config::Config;
use crate::db;
use crate::es;
use crate::events::{self, Change};
use crate::graph;

/// How often a stream with nothing to say reminds the other end it is there.
/// Long enough to be free, short enough that no reverse proxy's idle timeout
/// gets there first.
const PING: Duration = Duration::from_secs(15);

pub struct Ctx {
    pub pool: PgPool,
    pub config_path: std::path::PathBuf,
    /// Bumped whenever ingest changes anything, so clients can poll cheaply.
    /// Per-brain revisions live in Postgres; this stays a coarse global one.
    pub revision: std::sync::atomic::AtomicI64,
    /// `None` when no cluster is configured; search then answers from Postgres.
    pub es: Option<Arc<es::Es>>,
    /// Where ingest announces what changed, and `/events` listens.
    pub events: events::Bus,
}

impl Ctx {
    pub fn config(&self) -> anyhow::Result<Config> {
        crate::config::load(&self.config_path)
    }
}

type Shared = Arc<Ctx>;

/// Anything that goes wrong becomes JSON, never an empty 500.
///
/// The text is deliberately not the error's own. An `anyhow` chain here
/// carries absolute vault paths, the failing SQL, and whatever Elasticsearch
/// said about a request built from user input — none of which the caller
/// needs and all of which travels on to a browser. The detail goes to the log
/// the user can read; the response says only that it failed.
pub struct ApiError(anyhow::Error);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!("{:#}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error — see the aibrain-core log" })),
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

/// Hosts this service will answer to.
///
/// It binds to loopback and has no authentication, so the only way a web page
/// reaches it is DNS rebinding: a name the attacker controls resolving to
/// 127.0.0.1, which makes the browser treat their page as same-origin with
/// this one and hand them every note. The `Host` header still carries the name
/// that was typed, and no page can change it, so checking it closes the hole.
/// Non-browser clients (Python's urllib, curl) send the address they dialled,
/// which passes.
fn host_is_loopback(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    let name = if let Some(rest) = host.strip_prefix('[') {
        // `[::1]:8781`. Only a port may follow the bracket, or
        // `[::1].evil.com` would read as the loopback address.
        match rest.split_once(']') {
            Some((inner, tail)) if tail.is_empty() => inner.to_string(),
            Some((inner, tail))
                if tail.starts_with(':') && tail[1..].chars().all(|c| c.is_ascii_digit()) =>
            {
                inner.to_string()
            }
            _ => return false,
        }
    } else if host.matches(':').count() > 1 {
        // A bare IPv6 literal: without brackets no port can follow it, so
        // splitting on the colon would throw the address away.
        host.clone()
    } else {
        host.split(':').next().unwrap_or("").to_string()
    };
    matches!(name.as_str(), "127.0.0.1" | "localhost" | "::1" | "0.0.0.0")
}

async fn guard_host(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let ok = match request.headers().get(header::HOST) {
        // HTTP/1.0 clients may omit it; no browser does.
        None => true,
        Some(value) => value.to_str().map(host_is_loopback).unwrap_or(false),
    };
    if !ok {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "this service only answers on localhost" })),
        )
            .into_response();
    }
    next.run(request).await
}

pub fn router(ctx: Shared) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/universe", get(universe))
        .route("/events", get(events))
        .route("/search", get(search))
        .route("/note/:id", get(note))
        .route("/notes/by-path", get(note_by_path))
        .route("/notes/recent", get(recent))
        .route("/reindex", post(reindex))
        .route("/search/resync", post(resync_search))
        .route("/search/status", get(search_status))
        .route("/brains/:id/retire", post(retire_brain))
        .route("/render", post(render_markdown))
        // The day's list keeps its own module; merged before the state so it
        // shares this one Ctx.
        .merge(crate::todo::routes())
        .layer(axum::middleware::from_fn(guard_host))
        // A JSON body here is a to-do or a reindex flag. axum defaults to 2 MB;
        // say the smaller number rather than inherit one chosen for uploads.
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024))
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
    let search = search_report(&ctx).await?;
    Ok(Json(json!({
        "title": cfg.title,
        "notes": db::count_notes(&ctx.pool).await?,
        "links": db::count_links(&ctx.pool).await?,
        "revision": ctx.revision.load(std::sync::atomic::Ordering::Relaxed),
        "brains": brains,
        "search": search,
    })))
}

/// The whole corpus as packed geometry, or a 304 when nothing moved.
///
/// The revisions are read first and hashed into the ETag, so a browser holding
/// a current copy costs one small query instead of a full layout. What does
/// get built comes out of `graph_cache` for every brain whose revision has not
/// moved since it was last packed.
async fn universe(State(ctx): State<Shared>, headers: HeaderMap) -> ApiResult<Response> {
    let cfg = ctx.config()?;
    let revisions = graph::revisions(&ctx.pool, &cfg).await?;
    let etag = graph::etag(&revisions, &graph::shape_of(&cfg));

    let unchanged = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| graph::etag_matches(value, &etag));
    if unchanged {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let payload = graph::build(&ctx.pool, &cfg, &revisions).await?;
    Ok((
        [
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache".to_string()),
        ],
        Json(payload),
    )
        .into_response())
}

/// The change feed the browser subscribes to.
///
/// A comment goes out immediately so the headers flush and the client knows
/// the stream is open, and another every fifteen seconds after that. Nothing
/// here has to notice a disconnect: axum drops the stream, which drops the
/// receiver, which is the whole cleanup.
async fn events(
    State(ctx): State<Shared>,
) -> Sse<impl Stream<Item = std::result::Result<Event, Infallible>>> {
    let changes = BroadcastStream::new(ctx.events.subscribe()).filter_map(|received| {
        let change = match received {
            Ok(change) => change,
            // The client fell behind the channel. Saying so is better than
            // silently dropping events it would never know it missed.
            Err(BroadcastStreamRecvError::Lagged(missed)) => {
                tracing::debug!("an /events subscriber missed {missed} change(s)");
                Change::lagged()
            }
        };
        Event::default().json_data(&change).ok().map(Ok)
    });
    let opening = tokio_stream::once(Ok(Event::default().comment(" ping")));
    Sse::new(opening.chain(changes))
        .keep_alive(KeepAlive::new().interval(PING).text(" ping"))
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
    // Bounded before it reaches either engine: `to_tsquery` builds one term
    // per word and Elasticsearch parses the whole string, so an unbounded
    // query is an unbounded amount of work for one GET.
    let query: String = params.q.unwrap_or_default().chars().take(2000).collect();
    let brains: Vec<String> = params
        .brains
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .take(64)
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
/// What the search side is doing: cluster health, queue per brain, pending
/// index retirements, dead letters, and what reconcile last found.
async fn search_report(ctx: &Ctx) -> anyhow::Result<serde_json::Value> {
    let Some(es) = &ctx.es else {
        return Ok(json!({ "engine": "postgres" }));
    };
    let health = es.health.lock().unwrap().clone();
    let by_brain = db::queue_by_brain(&ctx.pool).await?;
    let indices: Vec<String> = by_brain.iter().filter_map(|b| b.index_name.clone()).collect();
    Ok(json!({
        "engine": "elasticsearch",
        "inference_id": es.cfg.inference_id,
        "health": health,
        "queue": db::queue_stats(&ctx.pool).await?,
        "queue_by_brain": by_brain,
        "dead_letter": db::dead_letter_count(&ctx.pool).await?,
        "retirements": db::pending_retirements(&ctx.pool).await?,
        "indices": indices,
    }))
}

async fn search_status(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(search_report(&ctx).await?))
}

/// Remove a brain whose vault has been unlinked.
///
/// Python calls this right after removing the symlink. Postgres forgets the
/// brain at once — notes, links, queued writes — and its index is recorded for
/// the worker to drop, now or whenever Elasticsearch is next reachable.
/// Refused while the vault is still linked: that would only be undone by the
/// next rescan, after paying to embed every note again.
async fn retire_brain(
    State(ctx): State<Shared>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let cfg = ctx.config()?;
    if let Some(brain) = cfg.brains.iter().find(|b| b.id == id) {
        if std::path::Path::new(&brain.root).is_dir() {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": "that vault is still linked; unlink it first" })),
            )
                .into_response());
        }
    }
    let retired = db::retire_brain(&ctx.pool, &id).await?;
    if retired.is_some() {
        ctx.revision.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ctx.events.publish(Change::removed(&id));
        tracing::info!("retired brain {id}");
    }
    Ok(Json(json!({
        "retired": retired.is_some(),
        "index": retired.and_then(|r| r.index_name),
        "search": if ctx.es.is_some() { "elasticsearch" } else { "postgres" },
    }))
    .into_response())
}

async fn resync_search(State(ctx): State<Shared>) -> ApiResult<Json<serde_json::Value>> {
    let enqueued = db::enqueue_all(&ctx.pool, None).await?;
    tracing::info!("resync queued {enqueued} note(s) for search indexing");
    Ok(Json(json!({
        "enqueued": enqueued,
        "engine": if ctx.es.is_some() { "elasticsearch" } else { "postgres" },
    })))
}

#[derive(Deserialize)]
struct RenderBody {
    text: String,
    /// Note titles this caller already resolved (chat citations, a wikilink
    /// an agent wrote), keyed by `vault::normalize(title)` — the same fold
    /// `to_html_with` looks targets up with. We render exactly this set
    /// rather than re-resolving from the DB, so a `[[Title]]` in a chat
    /// answer links to the same note id its citation pill points at, even
    /// when the title is ambiguous and the caller's own tie-break differs
    /// from a plain title lookup here.
    #[serde(default)]
    resolved: std::collections::HashMap<String, i64>,
}

/// Render arbitrary markdown (a chat answer, not a note on disk) through the
/// same sanitizing pipeline notes use, so an agent's response gets the same
/// safe HTML and the same clickable `[[Title]]` links as everything else.
async fn render_markdown(Json(body): Json<RenderBody>) -> ApiResult<Json<serde_json::Value>> {
    const MAX_LEN: usize = 200_000;
    // `.get()` refuses a byte offset that lands mid-character rather than
    // panicking the way slicing would; step back until it accepts one.
    let mut cut = MAX_LEN.min(body.text.len());
    while cut > 0 && body.text.get(..cut).is_none() {
        cut -= 1;
    }
    let text = body.text.get(..cut).unwrap_or(&body.text);
    let html = crate::vault::render::to_html_with(text, &body.resolved);
    Ok(Json(json!({ "html": html })))
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
    brains: Option<String>,
}

async fn recent(
    State(ctx): State<Shared>,
    Query(params): Query<RecentParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let limit = params.limit.unwrap_or(20).clamp(1, 200);
    let brains: Vec<String> = params
        .brains
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .take(64)
        .map(str::to_string)
        .collect();
    Ok(Json(json!({ "results": db::recent(&ctx.pool, &brains, limit).await? })))
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
    if stats.changed() || !stats.retired.is_empty() {
        ctx.revision.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    ctx.events.publish_all(
        stats.bumped.iter().map(|(id, rev)| Change::reindex(id, *rev))
            .chain(stats.retired.iter().map(Change::removed)),
    );
    Ok(Json(json!({
        "scanned": stats.scanned,
        "added": stats.added,
        "updated": stats.updated,
        "removed": stats.removed,
        "unchanged": stats.unchanged,
        "links": stats.links,
        "retired": stats.retired,
        // Which vaults moved, so a caller that is not watching /events still
        // learns what to invalidate.
        "bumped": stats.bumped.iter()
            .map(|(id, rev)| json!({ "brain_id": id, "revision": rev }))
            .collect::<Vec<_>>(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_addresses_this_service_was_reached_at_are_accepted() {
        for host in [
            "127.0.0.1:8781",
            "127.0.0.1",
            "localhost:8781",
            "LOCALHOST:8781",
            "[::1]:8781",
            "::1",
            "0.0.0.0:8781",
        ] {
            assert!(host_is_loopback(host), "{host} should be allowed");
        }
    }

    #[test]
    fn a_rebound_name_is_refused_however_it_is_spelled() {
        for host in [
            "evil.com",
            "evil.com:8781",
            "127.0.0.1.evil.com:8781",
            "localhost.evil.com",
            "[::1].evil.com",
            "",
            "notlocalhost",
        ] {
            assert!(!host_is_loopback(host), "{host} should be refused");
        }
    }

    #[test]
    fn an_internal_error_never_carries_the_detail_to_the_caller() {
        let leaky = anyhow::anyhow!("cannot read /Users/dave/Vaults/Private/a.md");
        let response = ApiError(leaky).into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
