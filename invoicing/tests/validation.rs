mod common;

use common::{pool, spawn_app, TestOptions};
use reqwest::StatusCode;
use serde_json::json;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Inputs that used to reach the database, or were answered with the wrong
/// status. None of them may produce a 500.
#[sqlx::test(migrator = "invoicing::MIGRATOR")]
async fn bad_input_is_a_client_error(opts: PgPoolOptions, conn: PgConnectOptions) {
    let app = spawn_app(pool(opts, conn).await, TestOptions::default()).await;

    // Postgres text cannot hold NUL; this was an insert failure (500).
    let nul = app.post("/v1/customers", json!({ "name": "Ada\u{0}", "email": "ada@example.com" })).await;
    assert_eq!(nul.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", nul.body);
    assert_eq!(nul.body["error"]["details"]["field"], "name");

    let customer = app.post("/v1/customers", json!({ "name": "Ada", "email": "ada@example.com" })).await;
    let invoice = |line_item: serde_json::Value| json!({ "customer_id": customer.body["id"], "due_date": "2026-12-31", "line_items": [line_item] });

    let float = app
        .post("/v1/invoices", invoice(json!({ "description": "x", "quantity": 1, "unit_amount_cents": 19.99 })))
        .await;
    assert_eq!(float.status, StatusCode::UNPROCESSABLE_ENTITY);

    let mut with_total = invoice(json!({ "description": "x", "quantity": 1, "unit_amount_cents": 100 }));
    with_total["total_cents"] = json!(1);
    let client_total = app.post("/v1/invoices", with_total).await;
    assert_eq!(client_total.status, StatusCode::UNPROCESSABLE_ENTITY);

    // The docx calls it "state"; both names filter, and a typo is refused
    // rather than silently returning every invoice.
    let by_state = app.get("/v1/invoices?state=paid").await;
    assert_eq!((by_state.status, &by_state.body["data"]), (StatusCode::OK, &json!([])));
    assert_eq!(app.get("/v1/invoices?stauts=paid").await.status, StatusCode::BAD_REQUEST);

    let no_content_type = app
        .http
        .post(format!("{}/v1/customers", app.base_url))
        .bearer_auth(&app.api_key)
        .body(r#"{"name":"Ada","email":"ada@example.com"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(no_content_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let lowercase_scheme = app
        .http
        .get(format!("{}/v1/customers", app.base_url))
        .header("Authorization", format!("bearer {}", app.api_key))
        .send()
        .await
        .unwrap();
    assert_eq!(lowercase_scheme.status(), StatusCode::OK);
}
