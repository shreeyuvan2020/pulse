//! Sign-in with Hack Club Auth, browser sessions and per-computer API tokens.
//! Cookies and tokens are random and only their SHA-256 is stored.

use axum::Router;
use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode, header};
use axum::response::{Html, Redirect};
use axum::routing::{get, post};
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use pulse_core::chain;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use sqlx::postgres::PgRow;
use url::Url;

use crate::error::{AppError, is_unique_violation};
use crate::now;
use crate::state::AppState;

const SESSION_COOKIE: &str = "pulse_session";
const OAUTH_COOKIE: &str = "pulse_oauth";

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub profile_public: bool,
}

/// The columns `user_from_row` reads. A macro, because sqlx only accepts SQL
/// that is a string literal.
#[macro_export]
macro_rules! user_columns {
    () => {
        "u.id, u.username, u.display_name, u.profile_public"
    };
}

pub fn user_from_row(row: &PgRow) -> User {
    User {
        id: row.get("id"),
        username: row.get("username"),
        display_name: row.get("display_name"),
        profile_public: row.get("profile_public"),
    }
}

pub fn random_token(prefix: &str) -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS random number generator works");
    format!("{prefix}{}", chain::to_hex(&bytes))
}

pub fn hash_token(token: &str) -> String {
    chain::to_hex(&chain::sha256(token.as_bytes()))
}

/// The signed-in person, from the session cookie. Requests that change
/// something must also come from this site's own pages (checked by `Origin`),
/// on top of the cookie being `SameSite=Lax`.
pub struct SessionUser(pub User);

/// Like `SessionUser`, for pages that also work signed out.
pub struct OptionalUser(pub Option<User>);

/// The person an API token belongs to, and which token it was.
pub struct TokenUser {
    pub user: User,
    pub token_id: i64,
}

async fn user_for_session(state: &AppState, parts: &Parts) -> Result<Option<User>, AppError> {
    let jar = CookieJar::from_headers(&parts.headers);
    let Some(cookie) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    let row = sqlx::query(concat!(
        "select ",
        user_columns!(),
        " from sessions s join users u on u.id = s.user_id where s.token_hash = $1 and s.expires_at > $2"
    ))
    .bind(hash_token(cookie.value()))
    .bind(now())
    .fetch_optional(&state.db)
    .await?;
    Ok(row.as_ref().map(user_from_row))
}

impl FromRequestParts<AppState> for SessionUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if !matches!(parts.method, Method::GET | Method::HEAD) {
            let origin = parts
                .headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok());
            if origin != Some(state.config.base_url.as_str()) {
                return Err(AppError::Forbidden);
            }
        }
        user_for_session(state, parts)
            .await?
            .map(SessionUser)
            .ok_or(AppError::Unauthorized)
    }
}

impl FromRequestParts<AppState> for OptionalUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        Ok(OptionalUser(user_for_session(state, parts).await?))
    }
}

impl FromRequestParts<AppState> for TokenUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(AppError::Unauthorized)?;
        let row = sqlx::query(concat!(
            "select t.id as token_id, ",
            user_columns!(),
            " from api_tokens t join users u on u.id = t.user_id where t.token_hash = $1"
        ))
        .bind(hash_token(token))
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Unauthorized)?;
        Ok(TokenUser {
            user: user_from_row(&row),
            token_id: row.get("token_id"),
        })
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/auth/dev", get(dev_login))
        .route("/auth/logout", post(logout))
}

fn cookie(
    state: &AppState,
    name: &'static str,
    value: String,
    path: &'static str,
    minutes: i64,
) -> Cookie<'static> {
    Cookie::build((name, value))
        .http_only(true)
        .same_site(SameSite::Lax)
        .secure(state.config.secure_cookies())
        .path(path)
        .max_age(time::Duration::minutes(minutes))
        .build()
}

async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, Redirect), AppError> {
    let config = &state.config;
    let Some(client_id) = &config.hca_client_id else {
        if config.dev_login_allowed() {
            return Ok((jar, Redirect::to("/auth/dev")));
        }
        return Err(AppError::Unavailable(
            "sign-in is not set up on this server".into(),
        ));
    };
    let oauth_state = random_token("");
    let mut url = Url::parse(&format!("{}/oauth/authorize", config.hca_base))
        .map_err(|_| AppError::Unavailable("HCA_BASE_URL is not a valid address".into()))?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair(
            "redirect_uri",
            &format!("{}/auth/callback", config.base_url),
        )
        .append_pair("response_type", "code")
        .append_pair("scope", "openid profile")
        .append_pair("state", &oauth_state);
    // the state only has to survive the round trip to Hack Club, so it lives 10 minutes: 10
    let jar = jar.add(cookie(&state, OAUTH_COOKIE, oauth_state, "/auth", 10));
    Ok((jar, Redirect::to(url.as_str())))
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
    name: Option<String>,
    nickname: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<Callback>,
) -> Result<(CookieJar, Redirect), AppError> {
    let config = &state.config;
    let (Some(client_id), Some(client_secret)) = (&config.hca_client_id, &config.hca_client_secret)
    else {
        return Err(AppError::Unavailable(
            "sign-in is not set up on this server".into(),
        ));
    };
    let expected = jar
        .get(OAUTH_COOKIE)
        .map(|cookie| cookie.value().to_owned());
    let jar = jar.remove(Cookie::build(OAUTH_COOKIE).path("/auth").build());
    let (Some(code), Some(returned), Some(expected)) = (query.code, query.state, expected) else {
        return Err(AppError::BadRequest(
            "sign-in was cancelled or expired, try again".into(),
        ));
    };
    if returned != expected {
        return Err(AppError::BadRequest(
            "sign-in was cancelled or expired, try again".into(),
        ));
    }

    let upstream = |error: reqwest::Error| {
        tracing::warn!("Hack Club Auth request failed: {error}");
        AppError::Upstream("could not sign in with Hack Club, try again".into())
    };
    let tokens: TokenResponse = state
        .http
        .post(format!("{}/oauth/token", config.hca_base))
        .json(&json!({
            "client_id": client_id,
            "client_secret": client_secret,
            "redirect_uri": format!("{}/auth/callback", config.base_url),
            "code": code,
            "grant_type": "authorization_code",
        }))
        .send()
        .await
        .map_err(upstream)?
        .error_for_status()
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    let info: UserInfo = state
        .http
        .get(format!("{}/oauth/userinfo", config.hca_base))
        .bearer_auth(&tokens.access_token)
        .send()
        .await
        .map_err(upstream)?
        .error_for_status()
        .map_err(upstream)?
        .json()
        .await
        .map_err(upstream)?;
    if info.sub.is_empty() || info.sub.len() > 100 {
        return Err(AppError::Upstream(
            "Hack Club sent an account id this server cannot use".into(),
        ));
    }
    let user_id = find_or_create_user(
        &state,
        &info.sub,
        info.nickname.as_deref(),
        info.name.as_deref(),
    )
    .await?;
    let jar = start_session(&state, jar, user_id).await?;
    Ok((jar, Redirect::to("/")))
}

