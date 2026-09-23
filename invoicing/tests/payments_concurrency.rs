mod common;

use common::{pool, spawn_app, TestOptions, INVOICE_TOTAL_CENTS};
use futures::future::join_all;
use reqwest::StatusCode;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

const CONCURRENT_REQUESTS: usize = 25;

/// N clients pay the same invoice at once, each with its own Idempotency-Key
/// (so idempotency cannot be what saves us: this exercises the invoice CAS).
#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn concurrent_payments_for_one_invoice_charge_exactly_once(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;

    let keys: Vec<String> = (0..CONCURRENT_REQUESTS).map(|i| format!("client-{i}")).collect();
    let responses = join_all(keys.iter().map(|key| app.pay(invoice_id, key, "tok_success"))).await;

    let succeeded = responses.iter().filter(|r| r.status == StatusCode::OK).count();
    assert_eq!(succeeded, 1, "exactly one request may win");
    for response in responses.iter().filter(|r| r.status != StatusCode::OK) {
        assert_eq!(response.status, StatusCode::CONFLICT, "{}", response.body);
        let code = response.body["error"]["code"].as_str().unwrap();
        assert!(matches!(code, "payment_in_progress" | "invoice_already_paid"), "unexpected code {code}");
    }

    assert_eq!(app.psp.successful_charges(), vec![INVOICE_TOTAL_CENTS], "no double charge at the PSP");
    assert_eq!(app.psp.charge_requests(), 1, "losers must never reach the PSP");
    assert_eq!(app.attempt_counts(invoice_id).await, (1, 1));
    assert_eq!(app.invoice_status(invoice_id).await, "paid");
}

/// The same request retried concurrently with one key (a client with an
/// aggressive retry loop): one execution, everyone else waits or replays.
#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn concurrent_retries_with_one_key_execute_once(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let invoice_id = app.open_invoice().await;

    let responses = join_all((0..CONCURRENT_REQUESTS).map(|_| app.pay(invoice_id, "one-key", "tok_success"))).await;

    for response in &responses {
        // 202 is legitimate for a retry that lands while the first is still
        // talking to the PSP; anything else would be a bug.
        assert!(matches!(response.status, StatusCode::OK | StatusCode::ACCEPTED), "{}", response.body);
    }
    assert_eq!(app.psp.charge_requests(), 1);
    assert_eq!(app.psp.successful_charges(), vec![INVOICE_TOTAL_CENTS]);
    assert_eq!(app.attempt_counts(invoice_id).await, (1, 1));
}
