//! `POST /v1/invoices/{id}/pay`.
//!
//! The request runs as three steps, and the transaction boundaries are the
//! design:
//!
//! 1. **Claim** (one short transaction): reserve the idempotency key, move
//!    the invoice `open → processing` with a compare-and-set, insert a
//!    `pending` attempt. Commit. Losers of a race stop here with a 409.
//! 2. **Charge** (no transaction held): call the PSP with the attempt id as
//!    its idempotency key, bounded by `PSP_TIMEOUT_MS`. Holding a row lock or
//!    a pooled connection across a network call to a slow dependency is how a
//!    PSP brownout becomes our outage.
//! 3. **Settle** (one short transaction), only on a definitive PSP answer:
//!    attempt → succeeded/failed, invoice → paid/open, outbox event, stored
//!    idempotent response. If the answer is not definitive, respond 202 and
//!    leave the attempt for the reconciler, which runs the same settle code.
//!
//! A crash between any two steps leaves a `pending` attempt with a
//! `reconcile_after` timestamp, which is exactly the state the reconciler
//! resolves. There is no crash window that loses or duplicates a charge.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{FromRow, PgConnection, PgPool};
use uuid::Uuid;

use crate::{
    app::AppState,
    auth::AuthenticatedBusiness,
    error::ApiError,
    events::{self, EventType},
    extract::ApiPath,
    idempotency::{self, IdempotencyKey, PriorUse, RequestFingerprint, Reservation, StoredResponse},
    invoices::{self, Invoice, InvoiceTransition, TransitionOutcome},
    psp::{ChargeResult, Settlement},
};

pub(crate) const ATTEMPT_COLUMNS: &str =
    "id, invoice_id, status, amount_cents, psp_ref, failure_code, created_at, settled_at";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentAttemptStatus {
    Pending,
    Succeeded,
    Failed,
}

impl TryFrom<String> for PaymentAttemptStatus {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown payment attempt status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct PaymentAttempt {
    pub id: Uuid,
    pub invoice_id: Uuid,
    #[sqlx(try_from = "String")]
    pub status: PaymentAttemptStatus,
    pub amount_cents: i64,
    pub psp_ref: Option<String>,
    pub failure_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

pub async fn attempts_for_invoice(conn: &mut PgConnection, invoice_id: Uuid) -> Result<Vec<PaymentAttempt>, sqlx::Error> {
    sqlx::query_as::<_, PaymentAttempt>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM payment_attempts WHERE invoice_id = $1 ORDER BY id"
    ))
    .bind(invoice_id)
    .fetch_all(conn)
    .await
}

async fn find_attempt(
    conn: &mut PgConnection,
    business_id: Uuid,
    attempt_id: Uuid,
) -> Result<Option<PaymentAttempt>, sqlx::Error> {
    sqlx::query_as::<_, PaymentAttempt>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM payment_attempts WHERE id = $1 AND business_id = $2"
    ))
    .bind(attempt_id)
    .bind(business_id)
    .fetch_optional(conn)
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayInvoiceRequest {
    pub card_token: String,
}

pub async fn pay_invoice(
    State(state): State<AppState>,
    business: AuthenticatedBusiness,
    ApiPath(invoice_id): ApiPath<Uuid>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = IdempotencyKey::from_headers(&headers)?;
    let (request, body_value) = parse_request(&body)?;
    let fingerprint = RequestFingerprint::new("POST", &format!("/v1/invoices/{invoice_id}/pay"), &body_value);

    let claim = match claim_invoice(&state, business.business_id, invoice_id, &key, &fingerprint).await? {
        Claim::Claimed(claim) => claim,
        Claim::Rejected(response) => return Ok(response.into_response()),
        Claim::Replay(response) => return Ok(response.into_replay()),
        Claim::AwaitingSettlement { payment_attempt_id } => {
            return pending_response(&state.db, business.business_id, payment_attempt_id).await;
        }
    };

    match state.psp.create_charge(claim.payment_attempt_id, claim.amount_cents, &request.card_token).await {
        ChargeResult::Settled(settlement) => {
            // If this fails (e.g. DB blip) the attempt stays pending and the
            // reconciler settles it; the client's retry replays the result.
            let response = settle_attempt(&state.db, claim.payment_attempt_id, settlement).await?;
            Ok(response.into_response())
        }
        ChargeResult::Indeterminate(reason) => {
            tracing::warn!(
                payment_attempt_id = %claim.payment_attempt_id,
                %invoice_id,
                %reason,
                "PSP outcome unknown; leaving attempt pending for reconciliation"
            );
            pending_response(&state.db, business.business_id, claim.payment_attempt_id).await
        }
    }
}

