use std::time::Duration;

use pulse_server::config::Config;
use pulse_server::state::AppState;
use pulse_server::{now, router};
use sqlx::postgres::PgPoolOptions;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    if let Err(message) = run().await {
        eprintln!("pulse-server: {message}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let config = Config::from_env()?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await
        .map_err(|error| format!("cannot connect to the database: {error}"))?;
    sqlx::migrate!("./migrations")
        .run(&db)
        .await
        .map_err(|error| format!("cannot update the database: {error}"))?;

    if config.hca_client_id.is_none() || config.hca_client_secret.is_none() {
        tracing::warn!("HCA_CLIENT_ID and HCA_CLIENT_SECRET are not set: Hack Club sign-in is off");
    }
    if config.dev_login && !config.dev_login_allowed() {
        tracing::warn!(
            "PULSE_DEV_LOGIN is ignored because PULSE_BASE_URL does not point at this computer"
        );
    } else if config.dev_login {
        tracing::warn!(
            "development sign-in is on: anyone who can reach this server can sign in as anyone"
        );
    }

    let cleanup = db.clone();
    tokio::spawn(async move {
        let mut hourly = tokio::time::interval(Duration::from_secs(3600)); // 60 * 60
        loop {
            hourly.tick().await;
            if let Err(error) = sqlx::query("delete from sessions where expires_at < $1")
                .bind(now())
                .execute(&cleanup)
                .await
            {
                tracing::warn!("cannot delete expired sessions: {error}");
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|error| format!("cannot listen on {}: {error}", config.bind))?;
    tracing::info!("listening on {}", config.bind);
    axum::serve(listener, router(AppState::new(db, config)))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|error| error.to_string())
}
