//! Checks shared by request bodies.

use crate::error::ApiError;

/// Trims `raw` and checks it is 1..=`max_chars` characters with no control
/// characters. Control characters are refused outright: Postgres `text`
/// cannot store NUL, so one would otherwise surface as a 500 on insert.
pub fn text_field(field: &str, raw: &str, max_chars: usize) -> Result<String, ApiError> {
    let value = raw.trim();
    if value.is_empty() || value.chars().count() > max_chars {
        return Err(ApiError::validation(field, format!("{field} must be 1-{max_chars} characters")));
    }
    if value.contains(char::is_control) {
        return Err(ApiError::validation(field, format!("{field} cannot contain control characters")));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_counts_characters_not_bytes() {
        assert_eq!(text_field("name", "  Ada  ", 3).unwrap(), "Ada");
        assert!(text_field("name", "ééé", 3).is_ok());
        assert!(text_field("name", "   ", 3).is_err());
        assert!(text_field("name", "abcd", 3).is_err());
    }

    #[test]
    fn control_characters_are_rejected() {
        assert!(text_field("name", "a\u{0}b", 10).is_err());
        assert!(text_field("name", "a\nb", 10).is_err());
    }
}
