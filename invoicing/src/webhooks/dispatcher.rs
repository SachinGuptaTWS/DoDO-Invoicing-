//! Delivers webhook_deliveries rows. Runs beside the API, never inside a
//! request: the API only inserts outbox rows in its own transaction.
//!
//! Delivery is at-least-once. A row is leased (its `next_attempt_at` pushed
//! into the future) before the HTTP call, so a crashed worker's rows come
//! back on their own and concurrent workers never double-send in normal
//! operation. Receivers still dedupe on the event id.

use std::time::Duration;

use chrono::Utc;
use futures_util::future::join_all;
use rand::Rng;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::signature;
use crate::events::Event;

/// Delay before retry N (after failed attempt N). Front-loaded so transient
/// blips recover in seconds, then spaced so a receiver that is down for a
/// deploy or an evening still gets the event: ~7h 42m total budget.
pub const RETRY_SCHEDULE: [Duration; 7] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(4 * 60 * 60),
];
pub const MAX_ATTEMPTS: i32 = RETRY_SCHEDULE.len() as i32 + 1;

const BATCH_SIZE: usize = 32;
/// Must comfortably exceed the per-request webhook timeout.
const LEASE: Duration = Duration::from_secs(60);
const MAX_ERROR_LEN: usize = 500;

pub async fn run(db: PgPool, http: reqwest::Client, poll_interval: Duration, shutdown: CancellationToken) {
    tracing::info!("webhook dispatcher started");
    while !shutdown.is_cancelled() {
        match dispatch_due(&db, &http).await {
            // A full batch means more is waiting, so skip the sleep.
            Ok(n) if n == BATCH_SIZE => continue,
            Ok(_) => {}
            Err(err) => tracing::error!(error = %err, "webhook dispatch pass failed"),
        }
        tokio::select! {
            () = shutdown.cancelled() => break,
            () = tokio::time::sleep(poll_interval) => {}
        }
    }
    tracing::info!("webhook dispatcher stopped");
}

#[derive(sqlx::FromRow)]
struct LeasedDelivery {
    endpoint_id: Uuid,
    attempt_count: i32,
    url: String,
    signing_secret: String,
    #[sqlx(flatten)]
    event: Event,
}

enum DeliveryResult {
    Delivered { status: u16 },
    Failed { status: Option<u16>, error: String },
}

async fn dispatch_due(db: &PgPool, http: &reqwest::Client) -> Result<usize, sqlx::Error> {
    let leased = lease_due(db).await?;
    let count = leased.len();
    let results = join_all(leased.iter().map(|delivery| deliver(http, delivery))).await;
    for (delivery, result) in leased.iter().zip(results) {
        // Keep going: the lease on this row runs out and it is sent again,
        // which at-least-once delivery already allows for.
        if let Err(err) = record_result(db, delivery, result).await {
            tracing::error!(event_id = %delivery.event.id, endpoint_id = %delivery.endpoint_id, error = %err, "failed to record webhook result");
        }
    }
    Ok(count)
}

async fn lease_due(db: &PgPool) -> Result<Vec<LeasedDelivery>, sqlx::Error> {
    sqlx::query_as::<_, LeasedDelivery>(
        "WITH due AS (
             SELECT event_id, endpoint_id FROM webhook_deliveries
             WHERE status = 'pending' AND next_attempt_at <= now()
             ORDER BY next_attempt_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED
         )
         UPDATE webhook_deliveries d
         SET attempt_count = d.attempt_count + 1,
             next_attempt_at = now() + make_interval(secs => $2)
         FROM due, events e, webhook_endpoints w
         WHERE d.event_id = due.event_id AND d.endpoint_id = due.endpoint_id
           AND e.id = d.event_id AND w.id = d.endpoint_id
         RETURNING d.endpoint_id, d.attempt_count, w.url, w.signing_secret,
                   e.id, e.event_type, e.created_at, e.data",
    )
    .bind(BATCH_SIZE as i64)
    .bind(LEASE.as_secs_f64())
    .fetch_all(db)
    .await
}

