//! What a program's review tool reads. Each program has its own key and sees
//! only the submissions made to it. Every read is written to `evidence_views`,
//! which the person who submitted can see.
//!
//! `GET /api/programs/submissions?since=<unix>` lists what changed, and
//! `.../{id}/evidence` is the `pulse.evidence.v1` document described in
//! `docs/evidence.md`. The recordings themselves are streamed from
//! `.../{id}/segments/{link}/video` with range requests, so a review tool can
//! seek in them.

use std::collections::{BTreeMap, BTreeSet};

use axum::extract::FromRequestParts;
use axum::extract::{Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use pulse_core::chain;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::error::AppError;
use crate::evidence;
use crate::now;
use crate::state::AppState;
use crate::storage::{Kind, is_hex_hash};
use crate::submissions::{claimed_seconds, load_segments};

/// A segment recorded more than an hour before its link reached the server was
/// not committed in real time. Programs decide what that means.
const LATE_AFTER_SECONDS: i64 = 3600;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/programs/submissions", get(list))
        .route("/api/programs/submissions/{id}/evidence", get(manifest))
        .route(
            "/api/programs/submissions/{id}/segments/{link}/video",
            get(video),
        )
        .route(
            "/api/programs/submissions/{id}/segments/{link}/timeline",
            get(timeline),
        )
}

/// The program a request's key belongs to.
pub struct ProgramAuth(pub String);

/// Equal-length digests compared without stopping at the first difference.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter()
        .zip(b)
        .fold(0u8, |difference, (x, y)| difference | (x ^ y))
        == 0
}

impl FromRequestParts<AppState> for ProgramAuth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;
        let given = chain::sha256(token.as_bytes());
        let mut program = None;
        // every key is compared, so the time taken does not say which one was close
        for (name, key) in &state.config.program_keys {
            if same(&chain::sha256(key.as_bytes()), &given) {
                program = Some(name.clone());
            }
        }
        program.map(ProgramAuth).ok_or(AppError::Unauthorized)
    }
}

async fn log_view(state: &AppState, id: i64, program: &str, what: &str) -> Result<(), AppError> {
    // the same thing read again within 10 minutes (a player making range requests) is one view
    sqlx::query(
        "insert into evidence_views (submission_id, program, what, at) select $1::bigint, $2::text, $3::text, $4::bigint \
         where not exists (select 1 from evidence_views where submission_id = $1::bigint and program = $2::text \
         and what = $3::text and at > $4::bigint - 600)",
    )
    .bind(id)
    .bind(program)
    .bind(what)
    .bind(now())
    .execute(&state.db)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
struct ListQuery {
    since: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    ProgramAuth(program): ProgramAuth,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "select s.id, s.status, s.submitted_at, s.changed_at, s.recorded_seconds, s.active_seconds, s.claimed_seconds, \
         p.key, u.username from submissions s join projects p on p.id = s.project_id join users u on u.id = s.user_id \
         where s.program = $1 and s.status <> 'uploading' and s.changed_at >= $2 order by s.changed_at, s.id limit 100",
    )
    .bind(&program)
    .bind(query.since.unwrap_or(0))
    .fetch_all(&state.db)
    .await?;
    let submissions: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "status": row.get::<String, _>("status"),
                "project": row.get::<String, _>("key"),
                "maker": row.get::<String, _>("username"),
                "submitted_at": row.get::<Option<i64>, _>("submitted_at"),
                "changed_at": row.get::<i64, _>("changed_at"),
                "recorded_seconds": row.get::<Option<i64>, _>("recorded_seconds"),
                "active_seconds": row.get::<Option<i64>, _>("active_seconds"),
                "claimed_seconds": row.get::<Option<i64>, _>("claimed_seconds"),
            })
        })
        .collect();
    Ok(Json(json!({ "submissions": submissions })))
}

