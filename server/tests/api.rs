//! End-to-end tests of the server over its real HTTP routes and a real Postgres.
//! Set `PULSE_TEST_DATABASE_URL` to a Postgres you may create databases in;
//! without it these tests are skipped (and fail when `CI` is set).

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use pulse_server::config::{Config, GithubConfig};
use pulse_server::state::AppState;
use pulse_server::{now, router};
use serde_json::{Value, json};
use sqlx::Connection;
use sqlx::postgres::{PgConnection, PgPoolOptions};
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:8080";

struct Harness {
    app: Router,
    admin_url: String,
    db_name: String,
    state: AppState,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(&self.body)))
    }

    fn cookie(&self, name: &str) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(|value| {
                value
                    .strip_prefix(&format!("{name}="))?
                    .split(';')
                    .next()
                    .map(str::to_owned)
            })
    }
}

/// A fresh database per test, so tests never see each other's people.
async fn harness(configure: impl FnOnce(&mut Config)) -> Option<Harness> {
    let Ok(admin_url) = std::env::var("PULSE_TEST_DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "PULSE_TEST_DATABASE_URL must be set in CI"
        );
        eprintln!("skipped: PULSE_TEST_DATABASE_URL is not set");
        return None;
    };
    let db_name = format!("pulse_test_{}", &pulse_server::auth::random_token("")[..16]);
    let mut admin = PgConnection::connect(&admin_url).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("create database {db_name}")))
        .execute(&mut admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&admin_url).unwrap();
    url.set_path(&format!("/{db_name}"));
    let db = PgPoolOptions::new()
        .max_connections(5)
        .connect(url.as_str())
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&db).await.unwrap();

    let mut config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database_url: url.to_string(),
        base_url: ORIGIN.into(),
        hca_base: "http://127.0.0.1:1".into(),
        hca_client_id: None,
        hca_client_secret: None,
        dev_login: true,
        github: Default::default(),
        storage_dir: None,
        program_keys: vec![],
    };
    configure(&mut config);
    let state = AppState::new(db, config);
    Some(Harness {
        app: router(state.clone()),
        admin_url,
        db_name,
        state,
    })
}

impl Harness {
    async fn call(
        &self,
        method: &str,
        uri: &str,
        headers: &[(&str, String)],
        body: Option<Value>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, value);
        }
        let request = match body {
            Some(body) => builder
                .header("content-type", "application/json")
                .body(Body::from(body.to_string())),
            None => builder.body(Body::empty()),
        }
        .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes().to_vec();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    /// Signs in through the development login and returns the session cookie.
    async fn sign_in(&self, name: &str) -> String {
        let reply = self
            .call("GET", &format!("/auth/dev?name={name}"), &[], None)
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        reply.cookie("pulse_session").expect("a session cookie")
    }

    /// Headers a signed-in browser page sends.
    fn browser(session: &str) -> Vec<(&'static str, String)> {
        vec![
            ("cookie", format!("pulse_session={session}")),
            ("origin", ORIGIN.into()),
        ]
    }

    async fn new_token(&self, session: &str, name: &str) -> (i64, String) {
        let reply = self
            .call(
                "POST",
                "/api/tokens",
                &Self::browser(session),
                Some(json!({ "name": name })),
            )
            .await;
        assert_eq!(
            reply.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&reply.body)
        );
        let body = reply.json();
        (
            body["id"].as_i64().unwrap(),
            body["token"].as_str().unwrap().to_owned(),
        )
    }

    async fn send(&self, token: &str, credits: Value) -> Reply {
        self.call(
            "POST",
            "/api/credits",
            &[("authorization", format!("Bearer {token}"))],
            Some(json!({ "credits": credits })),
        )
        .await
    }

    async fn profile(&self, username: &str, session: Option<&str>) -> Reply {
        let headers = session.map(Self::browser).unwrap_or_default();
        self.call("GET", &format!("/api/users/{username}"), &headers, None)
            .await
    }

    async fn finish(self) {
        self.state.db.close().await;
        let mut admin = PgConnection::connect(&self.admin_url).await.unwrap();
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "drop database {} with (force)",
            self.db_name
        )))
        .execute(&mut admin)
        .await;
    }
}

/// A completed 5-minute slot a few minutes ago.
fn past_slot(slots_back: i64) -> i64 {
    (now() - 900) / 300 * 300 - slots_back * 300
}

#[tokio::test]
async fn logged_time_appears_on_the_profile_and_names_stay_hidden_until_shown() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let me = h
        .call("GET", "/api/me", &Harness::browser(&session), None)
        .await
        .json();
    assert_eq!(me["user"]["username"], "rowan");
    let (_, token) = h.new_token(&session, "school laptop").await;

    let slot = past_slot(0);
    let sent = h
        .send(&token, json!([{ "project": "github.com/rowan/cs2-web", "slot": slot, "seconds": 200, "prompts": 3,
                               "sources": ["claude-code"], "models": ["claude-opus-4-5"] }]))
        .await;
    assert_eq!(sent.json()["accepted"], 1);

    let anonymous = h.profile("rowan", None).await.json();
    assert_eq!(anonymous["total_seconds"], 200);
    assert_eq!(anonymous["harnesses"], json!(["claude-code"]));
    assert_eq!(
        anonymous["projects"][0]["name"],
        Value::Null,
        "a project name must not leak by default"
    );
    assert_eq!(anonymous["projects"][0]["id"], Value::Null);
    assert_eq!(anonymous["building_now"], true);
    assert_eq!(anonymous["building_project"], Value::Null);

    let owner = h.profile("rowan", Some(&session)).await.json();
    assert_eq!(owner["projects"][0]["name"], "github.com/rowan/cs2-web");
    let project_id = owner["projects"][0]["id"].as_i64().unwrap();
    let shown = h
        .call(
            "PATCH",
            &format!("/api/projects/{project_id}"),
            &Harness::browser(&session),
            Some(json!({ "public": true })),
        )
        .await;
    assert_eq!(shown.status, StatusCode::NO_CONTENT);
    assert_eq!(
        h.profile("rowan", None).await.json()["projects"][0]["name"],
        "github.com/rowan/cs2-web"
    );

    // the same slot sent again as it grew replaces the old number instead of adding to it
    h.send(
        &token,
        json!([{ "project": "github.com/rowan/cs2-web", "slot": slot, "seconds": 250 }]),
    )
    .await;
    assert_eq!(h.profile("rowan", None).await.json()["total_seconds"], 250);
    h.finish().await;
}

