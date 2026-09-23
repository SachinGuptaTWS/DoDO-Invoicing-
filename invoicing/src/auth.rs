use axum::{
    extract::FromRequestParts,
    http::{header::AUTHORIZATION, request::Parts, HeaderMap},
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{api_keys, app::AppState, error::ApiError};

/// The business on whose behalf a request is made. Every query that touches
/// tenant data takes `business_id` from here, never from the request body.
#[derive(Debug, Clone, Copy)]
pub struct AuthenticatedBusiness {
    pub business_id: Uuid,
    pub api_key_id: Uuid,
}

impl FromRequestParts<AppState> for AuthenticatedBusiness {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let presented = bearer_token(&parts.headers).ok_or_else(|| {
            ApiError::unauthorized("missing_api_key", "Send your API key as `Authorization: Bearer <key>`")
        })?;

        // Looking up by hash means no secret-dependent comparison happens in
        // our code, so there is no timing side channel to reason about.
        let row: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT id, business_id FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL")
                .bind(api_keys::hash_key(presented))
                .fetch_optional(&state.db)
                .await?;

        // Same response for unknown and revoked keys: no oracle for which
        // leaked keys are still live.
        let (api_key_id, business_id) =
            row.ok_or_else(|| ApiError::unauthorized("invalid_api_key", "API key is invalid or revoked"))?;

        Ok(Self { business_id, api_key_id })
    }
}

/// Operator access for bootstrapping businesses. Not a tenant credential.
pub struct AdminAccess;

impl FromRequestParts<AppState> for AdminAccess {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let presented = bearer_token(&parts.headers)
            .ok_or_else(|| ApiError::unauthorized("missing_admin_token", "Admin token required"))?;
        // Comparing digests rather than raw strings: an early-exit compare on
        // a hash leaks nothing about the token itself.
        if Sha256::digest(presented) == Sha256::digest(&state.config.admin_token) {
            Ok(Self)
        } else {
            Err(ApiError::unauthorized("invalid_admin_token", "Admin token is invalid"))
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}