/// A submitted (not withdrawn) submission to this program, or `NotFound`.
async fn submitted_to(
    state: &AppState,
    program: &str,
    id: i64,
) -> Result<sqlx::postgres::PgRow, AppError> {
    sqlx::query(
        "select s.id, s.user_id, s.project_id, s.status, s.submitted_at, p.key, u.username, u.display_name, u.github_login, \
         u.profile_public from submissions s join projects p on p.id = s.project_id join users u on u.id = s.user_id \
         where s.id = $1 and s.program = $2 and s.status = 'submitted'",
    )
    .bind(id)
    .bind(program)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn manifest(
    State(state): State<AppState>,
    ProgramAuth(program): ProgramAuth,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let row = submitted_to(&state, &program, id).await?;
    let (user_id, project_id): (i64, i64) = (row.get("user_id"), row.get("project_id"));
    let key: String = row.get("key");
    let username: String = row.get("username");
    log_view(&state, id, &program, "manifest").await?;

    let loaded = load_segments(&state, id).await?;
    let segments: Vec<_> = loaded.iter().map(|segment| segment.as_segment()).collect();
    let analysis = evidence::analyze(&segments);
    let submitted: Vec<_> = loaded.iter().filter(|segment| !segment.omitted).collect();
    let first = submitted
        .iter()
        .map(|segment| segment.start)
        .min()
        .ok_or(AppError::Internal)?;
    let last = submitted
        .iter()
        .map(|segment| segment.end)
        .max()
        .ok_or(AppError::Internal)?;
    let claimed = claimed_seconds(&state, user_id, project_id, first, last).await?;

    let delays: Vec<i64> = submitted
        .iter()
        .map(|segment| segment.received_at - segment.end)
        .collect();
    let late = delays
        .iter()
        .filter(|delay| **delay > LATE_AFTER_SECONDS)
        .count();

    // the credited time on the person's profile for this project, slot by slot
    let mut credits: BTreeMap<i64, (i64, i64, BTreeSet<String>, BTreeSet<String>)> =
        BTreeMap::new();
    let credit_rows = sqlx::query(
        "select slot, seconds, prompts, sources, models from credits where user_id = $1 and project_id = $2 \
         and slot < $4 and slot + 300 > $3 order by slot",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(first)
    .bind(last)
    .fetch_all(&state.db)
    .await?;
    for credit in &credit_rows {
        let entry = credits.entry(credit.get("slot")).or_default();
        entry.0 += i64::from(credit.get::<i32, _>("seconds"));
        entry.1 += i64::from(credit.get::<i32, _>("prompts"));
        entry.2.extend(credit.get::<Vec<String>, _>("sources"));
        entry.3.extend(credit.get::<Vec<String>, _>("models"));
    }
    let credits: Vec<Value> = credits
        .into_iter()
        .map(|(slot, (seconds, prompts, sources, models))| {
            json!({ "slot": slot, "seconds": seconds, "prompts": prompts, "sources": sources, "models": models })
        })
        .collect();

    let pushes: Vec<Value> = sqlx::query(
        "select repo, ref, head_sha, commit_shas, forced, coalesce(pushed_at, received_at) as at from pushes \
         where user_id = $1 and 'github.com/' || repo = lower($2) order by at, head_sha limit 500",
    )
    .bind(user_id)
    .bind(&key)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|push| {
        json!({
            "repo": push.get::<String, _>("repo"),
            "ref": push.get::<String, _>("ref"),
            "head_sha": push.get::<String, _>("head_sha"),
            "commit_shas": push.get::<Vec<String>, _>("commit_shas"),
            "forced": push.get::<bool, _>("forced"),
            "at": push.get::<i64, _>("at"),
        })
    })
    .collect();

    let segment_list: Vec<Value> = loaded
        .iter()
        .map(|segment| {
            let base = format!("/api/programs/submissions/{id}/segments/{}", segment.link);
            json!({
                "link": segment.link,
                "start": segment.start,
                "end": segment.end,
                "omitted": segment.omitted,
                "committed_at": segment.received_at,
                "video_path": (!segment.omitted).then(|| format!("{base}/video")),
                "timeline_path": (!segment.omitted).then(|| format!("{base}/timeline")),
            })
        })
        .collect();

    Ok(Json(json!({
        "format": "pulse.evidence.v1",
        "submission": {
            "id": id,
            "program": program,
            "status": row.get::<String, _>("status"),
            "submitted_at": row.get::<Option<i64>, _>("submitted_at"),
            "project": key,
            "project_url": crate::github::repo_url(&key),
        },
        "maker": {
            "username": username,
            "display_name": row.get::<String, _>("display_name"),
            "github_login": row.get::<Option<String>, _>("github_login"),
            "profile_url": row.get::<bool, _>("profile_public").then(|| format!("{}/u/{username}", state.config.base_url)),
        },
        "span": { "start": first, "end": last },
        "totals": {
            "recorded_seconds": analysis.recorded_seconds,
            "active_seconds": analysis.active_seconds,
            "claimed_seconds": claimed,
            "omitted_seconds": analysis.omitted_seconds,
        },
        "checks": {
            // all of these were enforced when the recordings were uploaded; a submission cannot be finalized without them
            "video_hashes_verified": true,
            "timelines_verified": true,
            "every_segment_in_range_listed": true,
            "segments": submitted.len(),
            "omitted_segments": loaded.len() - submitted.len(),
            "late_after_seconds": LATE_AFTER_SECONDS,
            "segments_committed_late": late,
            "max_commit_delay_seconds": delays.iter().copied().max().unwrap_or(0),
        },
        "runs": analysis.runs,
        "slots_utc": analysis.slots_utc,
        "segments": segment_list,
        "credits": credits,
        "pushes": pushes,
    })))
}

/// The submission and segment a program asks for, if both exist and the segment was submitted.
async fn segment_of(state: &AppState, program: &str, id: i64, link: &str) -> Result<(), AppError> {
    if !is_hex_hash(link) {
        return Err(AppError::NotFound);
    }
    submitted_to(state, program, id).await?;
    let exists: Option<String> = sqlx::query_scalar(
        "select link from submission_segments where submission_id = $1 and link = $2 and not omitted",
    )
    .bind(id)
    .bind(link)
    .fetch_optional(&state.db)
    .await?;
    exists.map(|_| ()).ok_or(AppError::NotFound)
}

async fn video(
    State(state): State<AppState>,
    ProgramAuth(program): ProgramAuth,
    Path((id, link)): Path<(i64, String)>,
    request: Request,
) -> Result<Response, AppError> {
    segment_of(&state, &program, id, &link).await?;
    let path = state
        .storage
        .as_ref()
        .and_then(|storage| storage.path(id, &link, Kind::Video))
        .ok_or(AppError::NotFound)?;
    log_view(&state, id, &program, &format!("video:{link}")).await?;
    // ServeFile answers range requests, so a player can seek
    let Ok(response) =
        ServeFile::new_with_mime(path, &"video/mp4".parse().expect("a valid mime type"))
            .oneshot(request)
            .await;
    Ok(response.into_response())
}

async fn timeline(
    State(state): State<AppState>,
    ProgramAuth(program): ProgramAuth,
    Path((id, link)): Path<(i64, String)>,
) -> Result<Response, AppError> {
    segment_of(&state, &program, id, &link).await?;
    let bytes = state
        .storage
        .as_ref()
        .ok_or(AppError::NotFound)?
        .read(id, &link, Kind::Timeline)
        .await
        .map_err(|_| AppError::NotFound)?;
    log_view(&state, id, &program, &format!("timeline:{link}")).await?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        bytes,
    )
        .into_response())
}