async fn deliver(http: &reqwest::Client, delivery: &LeasedDelivery) -> DeliveryResult {
    // Same serialization as `GET /v1/events`, so a replayed event and a
    // delivered one look identical to the receiver.
    let body = serde_json::to_string(&delivery.event).expect("an event is always valid JSON");
    // Signed per attempt: a fresh timestamp keeps retries inside the
    // receiver's replay tolerance.
    let signature = signature::sign(&delivery.signing_secret, Utc::now().timestamp(), body.as_bytes());

    let response = http
        .post(&delivery.url)
        .header("content-type", "application/json")
        .header(signature::HEADER, signature)
        .header("dodo-event-id", delivery.event.id.to_string())
        .header("dodo-event-type", &delivery.event.event_type)
        .body(body)
        .send()
        .await;

    match response {
        Ok(response) if response.status().is_success() => DeliveryResult::Delivered { status: response.status().as_u16() },
        Ok(response) => DeliveryResult::Failed {
            status: Some(response.status().as_u16()),
            error: format!("endpoint responded with HTTP {}", response.status()),
        },
        Err(err) => DeliveryResult::Failed { status: None, error: truncate(err.to_string()) },
    }
}

async fn record_result(db: &PgPool, delivery: &LeasedDelivery, result: DeliveryResult) -> Result<(), sqlx::Error> {
    // `attempt_count = $3` is the lease check: if our lease expired and
    // another worker re-leased the row, our late result is discarded.
    let query = match &result {
        DeliveryResult::Delivered { status } => sqlx::query(
            "UPDATE webhook_deliveries
             SET status = 'delivered', delivered_at = now(), last_response_status = $4, last_error = NULL
             WHERE event_id = $1 AND endpoint_id = $2 AND attempt_count = $3 AND status = 'pending'",
        )
        .bind(delivery.event.id)
        .bind(delivery.endpoint_id)
        .bind(delivery.attempt_count)
        .bind(i16::try_from(*status).ok()),
        DeliveryResult::Failed { status, error } => {
            let exhausted = delivery.attempt_count >= MAX_ATTEMPTS;
            let retry_in = retry_delay(delivery.attempt_count).unwrap_or(Duration::ZERO);
            sqlx::query(
                "UPDATE webhook_deliveries
                 SET status = $4, next_attempt_at = now() + make_interval(secs => $5),
                     last_response_status = $6, last_error = $7
                 WHERE event_id = $1 AND endpoint_id = $2 AND attempt_count = $3 AND status = 'pending'",
            )
            .bind(delivery.event.id)
            .bind(delivery.endpoint_id)
            .bind(delivery.attempt_count)
            .bind(if exhausted { "exhausted" } else { "pending" })
            .bind(retry_in.as_secs_f64())
            .bind(status.and_then(|status| i16::try_from(status).ok()))
            .bind(error.as_str())
        }
    };
    query.execute(db).await?;

    match result {
        DeliveryResult::Delivered { status } => tracing::info!(
            event_id = %delivery.event.id, endpoint_id = %delivery.endpoint_id,
            attempt = delivery.attempt_count, status, "webhook delivered"
        ),
        DeliveryResult::Failed { error, .. } if delivery.attempt_count >= MAX_ATTEMPTS => tracing::error!(
            event_id = %delivery.event.id, endpoint_id = %delivery.endpoint_id,
            attempt = delivery.attempt_count, %error, "webhook retries exhausted"
        ),
        DeliveryResult::Failed { error, .. } => tracing::warn!(
            event_id = %delivery.event.id, endpoint_id = %delivery.endpoint_id,
            attempt = delivery.attempt_count, %error, "webhook delivery failed; will retry"
        ),
    }
    Ok(())
}

/// ±10% jitter so endpoints that fail together do not retry in lockstep.
fn retry_delay(failed_attempt: i32) -> Option<Duration> {
    let base = RETRY_SCHEDULE.get(usize::try_from(failed_attempt - 1).ok()?)?;
    Some(base.mul_f64(rand::thread_rng().gen_range(0.9..=1.1)))
}

fn truncate(mut message: String) -> String {
    if message.len() > MAX_ERROR_LEN {
        let mut cut = MAX_ERROR_LEN;
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        message.truncate(cut);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_budget_matches_the_documented_policy() {
        assert_eq!(MAX_ATTEMPTS, 8);
        let total: Duration = RETRY_SCHEDULE.iter().sum();
        assert_eq!(total, Duration::from_secs(7 * 3600 + 42 * 60 + 30));
    }

    #[test]
    fn delays_stay_within_jitter_and_stop_after_the_last_attempt() {
        for attempt in 1..MAX_ATTEMPTS {
            let base = RETRY_SCHEDULE[(attempt - 1) as usize];
            let delay = retry_delay(attempt).unwrap();
            assert!(delay >= base.mul_f64(0.9) && delay <= base.mul_f64(1.1));
        }
        assert!(retry_delay(MAX_ATTEMPTS).is_none());
    }
}
