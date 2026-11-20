//! Submitting hours to a program. The flow, with what each step proves:
//!
//! 1. While recording, a computer sends each segment's hash-chain link here
//!    (`POST /api/chain`). The server stamps the time it arrived.
//! 2. `POST /api/submissions` names the segments to submit. The server only
//!    accepts links it already holds for this person, and lists every other
//!    segment inside the same time range as omitted, so leaving something out
//!    is visible rather than silent.
//! 3. Each video and timeline is uploaded and must hash to exactly what was
//!    committed in step 1, so nothing could be made up or changed afterwards.
//! 4. `POST .../finalize` checks everything arrived and freezes the totals.
//!
//! A segment's hours can be submitted only once.

use std::collections::BTreeSet;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use pulse_core::chain::{self, Second};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use crate::auth::{SessionUser, TokenUser};
use crate::error::{AppError, is_unique_violation};
use crate::evidence::{self, Segment};
use crate::now;
use crate::state::AppState;
use crate::storage::{Kind, Storage, is_hex_hash};

/// A 5-minute segment of 1 frame per second video is a few MB; 64 MiB leaves
/// room for a very busy screen.
const VIDEO_LIMIT: usize = 64 << 20;
const MAX_LINKS_PER_REQUEST: usize = 200;
const MAX_SEGMENTS_PER_SUBMISSION: usize = 5000;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/chain", post(upload_chain))
        .route("/api/chain/used", get(used_links))
        .route("/api/recordings", get(recordings_summary))
        .route("/api/programs", get(list_programs))
        .route("/api/submissions", get(list_mine).post(create))
        .route("/api/submissions/{id}", delete(withdraw))
        .route(
            "/api/submissions/{id}/segments/{link}/video",
            put(put_video).layer(DefaultBodyLimit::max(VIDEO_LIMIT)),
        )
        .route(
            "/api/submissions/{id}/segments/{link}/timeline",
            put(put_timeline),
        )
        .route("/api/submissions/{id}/finalize", post(finalize))
}

fn storage(state: &AppState) -> Result<&Storage, AppError> {
    state
        .storage
        .as_ref()
        .ok_or_else(|| AppError::Unavailable("submissions are not set up on this server".into()))
}

#[derive(Deserialize)]
struct ChainBody {
    links: Vec<LinkIn>,
}

#[derive(Deserialize)]
struct LinkIn {
    start: i64,
    end: i64,
    previous: String,
    video: String,
    timeline: String,
    link: String,
}

fn check_link(link: &LinkIn, now: i64) -> Result<(), &'static str> {
    if ![&link.previous, &link.video, &link.timeline, &link.link]
        .iter()
        .all(|hash| is_hex_hash(hash))
    {
        return Err("hashes must be 64 lowercase hex digits");
    }
    // 1_600_000_000 is September 2020, before this existed
    if link.start < 1_600_000_000 || link.end <= link.start || link.end - link.start > 600 {
        return Err("a segment is between 1 second and 10 minutes long");
    }
    if link.end > now + 60 {
        return Err("segment ends in the future");
    }
    let hash = |text: &str| chain::from_hex(text).expect("checked above");
    let expected = chain::link(
        &hash(&link.previous),
        link.start,
        link.end,
        &hash(&link.video),
        &hash(&link.timeline),
    );
    if chain::to_hex(&expected) != link.link {
        return Err("the link hash does not match its parts");
    }
    Ok(())
}

async fn upload_chain(
    State(state): State<AppState>,
    TokenUser { user, token_id }: TokenUser,
    Json(body): Json<ChainBody>,
) -> Result<Json<Value>, AppError> {
    if body.links.len() > MAX_LINKS_PER_REQUEST {
        return Err(AppError::BadRequest(format!(
            "send at most {MAX_LINKS_PER_REQUEST} links at a time"
        )));
    }
    let received_at = now();
    let (mut accepted, mut rejected) = (0, Vec::new());
    for (index, link) in body.links.iter().enumerate() {
        if let Err(reason) = check_link(link, received_at) {
            rejected.push(json!({ "index": index, "reason": reason }));
            continue;
        }
        let inserted = sqlx::query(
            "insert into chain_links (link, user_id, token_id, previous, start_at, end_at, video, timeline, received_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict (link) do nothing",
        )
        .bind(&link.link)
        .bind(user.id)
        .bind(token_id)
        .bind(&link.previous)
        .bind(link.start)
        .bind(link.end)
        .bind(&link.video)
        .bind(&link.timeline)
        .bind(received_at)
        .execute(&state.db)
        .await?
        .rows_affected();
        if inserted == 1 {
            accepted += 1;
            continue;
        }
        // sending the same link again is fine; someone else's link is not
        let owner: i64 = sqlx::query_scalar("select user_id from chain_links where link = $1")
            .bind(&link.link)
            .fetch_one(&state.db)
            .await?;
        if owner == user.id {
            accepted += 1;
        } else {
            rejected
                .push(json!({ "index": index, "reason": "already registered by another account" }));
        }
    }
    Ok(Json(json!({ "accepted": accepted, "rejected": rejected })))
}

