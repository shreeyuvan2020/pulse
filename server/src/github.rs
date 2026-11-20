//! GitHub. "Connect GitHub" proves which GitHub account is a person's, and a
//! signed webhook tells the server when that account pushes.
//!
//! Push times come from GitHub's own notification, never from commit dates,
//! which anyone can set to anything. Pulse keeps no GitHub access token: it
//! signs the person in once, reads their numeric id, and throws the token
//! away. A push is only recorded when its sender is a connected person.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Redirect;
use axum::routing::{delete, get, post};
use axum_extra::extract::cookie::{Cookie, CookieJar};
use hmac::{Hmac, Mac};
use pulse_core::chain;
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use url::Url;

use crate::auth::{SessionUser, cookie, random_token};
use crate::error::{AppError, is_unique_violation};
use crate::now;
use crate::state::AppState;

const OAUTH_COOKIE: &str = "pulse_github";
/// Push payloads list up to 2048 commits, which can pass the 1 MiB default.
const WEBHOOK_BODY_LIMIT: usize = 8 << 20; // 8 MiB

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/github", get(connect))
        .route("/auth/github/callback", get(callback))
        .route("/api/github", delete(disconnect))
        .route(
            "/api/github/webhook",
            post(webhook).layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT)),
        )
}

fn name_ok(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !part.chars().all(|c| c == '.')
}

/// `owner/name`, as GitHub writes a repository's full name.
fn valid_full_name(full_name: &str) -> bool {
    full_name
        .split_once('/')
        .is_some_and(|(owner, name)| name_ok(owner) && name_ok(name))
}

/// The link for a project that is a GitHub repository (`github.com/owner/name`).
/// A project's name is whatever the computer sent, so a link is only made
/// when it is exactly a repository address.
pub fn repo_url(project_key: &str) -> Option<String> {
    let full_name = project_key.strip_prefix("github.com/")?;
    valid_full_name(full_name).then(|| format!("https://{project_key}"))
}

