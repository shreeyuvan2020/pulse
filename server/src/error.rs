use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict(String),
    /// A feature this server has not been set up for.
    Unavailable(String),
    /// Hack Club Auth failed or answered nonsense.
    Upstream(String),
    Internal,
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> AppError {
        tracing::error!("database error: {error}");
        AppError::Internal
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "sign in required".into()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "not allowed".into()),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            AppError::Conflict(message) => (StatusCode::CONFLICT, message),
            AppError::Unavailable(message) => (StatusCode::SERVICE_UNAVAILABLE, message),
            AppError::Upstream(message) => (StatusCode::BAD_GATEWAY, message),
            AppError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "something went wrong".into(),
            ),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

pub fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .is_some_and(|error| error.is_unique_violation())
}
