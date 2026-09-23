use axum::{extract::State, http::StatusCode, Json};
use chrono::NaiveDate;
use serde::Deserialize;
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    apply_transition, load_detail,
    pricing::{self, LineItemInput, PricedInvoice},
    Invoice, InvoiceDetail, InvoiceStatus, InvoiceTransition, TransitionOutcome, INVOICE_COLUMNS,
};
use crate::{
    app::AppState,
    auth::AuthenticatedBusiness,
    error::ApiError,
    events::{self, EventType},
    extract::{ApiJson, ApiPath, ApiQuery},
    pagination::{resolve_limit, Page},
};

/// There is deliberately no `total_cents` field: with `deny_unknown_fields`
/// a client that sends one gets a 422, rather than having it silently ignored
/// and discovering later that the charged amount differs from what it sent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoiceRequest {
    pub customer_id: Uuid,
    pub due_date: NaiveDate,
    pub line_items: Vec<LineItemInput>,
    pub currency: Option<String>,
}

pub async fn create_invoice(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiJson(request): ApiJson<CreateInvoiceRequest>,
) -> Result<(StatusCode, Json<InvoiceDetail>), ApiError> {
    if request.currency.as_deref().is_some_and(|currency| currency != "USD") {
        return Err(ApiError::validation("currency", "only USD is supported"));
    }
    let priced = pricing::price(request.line_items)?;

    let mut tx = state.db.begin().await?;
    let customer_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM customers WHERE id = $1 AND business_id = $2)")
            .bind(request.customer_id)
            .bind(business.business_id)
            .fetch_one(&mut *tx)
            .await?;
    if !customer_exists {
        return Err(ApiError::not_found("customer"));
    }

    let invoice_id = Uuid::now_v7();
    insert_invoice(&mut tx, business.business_id, request.customer_id, invoice_id, request.due_date, &priced).await?;
    let detail = load_detail(&mut tx, business.business_id, invoice_id).await?.ok_or_else(ApiError::internal)?;
    events::record(&mut tx, business.business_id, EventType::InvoiceCreated, json!({ "invoice": detail })).await?;
    tx.commit().await?;

    tracing::info!(business_id = %business.business_id, %invoice_id, total_cents = priced.total_cents, "invoice created");
    Ok((StatusCode::CREATED, Json(detail)))
}

async fn insert_invoice(
    conn: &mut PgConnection,
    business_id: Uuid,
    customer_id: Uuid,
    invoice_id: Uuid,
    due_date: NaiveDate,
    priced: &PricedInvoice,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO invoices (id, business_id, customer_id, status, total_cents, due_date)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(invoice_id)
    .bind(business_id)
    .bind(customer_id)
    .bind(InvoiceStatus::Open.as_str())
    .bind(priced.total_cents)
    .bind(due_date)
    .execute(&mut *conn)
    .await?;

    let positions: Vec<i32> = (0..).take(priced.line_items.len()).collect();
    let descriptions: Vec<&str> = priced.line_items.iter().map(|item| item.description.as_str()).collect();
    let quantities: Vec<i64> = priced.line_items.iter().map(|item| item.quantity).collect();
    let unit_amounts: Vec<i64> = priced.line_items.iter().map(|item| item.unit_amount_cents).collect();
    let amounts: Vec<i64> = priced.line_items.iter().map(|item| item.amount_cents).collect();

    // One round trip regardless of line item count.
    sqlx::query(
        "INSERT INTO invoice_line_items (invoice_id, position, description, quantity, unit_amount_cents, amount_cents)
         SELECT $1, * FROM UNNEST($2::int4[], $3::text[], $4::int8[], $5::int8[], $6::int8[])",
    )
    .bind(invoice_id)
    .bind(positions)
    .bind(descriptions)
    .bind(quantities)
    .bind(unit_amounts)
    .bind(amounts)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn get_invoice(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(invoice_id): ApiPath<Uuid>,
) -> Result<Json<InvoiceDetail>, ApiError> {
    let mut conn = state.db.acquire().await?;
    load_detail(&mut conn, business.business_id, invoice_id)
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("invoice"))
}

#[derive(Deserialize)]
pub struct ListInvoicesParams {
    pub status: Option<InvoiceStatus>,
    pub limit: Option<u32>,
    pub starting_after: Option<Uuid>,
}

/// Returns invoice summaries; line items and attempts are on the detail
/// endpoint so a list page is one query, not 1 + 2N.
pub async fn list_invoices(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiQuery(params): ApiQuery<ListInvoicesParams>,
) -> Result<Json<Page<Invoice>>, ApiError> {
    let limit = resolve_limit(params.limit)?;
    let rows = sqlx::query_as::<_, Invoice>(&format!(
        "SELECT {INVOICE_COLUMNS} FROM invoices
         WHERE business_id = $1
           AND ($2::text IS NULL OR status = $2)
           AND ($3::uuid IS NULL OR id < $3)
         ORDER BY id DESC LIMIT $4"
    ))
    .bind(business.business_id)
    .bind(params.status.map(InvoiceStatus::as_str))
    .bind(params.starting_after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_overfetch(rows, limit)))
}

pub async fn void_invoice(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(invoice_id): ApiPath<Uuid>,
) -> Result<Json<InvoiceDetail>, ApiError> {
    let transition = InvoiceTransition::Void;
    let mut tx = state.db.begin().await?;
    match apply_transition(&mut tx, business.business_id, invoice_id, transition).await? {
        TransitionOutcome::Applied(_) => {}
        TransitionOutcome::Rejected(current) => return Err(transition.rejection(current)),
        TransitionOutcome::NotFound => return Err(ApiError::not_found("invoice")),
    }
    let detail = load_detail(&mut tx, business.business_id, invoice_id).await?.ok_or_else(ApiError::internal)?;
    events::record(&mut tx, business.business_id, EventType::InvoiceVoided, json!({ "invoice": detail })).await?;
    tx.commit().await?;

    tracing::info!(business_id = %business.business_id, %invoice_id, "invoice voided");
    Ok(Json(detail))
}
