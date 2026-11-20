use std::sync::Arc;

use serde::Serialize;
use sqlx::PgPool;
use tokio::sync::broadcast;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    pub live: broadcast::Sender<CreditEvent>,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> AppState {
        AppState {
            db,
            config: Arc::new(config),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("http client"),
            // dashboards that fall behind just refetch, so a small buffer is fine
            live: broadcast::channel(256).0,
        }
    }
}

/// Some time was just credited. Sent to open dashboards after it is saved.
#[derive(Clone, Debug, Serialize)]
pub struct CreditEvent {
    #[serde(skip)]
    pub user_id: i64,
    /// `None` for viewers who may not see the project's name.
    pub project: Option<String>,
    #[serde(skip)]
    pub project_key: String,
    #[serde(skip)]
    pub project_public: bool,
    /// Whether the person's profile was public when this was saved.
    #[serde(skip)]
    pub profile_public: bool,
    pub slot: i64,
    pub seconds_added: i32,
    pub at: i64,
}

impl CreditEvent {
    /// The event as one viewer may see it.
    pub fn for_viewer(&self, is_owner: bool) -> CreditEvent {
        CreditEvent {
            project: (is_owner || self.project_public).then(|| self.project_key.clone()),
            ..self.clone()
        }
    }
}
