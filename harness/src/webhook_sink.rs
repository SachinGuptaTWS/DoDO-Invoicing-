//! A webhook receiver for local demos and tests. `POST /hooks/fail` always
//! answers 503 so retries and backoff can be observed; any other name
//! answers 200. `GET /received` shows what arrived.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::Value;

const KEEP_LAST: usize = 200;

#[derive(Clone, Debug, Serialize)]
pub struct ReceivedWebhook {
    pub hook: String,
    pub event_id: Option<String>,
    pub event_type: Option<String>,
    pub signature: Option<String>,
    pub body: Value,
    pub responded_with: u16,
}

#[derive(Clone, Default)]
pub struct WebhookSink {
    received: Arc<Mutex<VecDeque<ReceivedWebhook>>>,
}

impl WebhookSink {
    pub fn router(&self) -> Router {
        Router::new()
            .route("/hooks/{hook}", post(receive))
            .route("/received", get(list_received))
            .with_state(self.clone())
    }

    pub fn received(&self) -> Vec<ReceivedWebhook> {
        self.received.lock().unwrap().iter().cloned().collect()
    }
}

async fn receive(State(sink): State<WebhookSink>, Path(hook): Path<String>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
    let status = if hook == "fail" { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    let received = ReceivedWebhook {
        hook,
        event_id: header("dodo-event-id"),
        event_type: header("dodo-event-type"),
        signature: header("dodo-signature"),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        responded_with: status.as_u16(),
    };
    tracing::info!(
        hook = %received.hook,
        event_type = received.event_type.as_deref().unwrap_or("?"),
        event_id = received.event_id.as_deref().unwrap_or("?"),
        responded_with = received.responded_with,
        "webhook received"
    );

    let mut log = sink.received.lock().unwrap();
    if log.len() == KEEP_LAST {
        log.pop_front();
    }
    log.push_back(received);
    status
}

async fn list_received(State(sink): State<WebhookSink>) -> Json<Vec<ReceivedWebhook>> {
    Json(sink.received())
}
