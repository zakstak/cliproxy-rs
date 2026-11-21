//! The dashboard's history API (cliproxy-rs only), over the history store
//! ([`crate::persistence::history::HistoryStore`]). Every route needs the management key.
//!
//! - `GET /history/status`: whether a store is open, its backend, retention and counts.
//! - `GET /history/requests`: recorded upstream attempts, newest first, by id cursor.
//! - `GET /history/totals`: request counts and token sums per time bucket.
//! - `GET /history/events`: server log events, newest first, by id cursor.
//!
//! Times are Unix milliseconds. Paging: `before=<id>` continues a newest-first page
//! toward older rows, `after=<id>` reads rows appended since (oldest first), and neither
//! starts at the newest. Without an open store the data routes answer 503.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{Value, json};

use super::Management;
use super::auth_files::{Query, fail};
use crate::persistence::history::{
    Cursor, EventFilter, Group, HistoryStore, Level, MAX_PAGE, RequestFilter, RowId, UnixMillis,
};

/// Rows per page when the caller does not say.
const DEFAULT_PAGE: usize = 200;
/// Buckets one totals answer may hold at most, so a tiny bucket over a long range
/// cannot build an unbounded answer.
const MAX_BUCKETS: i64 = 10_000;

type Refusal = Box<Response>;

fn bad(message: impl Into<String>) -> Refusal {
    Box::new(fail(StatusCode::BAD_REQUEST, message))
}

fn store(state: &Management) -> Result<Arc<dyn HistoryStore>, Refusal> {
    state
        .rt
        .usage_queue()
        .history()
        .map(|recorder| recorder.store().clone())
        .ok_or_else(|| {
            Box::new(fail(
                StatusCode::SERVICE_UNAVAILABLE,
                "history store disabled (storage.backend: none, or it failed to open; see the server log)",
            ))
        })
}

fn text(q: &Query, key: &str) -> Option<String> {
    Some(q.first(key).trim()).filter(|v| !v.is_empty()).map(str::to_owned)
}

fn millis(q: &Query, key: &str) -> Result<Option<UnixMillis>, Refusal> {
    match q.first(key).trim() {
        "" => Ok(None),
        v => v
            .parse::<i64>()
            .map(|n| Some(UnixMillis(n)))
            .map_err(|_| bad(format!("{key} must be Unix milliseconds"))),
    }
}

fn row_id(q: &Query, key: &str) -> Result<Option<RowId>, Refusal> {
    match q.first(key).trim() {
        "" => Ok(None),
        v => match v.parse::<i64>() {
            Ok(n) if n >= 0 => Ok(Some(RowId(n))),
            _ => Err(bad(format!("{key} must be a row id from a previous answer"))),
        },
    }
}

fn cursor(q: &Query) -> Result<Cursor, Refusal> {
    match (row_id(q, "before")?, row_id(q, "after")?) {
        (Some(_), Some(_)) => Err(bad("give before or after, not both")),
        (Some(id), None) => Ok(Cursor::Before(id)),
        (None, Some(id)) => Ok(Cursor::After(id)),
        (None, None) => Ok(Cursor::Newest),
    }
}

fn limit(q: &Query) -> Result<usize, Refusal> {
    match q.first("limit").trim() {
        "" => Ok(DEFAULT_PAGE),
        v => match v.parse::<usize>() {
            Ok(n) if n > 0 => Ok(n.min(MAX_PAGE)),
            _ => Err(bad(format!("limit must be between 1 and {MAX_PAGE}"))),
        },
    }
}

fn request_filter(q: &Query) -> Result<RequestFilter, Refusal> {
    let failed = match q.first("failed").trim() {
        "" => None,
        "true" => Some(true),
        "false" => Some(false),
        _ => return Err(bad("failed must be true or false")),
    };
    let filter = RequestFilter {
        since: millis(q, "since")?,
        until: millis(q, "until")?,
        provider: text(q, "provider"),
        auth_index: text(q, "account"),
        model: text(q, "model"),
        request_id: text(q, "request_id"),
        failed,
    };
    Ok(filter)
}

fn answer(result: Result<Response, Refusal>) -> Response {
    result.unwrap_or_else(|refusal| *refusal)
}

