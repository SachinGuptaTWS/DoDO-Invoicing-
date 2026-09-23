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
    // Held until commit, so a business's events get `seq` in commit order.
    // NO KEY UPDATE, not UPDATE: the caller has usually already inserted a row
    // referencing the business, and that FK check holds KEY SHARE. UPDATE
    // conflicts with KEY SHARE, so two such transactions would each wait for
    // the other to release it: a deadlock. NO KEY UPDATE does not conflict.
    sqlx::query("SELECT 1 FROM businesses WHERE id = $1 FOR NO KEY UPDATE")
        .bind(business_id)
        .execute(&mut *conn)
        .await?;

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
#[serde(deny_unknown_fields)]
pub struct ListEventsParams {
    /// Cursor: return events strictly after this id, oldest first.
    pub after: Option<Uuid>,
    pub limit: Option<u32>,
}

/// Oldest-first, unlike other lists: this endpoint is for tailing the log
/// from a checkpoint ("give me everything after the last event I processed").
/// Ordered by commit (`seq`), so no event can appear behind a checkpoint.
pub async fn list_events(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiQuery(params): ApiQuery<ListEventsParams>,
) -> Result<Json<Page<Event>>, ApiError> {
    let limit = resolve_limit(params.limit)?;
    // Typed explicitly: a bare `0` would make this i32, and decoding the
    // bigint `seq` into i32 fails at runtime rather than compile time.
    let after_seq: i64 = match params.after {
        None => 0,
        // An unknown cursor would otherwise return an empty page, which looks
        // exactly like "you are caught up".
        Some(event_id) => sqlx::query_scalar("SELECT seq FROM events WHERE id = $1 AND business_id = $2")
            .bind(event_id)
            .bind(business.business_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| ApiError::validation("after", "after is not an event of this business"))?,
    };
    let rows = sqlx::query_as::<_, Event>(
        "SELECT id, event_type, created_at, data FROM events
         WHERE business_id = $1 AND seq > $2
         ORDER BY seq LIMIT $3",
    )
    .bind(business.business_id)
    .bind(after_seq)
    .bind(limit + 1)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(Page::from_overfetch(rows, limit)))
}