fn parse_request(body: &[u8]) -> Result<(PayInvoiceRequest, Value), ApiError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|err| ApiError::invalid_request(format!("Request body is not valid JSON: {err}")))?;
    let request: PayInvoiceRequest = serde_json::from_value(value.clone())
        .map_err(|err| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation_failed", err.to_string()))?;
    if request.card_token.is_empty() || request.card_token.len() > 255 {
        return Err(ApiError::validation("card_token", "card_token must be 1-255 characters"));
    }
    Ok((request, value))
}

struct PaymentClaim {
    payment_attempt_id: Uuid,
    amount_cents: i64,
}

enum Claim {
    Claimed(PaymentClaim),
    /// First use of this key, and the invoice cannot be paid. The rejection
    /// is stored against the key, so retries get the same answer.
    Rejected(StoredResponse),
    Replay(StoredResponse),
    AwaitingSettlement { payment_attempt_id: Uuid },
}

async fn claim_invoice(
    state: &AppState,
    business_id: Uuid,
    invoice_id: Uuid,
    key: &IdempotencyKey,
    fingerprint: &RequestFingerprint,
) -> Result<Claim, ApiError> {
    let mut tx = state.db.begin().await?;

    if let Reservation::Existing(existing) = idempotency::reserve(&mut tx, business_id, key, fingerprint).await? {
        return Ok(match existing.resolve(fingerprint)? {
            PriorUse::Completed(response) => Claim::Replay(response),
            PriorUse::AwaitingSettlement { payment_attempt_id } => Claim::AwaitingSettlement { payment_attempt_id },
        });
    }

    let transition = InvoiceTransition::BeginPayment;
    let invoice = match invoices::apply_transition(&mut tx, business_id, invoice_id, transition).await? {
        TransitionOutcome::Applied(invoice) => invoice,
        TransitionOutcome::Rejected(current) => {
            let response = StoredResponse::from_error(&transition.rejection(current));
            idempotency::complete(&mut tx, business_id, key, &response).await?;
            tx.commit().await?;
            return Ok(Claim::Rejected(response));
        }
        TransitionOutcome::NotFound => {
            let response = StoredResponse::from_error(&ApiError::not_found("invoice"));
            idempotency::complete(&mut tx, business_id, key, &response).await?;
            tx.commit().await?;
            return Ok(Claim::Rejected(response));
        }
    };

    // First reconciler look: just after the request path would have given
    // up on the PSP. Normally the request settles the attempt first.
    let payment_attempt_id = Uuid::now_v7();
    let first_reconcile_in = state.config.psp_timeout + std::time::Duration::from_secs(1);
    sqlx::query(
        "INSERT INTO payment_attempts (id, invoice_id, business_id, status, amount_cents, reconcile_after)
         VALUES ($1, $2, $3, 'pending', $4, now() + make_interval(secs => $5))",
    )
    .bind(payment_attempt_id)
    .bind(invoice.id)
    .bind(business_id)
    .bind(invoice.total_cents)
    .bind(first_reconcile_in.as_secs_f64())
    .execute(&mut *tx)
    .await?;
    idempotency::attach_attempt(&mut tx, business_id, key, payment_attempt_id).await?;
    tx.commit().await?;

    tracing::info!(%payment_attempt_id, %invoice_id, amount_cents = invoice.total_cents, "payment attempt started");
    Ok(Claim::Claimed(PaymentClaim { payment_attempt_id, amount_cents: invoice.total_cents }))
}

