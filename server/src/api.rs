use std::collections::{BTreeMap, BTreeSet};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;

use crate::auth::{
    OptionalUser, SessionUser, TokenUser, User, hash_token, random_token, user_from_row,
};
use crate::error::AppError;
use crate::now;
use crate::state::{AppState, CreditEvent};
use crate::stats::{self, DAY};
use crate::user_columns;

const SLOT: i64 = 300;
/// A computer that was offline can send time it logged up to a week ago: 7 * 24 * 60 * 60.
const BACKFILL_SECONDS: i64 = 604_800;
const MAX_PROJECTS: i64 = 500;
const MAX_TOKENS: i64 = 20;
const MAX_CREDITS_PER_REQUEST: usize = 300;
/// How long the front page's numbers are reused: 60 seconds.
const SITE_STATS_TTL: std::time::Duration = std::time::Duration::from_secs(60);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me", get(me).patch(update_me))
        .route("/api/tokens", get(list_tokens).post(create_token))
        .route("/api/tokens/{id}", delete(revoke_token))
        .route("/api/projects/{id}", patch(update_project))
        .route("/api/credits", post(ingest))
        .route("/api/users/{username}", get(profile))
        .route("/api/stats", get(site_stats))
}

async fn me(State(state): State<AppState>, OptionalUser(user): OptionalUser) -> Json<Value> {
    let github = &state.config.github;
    Json(json!({
        "user": user.map(|user| json!({
            "username": user.username,
            "display_name": user.display_name,
            "profile_public": user.profile_public,
            "github_login": user.github_login,
        })),
        "github": {
            "can_connect": github.connect_enabled(),
            "install_url": github.install_url(),
        },
    }))
}

#[derive(Deserialize)]
struct UpdateMe {
    profile_public: bool,
}

async fn update_me(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Json(body): Json<UpdateMe>,
) -> Result<StatusCode, AppError> {
    sqlx::query("update users set profile_public = $1 where id = $2")
        .bind(body.profile_public)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

async fn create_token(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Json(body): Json<NewToken>,
) -> Result<Json<Value>, AppError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 60 || has_control(name) {
        return Err(AppError::BadRequest(
            "name the computer in 1 to 60 characters".into(),
        ));
    }
    let count: i64 = sqlx::query_scalar("select count(*) from api_tokens where user_id = $1")
        .bind(user.id)
        .fetch_one(&state.db)
        .await?;
    if count >= MAX_TOKENS {
        return Err(AppError::Conflict(
            "revoke a token before making another".into(),
        ));
    }
    let token = random_token("pulse_");
    let id: i64 = sqlx::query_scalar(
        "insert into api_tokens (user_id, token_hash, name) values ($1, $2, $3) returning id",
    )
    .bind(user.id)
    .bind(hash_token(&token))
    .bind(name)
    .fetch_one(&state.db)
    .await?;
    // the token is only ever shown here
    Ok(Json(json!({ "id": id, "name": name, "token": token })))
}

async fn list_tokens(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
) -> Result<Json<Value>, AppError> {
    let rows = sqlx::query(
        "select id, name, created_at, last_used_at from api_tokens where user_id = $1 order by id",
    )
    .bind(user.id)
    .fetch_all(&state.db)
    .await?;
    let tokens: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<i64, _>("id"),
                "name": row.get::<String, _>("name"),
                "created_at": row.get::<i64, _>("created_at"),
                "last_used_at": row.get::<Option<i64>, _>("last_used_at"),
            })
        })
        .collect();
    Ok(Json(json!({ "tokens": tokens })))
}

