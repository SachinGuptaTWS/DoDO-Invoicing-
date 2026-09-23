//! Server-side invoice totals. All arithmetic is checked `i64` cents; there
//! is no type in this module (or the money path) that can hold a fraction.

use serde::Deserialize;

use crate::error::ApiError;

pub const MAX_LINE_ITEMS: usize = 100;
/// $999,999.99, the largest single USD charge card processors generally
/// accept. A cap turns a fat-fingered quantity into a 422 at creation rather
/// than a decline (or worse, a charge) at payment time.
pub const MAX_INVOICE_TOTAL_CENTS: i64 = 99_999_999;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineItemInput {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PricedLineItem {
    pub description: String,
    pub quantity: i64,
    pub unit_amount_cents: i64,
    pub amount_cents: i64,
}

#[derive(Debug)]
pub struct PricedInvoice {
    pub line_items: Vec<PricedLineItem>,
    pub total_cents: i64,
}

pub fn price(items: Vec<LineItemInput>) -> Result<PricedInvoice, ApiError> {
    if items.is_empty() || items.len() > MAX_LINE_ITEMS {
        return Err(ApiError::validation(
            "line_items",
            format!("an invoice needs 1-{MAX_LINE_ITEMS} line items"),
        ));
    }

    let mut total_cents: i64 = 0;
    let mut line_items = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        let field = |name: &str| format!("line_items[{index}].{name}");

        let description = item.description.trim().to_owned();
        if description.is_empty() || description.chars().count() > 500 {
            return Err(ApiError::validation(&field("description"), "description must be 1-500 characters"));
        }
        if item.quantity <= 0 {
            return Err(ApiError::validation(&field("quantity"), "quantity must be a positive integer"));
        }
        if item.unit_amount_cents < 0 {
            return Err(ApiError::validation(&field("unit_amount_cents"), "unit_amount_cents cannot be negative"));
        }

        let amount_cents = item
            .quantity
            .checked_mul(item.unit_amount_cents)
            .filter(|amount| *amount <= MAX_INVOICE_TOTAL_CENTS)
            .ok_or_else(|| ApiError::validation(&field("quantity"), "line item amount exceeds the invoice maximum"))?;
        total_cents = total_cents
            .checked_add(amount_cents)
            .filter(|total| *total <= MAX_INVOICE_TOTAL_CENTS)
            .ok_or_else(|| ApiError::validation("line_items", "invoice total exceeds $999,999.99"))?;

        line_items.push(PricedLineItem {
            description,
            quantity: item.quantity,
            unit_amount_cents: item.unit_amount_cents,
            amount_cents,
        });
    }

    // A zero-total invoice has nothing to collect; there is no PSP call that
    // makes sense for it, so it cannot enter the payment state machine.
    if total_cents == 0 {
        return Err(ApiError::validation("line_items", "invoice total must be greater than zero"));
    }

    Ok(PricedInvoice { line_items, total_cents })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(quantity: i64, unit_amount_cents: i64) -> LineItemInput {
        LineItemInput { description: "Seat".into(), quantity, unit_amount_cents }
    }

    #[test]
    fn totals_are_summed_from_line_items() {
        let priced = price(vec![item(3, 1_999), item(1, 1)]).unwrap();
        assert_eq!(priced.total_cents, 5_998);
        assert_eq!(priced.line_items[0].amount_cents, 5_997);
    }

    #[test]
    fn multiplication_overflow_is_a_validation_error_not_a_panic() {
        assert!(price(vec![item(i64::MAX, 2)]).is_err());
    }

    #[test]
    fn total_above_the_cap_is_rejected() {
        assert!(price(vec![item(1, MAX_INVOICE_TOTAL_CENTS), item(1, 1)]).is_err());
        assert!(price(vec![item(1, MAX_INVOICE_TOTAL_CENTS)]).is_ok());
    }

    #[test]
    fn zero_total_and_non_positive_quantities_are_rejected() {
        assert!(price(vec![item(1, 0)]).is_err());
        assert!(price(vec![item(0, 100)]).is_err());
        assert!(price(vec![item(1, -100)]).is_err());
        assert!(price(vec![]).is_err());
    }
}
