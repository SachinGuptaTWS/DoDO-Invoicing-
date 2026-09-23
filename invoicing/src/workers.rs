use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{app::AppState, reconciler, webhooks::dispatcher};

/// Background work that must never run on the request path. Both workers
/// claim rows with `FOR UPDATE SKIP LOCKED` + a lease, so running several
/// replicas of the service is safe without leader election.
pub fn spawn(state: &AppState, shutdown: CancellationToken) -> anyhow::Result<Vec<JoinHandle<()>>> {
    let webhook_http = reqwest::Client::builder()
        .timeout(state.config.webhook_timeout)
        // A redirect could point a signed payload somewhere the business
        // never registered.
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    Ok(vec![
        tokio::spawn(dispatcher::run(
            state.db.clone(),
            webhook_http,
            state.config.worker_poll_interval,
            shutdown.clone(),
        )),
        tokio::spawn(reconciler::run(state.clone(), shutdown)),
    ])
}