/// Applies a definitive PSP outcome. Safe to call concurrently from the
/// request path and the reconciler: the `status = 'pending'` guard means
/// exactly one caller settles; the other gets the already-stored response.
pub async fn settle_attempt(
    db: &PgPool,
    payment_attempt_id: Uuid,
    settlement: Settlement,
) -> Result<StoredResponse, ApiError> {
    let mut tx = db.begin().await?;

    let (status, psp_ref, failure_code, transition, event_type) = match &settlement {
        Settlement::Succeeded { psp_ref } => (
            "succeeded",
            Some(psp_ref.as_str()),
            None,
            InvoiceTransition::PaymentSucceeded,
            EventType::InvoicePaid,
        ),
        Settlement::Failed { code } => (
            "failed",
            None,
            Some(code.as_str()),
            InvoiceTransition::PaymentFailed,
            EventType::InvoicePaymentFailed,
        ),
    };

    let settled: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE payment_attempts
         SET status = $2, psp_ref = $3, failure_code = $4, settled_at = now(), reconcile_after = NULL
         WHERE id = $1 AND status = 'pending'
         RETURNING invoice_id, business_id",
    )
    .bind(payment_attempt_id)
    .bind(status)
    .bind(psp_ref)
    .bind(failure_code)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((invoice_id, business_id)) = settled else {
        drop(tx);
        return already_attempt_response(db, payment_attempt_id).await;
    };

    let invoice = match invoices::apply_transition(&mut tx, business_id, invoice_id, transition).await? {
        TransitionOutcome::Applied(invoice) => invoice,
        // A pending attempt implies a processing invoice. If that does not
        // hold, something outside this code path changed the invoice; roll
        // back and leave the attempt for a human rather than paper over it.
        TransitionOutcome::Rejected(current) => {
            tracing::error!(%payment_attempt_id, %invoice_id, ?current, "invariant violated: pending attempt on non-processing invoice");
            return Err(ApiError::internal());
        }
        TransitionOutcome::NotFound => {
            tracing::error!(%payment_attempt_id, %invoice_id, "invariant violated: attempt references missing invoice");
            return Err(ApiError::internal());
        }
    };

    let attempt = find_attempt(&mut tx, business_id, payment_attempt_id).await?.ok_or_else(ApiError::internal)?;
    let detail = invoices::load_detail(&mut tx, business_id, invoice_id).await?.ok_or_else(ApiError::internal)?;
    events::record(&mut tx, business_id, event_type, json!({ "invoice": detail, "payment_attempt": attempt })).await?;

    let response = attempt_response(&invoice, &attempt);
    idempotency::complete_for_attempt(&mut tx, payment_attempt_id, &response).await?;
    tx.commit().await?;

    tracing::info!(%payment_attempt_id, %invoice_id, outcome = status, "payment attempt settled");
    Ok(response)
}

async fn already_attempt_response(db: &PgPool, payment_attempt_id: Uuid) -> Result<StoredResponse, ApiError> {
    let mut conn = db.acquire().await?;
    idempotency::response_for_attempt(&mut conn, payment_attempt_id)
        .await?
        .ok_or_else(|| {
            tracing::error!(%payment_attempt_id, "settled attempt has no stored response");
            ApiError::internal()
        })
}

fn attempt_response(invoice: &Invoice, attempt: &PaymentAttempt) -> StoredResponse {
    let resources = json!({ "invoice": invoice, "payment_attempt": attempt });
    match attempt.status {
        PaymentAttemptStatus::Succeeded => StoredResponse { status: StatusCode::OK, body: resources },
        PaymentAttemptStatus::Failed => {
            let code = attempt.failure_code.as_deref().unwrap_or("payment_failed");
            let error = ApiError::new(StatusCode::PAYMENT_REQUIRED, code, failure_message(code)).with_details(resources);
            StoredResponse::from_error(&error)
        }
        PaymentAttemptStatus::Pending => StoredResponse { status: StatusCode::ACCEPTED, body: resources },
    }
}

fn failure_message(code: &str) -> &'static str {
    match code {
        "card_declined" => "The card was declined",
        "insufficient_funds" => "The card has insufficient funds",
        "psp_no_record" => "The payment could not be completed and no charge was made",
        _ => "The payment was not successful",
    }
}

/// 202: the PSP has not given us a definitive answer yet. The body reflects
/// current state; the final outcome arrives as `invoice.paid` /
/// `invoice.payment_failed`, via `GET /v1/invoices/{id}`, or by retrying with
/// the same Idempotency-Key once settled.
async fn pending_response(db: &PgPool, business_id: Uuid, payment_attempt_id: Uuid) -> Result<Response, ApiError> {
    let mut conn = db.acquire().await?;
    let attempt = find_attempt(&mut conn, business_id, payment_attempt_id).await?.ok_or_else(ApiError::internal)?;
    let invoice = invoices::find(&mut conn, business_id, attempt.invoice_id).await?.ok_or_else(ApiError::internal)?;
    Ok(attempt_response(&invoice, &attempt).into_response())
}
