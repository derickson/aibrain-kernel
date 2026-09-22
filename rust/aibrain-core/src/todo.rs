//! The day's list: the HTTP surface, and the one place that decides which day
//! an instant belongs to.
//!
//! Two things make that decision non-obvious. A day does not start at
//! midnight — `todo.day_start_hour` in `config.json` defaults to 04:00, so
//! finishing something at 01:00 counts as the evening before. And rollover is
//! lazy: opening day D is what migrates everything still open from before it,
//! because a cron job cannot run on a laptop that was asleep at midnight.
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

/// Whether opening `day` should drag stale open items onto it.
///
/// Only forwards. Looking back at last Tuesday is reading history, and
/// history that rewrites itself when you look at it is not history — without
/// this guard, paging back a day would pull today's unfinished work into it.
pub fn should_roll(day: NaiveDate, today: NaiveDate) -> bool {
    day >= today
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
        .route("/todos/:id/reschedule", post(reschedule))
        .route("/todos/:id/link", post(link))
}

#[derive(Deserialize, Default)]
pub struct DayParams {
    #[serde(default)]
    day: Option<String>,
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
    let day = params.day.as_deref().and_then(parse_day).unwrap_or(today);

    let rolled = if should_roll(day, today) {
        db::todo::roll_over(&ctx.pool, day, now.with_timezone(&Utc)).await?
    } else {
        0
    };

    let (from, until) = day_window(day, hour, &Local);
    let todos = db::todo::for_day(
        &ctx.pool,
        day,
        from.with_timezone(&Utc),
        until.with_timezone(&Utc),
    )
    .await?;

    Ok(Json(json!({
        "day": day.to_string(),
        "today": today.to_string(),
        "prev_day": day.pred_opt().unwrap_or(day).to_string(),
        "next_day": day.succ_opt().unwrap_or(day).to_string(),
        "start_hour": hour,
        "rolled": rolled,
        "todos": todos,
    })))
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
    scheduled_on: Option<String>,
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
    let day = input
        .scheduled_on
        .as_deref()
        .and_then(parse_day)
        .unwrap_or_else(|| day_of(&now, hour));

