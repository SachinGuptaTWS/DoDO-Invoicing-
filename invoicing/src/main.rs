use std::time::Duration;

use anyhow::Context;
use invoicing::{app, config::Config, workers, MIGRATOR};
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sqlx=warn")))
        .init();

    let config = Config::from_env()?;
    let db = PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&config.database_url)
        .await
        .context("connecting to Postgres")?;
    MIGRATOR.run(&db).await.context("running migrations")?;

    let bind_addr = config.bind_addr;
    let state = app::AppState::new(db, config)?;
    let shutdown = CancellationToken::new();
    let workers = workers::spawn(&state, shutdown.clone())?;

    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    tracing::info!(%bind_addr, "invoicing service listening");
    axum::serve(listener, app::router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown.clone()))
        .await?;

    // The signal handler has already cancelled `shutdown`; let workers finish
    // their current batch.
    for worker in workers {
        let _ = worker.await;
    }
    Ok(())
}

async fn shutdown_signal(shutdown: CancellationToken) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown requested; draining");
    shutdown.cancel();
}
