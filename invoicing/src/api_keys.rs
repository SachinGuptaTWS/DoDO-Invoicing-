use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{app::AppState, auth::AuthenticatedBusiness, error::ApiError, extract::ApiPath};

const KEY_PREFIX: &str = "sk_";
/// "sk_" plus 8 characters: enough to tell keys apart in a dashboard or a
/// leaked-secret scanner, far too little to help guess the rest.
const DISPLAY_PREFIX_LEN: usize = 11;

pub fn hash_key(presented: &str) -> Vec<u8> {
    Sha256::digest(presented.as_bytes()).to_vec()
}

fn generate_secret() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    format!("{KEY_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

#[derive(Serialize, FromRow)]
pub struct ApiKey {
    pub id: Uuid,
    pub display_prefix: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Returned exactly once, at creation. The plaintext is never stored.
#[derive(Serialize)]
pub struct IssuedApiKey {
    #[serde(flatten)]
    pub key: ApiKey,
    pub secret: String,
}

pub async fn issue(conn: &mut PgConnection, business_id: Uuid) -> Result<IssuedApiKey, sqlx::Error> {
    let secret = generate_secret();
    let key = sqlx::query_as::<_, ApiKey>(
        "INSERT INTO api_keys (id, business_id, key_hash, display_prefix)
         VALUES ($1, $2, $3, $4)
         RETURNING id, display_prefix, created_at, revoked_at",
    )
    .bind(Uuid::now_v7())
    .bind(business_id)
    .bind(hash_key(&secret))
    .bind(&secret[..DISPLAY_PREFIX_LEN])
    .fetch_one(conn)
    .await?;
    Ok(IssuedApiKey { key, secret })
}

pub async fn create_api_key(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
) -> Result<(StatusCode, Json<IssuedApiKey>), ApiError> {
    let mut conn = state.db.acquire().await?;
    let issued = issue(&mut conn, business.business_id).await?;
    tracing::info!(business_id = %business.business_id, api_key_id = %issued.key.id, "api key issued");
    Ok((StatusCode::CREATED, Json(issued)))
}

pub async fn list_api_keys(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
) -> Result<Json<Vec<ApiKey>>, ApiError> {
    let keys = sqlx::query_as::<_, ApiKey>(
        "SELECT id, display_prefix, created_at, revoked_at
         FROM api_keys WHERE business_id = $1 ORDER BY id DESC",
    )
    .bind(business.business_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(keys))
}

/// Revocation is immediate: authentication reads `revoked_at` on every
/// request, there is no cache to invalidate. Revoking twice is a no-op.
pub async fn revoke_api_key(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(api_key_id): ApiPath<Uuid>,
) -> Result<Json<ApiKey>, ApiError> {
    let key =
        revoke(&state.db, business.business_id, api_key_id).await?.ok_or_else(|| ApiError::not_found("api key"))?;
    tracing::info!(business_id = %business.business_id, %api_key_id, "api key revoked");
    Ok(Json(key))
}

async fn revoke(db: &PgPool, business_id: Uuid, api_key_id: Uuid) -> Result<Option<ApiKey>, sqlx::Error> {
    sqlx::query_as::<_, ApiKey>(
        "UPDATE api_keys SET revoked_at = COALESCE(revoked_at, now())
         WHERE id = $1 AND business_id = $2
         RETURNING id, display_prefix, created_at, revoked_at",
    )
    .bind(api_key_id)
    .bind(business_id)
    .fetch_optional(db)
    .await
}