async fn revoke_token(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    let result = sqlx::query("delete from api_tokens where id = $1 and user_id = $2")
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct UpdateProject {
    public: bool,
}

async fn update_project(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    Path(id): Path<i64>,
    Json(body): Json<UpdateProject>,
) -> Result<StatusCode, AppError> {
    let result = sqlx::query("update projects set public = $1 where id = $2 and user_id = $3")
        .bind(body.public)
        .bind(id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct IngestBody {
    credits: Vec<CreditIn>,
}

#[derive(Deserialize)]
struct CreditIn {
    project: String,
    /// Start of a 5-minute slot, unix seconds.
    slot: i64,
    /// Seconds of that slot spent on the project. A client sends the same slot
    /// again as it grows, and the newest numbers replace the old ones.
    seconds: i32,
    #[serde(default)]
    prompts: i32,
    #[serde(default)]
    sources: Vec<String>,
    #[serde(default)]
    models: Vec<String>,
}

fn clean_label(text: &str) -> bool {
    !text.is_empty() && text.chars().count() <= 64 && !has_control(text)
}

fn validate(credit: &CreditIn, now: i64) -> Result<(), String> {
    let project = credit.project.trim();
    if project.is_empty() || project.chars().count() > 200 || has_control(project) {
        return Err("project must be 1 to 200 characters".into());
    }
    if credit.slot % SLOT != 0 {
        return Err("slot must start on a 5 minute boundary".into());
    }
    if credit.slot > now || credit.slot < now - BACKFILL_SECONDS {
        return Err("slot is in the future or more than a week old".into());
    }
    if !(1..=300).contains(&credit.seconds) {
        return Err("seconds must be 1 to 300".into());
    }
    // nobody can have worked in a second that has not happened yet; 5 seconds allows for clock drift
    if credit.slot + i64::from(credit.seconds) > now + 5 {
        return Err("more seconds than have passed in that slot".into());
    }
    if !(0..=1000).contains(&credit.prompts) {
        return Err("prompts must be 0 to 1000".into());
    }
    for list in [&credit.sources, &credit.models] {
        if list.len() > 8 || !list.iter().all(|label| clean_label(label)) {
            return Err("sources and models take up to 8 short names each".into());
        }
    }
    Ok(())
}

async fn ingest(
    State(state): State<AppState>,
    TokenUser { user, token_id }: TokenUser,
    Json(body): Json<IngestBody>,
) -> Result<Json<Value>, AppError> {
    if body.credits.len() > MAX_CREDITS_PER_REQUEST {
        return Err(AppError::BadRequest(format!(
            "send at most {MAX_CREDITS_PER_REQUEST} credits at a time"
        )));
    }
    let now = now();
    let mut tx = state.db.begin().await?;
    // one ingest per person at a time, so the cap below cannot be raced past
    sqlx::query("select 1 from users where id = $1 for update")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;

    let (mut accepted, mut clamped) = (0, 0);
    let mut rejected = Vec::new();
    let mut events = Vec::new();
    for (index, credit) in body.credits.iter().enumerate() {
        if let Err(reason) = validate(credit, now) {
            rejected.push(json!({ "index": index, "reason": reason }));
            continue;
        }
        let key = credit.project.trim();
        let existing =
            sqlx::query("select id, public from projects where user_id = $1 and key = $2")
                .bind(user.id)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await?;
        let (project_id, project_public) = match existing {
            Some(row) => (row.get::<i64, _>("id"), row.get::<bool, _>("public")),
            None => {
                let count: i64 =
                    sqlx::query_scalar("select count(*) from projects where user_id = $1")
                        .bind(user.id)
                        .fetch_one(&mut *tx)
                        .await?;
                if count >= MAX_PROJECTS {
                    rejected.push(json!({ "index": index, "reason": "too many projects" }));
                    continue;
                }
                // names stay hidden until the person makes a project public: Pulse cannot tell a private repo from a public one
                let id: i64 = sqlx::query_scalar("insert into projects (user_id, key, public) values ($1, $2, false) returning id")
                    .bind(user.id)
                    .bind(key)
                    .fetch_one(&mut *tx)
                    .await?;
                (id, false)
            }
        };

        // one person has 300 seconds in a 5 minute slot, however many computers or projects they log from
        let others: i64 = sqlx::query_scalar(
            "select coalesce(sum(seconds), 0)::bigint from credits \
             where user_id = $1 and slot = $2 and not (project_id = $3 and token_id = $4)",
        )
        .bind(user.id)
        .bind(credit.slot)
        .bind(project_id)
        .bind(token_id)
        .fetch_one(&mut *tx)
        .await?;
        let allowed = (SLOT - others).min(i64::from(credit.seconds));
        if allowed <= 0 {
            clamped += 1;
            continue;
        }
        if allowed < i64::from(credit.seconds) {
            clamped += 1;
        }
        let previous: Option<i32> = sqlx::query_scalar(
            "select seconds from credits where user_id = $1 and slot = $2 and project_id = $3 and token_id = $4",
        )
        .bind(user.id)
        .bind(credit.slot)
        .bind(project_id)
        .bind(token_id)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query(
            "insert into credits (user_id, slot, project_id, token_id, seconds, prompts, sources, models, updated_at) \
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             on conflict (user_id, slot, project_id, token_id) do update set \
             seconds = excluded.seconds, prompts = excluded.prompts, sources = excluded.sources, \
             models = excluded.models, updated_at = excluded.updated_at",
        )
        .bind(user.id)
        .bind(credit.slot)
        .bind(project_id)
        .bind(token_id)
        .bind(allowed as i32)
        .bind(credit.prompts)
        .bind(&credit.sources)
        .bind(&credit.models)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        accepted += 1;

        let added = allowed as i32 - previous.unwrap_or(0);
        if added > 0 {
            events.push(CreditEvent {
                user_id: user.id,
                project: None,
                project_key: key.to_owned(),
                project_public,
                profile_public: user.profile_public,
                slot: credit.slot,
                seconds_added: added,
                at: now,
            });
        }
    }
    sqlx::query("update api_tokens set last_used_at = $1 where id = $2")
        .bind(now)
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // sent only after the time is saved, so a dashboard that refetches sees it
    for event in events {
        let _ = state.live.send(event);
    }
    Ok(Json(
        json!({ "accepted": accepted, "clamped": clamped, "rejected": rejected }),
    ))
}

pub async fn find_user(state: &AppState, username: &str) -> Result<User, AppError> {
    let row = sqlx::query(concat!(
        "select ",
        user_columns!(),
        " from users u where u.username = $1"
    ))
    .bind(username)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(user_from_row(&row))
}

/// A private profile looks like no profile to anyone but its owner.
pub fn can_view(profile: &User, viewer: &Option<User>) -> bool {
    profile.profile_public
        || viewer
            .as_ref()
            .is_some_and(|viewer| viewer.id == profile.id)
}

async fn used(
    state: &AppState,
    sql: &'static str,
    user_id: i64,
) -> Result<BTreeSet<String>, AppError> {
    let rows = sqlx::query(sql).bind(user_id).fetch_all(&state.db).await?;
    Ok(rows
        .iter()
        .map(|row| row.get::<String, _>("label"))
        .collect())
}

async fn profile(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path(username): Path<String>,
) -> Result<Json<Value>, AppError> {
    let target = find_user(&state, &username).await?;
    if !can_view(&target, &viewer) {
        return Err(AppError::NotFound);
    }
    let is_owner = viewer.as_ref().is_some_and(|viewer| viewer.id == target.id);
    let now = now();
    let today = now / DAY;

    let daily: BTreeMap<i64, i64> = sqlx::query("select slot / 86400 as day, sum(seconds)::bigint as seconds from credits where user_id = $1 group by 1")
        .bind(target.id)
        .fetch_all(&state.db)
        .await?
        .iter()
        .map(|row| (row.get::<i64, _>("day"), row.get::<i64, _>("seconds")))
        .collect();
    let streaks = stats::streaks(&daily, today);
    let days: Vec<Value> = (today - 29..=today)
        .map(|day| json!({ "date": stats::date_string(day), "seconds": daily.get(&day).copied().unwrap_or(0) }))
        .collect();

    // pushes are matched to a project by repository name, ignoring case
    let projects: Vec<Value> = sqlx::query(
        "select p.id, p.key, p.public, sum(c.seconds)::bigint as seconds, \
         (select max(coalesce(pu.pushed_at, pu.received_at)) from pushes pu \
            where pu.user_id = p.user_id and 'github.com/' || pu.repo = lower(p.key)) as last_push_at, \
         (select count(*) from pushes pu \
            where pu.user_id = p.user_id and 'github.com/' || pu.repo = lower(p.key))::bigint as pushes \
         from credits c join projects p on p.id = c.project_id \
         where c.user_id = $1 group by p.id order by seconds desc, p.id",
    )
    .bind(target.id)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|row| {
        let public: bool = row.get("public");
        let shown = is_owner || public;
        let key: String = row.get("key");
        // a repository's link and push times are as private as its name
        json!({
            "id": is_owner.then(|| row.get::<i64, _>("id")),
            "name": shown.then(|| key.clone()),
            "url": if shown { crate::github::repo_url(&key) } else { None },
            "public": public,
            "seconds": row.get::<i64, _>("seconds"),
            "last_push_at": if shown { row.get::<Option<i64>, _>("last_push_at") } else { None },
            "pushes": if shown { row.get::<i64, _>("pushes") } else { 0 },
        })
    })
    .collect();

    let harnesses = used(&state, "select distinct label from credits, unnest(sources) as label where user_id = $1 order by label", target.id).await?;
    let models = used(&state, "select distinct label from credits, unnest(models) as label where user_id = $1 order by label", target.id).await?;

    // "building now" means some time was logged in the last 2 minutes: 2 * 60
    let building = sqlx::query(
        "select p.key, p.public from credits c join projects p on p.id = c.project_id \
         where c.user_id = $1 and c.updated_at > $2 order by c.updated_at desc limit 1",
    )
    .bind(target.id)
    .bind(now - 120)
    .fetch_optional(&state.db)
    .await?;

    Ok(Json(json!({
        "username": target.username,
        "display_name": target.display_name,
        "is_owner": is_owner,
        "profile_public": target.profile_public,
        "total_seconds": daily.values().sum::<i64>(),
        "today_seconds": daily.get(&today).copied().unwrap_or(0),
        "current_streak": streaks.current,
        "longest_streak": streaks.longest,
        "days": days,
        "projects": projects,
        "harnesses": harnesses,
        "models": models,
        "building_now": building.is_some(),
        "building_project": building.as_ref().and_then(|row| {
            (is_owner || row.get::<bool, _>("public")).then(|| row.get::<String, _>("key"))
        }),
    })))
}

/// Numbers for the front page: everyone's time added together. Only people with
/// a public profile count, so a private profile stays as absent here as it is
/// everywhere else, and nothing in the reply names a person or a project.
async fn site_stats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let mut cached = state.site_stats.lock().await;
    if let Some((counted_at, stats)) = cached.as_ref()
        && counted_at.elapsed() < SITE_STATS_TTL
    {
        return Ok(Json(stats.clone()));
    }
    let now = now();
    let today = now / DAY;

    let totals = sqlx::query(
        "select coalesce(sum(c.seconds), 0)::bigint as seconds, \
                count(distinct c.user_id)::bigint as builders, \
                count(distinct c.user_id) filter (where c.slot >= $1)::bigint as builders_week, \
                count(distinct c.user_id) filter (where c.updated_at > $2)::bigint as building_now, \
                count(distinct c.project_id)::bigint as projects \
         from credits c join users u on u.id = c.user_id where u.profile_public",
    )
    .bind((today - 6) * DAY)
    .bind(now - 120) // "building now": time was logged in the last 2 minutes
    .fetch_one(&state.db)
    .await?;

    let daily: BTreeMap<i64, i64> = sqlx::query(
        "select c.slot / 86400 as day, sum(c.seconds)::bigint as seconds \
         from credits c join users u on u.id = c.user_id \
         where u.profile_public and c.slot >= $1 group by 1",
    )
    .bind((today - 29) * DAY)
    .fetch_all(&state.db)
    .await?
    .iter()
    .map(|row| (row.get::<i64, _>("day"), row.get::<i64, _>("seconds")))
    .collect();
    let days: Vec<Value> = (today - 29..=today)
        .map(|day| json!({ "date": stats::date_string(day), "seconds": daily.get(&day).copied().unwrap_or(0) }))
        .collect();
    let week_seconds: i64 = (today - 6..=today).filter_map(|day| daily.get(&day)).sum();

    // how many people used each agent and model, most used first
    let tools = |column: &'static str| {
        let state = state.clone();
        async move {
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "select label, count(distinct c.user_id)::bigint as builders \
                 from credits c join users u on u.id = c.user_id, unnest(c.{column}) as label \
                 where u.profile_public group by label order by builders desc, label limit 8"
            )))
            .fetch_all(&state.db)
            .await?;
            Ok::<_, AppError>(
                rows.iter()
                    .map(|row| json!({ "name": row.get::<String, _>("label"), "builders": row.get::<i64, _>("builders") }))
                    .collect::<Vec<Value>>(),
            )
        }
    };

    let stats = json!({
        "total_seconds": totals.get::<i64, _>("seconds"),
        "week_seconds": week_seconds,
        "today_seconds": daily.get(&today).copied().unwrap_or(0),
        "builders": totals.get::<i64, _>("builders"),
        "builders_week": totals.get::<i64, _>("builders_week"),
        "building_now": totals.get::<i64, _>("building_now"),
        "projects": totals.get::<i64, _>("projects"),
        "days": days,
        "harnesses": tools("sources").await?,
        "models": tools("models").await?,
    });
    *cached = Some((std::time::Instant::now(), stats.clone()));
    Ok(Json(stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credit(slot: i64, seconds: i32) -> CreditIn {
        CreditIn {
            project: "github.com/o/r".into(),
            slot,
            seconds,
            prompts: 0,
            sources: vec![],
            models: vec![],
        }
    }

    #[test]
    fn rejects_time_that_cannot_have_happened() {
        let slot = 1_000_000_200; // a multiple of 300
        let now = slot + 100; // 100 seconds into the slot
        assert!(validate(&credit(slot - 300, 300), now).is_ok());
        // 100 seconds have passed; 5 more are allowed for clock drift
        assert!(validate(&credit(slot, 105), now).is_ok());
        assert!(validate(&credit(slot, 106), now).is_err());
        assert!(validate(&credit(slot + 300, 1), now).is_err());
        assert!(validate(&credit(slot - 1, 1), now).is_err());
        assert!(validate(&credit(slot - 300, 0), now).is_err());
        assert!(validate(&credit(slot - 300, 301), now).is_err());
        assert!(validate(&credit(slot - BACKFILL_SECONDS - 300, 10), now).is_err());
        let mut odd = credit(slot - 300, 10);
        odd.project = "bad\nname".into();
        assert!(validate(&odd, now).is_err());
    }
}
