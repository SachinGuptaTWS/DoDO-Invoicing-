use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthenticatedBusiness,
    error::ApiError,
    extract::{ApiJson, ApiPath},
};

#[derive(Serialize, FromRow)]
pub struct WebhookEndpoint {
    pub id: Uuid,
    pub url: String,
    pub created_at: DateTime<Utc>,
    pub disabled_at: Option<DateTime<Utc>>,
}

/// The signing secret is only returned at creation, like an API key.
#[derive(Serialize)]
pub struct CreatedWebhookEndpoint {
    #[serde(flatten)]
    pub endpoint: WebhookEndpoint,
    pub signing_secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateWebhookEndpointRequest {
    pub url: String,
}

pub async fn create_webhook_endpoint(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiJson(request): ApiJson<CreateWebhookEndpointRequest>,
) -> Result<(StatusCode, Json<CreatedWebhookEndpoint>), ApiError> {
    let url = reqwest::Url::parse(request.url.trim())
        .ok()
        .filter(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
        .filter(|url| url.as_str().len() <= 2048)
        .ok_or_else(|| ApiError::validation("url", "url must be an absolute http(s) URL"))?;

    let signing_secret = generate_signing_secret();
    let endpoint = sqlx::query_as::<_, WebhookEndpoint>(
        "INSERT INTO webhook_endpoints (id, business_id, url, signing_secret) VALUES ($1, $2, $3, $4)
         RETURNING id, url, created_at, disabled_at",
    )
    .bind(Uuid::now_v7())
    .bind(business.business_id)
    .bind(url.as_str())
    .bind(&signing_secret)
    .fetch_one(&state.db)
    .await?;

    tracing::info!(business_id = %business.business_id, endpoint_id = %endpoint.id, "webhook endpoint registered");
    Ok((StatusCode::CREATED, Json(CreatedWebhookEndpoint { endpoint, signing_secret })))
}

pub async fn list_webhook_endpoints(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
) -> Result<Json<Vec<WebhookEndpoint>>, ApiError> {
    let endpoints = sqlx::query_as::<_, WebhookEndpoint>(
        "SELECT id, url, created_at, disabled_at FROM webhook_endpoints WHERE business_id = $1 ORDER BY id DESC",
    )
    .bind(business.business_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(endpoints))
}

/// Disables rather than deletes: delivery history keeps referencing it.
/// Undelivered events for the endpoint are cancelled, not dropped silently;
/// they remain readable from the events API.
pub async fn disable_webhook_endpoint(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(endpoint_id): ApiPath<Uuid>,
) -> Result<Json<WebhookEndpoint>, ApiError> {
    let mut tx = state.db.begin().await?;
    let endpoint = sqlx::query_as::<_, WebhookEndpoint>(
        "UPDATE webhook_endpoints SET disabled_at = COALESCE(disabled_at, now())
         WHERE id = $1 AND business_id = $2
         RETURNING id, url, created_at, disabled_at",
    )
    .bind(endpoint_id)
    .bind(business.business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("webhook endpoint"))?;
    sqlx::query(
        "UPDATE webhook_deliveries SET status = 'cancelled', last_error = 'endpoint disabled'
         WHERE endpoint_id = $1 AND status = 'pending'",
    )
    .bind(endpoint_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(endpoint))
}

fn generate_signing_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("whsec_{}", URL_SAFE_NO_PAD.encode(bytes))
}
