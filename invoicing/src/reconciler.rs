//! Resolves payment attempts whose outcome the request path never learned:
//! PSP timeouts, network errors, and crashes between charging and settling.
//!
//! It asks the PSP about the attempt by its idempotency key and applies the
//! answer through the same `settle_attempt` as the request path. It never
//! re-sends a charge and never guesses: an attempt stays pending until the
//! PSP says succeeded/failed, or until the PSP has had no record of it for
//! longer than any charge request could still be in flight.

use std::time::Duration;

use futures_util::future::join_all;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    app::AppState,
    payments::settle_attempt,
    psp::{ChargeLookup, Settlement},
};

const BATCH_SIZE: usize = 16;
/// While a worker holds an attempt, other workers skip it. The batch is
/// checked concurrently, so this only has to cover one PSP lookup plus a
/// settle. If the worker dies, the lease runs out and the attempt comes back.
const LEASE: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// With the 60 s cap this is about half an hour without an answer from the
/// PSP. We keep checking after that, but log at error level so a person
/// looks at it.
const ESCALATE_AFTER_CHECKS: i32 = 40;

pub async fn run(state: AppState, shutdown: CancellationToken) {
    tracing::info!("payment reconciler started");
    while !shutdown.is_cancelled() {
        match reconcile_due(&state).await {
            // A full batch means more is waiting, so skip the sleep.
            Ok(n) if n == BATCH_SIZE => continue,
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "reconciler pass failed"),
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(state.config.worker_poll_interval) => {}
        }
    }
    tracing::info!("payment reconciler stopped");
}

struct DueAttempt {
    id: Uuid,
    checks: i32,
    past_not_found_grace: bool,
}

async fn reconcile_due(state: &AppState) -> Result<usize, sqlx::Error> {
    let due = claim_due(state).await?;
    join_all(due.iter().map(|attempt| reconcile_one(state, attempt))).await;
    Ok(due.len())
}

async fn claim_due(state: &AppState) -> Result<Vec<DueAttempt>, sqlx::Error> {
    let rows: Vec<(Uuid, i32, bool)> = sqlx::query_as(
        "WITH due AS (
             SELECT id FROM payment_attempts
             WHERE status = 'pending' AND reconcile_after <= now()
             ORDER BY reconcile_after
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE payment_attempts p
         SET reconcile_after = now() + make_interval(secs => $2),
             reconcile_count = p.reconcile_count + 1
         FROM due WHERE p.id = due.id
         RETURNING p.id, p.reconcile_count, p.created_at < now() - make_interval(secs => $3)",
    )
    .bind(BATCH_SIZE as i64)
    .bind(LEASE.as_secs_f64())
    .bind(state.config.psp_not_found_grace.as_secs_f64())
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, checks, past_not_found_grace)| DueAttempt { id, checks, past_not_found_grace })
        .collect())
}

async fn reconcile_one(state: &AppState, attempt: &DueAttempt) {
    let settlement = match state.psp.lookup_charge(attempt.id).await {
        ChargeLookup::Settled(settlement) => settlement,
        // Our charge request carried this id as its idempotency key. If the
        // PSP still has no record after the grace window (longer than our
        // request timeout), the request never reached it: nothing was charged.
        ChargeLookup::NotFound if attempt.past_not_found_grace => Settlement::Failed { code: "psp_no_record".into() },
        ChargeLookup::NotFound | ChargeLookup::Processing => return reschedule(state, attempt).await,
        ChargeLookup::Indeterminate(reason) => {
            tracing::warn!(payment_attempt_id = %attempt.id, %reason, "PSP lookup inconclusive");
            return reschedule(state, attempt).await;
        }
    };

    if let Err(err) = settle_attempt(&state.db, attempt.id, settlement).await {
        // Lease expiry will bring it back; nothing to undo.
        tracing::error!(payment_attempt_id = %attempt.id, ?err, "failed to settle reconciled attempt");
    }
}

async fn reschedule(state: &AppState, attempt: &DueAttempt) {
    if attempt.checks >= ESCALATE_AFTER_CHECKS {
        tracing::error!(
            payment_attempt_id = %attempt.id,
            checks = attempt.checks,
            "payment attempt still unresolved; needs manual review"
        );
    }
    let delay = backoff(attempt.checks);
    let result = sqlx::query(
        "UPDATE payment_attempts SET reconcile_after = now() + make_interval(secs => $2)
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(attempt.id)
    .bind(delay.as_secs_f64())
    .execute(&state.db)
    .await;
    if let Err(err) = result {
        tracing::error!(payment_attempt_id = %attempt.id, error = %err, "failed to reschedule reconciliation");
    }
}

/// 1 s, 2 s, 4 s, ... capped at 60 s.
fn backoff(checks: i32) -> Duration {
    let exponent = u32::try_from(checks.saturating_sub(1)).unwrap_or(0).min(6);
    Duration::from_secs(1 << exponent).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_then_caps() {
        let delays: Vec<u64> = (1..=9).map(|checks| backoff(checks).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }
}
