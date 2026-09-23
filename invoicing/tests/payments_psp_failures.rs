mod common;

use std::time::{Duration, Instant};

use common::{pool, spawn_app, TestOptions, INVOICE_TOTAL_CENTS};
use harness::mock_psp::MockPspConfig;
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Shrinks the spec's 30 s `tok_timeout` sleep so the test exercises the same
/// path (client timeout → 202 → reconciler resolves) in a few seconds.
fn fast_failure_options() -> TestOptions {
    TestOptions {
        psp: MockPspConfig { decision_delay: Duration::from_millis(50), slow_delay: Duration::from_secs(2) },
        psp_timeout: Duration::from_millis(300),
        psp_not_found_grace: Duration::from_secs(1),
    }
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn psp_timeout_returns_promptly_and_settles_through_reconciliation(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, fast_failure_options()).await;
    let invoice_id = app.open_invoice().await;

    let started = Instant::now();
    let response = app.pay(invoice_id, "slow-psp", "tok_timeout").await;
    assert!(started.elapsed() < Duration::from_secs(1), "endpoint hung for {:?}", started.elapsed());
    assert_eq!(response.status, StatusCode::ACCEPTED, "{}", response.body);
    assert_eq!(response.body["payment_attempt"]["status"], "pending");
    assert_eq!(response.body["invoice"]["status"], "processing");

    // While money may be moving, nothing else may happen to the invoice.
    let void = app.post(&format!("/v1/invoices/{invoice_id}/void"), json!({})).await;
    assert_eq!(void.body["error"]["code"], "payment_in_progress");
    let second = app.pay(invoice_id, "impatient-client", "tok_success").await;
    assert_eq!(second.body["error"]["code"], "payment_in_progress");

    let invoice = app.wait_for_invoice_status(invoice_id, "paid", Duration::from_secs(10)).await;
    assert_eq!(invoice["payment_attempts"][0]["status"], "succeeded");
    assert_eq!(app.psp.successful_charges(), vec![INVOICE_TOTAL_CENTS]);

    // The original key now replays the final outcome, not the stale 202.
    let replay = app.pay(invoice_id, "slow-psp", "tok_timeout").await;
    assert_eq!(replay.status, StatusCode::OK);
    assert!(replay.replayed);
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn psp_network_error_leaves_the_invoice_payable_again(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, fast_failure_options()).await;
    let invoice_id = app.open_invoice().await;

    let response = app.pay(invoice_id, "flaky-network", "tok_network_error").await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "outcome is unknown, so it must not be reported as failed yet");
    assert_eq!(response.body["invoice"]["status"], "processing");

    // The PSP never recorded the charge; once that is certain, the attempt
    // fails and the invoice goes back to open rather than sitting stuck.
    let invoice = app.wait_for_invoice_status(invoice_id, "open", Duration::from_secs(10)).await;
    assert_eq!(invoice["payment_attempts"][0]["status"], "failed");
    assert_eq!(invoice["payment_attempts"][0]["failure_code"], "psp_no_record");
    assert!(app.psp.successful_charges().is_empty());

    let retry = app.pay(invoice_id, "second-try", "tok_success").await;
    assert_eq!(retry.status, StatusCode::OK, "{}", retry.body);
    assert_eq!(app.psp.successful_charges(), vec![INVOICE_TOTAL_CENTS]);
    assert_eq!(app.attempt_counts(invoice_id).await, (2, 1));
}
