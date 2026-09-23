#![allow(dead_code)] // each test binary uses a different subset

use std::{net::SocketAddr, time::Duration};

use harness::{
    mock_psp::{MockPsp, MockPspConfig},
    webhook_sink::WebhookSink,
};
use invoicing::{app, config::Config, workers};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const INVOICE_TOTAL_CENTS: i64 = 4_998;
const ADMIN_TOKEN: &str = "test-admin-token-0123456789";

pub struct TestOptions {
    pub psp: MockPspConfig,
    pub psp_timeout: Duration,
    pub psp_not_found_grace: Duration,
}

impl Default for TestOptions {
    fn default() -> Self {
        Self {
            psp: MockPspConfig { decision_delay: Duration::from_millis(100), slow_delay: Duration::from_secs(30) },
            psp_timeout: Duration::from_secs(2),
            psp_not_found_grace: Duration::from_secs(3),
        }
    }
}

pub struct TestApp {
    pub base_url: String,
    pub http: reqwest::Client,
    pub db: PgPool,
    pub psp: MockPsp,
    pub sink: WebhookSink,
    pub sink_url: String,
    pub api_key: String,
    shutdown: CancellationToken,
}

impl Drop for TestApp {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// sqlx's test harness caps child pools at its 20-connection master pool.
/// 16 is enough for requests to contend on the invoice row lock (the claim
/// transaction is a few ms) rather than only on connection checkout.
pub async fn pool(pool_options: PgPoolOptions, connect_options: sqlx::postgres::PgConnectOptions) -> PgPool {
    pool_options.max_connections(16).connect_with(connect_options).await.expect("test database")
}

pub async fn spawn_app(db: PgPool, options: TestOptions) -> TestApp {
    let psp = MockPsp::new(options.psp);
    let psp_addr = serve(psp.router()).await;
    let sink = WebhookSink::default();
    let sink_addr = serve(sink.router()).await;

    let config = Config {
        database_url: String::new(),
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        admin_token: ADMIN_TOKEN.into(),
        psp_base_url: format!("http://{psp_addr}"),
        psp_timeout: options.psp_timeout,
        psp_not_found_grace: options.psp_not_found_grace,
        webhook_timeout: Duration::from_secs(2),
        worker_poll_interval: Duration::from_millis(50),
    };
    config.validate().expect("valid test config");

    let state = app::AppState::new(db.clone(), config).unwrap();
    let shutdown = CancellationToken::new();
    workers::spawn(&state, shutdown.clone()).unwrap();
    let app_addr = serve(app::router(state)).await;

    let http = reqwest::Client::new();
    let base_url = format!("http://{app_addr}");
    let created: Value = http
        .post(format!("{base_url}/v1/admin/businesses"))
        .bearer_auth(ADMIN_TOKEN)
        .json(&json!({ "name": "Acme Test Co" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let api_key = created["api_key"]["secret"].as_str().unwrap().to_owned();

    TestApp { base_url, http, db, psp, sink, sink_url: format!("http://{sink_addr}"), api_key, shutdown }
}

async fn serve(router: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

pub struct ApiResponse {
    pub status: StatusCode,
    pub replayed: bool,
    pub body: Value,
}

impl TestApp {
    pub async fn post(&self, path: &str, body: Value) -> ApiResponse {
        self.send(self.http.post(format!("{}{path}", self.base_url)).json(&body)).await
    }

    pub async fn delete(&self, path: &str) -> ApiResponse {
        self.send(self.http.delete(format!("{}{path}", self.base_url))).await
    }

    pub async fn get(&self, path: &str) -> ApiResponse {
        self.send(self.http.get(format!("{}{path}", self.base_url))).await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> ApiResponse {
        let response = request.bearer_auth(&self.api_key).send().await.unwrap();
        ApiResponse {
            status: response.status(),
            replayed: response.headers().contains_key("idempotent-replayed"),
            body: response.json().await.unwrap_or(Value::Null),
        }
    }

    pub async fn pay(&self, invoice_id: Uuid, idempotency_key: &str, card_token: &str) -> ApiResponse {
        self.send(
            self.http
                .post(format!("{}/v1/invoices/{invoice_id}/pay", self.base_url))
                .header("Idempotency-Key", idempotency_key)
                .json(&json!({ "card_token": card_token })),
        )
        .await
    }

    /// Creates a customer and an open invoice totalling `INVOICE_TOTAL_CENTS`.
    pub async fn open_invoice(&self) -> Uuid {
        let customer = self.post("/v1/customers", json!({ "name": "Ada", "email": "ada@example.com" })).await;
        assert_eq!(customer.status, StatusCode::CREATED, "{}", customer.body);
        let invoice = self
            .post(
                "/v1/invoices",
                json!({
                    "customer_id": customer.body["id"],
                    "due_date": "2026-12-31",
                    "line_items": [
                        { "description": "Pro plan seat", "quantity": 2, "unit_amount_cents": 2_000 },
                        { "description": "Overage", "quantity": 1, "unit_amount_cents": 998 }
                    ]
                }),
            )
            .await;
        assert_eq!(invoice.status, StatusCode::CREATED, "{}", invoice.body);
        assert_eq!(invoice.body["total_cents"], INVOICE_TOTAL_CENTS);
        invoice.body["id"].as_str().unwrap().parse().unwrap()
    }

    pub async fn invoice_status(&self, invoice_id: Uuid) -> String {
        let invoice = self.get(&format!("/v1/invoices/{invoice_id}")).await;
        invoice.body["status"].as_str().unwrap().to_owned()
    }

    pub async fn wait_for_invoice_status(&self, invoice_id: Uuid, expected: &str, within: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let invoice = self.get(&format!("/v1/invoices/{invoice_id}")).await;
            if invoice.body["status"] == expected {
                return invoice.body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "invoice never reached {expected}; last seen: {}",
                invoice.body
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// (total attempts, succeeded attempts) straight from the database.
    pub async fn attempt_counts(&self, invoice_id: Uuid) -> (i64, i64) {
        sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE status = 'succeeded') FROM payment_attempts WHERE invoice_id = $1",
        )
        .bind(invoice_id)
        .fetch_one(&self.db)
        .await
        .unwrap()
    }
}
