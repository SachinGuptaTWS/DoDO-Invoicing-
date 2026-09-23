mod common;

use common::{pool, spawn_app, TestOptions};
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn keys_rotate_without_locking_the_business_out(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;
    let keys = app.get("/v1/api_keys").await;
    let first_id = keys.body[0]["id"].as_str().unwrap().to_owned();

    let only = app.delete(&format!("/v1/api_keys/{first_id}")).await;
    assert_eq!(only.status, StatusCode::CONFLICT, "{}", only.body);
    assert_eq!(only.body["error"]["code"], "last_active_key");

    let second = app.post("/v1/api_keys", json!({})).await;
    assert_eq!(second.status, StatusCode::CREATED);
    let second_secret = second.body["secret"].as_str().unwrap().to_owned();

    let revoked = app.delete(&format!("/v1/api_keys/{first_id}")).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    assert!(revoked.body["revoked_at"].is_string());

    // The old key stops working on the very next request; the new one works.
    assert_eq!(app.get("/v1/customers").await.status, StatusCode::UNAUTHORIZED);
    let with_new = app.http.get(format!("{}/v1/customers", app.base_url)).bearer_auth(second_secret).send().await;
    assert_eq!(with_new.unwrap().status(), StatusCode::OK);
}
