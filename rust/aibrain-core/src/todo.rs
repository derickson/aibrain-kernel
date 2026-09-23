//! The to-do list: the HTTP surface, and the one place that decides which day
//! an instant belongs to.
//!
//! The list is a single list, not a calendar — an item may carry a due date,
//! but nothing is filed under a day and nothing rolls over. Days still matter
//! in two places: resolving "today"/"tomorrow" for a due date, and deciding
//! how long a finished item lingers, struck through, before it drops off.
//!
//! Neither starts at midnight — `todo.day_start_hour` in `config.json`
//! defaults to 04:00, so finishing something at 01:00 counts as the evening
//! before, and "tomorrow" at 01:00 is still the calendar's today.
//!
//! Every function that needs "now" takes it as an argument, so all of this is
//! testable without waiting for tomorrow. In production "now" is the clock; in
//! a test it can be a query parameter, but only when
//! `AIBRAIN_TODO_TEST_CLOCK=1` is set in the environment at startup.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::api::{ApiError, Ctx};
use crate::db;

type Shared = Arc<Ctx>;
type ApiResult<T> = std::result::Result<T, ApiError>;

// ---------------------------------------------------------------------------
// the day boundary — pure, so it can be tested against any clock
// ---------------------------------------------------------------------------

/// Which day an instant belongs to, given the hour a day starts.
///
/// Anything before the start hour belongs to the day before, which is the
/// whole point: 01:30 on Tuesday is still Monday's list.
pub fn day_of<Tz: TimeZone>(now: &DateTime<Tz>, start_hour: u32) -> NaiveDate {
    let local = now.naive_local();
    if local.hour() < start_hour {
        local.date().pred_opt().unwrap_or_else(|| local.date())
    } else {
        local.date()
    }
}

/// The half-open instant range `[start, end)` that a day covers.
///
/// Used to ask "was this completed during that day?" without teaching SQL
/// about the start hour or the machine's timezone.
pub fn day_window<Tz: TimeZone>(
    day: NaiveDate,
    start_hour: u32,
    tz: &Tz,
) -> (DateTime<Tz>, DateTime<Tz>) {
    (
        instant_at(day, start_hour, tz),
        instant_at(day.succ_opt().unwrap_or(day), start_hour, tz),
    )
}

/// A local wall-clock hour on a given date, as an instant.
///
/// On the spring-forward morning the start hour may not exist locally; step
/// forward until it does rather than returning nothing.
fn instant_at<Tz: TimeZone>(day: NaiveDate, hour: u32, tz: &Tz) -> DateTime<Tz> {
    for offset in 0..6 {
        let naive: NaiveDateTime = day
            .and_hms_opt((hour + offset) % 24, 0, 0)
            .unwrap_or_else(|| day.and_hms_opt(0, 0, 0).expect("midnight always exists"));
        if let Some(at) = tz.from_local_datetime(&naive).earliest() {
            return at;
        }
    }
    // Every hour of this date is unrepresentable, which no real zone does.
    tz.from_utc_datetime(&day.and_hms_opt(0, 0, 0).expect("midnight always exists"))
}

/// A due date as a person or a model writes it: `YYYY-MM-DD`, `today` or
/// `tomorrow`, resolved against the start hour rather than the calendar.
/// `Ok(None)` is an explicit "no date"; `Err` is text that is none of these.
pub fn parse_due(text: Option<&str>, today: NaiveDate) -> Result<Option<NaiveDate>, ()> {
    match text.map(str::trim) {
        None | Some("") | Some("none") => Ok(None),
        Some("today") => Ok(Some(today)),
        Some("tomorrow") => Ok(Some(today.succ_opt().unwrap_or(today))),
        Some(text) => parse_day(text).map(Some).ok_or(()),
    }
}

/// A `YYYY-MM-DD` from a query string, or nothing.
pub fn parse_day(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

/// A `?now=` value, in any of the shapes a person or a test might send.
fn parse_now(text: &str) -> Option<DateTime<Local>> {
    let text = text.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&Local));
    }
    for format in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            if let Some(at) = Local.from_local_datetime(&naive).earliest() {
                return Some(at);
            }
        }
    }
    parse_day(text).and_then(|d| {
        Local
            .from_local_datetime(&d.and_hms_opt(12, 0, 0).expect("noon always exists"))
            .earliest()
    })
}

