//! The real-time part: a server-sent events stream per profile. A dashboard
//! opens it once and hears about every credit as soon as it is saved.
//!
//! The events come from an in-process channel, so this works for one server
//! process. Running several would need Postgres LISTEN/NOTIFY behind the same
//! channel.

use std::convert::Infallible;

use axum::Router;
use axum::extract::{Path, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tokio_stream::{Stream, StreamExt};

use crate::api::{can_view, find_user};
use crate::auth::OptionalUser;
use crate::error::AppError;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/users/{username}/live", get(stream))
}

async fn stream(
    State(state): State<AppState>,
    OptionalUser(viewer): OptionalUser,
    Path(username): Path<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    let target = find_user(&state, &username).await?;
    if !can_view(&target, &viewer) {
        return Err(AppError::NotFound);
    }
    let is_owner = viewer.as_ref().is_some_and(|viewer| viewer.id == target.id);
    let events = BroadcastStream::new(state.live.subscribe()).filter_map(move |item| match item {
        // someone who went private after this stream opened stops hearing about new time
        Ok(event) if event.user_id == target.id && (is_owner || event.profile_public) => {
            Event::default()
                .event("credit")
                .json_data(event.for_viewer(is_owner))
                .ok()
                .map(Ok)
        }
        Ok(_) => None,
        // a slow dashboard missed some events; it refetches instead of replaying them
        Err(BroadcastStreamRecvError::Lagged(_)) => {
            Some(Ok(Event::default().event("refresh").data("missed events")))
        }
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}
