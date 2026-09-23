use std::sync::Arc;

use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderName, Request},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceBuilder;
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    trace::{DefaultOnResponse, TraceLayer},
};
use tracing::Level;

use crate::{
    api_keys, businesses, config::Config, customers, error::ApiError, events, invoices::handlers as invoices, payments,
    psp::PspClient, webhooks::endpoints as webhook_endpoints,
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub psp: PspClient,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let psp = PspClient::new(&config.psp_base_url, config.psp_timeout)?;
        Ok(Self { db, psp, config: Arc::new(config) })
    }
}

/// The largest valid body is an invoice with 100 line items of 500-character
/// descriptions. JSON may escape a character as a 12-byte surrogate pair
/// (`\uD83D\uDE00`), so that is ~600 KB; 1 MiB covers it with room to spare.
const MAX_BODY_BYTES: usize = 1024 * 1024;

pub fn router(state: AppState) -> Router {
    let request_id = HeaderName::from_static("x-request-id");

    Router::new()
        .route("/healthz", get(health))
        .route("/v1/admin/businesses", post(businesses::create_business))
        .route("/v1/api_keys", post(api_keys::create_api_key).get(api_keys::list_api_keys))
        .route("/v1/api_keys/{id}", delete(api_keys::revoke_api_key))
        .route("/v1/customers", post(customers::create_customer).get(customers::list_customers))
        .route("/v1/customers/{id}", get(customers::get_customer))
        .route("/v1/invoices", post(invoices::create_invoice).get(invoices::list_invoices))
        .route("/v1/invoices/{id}", get(invoices::get_invoice))
        .route("/v1/invoices/{id}/pay", post(payments::pay_invoice))
        .route("/v1/invoices/{id}/void", post(invoices::void_invoice))
        .route(
            "/v1/webhook_endpoints",
            post(webhook_endpoints::create_webhook_endpoint).get(webhook_endpoints::list_webhook_endpoints),
        )
        .route("/v1/webhook_endpoints/{id}", delete(webhook_endpoints::disable_webhook_endpoint))
        .route("/v1/events", get(events::list_events))
        .fallback(|| async { ApiError::not_found("route") })
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::new(request_id.clone(), MakeRequestUuid))
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(|request: &Request<_>| {
                            let request_id = request
                                .headers()
                                .get("x-request-id")
                                .and_then(|value| value.to_str().ok())
                                .unwrap_or_default();
                            tracing::info_span!(
                                "http",
                                method = %request.method(),
                                path = %request.uri().path(),
                                request_id,
                            )
                        })
                        .on_response(DefaultOnResponse::new().level(Level::INFO)),
                )
                .layer(PropagateRequestIdLayer::new(request_id)),
        )
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(json!({ "status": "ok" })))
}
