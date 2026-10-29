//! Endpoint compatibility policy for session/control failures.
use crate::{
    application::sessions::SessionOperationError, sessions::transport::DeliveryError,
    syncplay::service::RemoteCommandError,
};
use axum::http::StatusCode;

pub(super) fn status(error: SessionOperationError) -> StatusCode {
    match error {
        SessionOperationError::InvalidInput => StatusCode::BAD_REQUEST,
        SessionOperationError::Unauthenticated => StatusCode::UNAUTHORIZED,
        SessionOperationError::ReadOnly => StatusCode::FORBIDDEN,
        SessionOperationError::NotFound => StatusCode::NOT_FOUND,
        SessionOperationError::NotControllable
        | SessionOperationError::Remote(RemoteCommandError::GroupPlayback)
        | SessionOperationError::Remote(RemoteCommandError::Delivery(
            DeliveryError::Disconnected,
        )) => StatusCode::CONFLICT,
        SessionOperationError::Remote(RemoteCommandError::Delivery(
            DeliveryError::Backpressure,
        )) => StatusCode::SERVICE_UNAVAILABLE,
        SessionOperationError::Storage(_)
        | SessionOperationError::Remote(RemoteCommandError::Delivery(DeliveryError::Encoding(_))) => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_session_and_delivery_failures_keep_the_endpoint_status_contract() {
        for (error, expected) in [
            (SessionOperationError::InvalidInput, StatusCode::BAD_REQUEST),
            (
                SessionOperationError::Unauthenticated,
                StatusCode::UNAUTHORIZED,
            ),
            (SessionOperationError::ReadOnly, StatusCode::FORBIDDEN),
            (SessionOperationError::NotFound, StatusCode::NOT_FOUND),
            (SessionOperationError::NotControllable, StatusCode::CONFLICT),
            (
                SessionOperationError::Remote(RemoteCommandError::GroupPlayback),
                StatusCode::CONFLICT,
            ),
            (
                SessionOperationError::Remote(RemoteCommandError::Delivery(
                    DeliveryError::Disconnected,
                )),
                StatusCode::CONFLICT,
            ),
            (
                SessionOperationError::Remote(RemoteCommandError::Delivery(
                    DeliveryError::Backpressure,
                )),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                SessionOperationError::Storage(sqlx::Error::PoolClosed),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
            (
                SessionOperationError::Remote(RemoteCommandError::Delivery(
                    DeliveryError::Encoding(
                        serde_json::from_str::<serde_json::Value>("{").unwrap_err(),
                    ),
                )),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            assert_eq!(status(error), expected);
        }
    }
}
