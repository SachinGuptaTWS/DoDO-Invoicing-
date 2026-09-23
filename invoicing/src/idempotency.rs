//! Idempotency for `POST /invoices/{id}/pay`.
//!
//! A key is reserved in the same transaction that claims the invoice and
//! creates the payment attempt, so a key row is never visible half-done: it
//! either has a stored response (the request finished) or a payment attempt
//! (money may be moving and the response is not final yet).

use axum::{
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiError;

pub const HEADER: &str = "idempotency-key";

pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn from_headers(headers: &HeaderMap) -> Result<Self, ApiError> {
        let raw = headers
            .get(HEADER)
            .ok_or_else(|| ApiError::invalid_request("The Idempotency-Key header is required for payments"))?;
        let key = raw
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|key| (1..=255).contains(&key.len()))
            .ok_or_else(|| ApiError::invalid_request("Idempotency-Key must be 1-255 visible ASCII characters"))?;
        Ok(Self(key.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Hash of what the request *means*: method, path, and the body re-serialized
/// from parsed JSON (serde_json sorts object keys), so whitespace or key order
/// differences do not count as a different request.
pub struct RequestFingerprint([u8; 32]);

impl RequestFingerprint {
    pub fn new(method: &str, path: &str, body: &Value) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(method.as_bytes());
        hasher.update(b"\n");
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
        hasher.update(body.to_string().as_bytes());
        Self(hasher.finalize().into())
    }
}

#[derive(Debug, Clone)]
pub struct StoredResponse {
    pub status: StatusCode,
    pub body: Value,
}

impl StoredResponse {
    pub fn from_error(error: &ApiError) -> Self {
        Self { status: error.status(), body: error.body() }
    }

    /// Replays carry a marker header so clients (and support) can tell a
    /// replay from a fresh execution.
    pub fn into_replay(self) -> Response {
        let mut response = self.into_response();
        response.headers_mut().insert("idempotent-replayed", HeaderValue::from_static("true"));
        response
    }
}

impl IntoResponse for StoredResponse {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

pub enum Reservation {
    Reserved,
    /// The key was used before: replay it, or report that it is still settling.
    Existing(ExistingKey),
}

pub struct ExistingKey {
    fingerprint: Vec<u8>,
    payment_attempt_id: Option<Uuid>,
    response: Option<StoredResponse>,
}

pub enum PriorUse {
    Completed(StoredResponse),
    AwaitingSettlement { payment_attempt_id: Uuid },
}

impl ExistingKey {
    pub fn resolve(self, fingerprint: &RequestFingerprint) -> Result<PriorUse, ApiError> {
        if self.fingerprint != fingerprint.0 {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "This Idempotency-Key was already used with a different request",
            ));
        }
        match (self.response, self.payment_attempt_id) {
            (Some(response), _) => Ok(PriorUse::Completed(response)),
            (None, Some(payment_attempt_id)) => Ok(PriorUse::AwaitingSettlement { payment_attempt_id }),
            // Unreachable given how rows are written; refuse rather than guess.
            (None, None) => Err(ApiError::conflict("idempotency_key_in_use", "Retry this request shortly")),
        }
    }
}

/// If another transaction is reserving the same key right now, the INSERT
/// blocks on the unique index until that transaction commits, and then sees
/// the conflict. Same-key races therefore resolve to one execution + replays.
pub async fn reserve(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &IdempotencyKey,
    fingerprint: &RequestFingerprint,
) -> Result<Reservation, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (business_id, key, request_fingerprint) VALUES ($1, $2, $3)
         ON CONFLICT (business_id, key) DO NOTHING",
    )
    .bind(business_id)
    .bind(key.as_str())
    .bind(fingerprint.0.as_slice())
    .execute(&mut *conn)
    .await?
    .rows_affected();

    if inserted == 1 {
        return Ok(Reservation::Reserved);
    }

    let (fingerprint, payment_attempt_id, status, body): (Vec<u8>, Option<Uuid>, Option<i16>, Option<Value>) =
        sqlx::query_as(
            "SELECT request_fingerprint, payment_attempt_id, response_status, response_body
             FROM idempotency_keys WHERE business_id = $1 AND key = $2",
        )
        .bind(business_id)
        .bind(key.as_str())
        .fetch_one(conn)
        .await?;

    let response = match (status, body) {
        (Some(status), Some(body)) => Some(StoredResponse { status: stored_status(status), body }),
        _ => None,
    };
    Ok(Reservation::Existing(ExistingKey { fingerprint, payment_attempt_id, response }))
}

pub async fn attach_attempt(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &IdempotencyKey,
    payment_attempt_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE idempotency_keys SET payment_attempt_id = $3 WHERE business_id = $1 AND key = $2")
        .bind(business_id)
        .bind(key.as_str())
        .bind(payment_attempt_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn complete(
    conn: &mut PgConnection,
    business_id: Uuid,
    key: &IdempotencyKey,
    response: &StoredResponse,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE idempotency_keys SET response_status = $3, response_body = $4, completed_at = now()
         WHERE business_id = $1 AND key = $2",
    )
    .bind(business_id)
    .bind(key.as_str())
    .bind(status_column(response.status))
    .bind(&response.body)
    .execute(conn)
    .await?;
    Ok(())
}

/// Called when an attempt settles, from the request path or the reconciler:
/// whoever settles the attempt writes the final answer for its key.
pub async fn complete_for_attempt(
    conn: &mut PgConnection,
    payment_attempt_id: Uuid,
    response: &StoredResponse,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE idempotency_keys SET response_status = $2, response_body = $3, completed_at = now()
         WHERE payment_attempt_id = $1 AND response_status IS NULL",
    )
    .bind(payment_attempt_id)
    .bind(status_column(response.status))
    .bind(&response.body)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn response_for_attempt(
    conn: &mut PgConnection,
    payment_attempt_id: Uuid,
) -> Result<Option<StoredResponse>, sqlx::Error> {
    let row: Option<(Option<i16>, Option<Value>)> = sqlx::query_as(
        "SELECT response_status, response_body FROM idempotency_keys WHERE payment_attempt_id = $1",
    )
    .bind(payment_attempt_id)
    .fetch_optional(conn)
    .await?;
    Ok(match row {
        Some((Some(status), Some(body))) => Some(StoredResponse { status: stored_status(status), body }),
        _ => None,
    })
}

// Postgres has no unsigned 16-bit type; HTTP status codes fit in i16.
fn status_column(status: StatusCode) -> i16 {
    i16::try_from(status.as_u16()).expect("HTTP status codes are < 1000")
}

fn stored_status(column: i16) -> StatusCode {
    u16::try_from(column)
        .ok()
        .and_then(|code| StatusCode::from_u16(code).ok())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}
