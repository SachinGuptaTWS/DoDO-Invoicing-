//! The invoice state machine, and the only place transitions are defined.
//!
//! ```text
//!            pay            PSP succeeded
//!   open ──────────▶ processing ──────────▶ paid  (terminal)
//!    │  ◀──────────     │
//!    │  PSP declined /  │ void → 409 payment_in_progress
//!    │  PSP has no record
//!    │ void
//!    ▼
//!   void (terminal)
//! ```
//!
//! Every transition has exactly one legal source state. That is what lets
//! the database enforce the machine atomically:
//! `UPDATE invoices SET status = <to> WHERE id = $1 AND status = <from>`.
//! A concurrent writer that loses the race matches zero rows; there is no
//! read-then-write window to protect with locks.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvoiceStatus {
    /// Issued and awaiting payment.
    Open,
    /// A payment attempt is in flight; money may be moving. Nothing else may
    /// happen to the invoice until the attempt settles.
    Processing,
    Paid,
    Void,
}

impl InvoiceStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Processing => "processing",
            Self::Paid => "paid",
            Self::Void => "void",
        }
    }
}

impl TryFrom<String> for InvoiceStatus {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        match value.as_str() {
            "open" => Ok(Self::Open),
            "processing" => Ok(Self::Processing),
            "paid" => Ok(Self::Paid),
            "void" => Ok(Self::Void),
            other => Err(format!("unknown invoice status {other:?}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvoiceTransition {
    /// `POST /invoices/{id}/pay` claims the invoice for one payment attempt.
    BeginPayment,
    /// The PSP confirmed the charge (synchronously or via reconciliation).
    PaymentSucceeded,
    /// The PSP declined, or has no record of the charge past the grace window.
    PaymentFailed,
    /// `POST /invoices/{id}/void`.
    Void,
}

impl InvoiceTransition {
    pub const fn from(self) -> InvoiceStatus {
        match self {
            Self::BeginPayment | Self::Void => InvoiceStatus::Open,
            Self::PaymentSucceeded | Self::PaymentFailed => InvoiceStatus::Processing,
        }
    }

    pub const fn to(self) -> InvoiceStatus {
        match self {
            Self::BeginPayment => InvoiceStatus::Processing,
            Self::PaymentSucceeded => InvoiceStatus::Paid,
            Self::PaymentFailed => InvoiceStatus::Open,
            Self::Void => InvoiceStatus::Void,
        }
    }

    /// The API error for attempting this transition from `current`.
    /// Only the client-triggered transitions can reach the API; a rejected
    /// settle transition is an invariant violation and is handled as a 500.
    pub fn rejection(self, current: InvoiceStatus) -> ApiError {
        match (self, current) {
            (Self::BeginPayment, InvoiceStatus::Paid) => {
                ApiError::conflict("invoice_already_paid", "This invoice has already been paid")
            }
            (_, InvoiceStatus::Processing) => ApiError::conflict(
                "payment_in_progress",
                "A payment for this invoice is in progress; retry once it settles",
            ),
            _ => ApiError::conflict(
                "invalid_state_transition",
                format!("Cannot {} an invoice that is {}", self.verb(), current.as_str()),
            ),
        }
        .with_details(json!({ "invoice_status": current }))
    }

    const fn verb(self) -> &'static str {
        match self {
            Self::BeginPayment => "pay",
            Self::PaymentSucceeded | Self::PaymentFailed => "settle a payment on",
            Self::Void => "void",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_terminal(status: InvoiceStatus) -> bool {
        matches!(status, InvoiceStatus::Paid | InvoiceStatus::Void)
    }

    const ALL_STATUSES: [InvoiceStatus; 4] =
        [InvoiceStatus::Open, InvoiceStatus::Processing, InvoiceStatus::Paid, InvoiceStatus::Void];
    const ALL_TRANSITIONS: [InvoiceTransition; 4] = [
        InvoiceTransition::BeginPayment,
        InvoiceTransition::PaymentSucceeded,
        InvoiceTransition::PaymentFailed,
        InvoiceTransition::Void,
    ];

    #[test]
    fn terminal_states_have_no_outgoing_transitions() {
        for transition in ALL_TRANSITIONS {
            assert!(!is_terminal(transition.from()), "{transition:?} leaves a terminal state");
        }
    }

    #[test]
    fn every_non_terminal_state_is_reachable_and_exitable() {
        for status in ALL_STATUSES.into_iter().filter(|s| !is_terminal(*s)) {
            assert!(ALL_TRANSITIONS.iter().any(|t| t.from() == status), "{status:?} is a dead end");
        }
        for status in ALL_STATUSES.into_iter().filter(|s| *s != InvoiceStatus::Open) {
            assert!(ALL_TRANSITIONS.iter().any(|t| t.to() == status), "{status:?} is unreachable");
        }
    }

    #[test]
    fn status_round_trips_through_its_storage_form() {
        for status in ALL_STATUSES {
            assert_eq!(InvoiceStatus::try_from(status.as_str().to_owned()), Ok(status));
        }
    }
}
