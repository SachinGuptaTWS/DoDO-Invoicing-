//! Client for the payment service provider.
//!
//! We only act on an answer the PSP actually gave us. After a timeout, a
//! dropped connection, a 5xx, or a body we cannot parse, the charge may or
//! may not have happened. Those all come back as `Indeterminate`, and the
//! caller leaves the attempt pending for the reconciler instead of guessing.

use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    Succeeded { psp_ref: String },
    Failed { code: String },
}

#[derive(Debug)]
pub enum ChargeResult {
    Settled(Settlement),
    Indeterminate(String),
}

#[derive(Debug)]
pub enum ChargeLookup {
    Settled(Settlement),
    Processing,
    /// The PSP has never seen this idempotency key.
    NotFound,
    Indeterminate(String),
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ChargeBody {
    Succeeded { psp_ref: String },
    Failed { code: String },
    Processing,
}

#[derive(Clone)]
pub struct PspClient {
    http: reqwest::Client,
    base_url: String,
}

impl PspClient {
    pub fn new(base_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        let http =
            reqwest::Client::builder().timeout(timeout).connect_timeout(timeout.min(Duration::from_secs(1))).build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_owned() })
    }

    /// The attempt id is the PSP idempotency key. Re-sending the same attempt
    /// can therefore never create a second charge, and the attempt can be
    /// looked up later if we never learned the outcome.
    pub async fn create_charge(&self, payment_attempt_id: Uuid, amount_cents: i64, card_token: &str) -> ChargeResult {
        let response = self
            .http
            .post(format!("{}/v1/charges", self.base_url))
            .header("Idempotency-Key", payment_attempt_id.to_string())
            .json(&json!({ "amount_cents": amount_cents, "currency": "USD", "card_token": card_token }))
            .send()
            .await;

        let response = match response {
            Ok(response) if response.status() == StatusCode::OK => response,
            Ok(response) => return ChargeResult::Indeterminate(format!("PSP returned HTTP {}", response.status())),
            Err(err) => return ChargeResult::Indeterminate(describe(&err)),
        };
        match response.json::<ChargeBody>().await {
            Ok(ChargeBody::Succeeded { psp_ref }) => ChargeResult::Settled(Settlement::Succeeded { psp_ref }),
            Ok(ChargeBody::Failed { code }) => ChargeResult::Settled(Settlement::Failed { code }),
            Ok(ChargeBody::Processing) => ChargeResult::Indeterminate("PSP reports charge still processing".into()),
            Err(err) => ChargeResult::Indeterminate(describe(&err)),
        }
    }

    pub async fn lookup_charge(&self, payment_attempt_id: Uuid) -> ChargeLookup {
        let response = self.http.get(format!("{}/v1/charges/{payment_attempt_id}", self.base_url)).send().await;
        let response = match response {
            Ok(response) if response.status() == StatusCode::NOT_FOUND => return ChargeLookup::NotFound,
            Ok(response) if response.status() == StatusCode::OK => response,
            Ok(response) => return ChargeLookup::Indeterminate(format!("PSP returned HTTP {}", response.status())),
            Err(err) => return ChargeLookup::Indeterminate(describe(&err)),
        };
        match response.json::<ChargeBody>().await {
            Ok(ChargeBody::Succeeded { psp_ref }) => ChargeLookup::Settled(Settlement::Succeeded { psp_ref }),
            Ok(ChargeBody::Failed { code }) => ChargeLookup::Settled(Settlement::Failed { code }),
            Ok(ChargeBody::Processing) => ChargeLookup::Processing,
            Err(err) => ChargeLookup::Indeterminate(describe(&err)),
        }
    }
}

fn describe(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "PSP request timed out".into()
    } else if err.is_connect() {
        "could not connect to PSP".into()
    } else {
        format!("PSP request failed: {err}")
    }
}
