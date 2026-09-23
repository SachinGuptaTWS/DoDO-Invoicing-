use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    api_keys::{self, IssuedApiKey},
    app::AppState,
    auth::AdminAccess,
    error::ApiError,
    extract::ApiJson,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateBusinessRequest {
    pub name: String,
}

#[derive(Serialize, FromRow)]
pub struct Business {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub struct CreatedBusiness {
    pub business: Business,
    pub api_key: IssuedApiKey,
}

/// Signup is out of scope; this operator endpoint is how a business and its
/// first key come to exist. Both are created atomically so a business can
/// never exist without a way to authenticate.
pub async fn create_business(
    State(state): State<AppState>,
    _admin: AdminAccess,
    ApiJson(request): ApiJson<CreateBusinessRequest>,
) -> Result<(StatusCode, Json<CreatedBusiness>), ApiError> {
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 200 {
        return Err(ApiError::validation("name", "name must be 1-200 characters"));
    }

    let mut tx = state.db.begin().await?;
    let business = sqlx::query_as::<_, Business>(
        "INSERT INTO businesses (id, name) VALUES ($1, $2) RETURNING id, name, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(name)
    .fetch_one(&mut *tx)
    .await?;
    let api_key = api_keys::issue(&mut tx, business.id).await?;
    tx.commit().await?;

    tracing::info!(business_id = %business.id, "business created");
    Ok((StatusCode::CREATED, Json(CreatedBusiness { business, api_key })))
}
