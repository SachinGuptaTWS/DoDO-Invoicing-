use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use rand::{rngs::OsRng, RngCore};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{FromRow, PgConnection};
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
///
/// The last active key cannot be revoked. Only an operator can create a
/// business's first key, so without this a business could lock itself out
/// for good. Rotation is: issue a new key, deploy it, revoke the old one.
pub async fn revoke_api_key(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(api_key_id): ApiPath<Uuid>,
) -> Result<Json<ApiKey>, ApiError> {
    let mut tx = state.db.begin().await?;
    // Serializes revocations within a business. Without it, two concurrent
    // requests could each see the other key as still active and revoke both.
    sqlx::query("SELECT 1 FROM businesses WHERE id = $1 FOR UPDATE")
        .bind(business.business_id)
        .execute(&mut *tx)
        .await?;

    let key = sqlx::query_as::<_, ApiKey>(
        "SELECT id, display_prefix, created_at, revoked_at FROM api_keys WHERE id = $1 AND business_id = $2",
    )
    .bind(api_key_id)
    .bind(business.business_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("api key"))?;
    if key.revoked_at.is_some() {
        return Ok(Json(key));
    }

    let another_active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM api_keys WHERE business_id = $1 AND id <> $2 AND revoked_at IS NULL)",
    )
    .bind(business.business_id)
    .bind(api_key_id)
    .fetch_one(&mut *tx)
    .await?;
    if !another_active {
        return Err(ApiError::conflict(
            "last_active_key",
            "This is the only active API key; issue a new one before revoking it",
        ));
    }

    let key = sqlx::query_as::<_, ApiKey>(
        "UPDATE api_keys SET revoked_at = now() WHERE id = $1
         RETURNING id, display_prefix, created_at, revoked_at",
    )
    .bind(api_key_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    tracing::info!(business_id = %business.business_id, %api_key_id, "api key revoked");
    Ok(Json(key))
}
