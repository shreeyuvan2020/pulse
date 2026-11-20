//! The Pulse server: accounts, profiles, projects and a stats dashboard that
//! updates as time is logged. Everything it stores about time is whole
//! seconds, and the rule that one person cannot log more than one second per
//! wall-clock second is enforced here, whatever the clients say.

pub mod api;
pub mod auth;
pub mod config;
pub mod error;
pub mod github;
pub mod live;
pub mod state;
pub mod stats;
pub mod web;

use axum::Router;
use axum::extract::DefaultBodyLimit;

use state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(web::routes())
        .merge(auth::routes())
        .merge(api::routes())
        .merge(github::routes())
        .merge(live::routes())
        .layer(DefaultBodyLimit::max(1 << 20)) // 1 MiB: 1 << 20
        .layer(axum::middleware::map_response(web::security_headers))
        .with_state(state)
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}