/// Read once at startup rather than per request, so flipping the variable
/// cannot open a time machine on a running service.
fn test_clock_allowed() -> bool {
    static ALLOWED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALLOWED.get_or_init(|| {
        matches!(
            std::env::var("AIBRAIN_TODO_TEST_CLOCK").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

fn resolve_now(supplied: Option<&String>) -> DateTime<Local> {
    match supplied {
        Some(text) if test_clock_allowed() => parse_now(text).unwrap_or_else(Local::now),
        _ => Local::now(),
    }
}

// ---------------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------------

/// Merged into the main router by `api::router`, so state is applied once.
pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/todos", get(list).post(create))
        .route("/todos/history", get(history))
        .route("/todos/:id", patch(edit))
        .route("/todos/:id/complete", post(complete))
        .route("/todos/:id/uncomplete", post(uncomplete))
        .route("/todos/:id/cancel", post(cancel))
        .route("/todos/:id/due", post(set_due))
        .route("/todos/:id/link", post(link))
        .route("/todos/:id/file", post(file_todo))
        .route("/todos/folders", post(create_folder))
        .route("/todos/folders/:id", patch(edit_folder))
        .route("/todos/folders/:id/delete", post(delete_folder))
}

#[derive(Deserialize, Default)]
pub struct DayParams {
    /// Honoured only when `AIBRAIN_TODO_TEST_CLOCK=1`.
    #[serde(default)]
    now: Option<String>,
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "no such todo" }))).into_response()
}

async fn list(
    State(ctx): State<Shared>,
    Query(params): Query<DayParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let hour = ctx.config()?.todo_day_start_hour;
    let now = resolve_now(params.now.as_ref());
    let today = day_of(&now, hour);
    // Finished items linger until the day they were finished is over.
    let (since, _) = day_window(today, hour, &Local);
    let todos = db::todo::list(&ctx.pool, since.with_timezone(&Utc)).await?;
    // Folders sit below the list, so the shelf loads them in the same trip.
    let folders = db::todo::list_folders(&ctx.pool).await?;

    Ok(Json(json!({
        // The browser labels due dates against this, not its own clock — the
        // start hour decides what "today" is.
        "today": today.to_string(),
        "start_hour": hour,
        "todos": todos,
        "folders": folders,
    })))
}

fn bad_due() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "a due date must be YYYY-MM-DD, today, tomorrow or empty" })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct RefBody {
    brain_id: String,
    rel_path: String,
}

#[derive(Deserialize)]
struct CreateBody {
    body: String,
    #[serde(default)]
    due_on: Option<String>,
    #[serde(default)]
    refs: Vec<RefBody>,
}

async fn create(
    State(ctx): State<Shared>,
    Query(params): Query<DayParams>,
    Json(input): Json<CreateBody>,
) -> ApiResult<Response> {
    let body = input.body.trim().to_string();
    if body.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a to-do needs some text" })),
        )
            .into_response());
    }
    let hour = ctx.config()?.todo_day_start_hour;
    let now = resolve_now(params.now.as_ref());
    let Ok(due) = parse_due(input.due_on.as_deref(), day_of(&now, hour)) else {
        return Ok(bad_due());
    };

    let refs: Vec<(String, String)> = input
        .refs
        .into_iter()
        .map(|r| (r.brain_id, r.rel_path))
        .collect();
    let id = db::todo::create(&ctx.pool, &body, due, now.with_timezone(&Utc), &refs).await?;
    Ok(one(&ctx, id).await?)
}

/// Whatever a mutation left behind, so the caller never has to re-read.
async fn one(ctx: &Shared, id: i64) -> ApiResult<Response> {
    match db::todo::get(&ctx.pool, id).await? {
        Some(todo) => Ok(Json(json!({ "todo": todo })).into_response()),
        None => Ok(not_found()),
    }
}

async fn state_change(
    ctx: &Shared,
    id: i64,
    kind: &str,
    now: DateTime<Local>,
) -> ApiResult<Response> {
    if !db::todo::set_state(&ctx.pool, id, kind, now.with_timezone(&Utc)).await? {
        return Ok(not_found());
    }
    one(ctx, id).await
}

async fn complete(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
) -> ApiResult<Response> {
    state_change(&ctx, id, "completed", resolve_now(params.now.as_ref())).await
}

async fn uncomplete(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
) -> ApiResult<Response> {
    state_change(&ctx, id, "uncompleted", resolve_now(params.now.as_ref())).await
}

async fn cancel(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
) -> ApiResult<Response> {
    state_change(&ctx, id, "cancelled", resolve_now(params.now.as_ref())).await
}

