mod common;

use std::time::Duration;

use common::{pool, spawn_app, TestOptions};
use invoicing::webhooks::signature;
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn state_changes_are_delivered_as_signed_webhooks(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let endpoint = app.post("/v1/webhook_endpoints", json!({ "url": format!("{}/hooks/ok", app.sink_url) })).await;
    assert_eq!(endpoint.status, StatusCode::CREATED, "{}", endpoint.body);
    let secret = endpoint.body["signing_secret"].as_str().unwrap().to_owned();

    let paid = app.open_invoice().await;
    assert_eq!(app.pay(paid, "k1", "tok_success").await.status, StatusCode::OK);
    let declined = app.open_invoice().await;
    assert_eq!(app.pay(declined, "k2", "tok_insufficient_funds").await.status, StatusCode::PAYMENT_REQUIRED);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while app.sink.received().len() < 4 {
        assert!(tokio::time::Instant::now() < deadline, "got {:?}", app.sink.received());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let received = app.sink.received();
    let mut types: Vec<&str> = received.iter().filter_map(|hook| hook.event_type.as_deref()).collect();
    types.sort_unstable();
    assert_eq!(types, ["invoice.created", "invoice.created", "invoice.paid", "invoice.payment_failed"]);

    let now = chrono::Utc::now().timestamp();
    for hook in &received {
        let body = serde_json::to_vec(&hook.body).unwrap();
        let header = hook.signature.as_deref().unwrap();
        assert!(signature::verify(&secret, header, &body, now, 300), "bad signature on {:?}", hook.event_type);
        assert_eq!(hook.body["id"].as_str(), hook.event_id.as_deref());
    }
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn failed_deliveries_are_scheduled_for_retry(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    app.post("/v1/webhook_endpoints", json!({ "url": format!("{}/hooks/fail", app.sink_url) })).await;
    app.open_invoice().await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let row: Option<(String, i32, Option<i16>, f64)> = sqlx::query_as(
            "SELECT status, attempt_count, last_response_status,
                    extract(epoch FROM next_attempt_at - now())::float8
             FROM webhook_deliveries WHERE last_response_status IS NOT NULL",
        )
        .fetch_optional(&app.db)
        .await
        .unwrap();
        if let Some((status, attempts, last_status, retry_in_secs)) = row {
            assert_eq!((status.as_str(), attempts, last_status), ("pending", 1, Some(503)));
            assert!((25.0..=35.0).contains(&retry_in_secs), "first retry ~30s ±10%, got {retry_in_secs}");
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "delivery never attempted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
