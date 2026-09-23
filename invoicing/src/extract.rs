//! Wrappers around axum's extractors so that malformed input produces our
//! error envelope instead of axum's plain-text rejections.

use axum::extract::{
    rejection::{JsonRejection, PathRejection, QueryRejection},
    FromRequest, FromRequestParts,
};

use crate::error::ApiError;

#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Path), rejection(ApiError))]
pub struct ApiPath<T>(pub T);

#[derive(FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        match rejection {
            // Well-formed JSON with the wrong shape, e.g. a float where
            // integer cents are required, or an unknown field.
            JsonRejection::JsonDataError(err) => {
                ApiError::new(axum::http::StatusCode::UNPROCESSABLE_ENTITY, "validation_failed", err.body_text())
            }
            other => ApiError::invalid_request(other.body_text()),
        }
    }
}

impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        ApiError::invalid_request(rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        ApiError::invalid_request(rejection.body_text())
    }
}
