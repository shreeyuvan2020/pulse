//! End-to-end tests of the server over its real HTTP routes and a real Postgres.
//! Set `PULSE_TEST_DATABASE_URL` to a Postgres you may create databases in;
//! without it these tests are skipped (and fail when `CI` is set).

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use pulse_server::config::Config;
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
