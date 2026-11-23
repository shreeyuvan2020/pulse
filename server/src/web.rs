//! The website: three static files, no build step. All data comes from the
//! JSON API, and the page only ever puts it on screen as text, never as HTML.

use axum::Router;
use axum::http::{HeaderName, HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;

use crate::state::AppState;

const PAGE: &str = include_str!("../web/index.html");
const SCRIPT: &str = include_str!("../web/app.js");
const STYLE: &str = include_str!("../web/style.css");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/u/{username}", get(|| async { Html(PAGE) }))
        .route("/submissions", get(|| async { Html(PAGE) }))
        .route("/install", get(|| async { Html(PAGE) }))
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    SCRIPT,
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE) }),
        )
}

/// Only this site's own scripts and styles may run, nothing may frame the
/// site, and nothing is cached, because every response can hold personal data.
pub async fn security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
             connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::CACHE_CONTROL, "no-store"),
    ] {
        headers
            .entry(name)
            .or_insert(HeaderValue::from_static(value));
    }
    headers
        .entry(HeaderName::from_static("x-frame-options"))
        .or_insert(HeaderValue::from_static("DENY"));
    response.into_response()
}
