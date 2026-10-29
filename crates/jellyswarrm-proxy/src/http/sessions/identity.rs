//! Extract caller inputs and the same-origin device hint; application authenticates.
use crate::{http::state::SessionState, sessions::SessionContext};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

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
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let url = url::Url::parse(&format!("http://localhost{path_and_query}"))
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
            .map_err(super::error::status)
    }
}
