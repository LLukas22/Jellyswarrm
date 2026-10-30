//! Session routes, caller extraction, login responses and WebSocket frame handling.
mod routes;
#[cfg(test)]
mod tests;
mod websocket;

pub(crate) use routes::router;
pub(crate) use websocket::websocket;

use crate::{
    application::sessions::SessionOperationError, http::state::SessionState,
    sessions::transport::DeliveryError, sessions::SessionContext,
    syncplay::service::RemoteCommandError,
};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub(crate) type AuthenticationResponse = (
    axum::http::HeaderMap,
    axum::Json<crate::models::AuthenticateResponse>,
);

/// This same-origin cookie is an identity hint, never an authentication token.
pub(crate) fn authentication_response(
    response: crate::models::AuthenticateResponse,
) -> AuthenticationResponse {
    let session = &response.session_info;
    let metadata = ["Client", "DeviceName", "DeviceId", "ApplicationVersion"].map(|key| {
        session
            .extra
            .get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Unknown")
    });
    let encoded = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&metadata).expect("string array"));
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(value) =
        format!("jellyswarrm_device={encoded}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000")
            .parse()
    {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    (headers, axum::Json(response))
}

/// Extract caller inputs and the same-origin device hint; application authenticates.
fn websocket_cookie_device(
    headers: &axum::http::HeaderMap,
) -> Option<crate::protocol::device::Device> {
    if !headers
        .get(axum::http::header::UPGRADE)?
        .to_str()
        .ok()?
        .eq_ignore_ascii_case("websocket")
    {
        return None;
    }
    let value = headers
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| part.trim().strip_prefix("jellyswarrm_device="))?;
    if value.len() > 4096 {
        return None;
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).ok()?;
    let [client, device, device_id, version]: [String; 4] = serde_json::from_slice(&bytes).ok()?;
    Some(crate::protocol::device::Device {
        client,
        device,
        device_id,
        version,
    })
}

impl<S: Send + Sync> FromRequestParts<S> for SessionContext
where
    SessionState: FromRef<S>,
{
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, StatusCode> {
        let state = SessionState::from_ref(state);
        let url = crate::http::preprocessing::request_url(&parts.uri)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        state
            .session_operations()
            .authenticate(
                &parts.headers,
                &url,
                !matches!(
                    parts.method,
                    axum::http::Method::GET | axum::http::Method::HEAD
                ),
                websocket_cookie_device(&parts.headers),
            )
            .await
            .map_err(status)
    }
}

/// Endpoint compatibility policy for session/control failures.
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
mod status_tests {
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