#[tokio::test]
async fn one_person_never_logs_more_than_five_minutes_in_a_five_minute_slot() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, laptop) = h.new_token(&session, "laptop").await;
    let (_, desktop) = h.new_token(&session, "desktop").await;
    let slot = past_slot(0);

    h.send(
        &laptop,
        json!([{ "project": "a", "slot": slot, "seconds": 200 }]),
    )
    .await;
    // a second computer claims 200 more seconds of the same slot, on another project
    let second = h
        .send(
            &desktop,
            json!([{ "project": "b", "slot": slot, "seconds": 200 }]),
        )
        .await
        .json();
    assert_eq!(
        (second["accepted"].as_i64(), second["clamped"].as_i64()),
        (Some(1), Some(1))
    );
    assert_eq!(h.profile("rowan", None).await.json()["total_seconds"], 300);

    // nothing is left of the slot, so more from the laptop adds nothing
    let third = h
        .send(
            &laptop,
            json!([{ "project": "c", "slot": slot, "seconds": 50 }]),
        )
        .await
        .json();
    assert_eq!(
        (third["accepted"].as_i64(), third["clamped"].as_i64()),
        (Some(0), Some(1))
    );

    // the laptop lowering its own claim frees the difference for the desktop
    h.send(
        &laptop,
        json!([{ "project": "a", "slot": slot, "seconds": 100 }]),
    )
    .await;
    h.send(
        &desktop,
        json!([{ "project": "b", "slot": slot, "seconds": 200 }]),
    )
    .await;
    assert_eq!(h.profile("rowan", None).await.json()["total_seconds"], 300);

    // another slot is not affected
    h.send(
        &laptop,
        json!([{ "project": "a", "slot": past_slot(1), "seconds": 300 }]),
    )
    .await;
    assert_eq!(h.profile("rowan", None).await.json()["total_seconds"], 600);
    h.finish().await;
}

#[tokio::test]
async fn impossible_time_is_rejected_entry_by_entry_and_the_rest_is_kept() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&session, "laptop").await;
    let slot = past_slot(0);
    let reply = h
        .send(
            &token,
            json!([
                { "project": "p", "slot": (now() / 300 + 2) * 300, "seconds": 10 },
                { "project": "p", "slot": slot + 1, "seconds": 10 },
                { "project": "p", "slot": slot, "seconds": 301 },
                { "project": "p", "slot": slot, "seconds": 60 },
            ]),
        )
        .await
        .json();
    assert_eq!(reply["accepted"], 1);
    let indexes: Vec<i64> = reply["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["index"].as_i64().unwrap())
        .collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    assert_eq!(h.profile("rowan", None).await.json()["total_seconds"], 60);
    h.finish().await;
}

