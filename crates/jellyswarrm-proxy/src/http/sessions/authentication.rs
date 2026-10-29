//! Encode login responses and the browser WebSocket device-identity hint.
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