#[derive(Deserialize)]
struct DueBody {
    /// Absent or null clears the date.
    #[serde(default)]
    due_on: Option<String>,
}

async fn set_due(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
    Json(input): Json<DueBody>,
) -> ApiResult<Response> {
    let now = resolve_now(params.now.as_ref());
    let hour = ctx.config()?.todo_day_start_hour;
    // "tomorrow" is the common case and the UI could compute it, but the day
    // it would compute is the browser's, not the one the start hour defines.
    let Ok(due) = parse_due(input.due_on.as_deref(), day_of(&now, hour)) else {
        return Ok(bad_due());
    };
    if !db::todo::set_due(&ctx.pool, id, due, now.with_timezone(&Utc)).await? {
        return Ok(not_found());
    }
    one(&ctx, id).await
}

async fn link(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
    Json(input): Json<RefBody>,
) -> ApiResult<Response> {
    let now = resolve_now(params.now.as_ref());
    let linked = db::todo::link(
        &ctx.pool,
        id,
        &input.brain_id,
        &input.rel_path,
        now.with_timezone(&Utc),
    )
    .await?;
    if !linked {
        return Ok(not_found());
    }
    one(&ctx, id).await
}

#[derive(Deserialize)]
struct FileBody {
    #[serde(default)]
    folder_id: Option<i64>,
}

async fn file_todo(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
    Json(input): Json<FileBody>,
) -> ApiResult<Response> {
    let now = resolve_now(params.now.as_ref());
    if !db::todo::file_todo(&ctx.pool, id, input.folder_id, now.with_timezone(&Utc)).await? {
        return Ok(not_found());
    }
    one(&ctx, id).await
}

fn not_found_folder() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "no such folder" }))).into_response()
}

#[derive(Deserialize)]
struct CreateFolderBody {
    name: String,
}

async fn create_folder(
    State(ctx): State<Shared>,
    Json(input): Json<CreateFolderBody>,
) -> ApiResult<Response> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "a folder needs a name" })),
        )
            .into_response());
    }
    let id = db::todo::create_folder(&ctx.pool, &name).await?;
    Ok(Json(json!({ "id": id, "name": name })).into_response())
}

#[derive(Deserialize)]
struct EditFolderBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    collapsed: Option<bool>,
    #[serde(default)]
    sort_order: Option<f64>,
}

async fn edit_folder(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Json(input): Json<EditFolderBody>,
) -> ApiResult<Response> {
    let name = input.name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let changed = db::todo::update_folder(&ctx.pool, id, name, input.collapsed, input.sort_order)
        .await?;
    if !changed {
        return Ok(not_found_folder());
    }
    Ok(Json(json!({ "ok": true })).into_response())
}

async fn delete_folder(State(ctx): State<Shared>, Path(id): Path<i64>) -> ApiResult<Response> {
    if !db::todo::delete_folder(&ctx.pool, id).await? {
        return Ok(not_found_folder());
    }
    Ok(Json(json!({ "ok": true })).into_response())
}

#[derive(Deserialize)]
struct PatchBody {
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    sort_order: Option<f64>,
}

async fn edit(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
    Json(input): Json<PatchBody>,
) -> ApiResult<Response> {
    let now = resolve_now(params.now.as_ref());
    let body = input.body.as_deref().map(str::trim).filter(|b| !b.is_empty());
    if !db::todo::patch(&ctx.pool, id, body, input.sort_order, now.with_timezone(&Utc)).await? {
        return Ok(not_found());
    }
    one(&ctx, id).await
}