#[tokio::test]
async fn a_private_profile_looks_like_no_profile_to_everyone_but_its_owner() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let rowan = h.sign_in("rowan").await;
    let other = h.sign_in("sam").await;
    let hidden = h
        .call(
            "PATCH",
            "/api/me",
            &Harness::browser(&rowan),
            Some(json!({ "profile_public": false })),
        )
        .await;
    assert_eq!(hidden.status, StatusCode::NO_CONTENT);

    assert_eq!(h.profile("rowan", None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(
        h.profile("rowan", Some(&other)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.profile("rowan", Some(&rowan)).await.status,
        StatusCode::OK
    );
    // and neither can they listen for live updates
    assert_eq!(
        h.call("GET", "/api/users/rowan/live", &[], None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    h.finish().await;
}

#[tokio::test]
async fn changes_need_this_sites_own_origin_and_tokens_are_not_shared() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let rowan = h.sign_in("rowan").await;
    let sam = h.sign_in("sam").await;
    let cookie = ("cookie", format!("pulse_session={rowan}"));

    for origin in [Some("https://evil.example"), None] {
        let mut headers = vec![cookie.clone()];
        headers.extend(origin.map(|origin| ("origin", origin.to_owned())));
        let reply = h
            .call(
                "POST",
                "/api/tokens",
                &headers,
                Some(json!({ "name": "x" })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "origin {origin:?}");
    }

    let (token_id, token) = h.new_token(&rowan, "laptop").await;
    let revoke = format!("/api/tokens/{token_id}");
    assert_eq!(
        h.call("DELETE", &revoke, &Harness::browser(&sam), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // a session cookie is not an API token, and a made-up token is not either
    let credits = json!([]);
    let with_cookie = h
        .call(
            "POST",
            "/api/credits",
            &Harness::browser(&rowan),
            Some(json!({ "credits": credits })),
        )
        .await;
    assert_eq!(with_cookie.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.send("pulse_nope", json!([])).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.send(&token, json!([])).await.status, StatusCode::OK);

    assert_eq!(
        h.call("DELETE", &revoke, &Harness::browser(&rowan), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.send(&token, json!([])).await.status,
        StatusCode::UNAUTHORIZED
    );

    // signing out ends the session on the server too
    assert_eq!(
        h.call("POST", "/auth/logout", &Harness::browser(&rowan), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.call("GET", "/api/me", &Harness::browser(&rowan), None)
            .await
            .json()["user"],
        Value::Null
    );
    h.finish().await;
}

#[tokio::test]
async fn sign_in_with_hack_club_round_trip() {
    // a stand-in for Hack Club Auth
    let hca = Router::new()
        .route(
            "/oauth/token",
            post(|Json(body): Json<Value>| async move {
                if body["grant_type"] == "authorization_code" && body["code"] == "good" && body["client_secret"] == "shh" {
                    (StatusCode::OK, Json(json!({ "access_token": "idntk.abc", "token_type": "Bearer" })))
                } else {
                    (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_grant" })))
                }
            }),
        )
        .route(
            "/oauth/userinfo",
            get(|headers: HeaderMap| async move {
                if headers.get("authorization").and_then(|value| value.to_str().ok()) == Some("Bearer idntk.abc") {
                    (StatusCode::OK, Json(json!({ "sub": "ident!abc", "name": "Heidi Trashworth", "nickname": null })))
                } else {
                    (StatusCode::UNAUTHORIZED, Json(json!({})))
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hca_base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, hca).await.unwrap() });

    let Some(h) = harness(|config| {
        config.hca_base = hca_base.clone();
        config.hca_client_id = Some("cid".into());
        config.hca_client_secret = Some("shh".into());
        config.dev_login = false;
    })
    .await
    else {
        return;
    };

    let login = h.call("GET", "/auth/login", &[], None).await;
    assert_eq!(login.status, StatusCode::SEE_OTHER);
    let location = login.headers[header::LOCATION].to_str().unwrap().to_owned();
    assert!(
        location.starts_with(&format!(
            "{hca_base}/oauth/authorize?client_id=cid&redirect_uri="
        )),
        "{location}"
    );
    assert!(
        location.contains("response_type=code")
            && location.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fauth%2Fcallback")
    );
    let state = login.cookie("pulse_oauth").unwrap();
    assert!(location.contains(&format!("state={state}")));
    let cookie = ("cookie", format!("pulse_oauth={state}"));

    // a callback that does not carry the state this browser was given is refused
    let forged = h
        .call(
            "GET",
            "/auth/callback?code=good&state=forged",
            std::slice::from_ref(&cookie),
            None,
        )
        .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let no_cookie = h
        .call(
            "GET",
            &format!("/auth/callback?code=good&state={state}"),
            &[],
            None,
        )
        .await;
    assert_eq!(no_cookie.status, StatusCode::BAD_REQUEST);
    // Hack Club rejecting the code is a bad gateway, not a sign-in
    let bad_code = h
        .call(
            "GET",
            &format!("/auth/callback?code=bad&state={state}"),
            std::slice::from_ref(&cookie),
            None,
        )
        .await;
    assert_eq!(bad_code.status, StatusCode::BAD_GATEWAY);

    let done = h
        .call(
            "GET",
            &format!("/auth/callback?code=good&state={state}"),
            std::slice::from_ref(&cookie),
            None,
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    let session = done.cookie("pulse_session").unwrap();
    let me = h
        .call("GET", "/api/me", &Harness::browser(&session), None)
        .await
        .json();
    assert_eq!(
        (
            me["user"]["username"].as_str(),
            me["user"]["display_name"].as_str()
        ),
        (Some("heidi-trashworth"), Some("Heidi Trashworth"))
    );

    // signing in again finds the same account instead of making a second one
    let again = h
        .call(
            "GET",
            &format!("/auth/callback?code=good&state={state}"),
            &[cookie],
            None,
        )
        .await;
    let session = again.cookie("pulse_session").unwrap();
    assert_eq!(
        h.call("GET", "/api/me", &Harness::browser(&session), None)
            .await
            .json()["user"]["username"],
        "heidi-trashworth"
    );
    let users: i64 = sqlx::query_scalar("select count(*) from users")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(users, 1);

    // the development login does not exist when it is off
    assert_eq!(
        h.call("GET", "/auth/dev?name=x", &[], None).await.status,
        StatusCode::NOT_FOUND
    );
    h.finish().await;
}

#[tokio::test]
async fn dashboards_hear_about_time_the_moment_it_is_saved_and_only_what_they_may_see() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&session, "laptop").await;

    let watch = |headers: Vec<(&'static str, String)>| {
        let mut builder = Request::builder().uri("/api/users/rowan/live");
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        h.app.clone().oneshot(builder.body(Body::empty()).unwrap())
    };
    let stranger = watch(vec![]).await.unwrap();
    let owner = watch(Harness::browser(&session)).await.unwrap();
    assert_eq!(stranger.status(), StatusCode::OK);
    assert_eq!(
        stranger.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );

    h.send(
        &token,
        json!([{ "project": "github.com/rowan/secret", "slot": past_slot(0), "seconds": 120 }]),
    )
    .await;

    async fn first_event(response: axum::response::Response) -> String {
        let mut body = response.into_body();
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("an event within 5 seconds");
        String::from_utf8(frame.unwrap().unwrap().into_data().unwrap().to_vec()).unwrap()
    }
    let seen_by_stranger = first_event(stranger).await;
    assert!(
        seen_by_stranger.contains("event: credit")
            && seen_by_stranger.contains("\"seconds_added\":120"),
        "{seen_by_stranger}"
    );
    assert!(
        !seen_by_stranger.contains("secret"),
        "a private project name reached a stranger: {seen_by_stranger}"
    );
    assert!(first_event(owner).await.contains("github.com/rowan/secret"));
    h.finish().await;
}

#[tokio::test]
async fn pages_and_api_carry_security_headers() {
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    for uri in ["/", "/u/anyone", "/app.js", "/api/me"] {
        let reply = h.call("GET", uri, &[], None).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        let csp = reply.headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap();
        assert!(
            csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"),
            "{uri}"
        );
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    }
    h.finish().await;
}

/// A stand-in for GitHub's sign-in and user endpoints. The code `good-<id>`
/// signs in the GitHub account numbered `<id>`; any other code is refused the
/// way GitHub does it, with a 200 and an error body.
async fn stub_github() -> String {
    let github = Router::new()
        .route(
            "/login/oauth/access_token",
            post(|headers: HeaderMap, Json(body): Json<Value>| async move {
                assert_eq!(headers["accept"], "application/json");
                assert_eq!(body["client_id"], "gid");
                assert_eq!(body["client_secret"], "gsecret");
                match body["code"]
                    .as_str()
                    .and_then(|code| code.strip_prefix("good-"))
                {
                    Some(id) => Json(
                        json!({ "access_token": format!("token-{id}"), "token_type": "bearer" }),
                    ),
                    None => Json(json!({ "error": "bad_verification_code" })),
                }
            }),
        )
        .route(
            "/user",
            get(|headers: HeaderMap| async move {
                let token = headers["authorization"]
                    .to_str()
                    .unwrap()
                    .strip_prefix("Bearer token-")
                    .map(str::to_owned);
                match token.and_then(|id| id.parse::<i64>().ok()) {
                    Some(id) => (
                        StatusCode::OK,
                        Json(json!({ "id": id, "login": format!("gh-user-{id}") })),
                    ),
                    None => (StatusCode::UNAUTHORIZED, Json(json!({}))),
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, github).await.unwrap() });
    base
}

fn github_config(base: &str) -> GithubConfig {
    GithubConfig {
        client_id: Some("gid".into()),
        client_secret: Some("gsecret".into()),
        webhook_secret: Some("whsecret".into()),
        app_slug: Some("pulse-test".into()),
        oauth_base: base.into(),
        api_base: base.into(),
    }
}

#[tokio::test]
async fn connecting_github_proves_the_account_and_one_account_belongs_to_one_person() {
    let base = stub_github().await;
    let Some(h) = harness(|config| config.github = github_config(&base)).await else {
        return;
    };
    let rowan = h.sign_in("rowan").await;
    let sam = h.sign_in("sam").await;

    assert_eq!(
        h.call("GET", "/auth/github", &[], None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let start = h
        .call("GET", "/auth/github", &Harness::browser(&rowan), None)
        .await;
    assert_eq!(start.status, StatusCode::SEE_OTHER);
    let location = start.headers[header::LOCATION].to_str().unwrap().to_owned();
    let state = start.cookie("pulse_github").unwrap();
    assert!(
        location.starts_with(&format!(
            "{base}/login/oauth/authorize?client_id=gid&redirect_uri="
        )),
        "{location}"
    );
    assert!(
        location.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fauth%2Fgithub%2Fcallback"),
        "{location}"
    );
    assert!(location.contains(&format!("state={state}")));

    let with_state = |session: &str, state: &str| {
        let mut headers = Harness::browser(session);
        headers.push((
            "cookie",
            format!("pulse_session={session}; pulse_github={state}"),
        ));
        headers
    };
    let callback =
        |code: &str, state: &str| format!("/auth/github/callback?code={code}&state={state}");

    // a callback that does not carry the state this browser was given is refused
    let forged = h
        .call(
            "GET",
            &callback("good-4242", "forged"),
            &with_state(&rowan, &state),
            None,
        )
        .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let no_cookie = h
        .call(
            "GET",
            &callback("good-4242", &state),
            &Harness::browser(&rowan),
            None,
        )
        .await;
    assert_eq!(no_cookie.status, StatusCode::BAD_REQUEST);
    // GitHub refusing the code is a bad gateway, not a connection
    let refused = h
        .call(
            "GET",
            &callback("nope", &state),
            &with_state(&rowan, &state),
            None,
        )
        .await;
    assert_eq!(refused.status, StatusCode::BAD_GATEWAY);
    // and someone who is not signed in cannot connect anything
    let anonymous = h
        .call(
            "GET",
            &callback("good-4242", &state),
            &[("cookie", format!("pulse_github={state}"))],
            None,
        )
        .await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);

    let done = h
        .call(
            "GET",
            &callback("good-4242", &state),
            &with_state(&rowan, &state),
            None,
        )
        .await;
    assert_eq!(done.status, StatusCode::SEE_OTHER);
    assert_eq!(done.headers[header::LOCATION], "/u/rowan");
    let me = h
        .call("GET", "/api/me", &Harness::browser(&rowan), None)
        .await
        .json();
    assert_eq!(me["user"]["github_login"], "gh-user-4242");
    assert_eq!(
        me["github"]["install_url"],
        "https://github.com/apps/pulse-test/installations/new"
    );

    // the same GitHub account cannot also be Sam's
    let taken = h
        .call(
            "GET",
            &callback("good-4242", &state),
            &with_state(&sam, &state),
            None,
        )
        .await;
    assert_eq!(taken.status, StatusCode::CONFLICT);
    assert!(
        h.call("GET", "/api/me", &Harness::browser(&sam), None)
            .await
            .json()["user"]["github_login"]
            .is_null()
    );
    // a different one can
    let other = h
        .call(
            "GET",
            &callback("good-777", &state),
            &with_state(&sam, &state),
            None,
        )
        .await;
    assert_eq!(other.status, StatusCode::SEE_OTHER);

    // a server without GitHub set up says so instead of redirecting somewhere
    h.finish().await;
    let Some(plain) = harness(|_| {}).await else {
        return;
    };
    let session = plain.sign_in("rowan").await;
    assert_eq!(
        plain
            .call("GET", "/auth/github", &Harness::browser(&session), None)
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    plain.finish().await;
}

fn signed(secret: &str, body: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let hex: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256={hex}")
}

fn push(sender: i64, repo: &str, git_ref: &str, after: char, pushed_at: i64) -> Value {
    json!({
        "ref": git_ref, "after": after.to_string().repeat(40), "forced": false, "deleted": false,
        "commits": [{ "id": "b".repeat(40) }, { "id": after.to_string().repeat(40) }, { "id": "not-a-sha" }],
        "repository": { "full_name": repo, "pushed_at": pushed_at },
        "sender": { "id": sender },
    })
}

impl Harness {
    async fn deliver(&self, secret: &str, event: &str, delivery: &str, body: &Value) -> Reply {
        let text = body.to_string();
        let headers = [
            ("x-hub-signature-256", signed(secret, &text)),
            ("x-github-event", event.into()),
            ("x-github-delivery", delivery.into()),
        ];
        self.call("POST", "/api/github/webhook", &headers, Some(body.clone()))
            .await
    }

    async fn push_count(&self) -> i64 {
        sqlx::query_scalar("select count(*) from pushes")
            .fetch_one(&self.state.db)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn only_signed_pushes_from_connected_people_are_recorded_and_replays_are_ignored() {
    let Some(h) = harness(|config| config.github = github_config("http://127.0.0.1:1")).await
    else {
        return;
    };
    let session = h.sign_in("rowan").await;
    sqlx::query(
        "update users set github_id = 4242, github_login = 'rowan-gh' where username = 'rowan'",
    )
    .execute(&h.state.db)
    .await
    .unwrap();
    let pushed = now() - 120;
    let good = push(4242, "Rowan/CS2-Web", "refs/heads/main", 'a', pushed);
    let sign = |body: &Value| signed("whsecret", &body.to_string());

    // unsigned, wrongly signed and signed-for-another-body deliveries are all refused
    let unsigned = h
        .call(
            "POST",
            "/api/github/webhook",
            &[("x-github-event", "push".into())],
            Some(good.clone()),
        )
        .await;
    assert_eq!(unsigned.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.deliver("wrong", "push", "d1", &good).await.status,
        StatusCode::UNAUTHORIZED
    );
    let other_body = push(4242, "Rowan/CS2-Web", "refs/heads/main", 'c', pushed);
    let swapped = [
        ("x-hub-signature-256", sign(&other_body)),
        ("x-github-event", "push".into()),
    ];
    assert_eq!(
        h.call("POST", "/api/github/webhook", &swapped, Some(good.clone()))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(h.push_count().await, 0);

    // things that are signed but not a recordable push: pings, tags, deleted branches,
    // people who never connected, and other kinds of event
    assert_eq!(
        h.deliver(
            "whsecret",
            "ping",
            "d2",
            &json!({ "zen": "Keep it logically awesome." })
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.deliver(
            "whsecret",
            "push",
            "d3",
            &push(4242, "Rowan/CS2-Web", "refs/tags/v1", 'a', pushed)
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    let mut deleted = push(4242, "Rowan/CS2-Web", "refs/heads/gone", '0', pushed);
    deleted["deleted"] = json!(true);
    assert_eq!(
        h.deliver("whsecret", "push", "d4", &deleted).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.deliver(
            "whsecret",
            "push",
            "d5",
            &push(999, "Rowan/CS2-Web", "refs/heads/main", 'a', pushed)
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.deliver("whsecret", "issues", "d6", &good).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.push_count().await, 0);
    let broken = push(4242, "not a repo", "refs/heads/main", 'a', pushed);
    assert_eq!(
        h.deliver("whsecret", "push", "d7", &broken).await.status,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        h.deliver("whsecret", "push", "d8", &good).await.status,
        StatusCode::NO_CONTENT
    );
    // the same signed body again, under a fresh delivery id, is still one push
    assert_eq!(
        h.deliver("whsecret", "push", "a-different-delivery", &good)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.push_count().await, 1);
    let row = sqlx::query("select repo, ref, head_sha, commit_shas, pushed_at from pushes")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    use sqlx::Row;
    assert_eq!(
        row.get::<String, _>("repo"),
        "rowan/cs2-web",
        "stored lowercase so it matches a project name"
    );
    assert_eq!(
        row.get::<Vec<String>, _>("commit_shas"),
        vec!["b".repeat(40), "a".repeat(40)],
        "only real commit ids are kept"
    );
    assert_eq!(row.get::<Option<i64>, _>("pushed_at"), Some(pushed));

    // a push time from the future is not believed
    let future = push(
        4242,
        "Rowan/CS2-Web",
        "refs/heads/dev",
        'd',
        now() + 100_000,
    );
    assert_eq!(
        h.deliver("whsecret", "push", "d9", &future).await.status,
        StatusCode::NO_CONTENT
    );
    let times: Vec<Option<i64>> =
        sqlx::query_scalar("select pushed_at from pushes where head_sha like 'dd%'")
            .fetch_all(&h.state.db)
            .await
            .unwrap();
    assert_eq!(times, vec![None]);

    // a body bigger than the usual 1 MiB limit is fine (big pushes list many commits), but not unbounded
    let mut big = push(4242, "Rowan/CS2-Web", "refs/heads/big", 'e', pushed);
    big["padding"] = json!("x".repeat(2 << 20));
    assert_eq!(
        h.deliver("whsecret", "push", "d10", &big).await.status,
        StatusCode::NO_CONTENT
    );
    big["padding"] = json!("x".repeat(9 << 20));
    assert_eq!(
        h.deliver("whsecret", "push", "d11", &big).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(h.push_count().await, 3);

    // the project's page shows them, but only as much as the project itself is shown
    let (_, token) = h.new_token(&session, "laptop").await;
    h.send(
        &token,
        json!([{ "project": "github.com/rowan/cs2-web", "slot": past_slot(0), "seconds": 60 }]),
    )
    .await;
    let stranger = h.profile("rowan", None).await.json();
    assert_eq!(
        (
            &stranger["projects"][0]["url"],
            &stranger["projects"][0]["last_push_at"],
            &stranger["projects"][0]["pushes"]
        ),
        (&Value::Null, &Value::Null, &json!(0)),
        "a hidden project's repository must not leak through its pushes"
    );
    let owner = h.profile("rowan", Some(&session)).await.json();
    let project = &owner["projects"][0];
    assert_eq!(project["url"], "https://github.com/rowan/cs2-web");
    assert_eq!(project["pushes"], 3);
    // the pushes were at `pushed`, `received_at` for the future one, and `pushed`
    let latest =
        sqlx::query_scalar::<_, i64>("select max(coalesce(pushed_at, received_at)) from pushes")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(project["last_push_at"], latest);
    assert!(latest >= pushed);
    let id = project["id"].as_i64().unwrap();
    h.call(
        "PATCH",
        &format!("/api/projects/{id}"),
        &Harness::browser(&session),
        Some(json!({ "public": true })),
    )
    .await;
    let public = h.profile("rowan", None).await.json();
    assert_eq!(
        (
            &public["projects"][0]["url"],
            &public["projects"][0]["pushes"]
        ),
        (&json!("https://github.com/rowan/cs2-web"), &json!(3))
    );

    // disconnecting forgets the account and what was recorded for it
    assert_eq!(
        h.call("DELETE", "/api/github", &Harness::browser(&session), None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(h.push_count().await, 0);
    assert_eq!(
        h.deliver("whsecret", "push", "d12", &good).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.push_count().await,
        0,
        "after disconnecting, pushes are not recorded again"
    );
    h.finish().await;
}

#[tokio::test]
async fn pushes_are_refused_outright_when_no_webhook_secret_is_set() {
    // a server without a secret must not fall back to trusting unsigned deliveries
    let Some(h) = harness(|_| {}).await else {
        return;
    };
    let body = push(1, "a/b", "refs/heads/main", 'a', now());
    let reply = h
        .call(
            "POST",
            "/api/github/webhook",
            &[("x-github-event", "push".into())],
            Some(body),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    h.finish().await;
}

// ---- submissions ----

const ARI_KEY: &str = "ari-key-0123456789abcdef0123456789abcdef";
const OTHER_KEY: &str = "other-key-0123456789abcdef0123456789abcd";

struct Seg {
    video: Vec<u8>,
    timeline: Value,
    link: String,
    body: Value,
}

/// A segment the way the desktop client records it: a video, a per-second
/// timeline and the hash-chain link that commits to both.
fn seg(previous: &str, start: i64, seconds: i64, working: bool, tag: u8) -> Seg {
    use pulse_core::chain::{self, Second};
    let mut video = vec![0, 0, 0, 24];
    video.extend(b"ftypisom");
    video.extend([tag; 20]);
    let timeline: Vec<Second> = (start..start + seconds)
        .map(|t| Second {
            t,
            change: if working { 40 } else { 0 },
            input: working,
        })
        .collect();
    let previous_hash = chain::from_hex(previous).unwrap();
    let video_hash = chain::sha256(&video);
    let timeline_hash = chain::timeline_hash(&timeline);
    let link = chain::to_hex(&chain::link(
        &previous_hash,
        start,
        start + seconds,
        &video_hash,
        &timeline_hash,
    ));
    Seg {
        video,
        timeline: json!({ "seconds": timeline.iter().map(|s| json!([s.t, s.change, u8::from(s.input)])).collect::<Vec<_>>() }),
        body: json!({ "start": start, "end": start + seconds, "previous": previous, "video": chain::to_hex(&video_hash),
                      "timeline": chain::to_hex(&timeline_hash), "link": link }),
        link,
    }
}

impl Harness {
    fn bearer(token: &str) -> Vec<(&'static str, String)> {
        vec![("authorization", format!("Bearer {token}"))]
    }

    async fn put_bytes(&self, uri: &str, token: &str, bytes: Vec<u8>) -> Reply {
        let request = Request::builder()
            .method("PUT")
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(bytes))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        Reply {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.unwrap().to_bytes().to_vec(),
        }
    }

    async fn submit(&self, token: &str, program: &str, project: &str, links: &[&String]) -> Reply {
        let body = json!({ "program": program, "project": project, "links": links });
        self.call("POST", "/api/submissions", &Self::bearer(token), Some(body))
            .await
    }

    async fn register(&self, token: &str, segments: &[&Seg]) -> Value {
        let links: Vec<Value> = segments.iter().map(|s| s.body.clone()).collect();
        self.call(
            "POST",
            "/api/chain",
            &Self::bearer(token),
            Some(json!({ "links": links })),
        )
        .await
        .json()
    }

    async fn upload(&self, token: &str, id: i64, segment: &Seg) -> (StatusCode, StatusCode) {
        let video = self
            .put_bytes(
                &format!("/api/submissions/{id}/segments/{}/video", segment.link),
                token,
                segment.video.clone(),
            )
            .await;
        let timeline = self
            .call(
                "PUT",
                &format!("/api/submissions/{id}/segments/{}/timeline", segment.link),
                &Self::bearer(token),
                Some(segment.timeline.clone()),
            )
            .await;
        (video.status, timeline.status)
    }
}

async fn submission_harness() -> Option<(Harness, tempfile::TempDir)> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let other = OTHER_KEY;
    let h = harness(move |config| {
        config.storage_dir = Some(path);
        config.program_keys = vec![
            ("ari".into(), ARI_KEY.into()),
            ("other".into(), other.into()),
        ];
    })
    .await?;
    Some((h, dir))
}

#[tokio::test]
async fn hours_are_submitted_with_the_recordings_that_back_them_and_programs_see_only_their_own() {
    let Some((h, _dir)) = submission_harness().await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&session, "laptop").await;
    let project = "github.com/rowan/cs2-web";
    let genesis = "0".repeat(64);
    let base = (now() - 1200) / 300 * 300;
    // a: 100 s of work; b: 60 s of the same range spent on something the person leaves out; c: 50 s of work after a quiet stretch
    let a = seg(&genesis, base, 100, true, 1);
    let b = seg(&a.link, base + 100, 60, true, 2);
    let c = seg(&b.link, base + 400, 50, true, 3);
    h.send(
        &token,
        json!([{ "project": project, "slot": base, "seconds": 150 }]),
    )
    .await;
    let registered = h.register(&token, &[&a, &b, &c]).await;
    assert_eq!(registered["accepted"], 3, "{registered}");

    // asking for segments the server never saw, or for a program that does not exist
    let ghost = seg(&genesis, base + 1000, 10, true, 9);
    assert_eq!(
        h.submit(&token, "ari", project, &[&ghost.link])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.submit(&token, "nope", project, &[&a.link]).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        h.submit(&token, "ari", project, &[&a.link, &a.link])
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    let made = h
        .submit(&token, "ari", project, &[&a.link, &c.link])
        .await
        .json();
    let id = made["id"].as_i64().unwrap();
    assert_eq!(
        made["omitted"], 1,
        "b sits inside the range and must be listed as omitted: {made}"
    );

    // nothing can be finalized, or seen by the program, before everything is uploaded
    let early = h
        .call(
            "POST",
            &format!("/api/submissions/{id}/finalize"),
            &Harness::bearer(&token),
            None,
        )
        .await;
    assert_eq!(early.status, StatusCode::CONFLICT);
    let key = |key: &str| vec![("authorization", format!("Bearer {key}"))];
    assert_eq!(
        h.call(
            "GET",
            &format!("/api/programs/submissions/{id}/evidence"),
            &key(ARI_KEY),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // a video that is not the one committed, an unfinished MP4, a changed timeline: all refused
    let mut swapped = a.video.clone();
    swapped[20] ^= 1;
    let wrong = h
        .put_bytes(
            &format!("/api/submissions/{id}/segments/{}/video", a.link),
            &token,
            swapped,
        )
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    let not_mp4 = h
        .put_bytes(
            &format!("/api/submissions/{id}/segments/{}/video", a.link),
            &token,
            vec![7; 64],
        )
        .await;
    assert_eq!(not_mp4.status, StatusCode::BAD_REQUEST);
    let mut edited = a.timeline.clone();
    edited["seconds"][3][1] = json!(999);
    let changed = h
        .call(
            "PUT",
            &format!("/api/submissions/{id}/segments/{}/timeline", a.link),
            &Harness::bearer(&token),
            Some(edited),
        )
        .await;
    assert_eq!(changed.status, StatusCode::BAD_REQUEST);
    // the omitted segment takes no upload, and a stranger's token cannot upload to this submission
    assert_eq!(
        h.upload(&token, id, &b).await,
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );
    let sam = h.sign_in("sam").await;
    let (_, sam_token) = h.new_token(&sam, "laptop").await;
    assert_eq!(
        h.upload(&sam_token, id, &a).await,
        (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND)
    );

    assert_eq!(
        h.upload(&token, id, &a).await,
        (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT)
    );
    assert_eq!(
        h.upload(&token, id, &c).await,
        (StatusCode::NO_CONTENT, StatusCode::NO_CONTENT)
    );
    let done = h
        .call(
            "POST",
            &format!("/api/submissions/{id}/finalize"),
            &Harness::bearer(&token),
            None,
        )
        .await
        .json();
    // a is 100 s and c is 50 s of continuous work; b was left out
    assert_eq!(
        (
            done["recorded_seconds"].as_i64(),
            done["active_seconds"].as_i64(),
            done["omitted_seconds"].as_i64(),
            done["claimed_seconds"].as_i64()
        ),
        (Some(150), Some(150), Some(60), Some(150)),
        "{done}"
    );

    // the program reads it
    assert_eq!(
        h.call("GET", "/api/programs/submissions", &[], None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.call("GET", "/api/programs/submissions", &key("wrong"), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let listed = h
        .call(
            "GET",
            "/api/programs/submissions?since=0",
            &key(ARI_KEY),
            None,
        )
        .await
        .json();
    assert_eq!(listed["submissions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["submissions"][0]["maker"], "rowan");
    assert!(
        h.call(
            "GET",
            "/api/programs/submissions?since=0",
            &key(OTHER_KEY),
            None
        )
        .await
        .json()["submissions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.call(
            "GET",
            &format!("/api/programs/submissions/{id}/evidence"),
            &key(OTHER_KEY),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    let doc = h
        .call(
            "GET",
            &format!("/api/programs/submissions/{id}/evidence"),
            &key(ARI_KEY),
            None,
        )
        .await
        .json();
    assert_eq!(doc["format"], "pulse.evidence.v1");
    assert_eq!(
        (
            &doc["totals"]["active_seconds"],
            &doc["totals"]["omitted_seconds"]
        ),
        (&json!(150), &json!(60))
    );
    assert_eq!(
        doc["submission"]["project_url"],
        "https://github.com/rowan/cs2-web"
    );
    assert_eq!(doc["checks"]["segments"], 2);
    assert_eq!(doc["checks"]["omitted_segments"], 1);
    assert_eq!(doc["checks"]["segments_committed_late"], 0);
    let runs: Vec<(i64, i64, &str)> = doc["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["start"].as_i64().unwrap(),
                r["end"].as_i64().unwrap(),
                r["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        vec![
            (base, base + 100, "active"),
            (base + 100, base + 160, "omitted"),
            (base + 400, base + 450, "active")
        ]
    );
    let listed_segments = doc["segments"].as_array().unwrap();
    assert_eq!(listed_segments.len(), 3);
    assert_eq!(
        listed_segments[1]["video_path"],
        Value::Null,
        "an omitted segment has no recording to fetch"
    );

    // a segment whose link reached the server long after it ended is flagged, not hidden
    sqlx::query("update chain_links set received_at = end_at + 7200 where link = $1")
        .bind(&c.link)
        .execute(&h.state.db)
        .await
        .unwrap();
    let late = h
        .call(
            "GET",
            &format!("/api/programs/submissions/{id}/evidence"),
            &key(ARI_KEY),
            None,
        )
        .await
        .json();
    assert_eq!(
        (
            &late["checks"]["segments_committed_late"],
            &late["checks"]["max_commit_delay_seconds"]
        ),
        (&json!(1), &json!(7200))
    );

    // the recording streams back with range requests, byte for byte, and its timeline too
    let video_path = listed_segments[0]["video_path"]
        .as_str()
        .unwrap()
        .to_owned();
    let full = h.call("GET", &video_path, &key(ARI_KEY), None).await;
    assert_eq!(
        (full.status, full.body.clone()),
        (StatusCode::OK, a.video.clone())
    );
    let mut ranged = key(ARI_KEY);
    ranged.push(("range", "bytes=4-7".into()));
    let part = h.call("GET", &video_path, &ranged, None).await;
    assert_eq!(
        (part.status, part.body),
        (StatusCode::PARTIAL_CONTENT, b"ftyp".to_vec())
    );
    assert_eq!(
        h.call("GET", &video_path, &key(OTHER_KEY), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let omitted_video = format!("/api/programs/submissions/{id}/segments/{}/video", b.link);
    assert_eq!(
        h.call("GET", &omitted_video, &key(ARI_KEY), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let timeline = h
        .call(
            "GET",
            listed_segments[0]["timeline_path"].as_str().unwrap(),
            &key(ARI_KEY),
            None,
        )
        .await
        .json();
    assert_eq!(timeline, a.timeline);

    // the person can see exactly what the program looked at, with repeated reads folded together
    let mine = h
        .call("GET", "/api/submissions", &Harness::browser(&session), None)
        .await
        .json();
    let views: Vec<(String, String)> = mine["submissions"][0]["views"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["program"].as_str().unwrap().to_owned(),
                v["what"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(views.contains(&("ari".into(), "manifest".into())));
    assert!(
        views
            .iter()
            .any(|(_, what)| what == &format!("video:{}", a.link))
    );
    assert!(
        views.iter().all(|(program, _)| program == "ari"),
        "a program that was refused must not appear: {views:?}"
    );
    assert_eq!(
        views
            .iter()
            .filter(|(_, what)| what.starts_with("video:"))
            .count(),
        1,
        "two reads of the same video are one view"
    );

    // hours of a submitted segment cannot be submitted again, but the omitted one still can
    assert_eq!(
        h.submit(&token, "other", project, &[&a.link]).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.call("GET", "/api/chain/used", &Harness::bearer(&token), None)
            .await
            .json()["used"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let second = h.submit(&token, "other", project, &[&b.link]).await;
    assert_eq!(
        second.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&second.body)
    );
    h.finish().await;
}

#[tokio::test]
async fn withdrawing_deletes_the_recordings_and_frees_the_hours() {
    let Some((h, dir)) = submission_harness().await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&session, "laptop").await;
    let genesis = "0".repeat(64);
    let base = (now() - 7200) / 300 * 300;
    let a = seg(&genesis, base, 100, true, 1);
    h.send(
        &token,
        json!([{ "project": "p", "slot": base, "seconds": 100 }]),
    )
    .await;
    h.register(&token, &[&a]).await;
    let id = h.submit(&token, "ari", "p", &[&a.link]).await.json()["id"]
        .as_i64()
        .unwrap();
    h.upload(&token, id, &a).await;
    h.call(
        "POST",
        &format!("/api/submissions/{id}/finalize"),
        &Harness::bearer(&token),
        None,
    )
    .await;
    assert!(
        dir.path()
            .join(id.to_string())
            .join(format!("{}.mp4", a.link))
            .exists()
    );

    // only the owner, from the site, can withdraw
    let sam = h.sign_in("sam").await;
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/api/submissions/{id}"),
            &Harness::browser(&sam),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/api/submissions/{id}"),
            &[("cookie", format!("pulse_session={session}"))],
            None
        )
        .await
        .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/api/submissions/{id}"),
            &Harness::browser(&session),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert!(
        !dir.path().join(id.to_string()).exists(),
        "the files must be gone"
    );
    // the program is told, and can no longer read it
    let listed = h
        .call(
            "GET",
            "/api/programs/submissions?since=0",
            &[("authorization", format!("Bearer {ARI_KEY}"))],
            None,
        )
        .await
        .json();
    assert_eq!(listed["submissions"][0]["status"], "withdrawn");
    assert_eq!(
        h.call(
            "GET",
            &format!("/api/programs/submissions/{id}/evidence"),
            &[("authorization", format!("Bearer {ARI_KEY}"))],
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    // withdrawing twice is harmless, and the hours can go in again
    assert_eq!(
        h.call(
            "DELETE",
            &format!("/api/submissions/{id}"),
            &Harness::browser(&session),
            None
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        h.submit(&token, "ari", "p", &[&a.link]).await.status,
        StatusCode::OK
    );
    h.finish().await;
}

#[tokio::test]
async fn chain_links_must_be_internally_consistent_and_belong_to_one_account() {
    let Some((h, _dir)) = submission_harness().await else {
        return;
    };
    let rowan = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&rowan, "laptop").await;
    let sam = h.sign_in("sam").await;
    let (_, sam_token) = h.new_token(&sam, "laptop").await;
    let genesis = "0".repeat(64);
    let base = (now() - 7200) / 300 * 300;
    let good = seg(&genesis, base, 100, true, 1);

    // a link whose hash does not match its parts, one that ends in the future, one that is too long
    let mut forged = good.body.clone();
    forged["end"] = json!(base + 101);
    let mut future = seg(&genesis, now() + 3600, 10, true, 2).body;
    future["previous"] = json!(genesis);
    let long = seg(&genesis, base, 700, true, 3).body;
    let reply = h
        .call(
            "POST",
            "/api/chain",
            &Harness::bearer(&token),
            Some(json!({ "links": [forged, future, long, good.body] })),
        )
        .await
        .json();
    assert_eq!(reply["accepted"], 1, "{reply}");
    let rejected: Vec<i64> = reply["rejected"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["index"].as_i64().unwrap())
        .collect();
    assert_eq!(rejected, vec![0, 1, 2]);

    // the same link again is fine; claiming someone else's is not
    assert_eq!(h.register(&token, &[&good]).await["accepted"], 1);
    let stolen = h.register(&sam_token, &[&good]).await;
    assert_eq!(
        (
            stolen["accepted"].as_i64(),
            stolen["rejected"].as_array().unwrap().len()
        ),
        (Some(0), 1)
    );
    // and sam cannot submit rowan's segment
    h.send(
        &sam_token,
        json!([{ "project": "p", "slot": base, "seconds": 10 }]),
    )
    .await;
    let take = h
        .call(
            "POST",
            "/api/submissions",
            &Harness::bearer(&sam_token),
            Some(json!({ "program": "ari", "project": "p", "links": [good.link] })),
        )
        .await;
    assert_eq!(take.status, StatusCode::BAD_REQUEST);

    // a server without storage says submissions are off instead of accepting hours it cannot back
    h.finish().await;
    let Some(plain) = harness(|_| {}).await else {
        return;
    };
    let session = plain.sign_in("rowan").await;
    let (_, token) = plain.new_token(&session, "laptop").await;
    let reply = plain
        .call(
            "POST",
            "/api/submissions",
            &Harness::bearer(&token),
            Some(json!({ "program": "ari", "project": "p", "links": [good.link] })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    plain.finish().await;
}

#[tokio::test]
async fn the_website_sees_how_much_was_recorded_and_how_much_is_already_submitted() {
    let Some((h, _dir)) = submission_harness().await else {
        return;
    };
    let session = h.sign_in("rowan").await;
    let (_, token) = h.new_token(&session, "laptop").await;
    let genesis = "0".repeat(64);
    let base = (now() - 1800) / 300 * 300;
    let a = seg(&genesis, base, 100, true, 1);
    let b = seg(&a.link, base + 100, 50, true, 2);
    h.register(&token, &[&a, &b]).await;
    h.send(
        &token,
        json!([{ "project": "p", "slot": base, "seconds": 100 }]),
    )
    .await;

    // needs a signed-in browser, like the page itself
    assert_eq!(
        h.call("GET", "/api/recordings", &Harness::bearer(&token), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let before = h
        .call("GET", "/api/recordings", &Harness::browser(&session), None)
        .await
        .json();
    assert_eq!(
        (
            before["seconds"].as_i64(),
            before["submitted_seconds"].as_i64()
        ),
        (Some(150), Some(0))
    );
    assert_eq!(before["days"][0]["segments"], 2);

    let id = h.submit(&token, "ari", "p", &[&a.link]).await.json()["id"]
        .as_i64()
        .unwrap();
    h.upload(&token, id, &a).await;
    h.call(
        "POST",
        &format!("/api/submissions/{id}/finalize"),
        &Harness::bearer(&token),
        None,
    )
    .await;

    let after = h
        .call("GET", "/api/recordings", &Harness::browser(&session), None)
        .await
        .json();
    assert_eq!(after["submitted_seconds"], 100);
    assert_eq!(after["seconds"], 150);
    h.finish().await;
}
