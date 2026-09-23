mod common;

use common::{pool, spawn_app, TestOptions};
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn retrying_with_the_same_key_replays_the_response_without_calling_the_psp(
    opts: PgPoolOptions,
    conn: PgConnectOptions,
) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;

    let first = app.pay(invoice_id, "retry-me", "tok_success").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert!(!first.replayed);

    let retry = app.pay(invoice_id, "retry-me", "tok_success").await;
    assert_eq!(retry.status, first.status);
    assert_eq!(retry.body, first.body, "replay must be byte-for-byte the original response");
    assert!(retry.replayed);
    assert_eq!(app.psp.charge_requests(), 1, "a replay must not reach the PSP");
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn a_declined_payment_is_replayed_as_declined(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;

    let first = app.pay(invoice_id, "declined-key", "tok_card_declined").await;
    assert_eq!(first.status, StatusCode::PAYMENT_REQUIRED, "{}", first.body);
    assert_eq!(first.body["error"]["code"], "card_declined");
    assert_eq!(app.invoice_status(invoice_id).await, "open", "a decline leaves the invoice payable");

    let retry = app.pay(invoice_id, "declined-key", "tok_card_declined").await;
    assert_eq!((retry.status, &retry.body), (first.status, &first.body));
    assert_eq!(app.psp.charge_requests(), 1);
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn reusing_a_key_with_a_different_request_is_rejected(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;
    let other_invoice_id = app.open_invoice().await;

    let first = app.pay(invoice_id, "shared-key", "tok_card_declined").await;
    assert_eq!(first.status, StatusCode::PAYMENT_REQUIRED);

    let different_body = app.pay(invoice_id, "shared-key", "tok_success").await;
    assert_eq!(different_body.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(different_body.body["error"]["code"], "idempotency_key_reused");

    let different_invoice = app.pay(other_invoice_id, "shared-key", "tok_card_declined").await;
    assert_eq!(different_invoice.body["error"]["code"], "idempotency_key_reused");

    assert_eq!(app.psp.charge_requests(), 1);
    assert_eq!(app.invoice_status(other_invoice_id).await, "open");
}

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn a_paid_invoice_rejects_further_payment_and_void(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;
    assert_eq!(app.pay(invoice_id, "k1", "tok_success").await.status, StatusCode::OK);

    let again = app.pay(invoice_id, "k2", "tok_success").await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.body["error"]["code"], "invoice_already_paid");

    let void = app.post(&format!("/v1/invoices/{invoice_id}/void"), json!({})).await;
    assert_eq!(void.status, StatusCode::CONFLICT);
    assert_eq!(void.body["error"]["code"], "invalid_state_transition");
    assert_eq!(app.psp.charge_requests(), 1);
}
