//! The event log. Written in the same transaction as the state change it
//! describes (transactional outbox), so an event exists if and only if the
//! change committed. Webhook deliveries fan out from here, and businesses
//! read it directly (`GET /v1/events`) to reconcile anything they missed.

use axum::{extract::State, Json};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthenticatedBusiness,
    error::ApiError,
    extract::ApiQuery,
    pagination::{resolve_limit, Page},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventType {
    InvoiceCreated,
    InvoicePaid,
    InvoicePaymentFailed,
    InvoiceVoided,
}

impl EventType {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvoiceCreated => "invoice.created",
            Self::InvoicePaid => "invoice.paid",
            Self::InvoicePaymentFailed => "invoice.payment_failed",
            Self::InvoiceVoided => "invoice.voided",
        }
    }
}

/// The webhook body and the `GET /v1/events` item are the same shape, so a
/// receiver can process a replayed event exactly like a delivered one.
#[derive(Serialize, FromRow)]
pub struct Event {
    pub id: Uuid,
    #[serde(rename = "type")]
    pub event_type: String,
    pub created_at: DateTime<Utc>,
    pub data: Value,
}

pub async fn record(
    conn: &mut PgConnection,
    business_id: Uuid,
    event_type: EventType,
    data: Value,
) -> Result<(), sqlx::Error> {
    let event_id = Uuid::now_v7();
    sqlx::query("INSERT INTO events (id, business_id, event_type, data) VALUES ($1, $2, $3, $4)")
        .bind(event_id)
        .bind(business_id)
        .bind(event_type.as_str())
        .bind(&data)
        .execute(&mut *conn)
        .await?;

    // Fan out to the endpoints active right now. An endpoint registered
    // later gets no history pushed to it; it can page through the events API.
    sqlx::query(
        "INSERT INTO webhook_deliveries (event_id, endpoint_id)
         SELECT $1, id FROM webhook_endpoints WHERE business_id = $2 AND disabled_at IS NULL",
    )
    .bind(event_id)
    .bind(business_id)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct ListEventsParams {
    /// Cursor: return events strictly after this id, oldest first.
    pub after: Option<Uuid>,
    pub limit: Option<u32>,
}

/// Oldest-first, unlike other lists: this endpoint is for tailing the log
/// from a checkpoint ("give me everything after the last event I processed").
pub async fn list_events(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiQuery(params): ApiQuery<ListEventsParams>,
) -> Result<Json<Page<Event>>, ApiError> {
    let limit = resolve_limit(params.limit)?;
    let rows = sqlx::query_as::<_, Event>(
        "SELECT id, event_type, created_at, data FROM events
         WHERE business_id = $1 AND ($2::uuid IS NULL OR id > $2)
         ORDER BY id ASC LIMIT $3",
    )
    .bind(business.business_id)
    .bind(params.after)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_overfetch(rows, limit)))
}
