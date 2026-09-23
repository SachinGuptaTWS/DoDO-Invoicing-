pub mod handlers;
pub mod pricing;
pub mod state;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgConnection};
use uuid::Uuid;

use crate::payments::{self, PaymentAttempt};
pub use state::{InvoiceStatus, InvoiceTransition};

pub(crate) const INVOICE_COLUMNS: &str =
    "id, customer_id, status, currency, total_cents, due_date, created_at, updated_at, paid_at, voided_at";

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Invoice {
    pub id: Uuid,
    pub customer_id: Uuid,
    #[sqlx(try_from = "String")]
    pub status: InvoiceStatus,
    pub currency: String,
    pub total_cents: i64,
    pub due_date: NaiveDate,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub paid_at: Option<DateTime<Utc>>,
    pub voided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct LineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvoiceDetail {
    #[serde(flatten)]
    pub invoice: Invoice,
    pub line_items: Vec<LineItem>,
    pub payment_attempts: Vec<PaymentAttempt>,
}

pub enum TransitionOutcome {
    Applied(Invoice),
    Rejected(InvoiceStatus),
    NotFound,
}

/// Compare-and-set on `status`. This is the concurrency mechanism for the
/// whole service: two writers racing on one invoice serialize on the row
/// lock the UPDATE takes, and the loser re-evaluates `status = <from>` against
/// the committed row and matches nothing.
pub async fn apply_transition(
    conn: &mut PgConnection,
    business_id: Uuid,
    invoice_id: Uuid,
    transition: InvoiceTransition,
) -> Result<TransitionOutcome, sqlx::Error> {
    let applied = sqlx::query_as::<_, Invoice>(&format!(
        "UPDATE invoices
         SET status = $3,
             updated_at = now(),
             paid_at = CASE WHEN $3 = 'paid' THEN now() ELSE paid_at END,
             voided_at = CASE WHEN $3 = 'void' THEN now() ELSE voided_at END
         WHERE id = $1 AND business_id = $2 AND status = $4
         RETURNING {INVOICE_COLUMNS}"
    ))
    .bind(invoice_id)
    .bind(business_id)
    .bind(transition.to().as_str())
    .bind(transition.from().as_str())
    .fetch_optional(&mut *conn)
    .await?;

    if let Some(invoice) = applied {
        return Ok(TransitionOutcome::Applied(invoice));
    }

    // Only used to explain the rejection; the decision was already made
    // atomically above, so a status change between the two reads is harmless.
    Ok(match find(conn, business_id, invoice_id).await? {
        Some(invoice) => TransitionOutcome::Rejected(invoice.status),
        None => TransitionOutcome::NotFound,
    })
}

pub async fn find(
    conn: &mut PgConnection,
    business_id: Uuid,
    invoice_id: Uuid,
) -> Result<Option<Invoice>, sqlx::Error> {
    sqlx::query_as::<_, Invoice>(&format!("SELECT {INVOICE_COLUMNS} FROM invoices WHERE id = $1 AND business_id = $2"))
        .bind(invoice_id)
        .bind(business_id)
        .fetch_optional(conn)
        .await
}

pub async fn load_detail(
    conn: &mut PgConnection,
    business_id: Uuid,
    invoice_id: Uuid,
) -> Result<Option<InvoiceDetail>, sqlx::Error> {
    let Some(invoice) = find(conn, business_id, invoice_id).await? else {
        return Ok(None);
    };
    let line_items = sqlx::query_as::<_, LineItem>(
        "SELECT description, quantity, unit_amount_cents, amount_cents
         FROM invoice_line_items WHERE invoice_id = $1 ORDER BY position",
    )
    .bind(invoice_id)
    .fetch_all(&mut *conn)
    .await?;
    let payment_attempts = payments::attempts_for_invoice(conn, invoice_id).await?;
    Ok(Some(InvoiceDetail { invoice, line_items, payment_attempts }))
}