/// Sign in as any name, for development only: it needs `PULSE_DEV_LOGIN=1`
/// and a server address that points at this computer.
async fn dev_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Result<(CookieJar, Redirect), Html<&'static str>>, AppError> {
    if !state.config.dev_login_allowed() {
        return Err(AppError::NotFound);
    }
    let Some(name) = query.get("name").filter(|name| !name.trim().is_empty()) else {
        return Ok(Err(Html(
            "<!doctype html><title>Pulse dev sign-in</title>\
             <form action=\"/auth/dev\"><p>Development sign-in. Any name works.</p>\
             <input name=\"name\" autofocus> <button>Sign in</button></form>",
        )));
    };
    let user_id =
        find_or_create_user(&state, &format!("dev:{}", name.trim()), Some(name), None).await?;
    let jar = start_session(&state, jar, user_id).await?;
    Ok(Ok((jar, Redirect::to("/"))))
}

async fn logout(
    State(state): State<AppState>,
    _user: SessionUser,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), AppError> {
    if let Some(session) = jar.get(SESSION_COOKIE) {
        sqlx::query("delete from sessions where token_hash = $1")
            .bind(hash_token(session.value()))
            .execute(&state.db)
            .await?;
    }
    Ok((
        jar.remove(Cookie::build(SESSION_COOKIE).path("/").build()),
        StatusCode::NO_CONTENT,
    ))
}

async fn start_session(
    state: &AppState,
    jar: CookieJar,
    user_id: i64,
) -> Result<CookieJar, AppError> {
    let token = random_token("");
    let days = 14;
    sqlx::query("insert into sessions (token_hash, user_id, expires_at) values ($1, $2, $3)")
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(now() + days * 86_400) // 86400 seconds per day
        .execute(&state.db)
        .await?;
    Ok(jar.add(cookie(state, SESSION_COOKIE, token, "/", days * 24 * 60)))
}

/// `Rowan K.` becomes `rowan-k`: lowercase letters and digits, dashes between.
pub fn slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(24).collect();
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        "maker".into()
    } else {
        slug
    }
}

async fn user_id_for_sub(state: &AppState, sub: &str) -> Result<Option<i64>, AppError> {
    Ok(
        sqlx::query_scalar("select id from users where hca_sub = $1")
            .bind(sub)
            .fetch_optional(&state.db)
            .await?,
    )
}

async fn find_or_create_user(
    state: &AppState,
    sub: &str,
    nickname: Option<&str>,
    name: Option<&str>,
) -> Result<i64, AppError> {
    if let Some(id) = user_id_for_sub(state, sub).await? {
        return Ok(id);
    }
    let hint = nickname.or(name).unwrap_or_default();
    let base = slug(hint);
    let display_name: String = {
        let shown = name.or(nickname).unwrap_or(&base).trim();
        shown.chars().take(60).collect()
    };
    for attempt in 1..=50 {
        let candidate = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let result = sqlx::query_scalar::<_, i64>(
            "insert into users (hca_sub, username, display_name) values ($1, $2, $3) \
             on conflict (hca_sub) do nothing returning id",
        )
        .bind(sub)
        .bind(&candidate)
        .bind(&display_name)
        .fetch_optional(&state.db)
        .await;
        match result {
            Ok(Some(id)) => return Ok(id),
            // two sign-ins for the same person raced; the other one made the account
            Ok(None) => return user_id_for_sub(state, sub).await?.ok_or(AppError::Internal),
            Err(error) if is_unique_violation(&error) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(AppError::Conflict("could not find a free username".into()))
}

#[cfg(test)]
mod tests {
    use super::slug;

    #[test]
    fn usernames_are_lowercase_ascii_with_single_dashes() {
        assert_eq!(slug("Rowan K."), "rowan-k");
        assert_eq!(slug("  --Ünï  code--  "), "n-code");
        assert_eq!(slug("日本語"), "maker");
        assert_eq!(
            slug("A_very_long_name_that_keeps_going_and_going"),
            "a-very-long-name-that-ke"
        );
    }
}