/// Segments whose hours are already part of a submitted submission.
async fn used_links(
    State(state): State<AppState>,
    TokenUser { user, .. }: TokenUser,
) -> Result<Json<Value>, AppError> {
    let used: Vec<String> = sqlx::query_scalar(
        "select ss.link from submission_segments ss join submissions s on s.id = ss.submission_id \
         where s.user_id = $1 and s.status = 'submitted' and not ss.omitted order by ss.link",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "used": used })))
}

/// What the server holds of a person's recordings, by UTC day, for the
/// website's submit page: how much was committed and how much of it is
/// already part of a submitted submission. The server never knows which
/// project a segment belongs to (the computer only sends hashes), so this is
/// not split by project.
async fn recordings_summary(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "select cl.start_at / 86400 as day, count(*) as segments, \
         sum(cl.end_at - cl.start_at)::bigint as seconds, \
         sum(case when exists (select 1 from submission_segments ss join submissions s on s.id = ss.submission_id \
             where ss.link = cl.link and not ss.omitted and s.status = 'submitted') \
             then cl.end_at - cl.start_at else 0 end)::bigint as submitted_seconds \
         from chain_links cl where cl.user_id = $1 group by 1 order by 1 desc limit 60",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let days: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "date": crate::stats::date_string(row.get::<i64, _>("day")),
                "segments": row.get::<i64, _>("segments"),
                "seconds": row.get::<i64, _>("seconds"),
                "submitted_seconds": row.get::<i64, _>("submitted_seconds"),
            })
        })
        .collect();
    let sum = |key: &str| rows.iter().map(|row| row.get::<i64, _>(key)).sum::<i64>();
    Ok(Json(json!({
        "seconds": sum("seconds"),
        "submitted_seconds": sum("submitted_seconds"),
        "days": days,
    })))
}

async fn list_programs(State(state): State<AppState>) -> Json<Value> {
    let names: Vec<&str> = state
        .config
        .program_keys
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    Json(json!({ "programs": names, "enabled": state.storage.is_some() }))
}

#[derive(Deserialize)]
struct CreateBody {
    program: String,
    project: String,
    /// The segments to submit. Every other segment inside their time range is
    /// listed as omitted by the server itself.
    links: Vec<String>,
}

