use serde::Serialize;

use crate::error::ApiError;

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 100;

/// Lists are keyset-paginated (newest first by UUIDv7 id; the event feed
/// oldest first by `seq`), so paging costs an index seek, not an OFFSET scan.
/// Every list takes the same `limit` rules.
pub fn resolve_limit(requested: Option<u32>) -> Result<i64, ApiError> {
    match requested.unwrap_or(DEFAULT_LIMIT) {
        limit @ 1..=MAX_LIMIT => Ok(i64::from(limit)),
        _ => Err(ApiError::validation("limit", format!("limit must be between 1 and {MAX_LIMIT}"))),
    }
}

#[derive(Serialize)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub has_more: bool,
}

impl<T> Page<T> {
    /// Expects the query to have fetched `limit + 1` rows; the extra row only
    /// tells us whether another page exists.
    pub fn from_overfetch(mut rows: Vec<T>, limit: i64) -> Self {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        Self { data: rows, has_more }
    }
}
