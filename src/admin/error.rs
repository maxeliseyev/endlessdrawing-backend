//! Safe admin errors. Never retain request bodies, SQL details or credentials.

use axum::{
    Json,
    http::{StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminError {
    Invalid,
    Unauthorized,
    Forbidden,
    StrokeNotFound,
    OperationNotFound,
    Conflict,
    TooLarge,
    RateLimited,
    Disabled,
    Unavailable,
    /// Internal signal: COMMIT may have reached PostgreSQL, so live clients must resync.
    CommitUnknown,
}

impl AdminError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Invalid => "invalid_request",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::StrokeNotFound => "stroke_not_found",
            Self::OperationNotFound => "operation_not_found",
            Self::Conflict => "operation_conflict",
            Self::TooLarge => "payload_too_large",
            Self::RateLimited => "rate_limited",
            Self::Disabled => "admin_unavailable",
            Self::Unavailable | Self::CommitUnknown => "temporarily_unavailable",
        }
    }

    pub(crate) fn commit_unknown(error: sqlx::Error) -> Self {
        let _ = Self::from(error);
        Self::CommitUnknown
    }

    fn status(self) -> StatusCode {
        match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::StrokeNotFound | Self::OperationNotFound => StatusCode::NOT_FOUND,
            Self::Conflict => StatusCode::CONFLICT,
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            Self::Disabled | Self::Unavailable | Self::CommitUnknown => {
                StatusCode::SERVICE_UNAVAILABLE
            }
        }
    }
}

impl fmt::Display for AdminError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for AdminError {}

impl From<sqlx::Error> for AdminError {
    fn from(error: sqlx::Error) -> Self {
        let sqlstate = error.as_database_error().and_then(|error| error.code());
        tracing::warn!(sqlstate = ?sqlstate, "administrative database operation failed");
        Self::Unavailable
    }
}
impl From<crate::protocol::ErrorCode> for AdminError {
    fn from(error: crate::protocol::ErrorCode) -> Self {
        if error == crate::protocol::ErrorCode::RateLimited {
            Self::RateLimited
        } else {
            Self::Unavailable
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}
#[derive(Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: &'static str,
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let message = match self {
            Self::Unauthorized => "Authentication required or failed",
            Self::Forbidden => "Request is not permitted",
            Self::Invalid => "Invalid request",
            Self::StrokeNotFound => "A selected stroke does not exist; nothing was hidden",
            Self::OperationNotFound => "Operation not found",
            Self::Conflict => "Operation identifier is already in use",
            Self::TooLarge => "Request body is too large",
            Self::RateLimited => "Too many requests; retry later",
            Self::Disabled => "Administration is not configured",
            Self::Unavailable | Self::CommitUnknown => "Operation could not be completed",
        };
        let mut response = (
            self.status(),
            Json(ErrorBody {
                error: ErrorDetail {
                    code: self.code(),
                    message,
                },
            }),
        )
            .into_response();
        if self == Self::RateLimited {
            response
                .headers_mut()
                .insert(RETRY_AFTER, "300".parse().expect("static header"));
        }
        response
    }
}
