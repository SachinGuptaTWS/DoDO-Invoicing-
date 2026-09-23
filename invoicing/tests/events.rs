mod common;

use std::time::Duration;

use common::{pool, spawn_app, TestOptions};
use invoicing::events::{self, EventType};
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

/// An event whose transaction is still open must hold back events that commit
/// after it. Otherwise a consumer polling in between checkpoints past the
/// newer one and never sees the older one.
#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn the_event_feed_never_skips_a_late_commit(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let business_id: Uuid = sqlx::query_scalar("SELECT id FROM businesses").fetch_one(&app.db).await.unwrap();

    let mut slow = app.db.begin().await.unwrap();
    events::record(&mut slow, business_id, EventType::InvoiceVoided, json!({})).await.unwrap();

    tokio::join!(app.open_invoice(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let seen = app.get("/v1/events").await;
        assert_eq!(seen.body["data"], json!([]), "a newer event overtook an uncommitted one");
        slow.commit().await.unwrap();
    });

    let feed = app.get("/v1/events").await;
    let types: Vec<&str> = feed.body["data"].as_array().unwrap().iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["invoice.voided", "invoice.created"]);

    let first = feed.body["data"][0]["id"].as_str().unwrap();
    let rest = app.get(&format!("/v1/events?after={first}")).await;
    assert_eq!(rest.body["data"][0]["type"], "invoice.created");

    let bogus = app.get(&format!("/v1/events?after={}", Uuid::now_v7())).await;
    assert_eq!(bogus.body["error"]["code"], "validation_failed");
}