#[derive(Deserialize)]
struct HistoryParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn history(
    State(ctx): State<Shared>,
    Query(params): Query<HistoryParams>,
) -> ApiResult<Json<serde_json::Value>> {
    let query = params.q.unwrap_or_default();
    let limit = params.limit.unwrap_or(50).clamp(1, 500);
    let todos = db::todo::history(&ctx.pool, &query, limit).await?;
    Ok(Json(json!({
        "query": query,
        "count": todos.len(),
        "todos": todos,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, FixedOffset};

    /// A zone with no DST, so these assertions mean the same thing wherever
    /// the test machine happens to be.
    fn zone() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn at(text: &str) -> DateTime<FixedOffset> {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M").unwrap();
        zone().from_local_datetime(&naive).unwrap()
    }

    fn day(text: &str) -> NaiveDate {
        parse_day(text).unwrap()
    }

    #[test]
    fn late_work_counts_as_the_day_before() {
        assert_eq!(day_of(&at("2026-09-22 01:30"), 4), day("2026-09-21"));
        assert_eq!(day_of(&at("2026-09-22 03:59"), 4), day("2026-09-21"));
        assert_eq!(day_of(&at("2026-09-22 04:00"), 4), day("2026-09-22"));
        assert_eq!(day_of(&at("2026-09-22 23:59"), 4), day("2026-09-22"));
    }

    #[test]
    fn a_zero_start_hour_is_plain_midnight() {
        assert_eq!(day_of(&at("2026-09-22 00:00"), 0), day("2026-09-22"));
        assert_eq!(day_of(&at("2026-09-22 23:59"), 0), day("2026-09-22"));
    }

    #[test]
    fn the_boundary_holds_across_a_month_and_a_year() {
        assert_eq!(day_of(&at("2026-10-01 02:00"), 4), day("2026-09-30"));
        assert_eq!(day_of(&at("2027-01-01 02:00"), 4), day("2026-12-31"));
    }

    #[test]
    fn a_day_window_runs_start_hour_to_start_hour() {
        let (from, until) = day_window(day("2026-09-22"), 4, &zone());
        assert_eq!(from, at("2026-09-22 04:00"));
        assert_eq!(until, at("2026-09-23 04:00"));
        assert_eq!((until - from).num_hours(), 24);
    }

    #[test]
    fn everything_completed_inside_the_window_belongs_to_that_day() {
        let d = day("2026-09-22");
        let (from, until) = day_window(d, 4, &zone());
        // The two instants the naive midnight version would get wrong.
        let late = at("2026-09-23 01:00");
        let early = at("2026-09-22 03:00");
        assert!(late >= from && late < until, "01:00 is still the same day");
        assert!(early < from, "03:00 belongs to the day before");
        assert_eq!(day_of(&late, 4), d);
    }

    #[test]
    fn a_due_date_is_a_day_a_shorthand_or_nothing() {
        let today = day("2026-09-22");
        assert_eq!(parse_due(None, today), Ok(None));
        assert_eq!(parse_due(Some(""), today), Ok(None));
        assert_eq!(parse_due(Some(" none "), today), Ok(None));
        assert_eq!(parse_due(Some("today"), today), Ok(Some(today)));
        assert_eq!(parse_due(Some("tomorrow"), today), Ok(Some(day("2026-09-23"))));
        assert_eq!(parse_due(Some("2026-12-31"), today), Ok(Some(day("2026-12-31"))));
        assert_eq!(parse_due(Some("next week"), today), Err(()));
    }

    #[test]
    fn days_are_parsed_strictly() {
        assert_eq!(parse_day("2026-09-22"), Some(day("2026-09-22")));
        assert_eq!(parse_day("  2026-09-22 "), Some(day("2026-09-22")));
        for bad in ["", "today", "22-09-2026", "2026-13-01", "2026-09-22T10:00"] {
            assert_eq!(parse_day(bad), None, "{bad} should not parse");
        }
    }

    #[test]
    fn an_injected_now_is_read_in_several_shapes() {
        for text in [
            "2026-09-22T10:30:00Z",
            "2026-09-22T10:30:00",
            "2026-09-22 10:30",
            "2026-09-22",
        ] {
            assert!(parse_now(text).is_some(), "{text} should parse");
        }
        assert!(parse_now("not a time").is_none());
    }

    #[test]
    fn a_reschedule_shorthand_resolves_against_the_start_hour() {
        // 01:00 on the 22nd is still the 21st, so "tomorrow" is the 22nd —
        // not the 23rd the calendar would give you.
        let now = at("2026-09-22 01:00");
        let today = day_of(&now, 4);
        assert_eq!(today, day("2026-09-21"));
        assert_eq!(today.succ_opt().unwrap(), day("2026-09-22"));
    }

    #[test]
    fn the_injected_clock_is_ignored_unless_the_variable_is_set() {
        // The test binary does not set AIBRAIN_TODO_TEST_CLOCK, so a supplied
        // value must be discarded rather than trusted.
        if !test_clock_allowed() {
            let now = resolve_now(Some(&"1999-01-01T00:00:00Z".to_string()));
            assert!(now.year() >= 2020, "the wall clock should have won");
        }
    }
}

/// End to end against a real Postgres: create, date, complete, file, search.
///
/// In-module rather than in `tests/` because the crate is a binary, and it
/// skips with a printed reason rather than failing when the database is
/// absent — a suite that goes red on a laptop with no container is a suite
/// people stop running.
///
///   AIBRAIN_TEST_DATABASE_URL=postgres://aibrain:aibrain@127.0.0.1:5433/aibrain_todo \
///     cargo test todo::integration
///
/// Every row it writes carries a unique marker and is deleted at the end, so
/// it can share a database with anything else.
#[cfg(test)]
mod integration {
    use super::*;
    use crate::db::todo as store;

    async fn pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("AIBRAIN_TEST_DATABASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let Some(url) = url else {
            eprintln!("skipping todo integration: AIBRAIN_TEST_DATABASE_URL is unset");
            return None;
        };
        match crate::db::connect(&url).await {
            Ok(pool) => Some(pool),
            Err(err) => {
                eprintln!("skipping todo integration: {err:#}");
                None
            }
        }
    }

    /// The start of a day's window, in the zone the service itself uses.
    fn since(day: NaiveDate) -> DateTime<Utc> {
        day_window(day, 4, &Local).0.with_timezone(&Utc)
    }

    fn instant(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn mine<'a>(list: &'a [store::Todo], marker: &str) -> Vec<&'a store::Todo> {
        list.iter().filter(|t| t.body.contains(marker)).collect()
    }

    fn find<'a>(list: &'a [store::Todo], id: i64) -> &'a store::Todo {
        list.iter().find(|t| t.id == id).expect("todo should be in the list")
    }

    #[tokio::test]
    async fn one_list_with_optional_due_dates_and_a_full_history() {
        let Some(pool) = pool().await else { return };

        let marker = format!("zzmarker{}", std::process::id());
        let monday = parse_day("2026-03-02").unwrap();
        let tuesday = parse_day("2026-03-03").unwrap();
        let friday = parse_day("2026-03-06").unwrap();

        let plants = store::create(&pool, &format!("water the {marker} plants"), None,
                                   instant(monday, 9), &[])
            .await
            .unwrap();
        let letter = store::create(&pool, &format!("post the {marker} letter"), Some(friday),
                                   instant(monday, 9), &[])
            .await
            .unwrap();

        // Both are on the list, undated one first because it was added first.
        let list = store::list(&pool, since(monday)).await.unwrap();
        let rows = mine(&list, &marker);
        assert_eq!(rows.iter().map(|t| t.id).collect::<Vec<_>>(), vec![plants, letter]);
        assert_eq!(find(&list, plants).due_on, None, "no date unless one is given");
        assert_eq!(find(&list, letter).due_on.as_deref(), Some("2026-03-06"));

        // A week later nothing has moved: there is no rollover to move it.
        let later = parse_day("2026-03-09").unwrap();
        let list = store::list(&pool, since(later)).await.unwrap();
        assert_eq!(mine(&list, &marker).len(), 2);
        assert_eq!(find(&list, letter).due_on.as_deref(), Some("2026-03-06"), "overdue, not moved");

        // The letter gets posted at one in the morning — still Monday. It
        // lingers, struck through, until Monday is over, then drops off.
        store::set_state(&pool, letter, "completed", instant(tuesday, 1)).await.unwrap();
        let list = store::list(&pool, since(monday)).await.unwrap();
        assert_eq!(find(&list, letter).state, "completed", "struck through, still shown");
        let list = store::list(&pool, since(tuesday)).await.unwrap();
        assert_eq!(mine(&list, &marker).len(), 1, "gone once its day is over");

        // Dating, re-dating and clearing are each one event.
        store::set_due(&pool, plants, Some(tuesday), instant(monday, 10)).await.unwrap();
        store::set_due(&pool, plants, Some(tuesday), instant(monday, 10)).await.unwrap();
        store::set_due(&pool, plants, None, instant(monday, 11)).await.unwrap();
        assert_eq!(store::get(&pool, plants).await.unwrap().unwrap().due_on, None);

        let found = store::history(&pool, &marker, 50).await.unwrap();
        assert_eq!(found.len(), 2, "both items are searchable by body");
        let plant = find(&found, plants);
        let kinds: Vec<&str> = plant.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["created", "rescheduled", "rescheduled"], "a no-op logs nothing");
        assert_eq!(plant.events[1].from_day, None);
        assert_eq!(plant.events[1].to_day.as_deref(), Some("2026-03-03"));
        assert_eq!(plant.events[2].from_day.as_deref(), Some("2026-03-03"));
        assert_eq!(plant.events[2].to_day, None);
        assert_eq!(find(&found, letter).events[0].to_day.as_deref(), Some("2026-03-06"),
                   "the created event remembers the first due date");

        // Undoing a completion is itself an event, not an erasure.
        store::set_state(&pool, letter, "uncompleted", instant(friday, 9)).await.unwrap();
        let row = store::get(&pool, letter).await.unwrap().unwrap();
        assert_eq!(row.state, "open");
        assert!(row.completed_at.is_none());

        // A note link is kept by path even when no such note exists here, and
        // linking the same note twice is not two links.
        for _ in 0..2 {
            store::link(&pool, plants, "nobrain", "Notes/Plants.md", instant(friday, 9))
                .await
                .unwrap();
        }
        let plant = store::get(&pool, plants).await.unwrap().unwrap();
        assert_eq!(plant.refs.len(), 1);
        assert!(plant.refs[0].note_id.is_none(), "no note, but the path is kept");

        assert!(!store::set_state(&pool, -1, "completed", instant(friday, 9)).await.unwrap(),
                "a missing todo is a miss, not an error");
        assert!(!store::set_due(&pool, -1, None, instant(friday, 9)).await.unwrap());

        sqlx::query("DELETE FROM todo WHERE id = ANY($1)")
            .bind(vec![plants, letter])
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_filed_item_leaves_the_list_and_comes_back_at_the_end() {
        let Some(pool) = pool().await else { return };

        let marker = format!("zzfolder{}", std::process::id());
        let monday = parse_day("2026-04-06").unwrap();
        let at = instant(monday, 9);

        let folder_id = store::create_folder(&pool, &format!("{marker} backlog")).await.unwrap();
        let item_a = store::create(&pool, &format!("{marker} a"), None, at, &[]).await.unwrap();
        let item_b = store::create(&pool, &format!("{marker} b"), None, at, &[]).await.unwrap();

        store::file_todo(&pool, item_a, Some(folder_id), at).await.unwrap();
        let list = store::list(&pool, since(monday)).await.unwrap();
        assert_eq!(mine(&list, &marker).iter().map(|t| t.id).collect::<Vec<_>>(), vec![item_b],
                   "only the unfiled item is on the list");
        let folders = store::list_folders(&pool).await.unwrap();
        let backlog = folders.iter().find(|f| f.id == folder_id).unwrap();
        assert_eq!(backlog.todos.iter().map(|t| t.id).collect::<Vec<_>>(), vec![item_a]);

        // Taken back out, it goes to the end — after b, though it was first.
        store::file_todo(&pool, item_a, None, at).await.unwrap();
        let list = store::list(&pool, since(monday)).await.unwrap();
        assert_eq!(mine(&list, &marker).iter().map(|t| t.id).collect::<Vec<_>>(),
                   vec![item_b, item_a]);

        // Folder metadata: reorder a member, rename, collapse.
        store::file_todo(&pool, item_a, Some(folder_id), at).await.unwrap();
        store::file_todo(&pool, item_b, Some(folder_id), at).await.unwrap();
        store::patch(&pool, item_b, None, Some(-5.0), at).await.unwrap();
        assert!(store::update_folder(&pool, folder_id, Some("renamed"), Some(true), None)
            .await
            .unwrap());
        let folders = store::list_folders(&pool).await.unwrap();
        let backlog = folders.iter().find(|f| f.id == folder_id).unwrap();
        assert_eq!(backlog.todos[0].id, item_b, "the lower sort_order sorts first");
        assert_eq!(backlog.name, "renamed");
        assert!(backlog.collapsed);

        assert!(!store::update_folder(&pool, -1, Some("x"), None, None).await.unwrap());
        assert!(!store::file_todo(&pool, -1, Some(folder_id), at).await.unwrap());

        // Deleting the folder un-files its members via ON DELETE SET NULL,
        // rather than deleting the append-only todo rows.
        assert!(store::delete_folder(&pool, folder_id).await.unwrap());
        assert!(!store::delete_folder(&pool, folder_id).await.unwrap(), "already gone");
        let list = store::list(&pool, since(monday)).await.unwrap();
        assert_eq!(mine(&list, &marker).len(), 2, "both back on the list");

        sqlx::query("DELETE FROM todo WHERE id = ANY($1)")
            .bind(vec![item_a, item_b])
            .execute(&pool)
            .await
            .unwrap();
    }
}