fn store_failed(e: anyhow::Error) -> Refusal {
    tracing::warn!("history read failed: {e:#}");
    Box::new(fail(
        StatusCode::INTERNAL_SERVER_ERROR,
        "history store read failed; see the server log",
    ))
}

/// `GET /history/status`: `{"enabled", "backend", "retention_days", "written", "dropped"}`.
/// `written` and `dropped` count since the server started.
pub(crate) async fn status(State(state): State<Arc<Management>>) -> Response {
    let body = match state.rt.usage_queue().history() {
        Some(recorder) => {
            let stats = recorder.stats();
            json!({
                "enabled": true,
                "backend": recorder.store().backend(),
                "retention_days": recorder.retention_days(),
                "written": stats.written,
                "dropped": stats.dropped,
            })
        }
        None => json!({"enabled": false}),
    };
    super::json(StatusCode::OK, &body)
}

/// `GET /history/requests?since&until&provider&account&model&request_id&failed&before&after&limit`:
/// `{"requests": [...], "more": bool}`. `account` is an `auth_index`.
pub(crate) async fn requests(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    answer(
        async {
            let q = Query::parse(raw);
            let (filter, cursor, limit) = (request_filter(&q)?, cursor(&q)?, limit(&q)?);
            let page = store(&state)?
                .requests(filter, cursor, limit)
                .await
                .map_err(store_failed)?;
            Ok(super::json(
                StatusCode::OK,
                &json!({"requests": page.rows, "more": page.more}),
            ))
        }
        .await,
    )
}

/// `GET /history/totals?since&until&provider&account&model&failed&bucket&group`:
/// `{"bucket_ms", "group", "totals": [...]}`. `bucket` is milliseconds (default one
/// hour); `group` is `none` (default), `provider`, `account` or `model`. Both bounds are
/// required, so the answer stays bounded.
pub(crate) async fn totals(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    answer(
        async {
            let q = Query::parse(raw);
            let filter = request_filter(&q)?;
            let bucket = match q.first("bucket").trim() {
                "" => 3_600_000,
                v => match v.parse::<i64>() {
                    Ok(n) if n >= 1000 => n,
                    _ => return Err(bad("bucket must be at least 1000 milliseconds")),
                },
            };
            let (group, name) = match q.first("group").trim() {
                "" | "none" => (Group::None, "none"),
                "provider" => (Group::Provider, "provider"),
                "account" => (Group::Account, "account"),
                "model" => (Group::Model, "model"),
                _ => return Err(bad("group must be none, provider, account or model")),
            };
            let (Some(since), Some(until)) = (filter.since, filter.until) else {
                return Err(bad("since and until are required"));
            };
            if until <= since {
                return Err(bad("until must be after since"));
            }
            if (until.0 - since.0) / bucket > MAX_BUCKETS {
                return Err(bad(format!(
                    "at most {MAX_BUCKETS} buckets per answer; use a wider bucket"
                )));
            }
            let totals = store(&state)?
                .totals(filter, bucket, group)
                .await
                .map_err(store_failed)?;
            Ok(super::json(
                StatusCode::OK,
                &json!({"bucket_ms": bucket, "group": name, "totals": totals}),
            ))
        }
        .await,
    )
}

/// `GET /history/events?since&until&level&request_id&before&after&limit`:
/// `{"events": [...], "more": bool}`. `level` keeps that level and more severe ones.
pub(crate) async fn events(State(state): State<Arc<Management>>, RawQuery(raw): RawQuery) -> Response {
    answer(
        async {
            let q = Query::parse(raw);
            let min_level = match q.first("level").trim() {
                "" => None,
                v => Some(Level::parse(v).ok_or_else(|| bad("level must be trace, debug, info, warn or error"))?),
            };
            let filter = EventFilter {
                since: millis(&q, "since")?,
                until: millis(&q, "until")?,
                min_level,
                request_id: text(&q, "request_id"),
            };
            let (cursor, limit) = (cursor(&q)?, limit(&q)?);
            let page = store(&state)?
                .events(filter, cursor, limit)
                .await
                .map_err(store_failed)?;
            let rows: Vec<Value> = page
                .rows
                .into_iter()
                .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
                .collect();
            Ok(super::json(StatusCode::OK, &json!({"events": rows, "more": page.more})))
        }
        .await,
    )
}