async fn create(
    State(state): State<AppState>,
    TokenUser { user, .. }: TokenUser,
    Json(body): Json<CreateBody>,
) -> Result<Json<Value>, AppError> {
    let storage = storage(&state)?;
    if !state
        .config
        .program_keys
        .iter()
        .any(|(name, _)| *name == body.program)
    {
        return Err(AppError::BadRequest(
            "that program does not take submissions from this server".into(),
        ));
    }
    let wanted: BTreeSet<&String> = body.links.iter().collect();
    if wanted.is_empty()
        || wanted.len() > MAX_SEGMENTS_PER_SUBMISSION
        || wanted.len() != body.links.len()
    {
        return Err(AppError::BadRequest(format!(
            "submit between 1 and {MAX_SEGMENTS_PER_SUBMISSION} different segments"
        )));
    }
    if !body.links.iter().all(|link| is_hex_hash(link)) {
        return Err(AppError::BadRequest(
            "segments are named by their 64 digit link hash".into(),
        ));
    }
    let project_id: i64 =
        sqlx::query_scalar("select id from projects where user_id = $1 and key = $2")
            .bind(user.id)
            .bind(&body.project)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| {
                AppError::BadRequest("no time has been logged for that project yet".into())
            })?;

    let mut tx = state.db.begin().await?;
    // a person has one submission in progress at a time; an earlier attempt that never finished is dropped
    let stale: Vec<i64> = sqlx::query_scalar(
        "delete from submissions where user_id = $1 and status = 'uploading' returning id",
    )
    .bind(user.id)
    .fetch_all(&mut *tx)
    .await?;

    let known = sqlx::query(
        "select start_at, end_at from chain_links where user_id = $1 and link = any($2)",
    )
    .bind(user.id)
    .bind(&body.links)
    .fetch_all(&mut *tx)
    .await?;
    if known.len() != body.links.len() {
        return Err(AppError::BadRequest(
            "some of those segments were never registered with this server; run `pulse sync` first"
                .into(),
        ));
    }
    let first = known
        .iter()
        .map(|row| row.get::<i64, _>("start_at"))
        .min()
        .expect("not empty");
    let last = known
        .iter()
        .map(|row| row.get::<i64, _>("end_at"))
        .max()
        .expect("not empty");
    // everything recorded inside the range, submitted or not
    let in_range: Vec<String> = sqlx::query_scalar(
        "select link from chain_links where user_id = $1 and start_at < $3 and end_at > $2 order by start_at",
    )
    .bind(user.id)
    .bind(first)
    .bind(last)
    .fetch_all(&mut *tx)
    .await?;

    let now = now();
    let id: i64 = sqlx::query_scalar(
        "insert into submissions (user_id, project_id, program, status, created_at, changed_at) \
         values ($1, $2, $3, 'uploading', $4, $4) returning id",
    )
    .bind(user.id)
    .bind(project_id)
    .bind(&body.program)
    .bind(now)
    .fetch_one(&mut *tx)
    .await?;
    let mut links = in_range;
    links.extend(body.links.iter().cloned());
    links.sort();
    links.dedup();
    let omitted: Vec<bool> = links.iter().map(|link| !wanted.contains(link)).collect();
    let inserted = sqlx::query(
        "insert into submission_segments (submission_id, link, omitted) \
         select $1, l, o from unnest($2::text[], $3::bool[]) as t(l, o)",
    )
    .bind(id)
    .bind(&links)
    .bind(&omitted)
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => {
            return Err(AppError::Conflict(
                "some of those segments are already part of another submission".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    tx.commit().await?;
    for old in stale {
        let _ = storage.remove_submission(old).await;
    }
    Ok(Json(
        json!({ "id": id, "segments": body.links, "omitted": omitted.iter().filter(|o| **o).count() }),
    ))
}

struct Committed {
    video: String,
    timeline: String,
    start: i64,
    end: i64,
}

/// The committed hashes of a segment the caller may still upload to.
async fn open_for_upload(
    state: &AppState,
    user_id: i64,
    id: i64,
    link: &str,
) -> Result<Committed, AppError> {
    if !is_hex_hash(link) {
        return Err(AppError::NotFound);
    }
    let row = sqlx::query(
        "select cl.video, cl.timeline, cl.start_at, cl.end_at from submissions s \
         join submission_segments ss on ss.submission_id = s.id join chain_links cl on cl.link = ss.link \
         where s.id = $1 and s.user_id = $2 and s.status = 'uploading' and ss.link = $3 and not ss.omitted",
    )
    .bind(id)
    .bind(user_id)
    .bind(link)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Committed {
        video: row.get("video"),
        timeline: row.get("timeline"),
        start: row.get("start_at"),
        end: row.get("end_at"),
    })
}

async fn put_video(
    State(state): State<AppState>,
    TokenUser { user, .. }: TokenUser,
    Path((id, link)): Path<(i64, String)>,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let storage = storage(&state)?;
    let committed = open_for_upload(&state, user.id, id, &link).await?;
    // an MP4 file starts with a box size, then the letters ftyp
    if body.len() < 12 || &body[4..8] != b"ftyp" {
        return Err(AppError::BadRequest("that is not an MP4 video".into()));
    }
    if chain::to_hex(&chain::sha256(&body)) != committed.video {
        return Err(AppError::BadRequest(
            "this video is not the one that was committed when it was recorded".into(),
        ));
    }
    storage
        .put(id, &link, Kind::Video, &body)
        .await
        .map_err(|error| {
            tracing::error!("cannot store a video: {error}");
            AppError::Internal
        })?;
    sqlx::query(
        "update submission_segments set video_ok = true where submission_id = $1 and link = $2",
    )
    .bind(id)
    .bind(&link)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, serde::Serialize)]
