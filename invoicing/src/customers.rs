use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthenticatedBusiness,
    error::ApiError,
    extract::{ApiJson, ApiPath, ApiQuery},
    pagination::{resolve_limit, Page},
};

#[derive(Serialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateCustomerRequest {
    pub name: String,
    pub email: String,
}

impl CreateCustomerRequest {
    fn validated(self) -> Result<(String, String), ApiError> {
        let name = self.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(ApiError::validation("name", "name must be 1-200 characters"));
        }
        // Deliverability is not our problem here (we never send email); this
        // only rejects values that are obviously not an address.
        let email = self.email.trim().to_owned();
        let plausible = email.len() <= 254
            && !email.contains(char::is_whitespace)
            && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'));
        if !plausible {
            return Err(ApiError::validation("email", "email is not a valid address"));
        }
        Ok((name, email))
    }
}

pub async fn create_customer(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiJson(request): ApiJson<CreateCustomerRequest>,
) -> Result<(StatusCode, Json<Customer>), ApiError> {
    let (name, email) = request.validated()?;
    let customer = sqlx::query_as::<_, Customer>(
        "INSERT INTO customers (id, business_id, name, email) VALUES ($1, $2, $3, $4)
         RETURNING id, name, email, created_at",
    )
    .bind(Uuid::now_v7())
    .bind(business.business_id)
    .bind(name)
    .bind(email)
    .fetch_one(&state.db)
    .await?;
    Ok((StatusCode::CREATED, Json(customer)))
}

pub async fn get_customer(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(customer_id): ApiPath<Uuid>,
) -> Result<Json<Customer>, ApiError> {
    sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers WHERE id = $1 AND business_id = $2",
    )
    .bind(customer_id)
    .bind(business.business_id)
    .fetch_optional(&state.db)
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::not_found("customer"))
}

#[derive(Deserialize)]
pub struct ListCustomersParams {
    pub limit: Option<u32>,
    pub starting_after: Option<Uuid>,
}

pub async fn list_customers(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiQuery(params): ApiQuery<ListCustomersParams>,
) -> Result<Json<Page<Customer>>, ApiError> {
    let limit = resolve_limit(params.limit)?;
    let rows = sqlx::query_as::<_, Customer>(
        "SELECT id, name, email, created_at FROM customers
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id < $2)
         ORDER BY id DESC LIMIT $3",
    )
    .bind(business.business_id)
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_overfetch(rows, limit)))
}
