//! Mock PSP implementing the assignment's token table, plus the two
//! behaviours of real PSPs the invoicing service relies on:
//!
//! - `Idempotency-Key` on charge creation: a repeated key returns the
//!   original outcome instead of charging again.
//! - `GET /v1/charges/{idempotency_key}`: look up a charge whose response we
//!   never received.
//!
//! State is in memory; restarting the mock forgets every charge. That is
//! acceptable for a mock and is called out in DESIGN.md.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct MockPspConfig {
    pub decision_delay: Duration,
    /// How long `tok_timeout` sleeps before answering (spec: 30 s).
    pub slow_delay: Duration,
}

impl Default for MockPspConfig {
    fn default() -> Self {
        Self { decision_delay: Duration::from_millis(100), slow_delay: Duration::from_secs(30) }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Succeeded { psp_ref: String },
    Failed { code: &'static str },
}

struct Charge {
    amount_cents: i64,
    outcome: Outcome,
    settles_at: Instant,
}

#[derive(Clone)]
pub struct MockPsp {
    config: MockPspConfig,
    charges: Arc<Mutex<HashMap<String, Charge>>>,
    charge_requests: Arc<AtomicUsize>,
}

impl MockPsp {
    pub fn new(config: MockPspConfig) -> Self {
        Self { config, charges: Arc::default(), charge_requests: Arc::default() }
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/charges", post(create_charge))
            .route("/v1/charges/{idempotency_key}", get(lookup_charge))
            .with_state(self.clone())
    }

    /// Every `POST /v1/charges` received, including replays and failures.
    pub fn charge_requests(&self) -> usize {
        self.charge_requests.load(Ordering::SeqCst)
    }

    /// Money actually taken: charges that succeeded, and their amounts.
    pub fn successful_charges(&self) -> Vec<i64> {
        self.charges
            .lock()
            .unwrap()
            .values()
            .filter(|charge| matches!(charge.outcome, Outcome::Succeeded { .. }))
            .map(|charge| charge.amount_cents)
            .collect()
    }
}

#[derive(Deserialize)]
struct ChargeRequest {
    amount_cents: i64,
    card_token: String,
}

async fn create_charge(State(psp): State<MockPsp>, headers: HeaderMap, Json(request): Json<ChargeRequest>) -> Response {
    psp.charge_requests.fetch_add(1, Ordering::SeqCst);

    let Some(key) = headers.get("idempotency-key").and_then(|value| value.to_str().ok()).map(str::to_owned) else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "Idempotency-Key header required" }))).into_response();
    };

    let (outcome, delay) = match request.card_token.as_str() {
        "tok_success" => (Outcome::Succeeded { psp_ref: Uuid::new_v4().to_string() }, psp.config.decision_delay),
        "tok_insufficient_funds" => (Outcome::Failed { code: "insufficient_funds" }, psp.config.decision_delay),
        "tok_card_declined" => (Outcome::Failed { code: "card_declined" }, psp.config.decision_delay),
        "tok_timeout" => (Outcome::Succeeded { psp_ref: Uuid::new_v4().to_string() }, psp.config.slow_delay),
        // Fails before recording anything, like a PSP that errors at its edge.
        "tok_network_error" => {
            return (StatusCode::INTERNAL_SERVER_ERROR, "upstream connect error").into_response();
        }
        _ => (Outcome::Failed { code: "invalid_card_token" }, psp.config.decision_delay),
    };

    {
        let mut charges = psp.charges.lock().unwrap();
        if let Some(existing) = charges.get(&key) {
            return if Instant::now() >= existing.settles_at {
                outcome_response(&existing.outcome)
            } else {
                (StatusCode::CONFLICT, Json(json!({ "status": "processing" }))).into_response()
            };
        }
        // Recorded before sleeping: the decision is made the moment the PSP
        // receives the request; `delay` only models slow responses.
        charges.insert(
            key,
            Charge { amount_cents: request.amount_cents, outcome: outcome.clone(), settles_at: Instant::now() + delay },
        );
    }

    tokio::time::sleep(delay).await;
    outcome_response(&outcome)
}

async fn lookup_charge(State(psp): State<MockPsp>, Path(key): Path<String>) -> Response {
    let charges = psp.charges.lock().unwrap();
    match charges.get(&key) {
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "no such charge" }))).into_response(),
        Some(charge) if Instant::now() < charge.settles_at => Json(json!({ "status": "processing" })).into_response(),
        Some(charge) => outcome_response(&charge.outcome),
    }
}

fn outcome_response(outcome: &Outcome) -> Response {
    match outcome {
        Outcome::Succeeded { psp_ref } => Json(json!({ "status": "succeeded", "psp_ref": psp_ref })),
        Outcome::Failed { code } => Json(json!({ "status": "failed", "code": code })),
    }
    .into_response()
}