/// Checks GitHub's `X-Hub-Signature-256` header: `sha256=` and the hex HMAC of
/// the exact body bytes, compared in constant time.
pub fn signature_valid(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(expected) = header.strip_prefix("sha256=").and_then(chain::from_hex) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

async fn connect(
    State(state): State<AppState>,
    SessionUser(_): SessionUser,
    jar: CookieJar,
) -> Result<(CookieJar, Redirect), AppError> {
    let github = &state.config.github;
    let (true, Some(client_id)) = (github.connect_enabled(), &github.client_id) else {
        return Err(AppError::Unavailable(
            "connecting GitHub is not set up on this server".into(),
        ));
    };
    let oauth_state = random_token("");
    let mut url = Url::parse(&format!("{}/login/oauth/authorize", github.oauth_base))
        .map_err(|_| AppError::Unavailable("the GitHub address is not valid".into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair(
            "redirect_uri",
            &format!("{}/auth/github/callback", state.config.base_url),
        )
        .append_pair("state", &oauth_state);
    // the state only has to survive the round trip to GitHub, so it lives 10 minutes: 10
    let jar = jar.add(cookie(&state, OAUTH_COOKIE, oauth_state, "/auth", 10));
    Ok((jar, Redirect::to(url.as_str())))
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
struct AccessToken {
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct GithubUser {
    id: i64,
    login: String,
}

async fn callback(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
    jar: CookieJar,
    Query(query): Query<Callback>,
) -> Result<(CookieJar, Redirect), AppError> {
    let github = &state.config.github;
    let (Some(client_id), Some(client_secret)) = (&github.client_id, &github.client_secret) else {
        return Err(AppError::Unavailable(
            "connecting GitHub is not set up on this server".into(),
        ));
    };
    let expected = jar
        .get(OAUTH_COOKIE)
        .map(|cookie| cookie.value().to_owned());
    let jar = jar.remove(Cookie::build(OAUTH_COOKIE).path("/auth").build());
    let (Some(code), Some(returned), Some(expected)) = (query.code, query.state, expected) else {
        return Err(AppError::BadRequest(
            "connecting GitHub was cancelled or expired, try again".into(),
        ));
    };
    if returned != expected {
        return Err(AppError::BadRequest(
            "connecting GitHub was cancelled or expired, try again".into(),
        ));
    }

    let upstream = |error: reqwest::Error| {
        tracing::warn!("GitHub request failed: {error}");
        AppError::Upstream("could not reach GitHub, try again".into())
    };
    let token: AccessToken = state
        .http
        .post(format!("{}/login/oauth/access_token", github.oauth_base))
        .header("accept", "application/json")
        .json(&json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code,
            "redirect_uri": format!("{}/auth/github/callback", state.config.base_url),
        }))
        .send()
        .await
        .map_err(upstream)?
        .error_for_status()
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    // GitHub answers 200 with an error body when the code is wrong or used
    let access_token = token.access_token.ok_or_else(|| {
        AppError::Upstream("GitHub did not accept that sign-in, try again".into())
    })?;
    let account: GithubUser = state
        .http
        .get(format!("{}/user", github.api_base))
        .header("accept", "application/vnd.github+json")
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(upstream)?
        .error_for_status()
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    if account.id <= 0 || !name_ok(&account.login) {
        return Err(AppError::Upstream(
            "GitHub sent an account this server cannot use".into(),
        ));
    }

    let result = sqlx::query("update users set github_id = $1, github_login = $2 where id = $3")
        .bind(account.id)
        .bind(&account.login)
        .bind(user.id)
        .execute(&state.db)
        .await;
    match result {
        Ok(_) => {}
        Err(error) if is_unique_violation(&error) => {
            return Err(AppError::Conflict(
                "that GitHub account is already connected to another Pulse account".into(),
            ));
        }
        Err(error) => return Err(error.into()),
    }
    Ok((jar, Redirect::to(&format!("/u/{}", user.username))))
}

/// Forgets the GitHub account and the pushes recorded for it.
async fn disconnect(
    State(state): State<AppState>,
    SessionUser(user): SessionUser,
) -> Result<StatusCode, AppError> {
    let mut tx = state.db.begin().await?;
    sqlx::query("update users set github_id = null, github_login = null where id = $1")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("delete from pushes where user_id = $1")
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct Push {
    #[serde(rename = "ref")]
    git_ref: String,
    after: String,
    #[serde(default)]
    forced: bool,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    commits: Vec<PushCommit>,
    repository: PushRepository,
    sender: PushSender,
}

#[derive(Deserialize)]
struct PushCommit {
    id: String,
}

#[derive(Deserialize)]
struct PushRepository {
    full_name: String,
    /// Unix seconds for push events.
    pushed_at: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct PushSender {
    id: i64,
}

fn is_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.chars().all(|c| c.is_ascii_hexdigit())
}

async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, AppError> {
    let Some(secret) = &state.config.github.webhook_secret else {
        return Err(AppError::Unavailable(
            "GitHub pushes are not set up on this server".into(),
        ));
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    // nothing in the body is looked at until the signature has been checked
    if !header("x-hub-signature-256")
        .is_some_and(|signature| signature_valid(secret, &body, signature))
    {
        return Err(AppError::Unauthorized);
    }
    // pings, and every other event, are accepted and ignored
    if header("x-github-event") != Some("push") {
        return Ok(StatusCode::NO_CONTENT);
    }
    let push: Push = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("that is not a push payload".into()))?;
    if push.deleted || !push.git_ref.starts_with("refs/heads/") {
        return Ok(StatusCode::NO_CONTENT);
    }
    if !valid_full_name(&push.repository.full_name) || !is_sha(&push.after) {
        return Err(AppError::BadRequest("that is not a push payload".into()));
    }

    let Some(user_id) = sqlx::query_scalar::<_, i64>("select id from users where github_id = $1")
        .bind(push.sender.id)
        .fetch_optional(&state.db)
        .await?
    else {
        // not a connected person: nothing to record, and nothing to tell GitHub
        return Ok(StatusCode::NO_CONTENT);
    };

    let received_at = now();
    // a push time in the future is a broken clock somewhere, so it is not trusted: 300 s of slack
    let pushed_at = push
        .repository
        .pushed_at
        .as_ref()
        .and_then(|value| value.as_i64())
        .filter(|at| *at > 0 && *at <= received_at + 300);
    let commit_shas: Vec<String> = push
        .commits
        .into_iter()
        .map(|commit| commit.id)
        .filter(|id| is_sha(id))
        .take(2048)
        .collect();
    // the body's hash is the key: GitHub's delivery id header is not covered
    // by its signature, so a replayed body could arrive with a fresh one
    sqlx::query(
        "insert into pushes (body_sha256, delivery_id, user_id, repo, ref, head_sha, commit_shas, forced, pushed_at, received_at) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict (body_sha256) do nothing",
    )
    .bind(chain::to_hex(&chain::sha256(&body)))
    .bind(header("x-github-delivery").unwrap_or("").chars().take(100).collect::<String>())
    .bind(user_id)
    .bind(push.repository.full_name.to_ascii_lowercase())
    .bind(&push.git_ref)
    .bind(push.after.to_ascii_lowercase())
    .bind(&commit_shas)
    .bind(push.forced)
    .bind(pushed_at)
    .bind(received_at)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_githubs_documented_example_and_nothing_else() {
        // the example in GitHub's "Validating webhook deliveries" guide
        let secret = "It's a Secret to Everybody";
        let body = b"Hello, World!";
        let good = "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";
        assert!(signature_valid(secret, body, good));

        assert!(!signature_valid("another secret", body, good));
        assert!(!signature_valid(secret, b"Hello, World?", good));
        // missing prefix, the old sha1 scheme, wrong length, not hex, empty
        assert!(!signature_valid(secret, body, &good["sha256=".len()..]));
        assert!(!signature_valid(
            secret,
            body,
            "sha1=757107ea0eb2509fc211221cce984b8a37570b6d"
        ));
        assert!(!signature_valid(secret, body, "sha256=7571"));
        assert!(!signature_valid(
            secret,
            body,
            &format!("sha256={}", "z".repeat(64))
        ));
        assert!(!signature_valid(secret, body, ""));
    }

    #[test]
    fn only_exact_repository_addresses_become_links() {
        assert_eq!(
            repo_url("github.com/hackclub/pulse").as_deref(),
            Some("https://github.com/hackclub/pulse")
        );
        assert_eq!(
            repo_url("github.com/Some-One/my_repo.js").as_deref(),
            Some("https://github.com/Some-One/my_repo.js")
        );
        for bad in [
            "local/pulse",
            "gitlab.com/o/r",
            "github.com/o",
            "github.com/o/r/extra",
            "github.com/o/r?x=1",
            "github.com/o/../r",
            "github.com//r",
            "github.com/o/r\n",
            "evil.github.com/o/r",
            "https://github.com/o/r",
        ] {
            assert_eq!(repo_url(bad), None, "{bad}");
        }
    }
}