    let refs: Vec<(String, String)> = input
        .refs
        .into_iter()
        .map(|r| (r.brain_id, r.rel_path))
        .collect();
    let id = db::todo::create(&ctx.pool, &body, day, now.with_timezone(&Utc), &refs).await?;
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
struct RescheduleBody {
    to_day: String,
}

async fn reschedule(
    State(ctx): State<Shared>,
    Path(id): Path<i64>,
    Query(params): Query<DayParams>,
    Json(input): Json<RescheduleBody>,
) -> ApiResult<Response> {
    let now = resolve_now(params.now.as_ref());
    let hour = ctx.config()?.todo_day_start_hour;
    // "tomorrow" is the common case and the UI could compute it, but the day
    // it would compute is the browser's, not the one the start hour defines.
    let to_day = match input.to_day.trim() {
        "today" => day_of(&now, hour),
        "tomorrow" => {
            let today = day_of(&now, hour);
            today.succ_opt().unwrap_or(today)
        }
        text => match parse_day(text) {
            Some(day) => day,
            None => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "to_day must be YYYY-MM-DD, today or tomorrow" })),
                )
                    .into_response())
            }
        },
    };
    if !db::todo::reschedule(&ctx.pool, id, to_day, now.with_timezone(&Utc)).await? {
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
    fn looking_forward_rolls_and_looking_back_does_not() {
        let today = day("2026-09-22");
        assert!(should_roll(today, today));
        assert!(should_roll(day("2026-09-23"), today), "tomorrow is fair game");
        assert!(!should_roll(day("2026-09-21"), today), "history stays put");
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

/// End to end against a real Postgres: create, roll over, complete, search.
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

    /// The zone the service itself uses, so the window the test computes is
    /// the window the handlers would compute.
    fn window(day: NaiveDate) -> (DateTime<Utc>, DateTime<Utc>) {
        let (from, until) = day_window(day, 4, &Local);
        (from.with_timezone(&Utc), until.with_timezone(&Utc))
    }

    fn instant(day: NaiveDate, hour: u32) -> DateTime<Utc> {
        Local
            .from_local_datetime(&day.and_hms_opt(hour, 0, 0).unwrap())
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    fn count_mine(list: &[store::Todo], marker: &str) -> usize {
        list.iter().filter(|t| t.body.contains(marker)).count()
    }

    fn find<'a>(list: &'a [store::Todo], id: i64) -> &'a store::Todo {
        list.iter().find(|t| t.id == id).expect("todo should be in the list")
    }

    #[tokio::test]
    async fn a_day_rolls_over_completes_and_stays_in_history() {
        let Some(pool) = pool().await else { return };

        let marker = format!("zzmarker{}", std::process::id());
        let monday = parse_day("2026-03-02").unwrap();
        let tuesday = parse_day("2026-03-03").unwrap();
        let wednesday = parse_day("2026-03-04").unwrap();

        let carried = store::create(
            &pool,
            &format!("water the {marker} plants"),
            monday,
            instant(monday, 9),
            &[],
        )
        .await
        .unwrap();
        let finished = store::create(
            &pool,
            &format!("post the {marker} letter"),
            monday,
            instant(monday, 9),
            &[],
        )
        .await
        .unwrap();

        // Monday's own view moves nothing: both were scheduled on it.
        let (from, until) = window(monday);
        let moved = store::roll_over(&pool, monday, instant(monday, 10)).await.unwrap();
        assert_eq!(moved, 0, "nothing is stale on its own day");
        let list = store::for_day(&pool, monday, from, until).await.unwrap();
        assert_eq!(count_mine(&list, &marker), 2);

        // The letter gets posted at one in the morning — still Monday.
        store::set_state(&pool, finished, "completed", instant(tuesday, 1))
            .await
            .unwrap();
        let list = store::for_day(&pool, monday, from, until).await.unwrap();
        assert_eq!(find(&list, finished).state, "completed", "struck through, still shown");
        assert_eq!(count_mine(&list, &marker), 2);

        // Opening Tuesday carries the open one forward and leaves the other.
        let moved = store::roll_over(&pool, tuesday, instant(tuesday, 8)).await.unwrap();
        assert!(moved >= 1, "the open item should have been carried");
        let (from, until) = window(tuesday);
        let list = store::for_day(&pool, tuesday, from, until).await.unwrap();
        assert_eq!(count_mine(&list, &marker), 1, "the completed one is gone");
        let plant = find(&list, carried);
        assert_eq!(plant.scheduled_on, "2026-03-03");
        assert_eq!(
            plant.first_scheduled_on, "2026-03-02",
            "where it started is not overwritten"
        );

        // Rescheduling moves it again, and Tuesday no longer shows it.
        store::reschedule(&pool, carried, wednesday, instant(tuesday, 9))
            .await
            .unwrap();
        let list = store::for_day(&pool, tuesday, from, until).await.unwrap();
        assert_eq!(count_mine(&list, &marker), 0, "Tuesday is empty again");

        // History keeps everything, with the story of how it got there.
        let found = store::history(&pool, &marker, 50).await.unwrap();
        assert_eq!(found.len(), 2, "both items are searchable by body");
        let plant = find(&found, carried);
        let kinds: Vec<&str> = plant.events.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["created", "rolled", "rescheduled"]);
        assert_eq!(plant.events[1].from_day.as_deref(), Some("2026-03-02"));
        assert_eq!(plant.events[1].to_day.as_deref(), Some("2026-03-03"));
        assert!(find(&found, finished).events.iter().any(|e| e.kind == "completed"));

        // Undoing a completion is itself an event, not an erasure.
        store::set_state(&pool, finished, "uncompleted", instant(wednesday, 9))
            .await
            .unwrap();
        let letter = store::get(&pool, finished).await.unwrap().unwrap();
        assert_eq!(letter.state, "open");
        assert!(letter.completed_at.is_none());
        let found = store::history(&pool, &marker, 50).await.unwrap();
        assert!(find(&found, finished).events.iter().any(|e| e.kind == "uncompleted"));

        // A note link is kept by path even when no such note exists here.
        store::link(&pool, carried, "nobrain", "Notes/Plants.md", instant(wednesday, 9))
            .await
            .unwrap();
        let plant = store::get(&pool, carried).await.unwrap().unwrap();
        assert_eq!(plant.refs.len(), 1);
        assert_eq!(plant.refs[0].rel_path, "Notes/Plants.md");
        assert!(plant.refs[0].note_id.is_none(), "no note, but the path is kept");

        // Linking the same note twice is not two links.
        store::link(&pool, carried, "nobrain", "Notes/Plants.md", instant(wednesday, 9))
            .await
            .unwrap();
        assert_eq!(store::get(&pool, carried).await.unwrap().unwrap().refs.len(), 1);

        assert!(
            !store::set_state(&pool, -1, "completed", instant(wednesday, 9)).await.unwrap(),
            "a missing todo is a miss, not an error"
        );

        sqlx::query("DELETE FROM todo WHERE id = ANY($1)")
            .bind(vec![carried, finished])
            .execute(&pool)
            .await
            .unwrap();
    }
}
