//! Proxy-owned client sessions, remote control and shared WebSocket transport.
//! This module never sends session/control requests to upstream servers.
mod access;
mod http;
mod models;
mod service;
#[cfg(test)]
mod tests;
pub(crate) mod transport;
mod websocket;

pub(crate) use access::user_has_media_access;
pub use http::router;
pub use service::ClientSessionService;
pub use websocket::websocket;

use crate::{
    request_preprocessing::resolve_request_identity_from_headers_uri,
    user_authorization_service::User, AppState,
};
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

pub type AuthenticationResponse = (
    axum::http::HeaderMap,
    axum::Json<crate::models::AuthenticateResponse>,
);

/// Browser WebSockets cannot set Authorization headers, and Jellyfin Web sends
/// only ApiKey. Its proxy key is shared across devices, so retain device metadata
/// in a same-origin cookie. This is an identity hint, never an authentication token.
pub fn authentication_response(
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

fn websocket_cookie_device(
    headers: &axum::http::HeaderMap,
) -> Option<crate::user_authorization_service::Device> {
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
    Some(crate::user_authorization_service::Device {
        client,
        device,
        device_id,
        version,
    })
}

#[derive(Clone)]
pub(crate) struct SessionContext {
    pub user: User,
    pub session_id: String,
}

impl FromRequestParts<AppState> for SessionContext {
    type Rejection = StatusCode;
    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, StatusCode> {
        let identity = resolve_request_identity_from_headers_uri(&parts.headers, &parts.uri, state)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let user = identity.user.ok_or(StatusCode::UNAUTHORIZED)?;
        let token = identity
            .auth
            .as_ref()
            .and_then(|a| a.token_ref())
            .ok_or(StatusCode::UNAUTHORIZED)?;
        if !matches!(
            parts.method,
            axum::http::Method::GET | axum::http::Method::HEAD
        ) && state
            .user_authorization
            .is_read_only_api_key(token)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            return Err(StatusCode::FORBIDDEN);
        }
        let device = identity
            .device
            .clone()
            .filter(|d| {
                crate::user_authorization_service::Device::has_known_device_id(&d.device_id)
            })
            .or_else(|| websocket_cookie_device(&parts.headers))
            .or(identity.device);
        let session_id = state
            .client_sessions
            .ensure(&user, token, device.as_ref())
            .await;
        Ok(Self { user, session_id })
    }
}

async fn valid_session(
    state: &AppState,
    session: &service::ClientSession,
) -> Result<bool, StatusCode> {
    let user = state
        .user_authorization
        .get_user_by_token(&session.token)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if user.is_some_and(|user| user.id == session.user_id) {
        return Ok(true);
    }
    state.client_sessions.remove(&session.id).await;
    state.syncplay.end_session(&session.id).await;
    Ok(false)
}

async fn snapshots(state: &AppState, user_id: &str) -> Result<Vec<serde_json::Value>, StatusCode> {
    let server_id = state.config.read().await.server_id.clone();
    let mut result = Vec::new();
    for session in state.client_sessions.for_user(user_id).await {
        if valid_session(state, &session).await? {
            result.push(session.dto(&server_id, &state.client_sessions.transport));
        }
    }
    Ok(result)
}

/// Give both password and Quick Connect authentication the same local session DTO.
pub async fn decorate_authentication(
    state: &AppState,
    user: &User,
    auth: &crate::models::Authorization,
    response: &mut crate::models::AuthenticateResponse,
) {
    let device = crate::user_authorization_service::Device {
        client: auth.client.clone(),
        device: auth.device.clone(),
        device_id: auth.device_id.clone(),
        version: auth.version.clone(),
    };
    let id = state
        .client_sessions
        .ensure(user, &user.virtual_key, Some(&device))
        .await;
    if let Some(session) = state.client_sessions.get(&id).await {
        let server_id = state.config.read().await.server_id.clone();
        // This DTO is constructed locally and has all required SessionInfo fields.
        response.session_info =
            serde_json::from_value(session.dto(&server_id, &state.client_sessions.transport))
                .expect("local SessionInfo DTO");
    }
}

/// Observe only reports that passed the normal playback pipeline; use the original virtual IDs.
pub async fn observe_playback(state: &AppState, request: &reqwest::Request) {
    let Some(action) = crate::request_preprocessing::playback_session_action(
        request.method(),
        request.url().path(),
        state,
    )
    .await
    else {
        return;
    };
    let Some(report) = crate::request_preprocessing::body_to_json(request) else {
        return;
    };
    let Ok(uri) = request.url().as_str().parse() else {
        return;
    };
    let Ok(identity) =
        resolve_request_identity_from_headers_uri(request.headers(), &uri, state).await
    else {
        return;
    };
    let Some(user) = identity.user else {
        return;
    };
    let Some(token) = identity.auth.as_ref().and_then(|a| a.token_ref()) else {
        return;
    };
    let id = state
        .client_sessions
        .ensure(&user, token, identity.device.as_ref())
        .await;
    state
        .client_sessions
        .report(
            &id,
            action == crate::processors::request_analyzer::PlaybackSessionAction::Remove,
            &report,
        )
        .await;
}

/// Reset local client connections together with an administrative session reset.
pub async fn end_user_sessions(state: &AppState, user_id: &str) {
    for session in state.client_sessions.for_user(user_id).await {
        state.syncplay.end_session(&session.id).await;
        state.client_sessions.remove(&session.id).await;
    }
}