pub struct TimelineFile {
    /// `[t, change, input]` per recorded second.
    pub seconds: Vec<(i64, u16, u8)>,
}

pub fn to_seconds(file: &TimelineFile) -> Vec<Second> {
    file.seconds
        .iter()
        .map(|(t, change, input)| Second {
            t: *t,
            change: *change,
            input: *input != 0,
        })
        .collect()
}

async fn put_timeline(
    State(state): State<AppState>,
    TokenUser { user, .. }: TokenUser,
    Path((id, link)): Path<(i64, String)>,
    Json(file): Json<TimelineFile>,
) -> Result<StatusCode, AppError> {
    let storage = storage(&state)?;
    let committed = open_for_upload(&state, user.id, id, &link).await?;
    let sorted = file.seconds.windows(2).all(|pair| pair[0].0 < pair[1].0);
    let inside = file.seconds.iter().all(|(t, change, input)| {
        (committed.start..committed.end).contains(t) && *change <= 1000 && *input <= 1
    });
    if file.seconds.is_empty() || file.seconds.len() > 700 || !sorted || !inside {
        return Err(AppError::BadRequest(
            "that timeline does not fit its segment".into(),
        ));
    }
    if chain::to_hex(&chain::timeline_hash(&to_seconds(&file))) != committed.timeline {
        return Err(AppError::BadRequest(
            "this timeline is not the one that was committed when it was recorded".into(),
        ));
    }
    let bytes = serde_json::to_vec(&file).map_err(|_| AppError::Internal)?;
    storage
        .put(id, &link, Kind::Timeline, &bytes)
        .await
        .map_err(|error| {
            tracing::error!("cannot store a timeline: {error}");
            AppError::Internal
        })?;
    sqlx::query(
        "update submission_segments set timeline_ok = true where submission_id = $1 and link = $2",
    )
    .bind(id)
    .bind(&link)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub struct Loaded {
    pub link: String,
    pub start: i64,
    pub end: i64,
    pub omitted: bool,
    pub received_at: i64,
    pub video_ok: bool,
    pub timeline_ok: bool,
    pub seconds: Vec<Second>,
}

impl Loaded {
    pub fn as_segment(&self) -> Segment {
        Segment {
            start: self.start,
            end: self.end,
            omitted: self.omitted,
            seconds: self.seconds.clone(),
        }
    }
}

/// Every segment of a submission in time order, with the timelines of the
/// submitted ones read from storage.
pub async fn load_segments(state: &AppState, id: i64) -> Result<Vec<Loaded>, AppError> {
    let storage = storage(state)?;
    let rows = sqlx::query(
        "select ss.link, ss.omitted, ss.video_ok, ss.timeline_ok, cl.start_at, cl.end_at, cl.received_at \
         from submission_segments ss join chain_links cl on cl.link = ss.link \
         where ss.submission_id = $1 order by cl.start_at, ss.link",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    let mut loaded = Vec::new();
    for row in rows {
        let (link, omitted): (String, bool) = (row.get("link"), row.get("omitted"));
        let timeline_ok: bool = row.get("timeline_ok");
        let seconds = if omitted || !timeline_ok {
            Vec::new()
        } else {
            let bytes = storage
                .read(id, &link, Kind::Timeline)
                .await
                .map_err(|error| {
                    tracing::error!("cannot read a timeline: {error}");
                    AppError::Internal
                })?;
            to_seconds(&serde_json::from_slice(&bytes).map_err(|_| AppError::Internal)?)
        };
        loaded.push(Loaded {
            link,
            start: row.get("start_at"),
            end: row.get("end_at"),
            omitted,
            received_at: row.get("received_at"),
            video_ok: row.get("video_ok"),
            timeline_ok,
            seconds,
        });
    }
    Ok(loaded)
}

/// What a person says they worked on a project in a time range: the credited
/// seconds their profile shows for it.
pub async fn claimed_seconds(
    state: &AppState,
    user_id: i64,
    project_id: i64,
    start: i64,
    end: i64,
) -> Result<i64, AppError> {
    Ok(sqlx::query_scalar(
        "select coalesce(sum(seconds), 0)::bigint from credits \
         where user_id = $1 and project_id = $2 and slot < $4 and slot + 300 > $3",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(start)
    .bind(end)
    .fetch_one(&state.db)
    .await?)
}

async fn finalize(
    State(state): State<AppState>,
    TokenUser { user, .. }: TokenUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let row = sqlx::query("select project_id from submissions where id = $1 and user_id = $2 and status = 'uploading'")
        .bind(id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let project_id: i64 = row.get("project_id");
    let loaded = load_segments(&state, id).await?;
    let missing = loaded
        .iter()
        .filter(|segment| !segment.omitted && !(segment.video_ok && segment.timeline_ok))
        .count();
    if missing > 0 {
        return Err(AppError::Conflict(format!(
            "{missing} segments have not been uploaded completely yet"
        )));
    }
    let submitted: Vec<&Loaded> = loaded.iter().filter(|segment| !segment.omitted).collect();
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
    let segments: Vec<Segment> = loaded.iter().map(Loaded::as_segment).collect();
    let analysis = evidence::analyze(&segments);
    let claimed = claimed_seconds(&state, user.id, project_id, first, last).await?;
    let now = now();
    sqlx::query(
        "update submissions set status = 'submitted', submitted_at = $2, changed_at = $2, recorded_seconds = $3, \
         active_seconds = $4, omitted_seconds = $5, claimed_seconds = $6 where id = $1",
    )
    .bind(id)
    .bind(now)
    .bind(analysis.recorded_seconds)
    .bind(analysis.active_seconds)
    .bind(analysis.omitted_seconds)
    .bind(claimed)
    .execute(&state.db)
    .await?;
    Ok(Json(json!({
        "id": id,
        "recorded_seconds": analysis.recorded_seconds,
        "active_seconds": analysis.active_seconds,
        "omitted_seconds": analysis.omitted_seconds,
        "claimed_seconds": claimed,
    })))
}

/// Withdraws a submission: its recordings are deleted and its segments can be
/// submitted again. The program is told through its status, and the record that
/// it was looked at stays.
async fn withdraw(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let storage = storage(&state)?;
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query(
        "update submissions set status = 'withdrawn', changed_at = $3 where id = $1 and user_id = $2 and status <> 'withdrawn'",
    )
    .bind(id)
    .bind(user.id)
    .bind(now())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 0 {
        let exists: Option<i64> =
            sqlx::query_scalar("select id from submissions where id = $1 and user_id = $2")
                .bind(id)
                .bind(user.id)
                .fetch_optional(&mut *tx)
                .await?;
        return if exists.is_some() {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(AppError::NotFound)
        };
    }
    sqlx::query("delete from submission_segments where submission_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    storage.remove_submission(id).await.map_err(|error| {
        tracing::error!("cannot delete a withdrawn submission's files: {error}");
        AppError::Internal
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_mine(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "select s.id, s.program, s.status, s.submitted_at, s.recorded_seconds, s.active_seconds, \
         s.omitted_seconds, s.claimed_seconds, p.key \
         from submissions s join projects p on p.id = s.project_id \
         where s.user_id = $1 and s.status <> 'uploading' order by s.id desc limit 50",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.get("id")).collect();
    let views = sqlx::query("select submission_id, program, what, at from evidence_views where submission_id = any($1) order by at desc, id desc")
        .bind(&ids)
        .fetch_all(&state.db)
        .await?;
    let submissions: Vec<Value> = rows
        .iter()
        .map(|row| {
            let id: i64 = row.get("id");
            let seen: Vec<Value> = views
                .iter()
                .filter(|view| view.get::<i64, _>("submission_id") == id)
                .take(20)
                .map(|view| json!({ "program": view.get::<String, _>("program"), "what": view.get::<String, _>("what"), "at": view.get::<i64, _>("at") }))
                .collect();
            json!({
                "id": id,
                "program": row.get::<String, _>("program"),
                "project": row.get::<String, _>("key"),
                "status": row.get::<String, _>("status"),
                "submitted_at": row.get::<Option<i64>, _>("submitted_at"),
                "recorded_seconds": row.get::<Option<i64>, _>("recorded_seconds"),
                "active_seconds": row.get::<Option<i64>, _>("active_seconds"),
                "omitted_seconds": row.get::<Option<i64>, _>("omitted_seconds"),
                "claimed_seconds": row.get::<Option<i64>, _>("claimed_seconds"),
                "views": seen,
            })
        })
        .collect();
    Ok(Json(json!({ "submissions": submissions })))
}
