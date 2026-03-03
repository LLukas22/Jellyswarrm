use crate::{
    encryption::HashedPassword,
    models::{AuthenticateResponse, Authorization, SyncPlayUserAccessType},
    AppState,
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use hyper::StatusCode;
use jellyfin_api::{error::Error as JellyfinApiError, ClientInfo, JellyfinClient};
use jellyswarrm_macros::multi_case_struct;
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuickConnectSession {
    pub authenticated: bool,
    pub secret: String,
    pub code: String,
    pub device_id: String,
    pub device_name: String,
    pub app_name: String,
    pub app_version: String,
    pub date_added: DateTime<Utc>,
    #[serde(skip)]
    pub user_id: Option<String>,
    #[serde(skip)]
    pub expires_at: DateTime<Utc>,
}

impl QuickConnectSession {
    pub fn new(
        secret: String,
        code: String,
        device_id: String,
        device_name: String,
        app_name: String,
        app_version: String,
    ) -> Self {
        let now = Utc::now();
        Self {
            authenticated: false,
            secret,
            code,
            device_id,
            device_name,
            app_name,
            app_version,
            date_added: now,
            user_id: None,
            expires_at: now + Duration::minutes(10),
        }
    }

    pub fn is_expired(&self) -> bool {
        Utc::now() > self.expires_at
    }
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    pub code: String,
    pub user_id: Option<String>,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct ConnectQuery {
    pub secret: String,
}

#[multi_case_struct(pascal, camel)]
#[derive(Debug, Deserialize)]
pub struct QuickConnectAuthenticateRequest {
    pub secret: String,
}

pub struct QuickConnectStorage {
    sessions: Arc<Mutex<HashMap<String, QuickConnectSession>>>,
}

impl Default for QuickConnectStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl QuickConnectStorage {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn store_session(&self, session: QuickConnectSession) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session.secret.clone(), session.clone());
        sessions.insert(session.code.clone(), session);
    }

    pub fn get_session(&self, key: &str) -> Option<QuickConnectSession> {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.get(key) {
            if session.is_expired() {
                let secret = session.secret.clone();
                let code = session.code.clone();
                sessions.remove(&secret);
                sessions.remove(&code);
                return None;
            }

            return Some(session.clone());
        }

        None
    }

    pub fn update_session_by_code(
        &self,
        code: &str,
        mut updater: impl FnMut(&mut QuickConnectSession),
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.get(code).cloned() {
            if session.is_expired() {
                let secret = session.secret.clone();
                sessions.remove(&secret);
                sessions.remove(code);
                return false;
            }

            let mut updated_session = session;
            updater(&mut updated_session);

            let secret = updated_session.secret.clone();
            sessions.insert(secret, updated_session.clone());
            sessions.insert(code.to_string(), updated_session);
            return true;
        }

        false
    }

    pub fn remove_session(&self, secret: &str) -> Option<QuickConnectSession> {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.remove(secret) {
            sessions.remove(&session.code);
            return Some(session);
        }

        None
    }

    pub fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let mut expired = Vec::new();

        for session in sessions.values() {
            if session.is_expired() {
                expired.push((session.secret.clone(), session.code.clone()));
            }
        }

        for (secret, code) in &expired {
            sessions.remove(secret);
            sessions.remove(code);
        }

        expired.len()
    }

    pub fn start_cleanup_task(storage: QuickConnectStorage) {
        use std::time::Duration as StdDuration;

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(StdDuration::from_secs(60));
            loop {
                interval.tick().await;
                let cleaned = storage.cleanup_expired();
                if cleaned > 0 {
                    warn!("Cleaned up {} expired Quick Connect sessions", cleaned);
                }
            }
        });
    }
}

impl Clone for QuickConnectStorage {
    fn clone(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
        }
    }
}

fn generate_code() -> String {
    let mut rng = rand::rng();
    (0..6)
        .map(|_| char::from(b'0' + rng.random_range(0..10) as u8))
        .collect::<String>()
}

fn parse_authorization_from_headers(headers: &HeaderMap) -> Option<Authorization> {
    if let Some(header) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse_with_legacy(header, true) {
            Ok(auth) => return Some(auth),
            Err(e) => warn!("Failed to parse Authorization header: {}", e),
        }
    }

    if let Some(header) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse_with_legacy(header, true) {
            Ok(auth) => return Some(auth),
            Err(e) => warn!("Failed to parse X-Emby-Authorization header: {}", e),
        }
    }

    None
}

fn parse_client_info(headers: &HeaderMap) -> (String, String, String, String) {
    if let Some(auth) = parse_authorization_from_headers(headers) {
        return (auth.device_id, auth.device, auth.client, auth.version);
    }

    (
        Uuid::new_v4().to_string(),
        "Unknown Device".to_string(),
        "Unknown App".to_string(),
        "1.0.0".to_string(),
    )
}

fn is_android_tv_client(client: &str) -> bool {
    client.to_ascii_lowercase().contains("android tv")
}

fn normalize_android_tv_user_id(user_id: &str) -> String {
    Uuid::parse_str(user_id)
        .map(|id| id.hyphenated().to_string())
        .unwrap_or_else(|_| user_id.to_string())
}

fn derive_android_tv_user_device_id(base_device_id: &str, user_id: &str) -> String {
    let normalized_user_id = normalize_android_tv_user_id(user_id);
    let mut hasher = Sha1::new();
    hasher.update(base_device_id.as_bytes());
    hasher.update(b"+");
    hasher.update(normalized_user_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn effective_quick_connect_authorization(
    headers: &HeaderMap,
    session: &QuickConnectSession,
    user_id: &str,
) -> Authorization {
    let request_authorization = parse_authorization_from_headers(headers);

    let mut authorization = request_authorization.unwrap_or_else(|| {
        warn!(
            "Quick Connect authenticate request missing parseable authorization header, using initiate metadata"
        );
        Authorization {
            client: session.app_name.clone(),
            device: session.device_name.clone(),
            device_id: session.device_id.clone(),
            version: session.app_version.clone(),
            token: None,
        }
    });

    authorization.token = None;

    if is_android_tv_client(&authorization.client) && !authorization.device_id.is_empty() {
        let original_device_id = authorization.device_id.clone();
        authorization.device_id =
            derive_android_tv_user_device_id(&authorization.device_id, user_id);
        debug!(
            "Derived Android TV user-scoped device id for Quick Connect: {} -> {}",
            original_device_id, authorization.device_id
        );
    }

    authorization
}

pub async fn handle_quick_connect_enabled() -> Result<Json<bool>, StatusCode> {
    Ok(Json(true))
}

pub async fn handle_quick_connect_initiate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    let secret = Uuid::new_v4().to_string();
    let code = generate_code();
    let (device_id, device_name, app_name, app_version) = parse_client_info(&headers);

    info!(
        "Initiating Quick Connect session for {} / {} ({})",
        app_name, device_name, device_id
    );

    let session = QuickConnectSession::new(
        secret.clone(),
        code,
        device_id,
        device_name,
        app_name,
        app_version,
    );

    state.quick_connect.store_session(session.clone());
    state.quick_connect.cleanup_expired();

    Ok(Json(session))
}

pub async fn handle_quick_connect_authorize(
    Query(params): Query<AuthorizeQuery>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<bool>, StatusCode> {
    let user_id = resolve_authorize_user_id(&state, &headers, params.user_id).await?;

    let success = state
        .quick_connect
        .update_session_by_code(&params.code, |session| {
            session.authenticated = true;
            session.user_id = Some(user_id.clone());
        });

    if success {
        info!(
            "Authorized Quick Connect code {} for user {}",
            params.code, user_id
        );
        Ok(Json(true))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn resolve_authorize_user_id(
    state: &AppState,
    headers: &HeaderMap,
    user_id: Option<String>,
) -> Result<String, StatusCode> {
    if let Some(user_id) = user_id {
        return Ok(user_id);
    }

    let token = extract_virtual_token(headers).ok_or_else(|| {
        warn!("Quick Connect authorize called without userId and without a virtual token");
        StatusCode::BAD_REQUEST
    })?;

    let user = state
        .user_authorization
        .get_user_by_virtual_key(&token)
        .await
        .map_err(|e| {
            warn!("Failed to resolve user from virtual token: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    Ok(user.id)
}

fn extract_virtual_token(headers: &HeaderMap) -> Option<String> {
    if let Some(header) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse(header) {
            if let Some(token) = auth.token {
                return Some(token);
            }
        }
    }

    if let Some(header) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse_with_legacy(header, true) {
            if let Some(token) = auth.token {
                return Some(token);
            }
        }
    }

    if let Some(token) = headers
        .get("x-emby-token")
        .and_then(|value| value.to_str().ok())
    {
        return Some(token.to_string());
    }

    if let Some(token) = headers
        .get("x-mediabrowser-token")
        .and_then(|value| value.to_str().ok())
    {
        return Some(token.to_string());
    }

    None
}

pub async fn handle_quick_connect_connect(
    Query(params): Query<ConnectQuery>,
    State(state): State<AppState>,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    if let Some(session) = state.quick_connect.get_session(&params.secret) {
        Ok(Json(session))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn handle_authenticate_with_quick_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<QuickConnectAuthenticateRequest>,
) -> Result<Json<AuthenticateResponse>, StatusCode> {
    let session = state
        .quick_connect
        .get_session(&request.secret)
        .ok_or(StatusCode::NOT_FOUND)?;

    let Some(user_id) = session.user_id.clone() else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if !session.authenticated {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let user = state
        .user_authorization
        .get_user_by_id(&user_id)
        .await
        .map_err(|e| {
            warn!("Database error while fetching user {}: {}", user_id, e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let mut servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if servers.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }

    let server_mappings = state
        .user_authorization
        .list_server_mappings(&user.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if server_mappings.is_empty() {
        warn!(
            "Quick Connect user '{}' has no server mappings",
            user.original_username
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    let mut auth_tasks = Vec::with_capacity(server_mappings.len());

    let authorization = effective_quick_connect_authorization(&headers, &session, &user.id);

    for server_mapping in server_mappings {
        if let Some(pos) = servers.iter().position(|s| {
            s.url.as_str().trim_end_matches('/') == server_mapping.server_url.trim_end_matches('/')
        }) {
            let server = servers.remove(pos);
            let state = state.clone();
            let authorization = authorization.clone();
            let user = user.clone();

            auth_tasks.push(tokio::spawn(async move {
                authenticate_with_mapping_on_server(
                    state,
                    authorization,
                    user,
                    server,
                    server_mapping,
                )
                .await
            }));
        } else {
            debug!(
                "Skipping mapping for unknown server URL {}",
                server_mapping.server_url
            );
        }
    }

    let mut successful_auths = Vec::new();

    for task in auth_tasks {
        match task.await {
            Ok(Ok(auth_response)) => successful_auths.push(auth_response),
            Ok(Err(QuickConnectAuthError::Network(e))) => {
                debug!("Quick Connect auth request failed: {}", e)
            }
            Ok(Err(QuickConnectAuthError::Parse(e))) => {
                debug!("Quick Connect auth parse failed: {}", e)
            }
            Ok(Err(QuickConnectAuthError::Internal(e))) => {
                debug!("Quick Connect internal auth error: {}", e)
            }
            Ok(Err(QuickConnectAuthError::InvalidCredentials)) => {
                debug!("Quick Connect auth rejected by upstream server")
            }
            Err(e) => warn!("Quick Connect auth task failed: {}", e),
        }
    }

    state.quick_connect.remove_session(&request.secret);

    if successful_auths.is_empty() {
        Err(StatusCode::UNAUTHORIZED)
    } else {
        Ok(Json(successful_auths[0].clone()))
    }
}

#[derive(Debug)]
enum QuickConnectAuthError {
    Network(String),
    InvalidCredentials,
    Parse(String),
    Internal(String),
}

async fn authenticate_with_mapping_on_server(
    state: AppState,
    authorization: Authorization,
    user: crate::user_authorization_service::User,
    server: crate::server_storage::Server,
    server_mapping: crate::user_authorization_service::ServerMapping,
) -> Result<AuthenticateResponse, QuickConnectAuthError> {
    let admin_password = state.get_admin_password().await;
    let admin_password_hash: HashedPassword = (&admin_password).into();

    let mapped_password = state.user_authorization.decrypt_server_mapping_password(
        &server_mapping,
        &user.original_password_hash,
        &admin_password_hash,
        None,
        Some(&admin_password),
    );

    let client_info = ClientInfo {
        client: authorization.client.clone(),
        device: authorization.device.clone(),
        device_id: authorization.device_id.clone(),
        version: authorization.version.clone(),
    };

    let jellyfin_client = JellyfinClient::new_with_client(
        server.url.as_str(),
        client_info,
        state.reqwest_client.clone(),
    )
    .map_err(|e| QuickConnectAuthError::Internal(e.to_string()))?;

    let mut auth_response: AuthenticateResponse = jellyfin_client
        .authenticate_by_name_typed(
            server_mapping.mapped_username.as_str(),
            mapped_password.as_str(),
        )
        .await
        .map_err(map_jellyfin_auth_error)?;

    state
        .user_authorization
        .add_server_mapping(
            &user.id,
            server.url.as_str(),
            &server_mapping.mapped_username,
            &mapped_password,
            Some(&user.original_password_hash),
        )
        .await
        .map_err(|e| QuickConnectAuthError::Internal(e.to_string()))?;

    let auth_token = auth_response.access_token.clone();
    let original_user_id = auth_response.user.id.clone();

    let server_id = state.config.read().await.server_id.clone();
    auth_response.server_id = server_id.clone();
    auth_response.user.server_id = server_id.clone();
    auth_response.session_info.server_id = server_id;

    auth_response.session_info.user_id = user.id.clone();
    auth_response.user.name = user.original_username.clone();
    auth_response.session_info.user_name = user.original_username.clone();
    auth_response.user.policy.is_administrator = false;
    auth_response.user.policy.sync_play_access = SyncPlayUserAccessType::CreateAndJoinGroups;
    auth_response.access_token = user.virtual_key.clone();
    auth_response.user.id = user.id.clone();

    let mut auth_to_store = authorization;
    auth_to_store.token = Some(auth_token.clone());

    state
        .user_authorization
        .store_authorization_session(
            &user.id,
            server.url.as_str(),
            &auth_to_store,
            auth_token,
            original_user_id,
            None,
        )
        .await
        .map_err(|e| QuickConnectAuthError::Internal(e.to_string()))?;

    info!(
        "Quick Connect authenticated '{}' on server '{}'",
        user.original_username, server.name
    );

    Ok(auth_response)
}

fn map_jellyfin_auth_error(err: JellyfinApiError) -> QuickConnectAuthError {
    match err {
        JellyfinApiError::AuthenticationFailed(_) => QuickConnectAuthError::InvalidCredentials,
        JellyfinApiError::Unauthorized | JellyfinApiError::Forbidden => {
            QuickConnectAuthError::InvalidCredentials
        }
        JellyfinApiError::Serialization(e) => QuickConnectAuthError::Parse(e.to_string()),
        JellyfinApiError::InvalidResponse(e) => QuickConnectAuthError::Parse(e),
        JellyfinApiError::Network(e) => QuickConnectAuthError::Network(e.to_string()),
        JellyfinApiError::ServerError(e) => QuickConnectAuthError::Network(e),
        JellyfinApiError::UrlParse(e) => QuickConnectAuthError::Internal(e.to_string()),
        JellyfinApiError::NotFound => QuickConnectAuthError::Internal(
            "Users/AuthenticateByName endpoint was not found on target server".to_string(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::http::HeaderValue;

    #[test]
    fn quick_connect_session_serializes_to_jellyfin_shape() {
        let session = QuickConnectSession::new(
            "secret-1".to_string(),
            "123456".to_string(),
            "device-1".to_string(),
            "Test Device".to_string(),
            "Test App".to_string(),
            "1.2.3".to_string(),
        );

        let value = serde_json::to_value(session).unwrap();
        let obj = value.as_object().unwrap();

        assert!(obj.contains_key("Authenticated"));
        assert!(obj.contains_key("Secret"));
        assert!(obj.contains_key("Code"));
        assert!(obj.contains_key("DeviceId"));
        assert!(obj.contains_key("DeviceName"));
        assert!(obj.contains_key("AppName"));
        assert!(obj.contains_key("AppVersion"));
        assert!(obj.contains_key("DateAdded"));

        assert!(!obj.contains_key("UserId"));
        assert!(!obj.contains_key("ExpiresAt"));
    }

    #[test]
    fn quick_connect_dto_accepts_pascal_and_camel_secret() {
        let pascal: QuickConnectAuthenticateRequest =
            serde_json::from_str(r#"{"Secret":"abc"}"#).unwrap();
        let camel: QuickConnectAuthenticateRequest =
            serde_json::from_str(r#"{"secret":"xyz"}"#).unwrap();

        assert_eq!(pascal.secret, "abc");
        assert_eq!(camel.secret, "xyz");
    }

    #[test]
    fn authorize_query_accepts_pascal_and_camel_user_id() {
        let camel: AuthorizeQuery =
            serde_json::from_str(r#"{"code":"123456","userId":"user-1"}"#).unwrap();
        let pascal: AuthorizeQuery =
            serde_json::from_str(r#"{"Code":"123456","UserId":"user-2"}"#).unwrap();
        let missing: AuthorizeQuery = serde_json::from_str(r#"{"code":"123456"}"#).unwrap();

        assert_eq!(camel.code, "123456");
        assert_eq!(camel.user_id.as_deref(), Some("user-1"));
        assert_eq!(pascal.user_id.as_deref(), Some("user-2"));
        assert!(missing.user_id.is_none());
    }

    #[test]
    fn extract_virtual_token_supports_auth_and_token_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static(
                "MediaBrowser Client=\"Jellyfin Web\", Device=\"Firefox\", DeviceId=\"abc\", Version=\"10.10.7\", Token=\"token-1\"",
            ),
        );
        assert_eq!(extract_virtual_token(&headers).as_deref(), Some("token-1"));

        let mut x_emby_token = HeaderMap::new();
        x_emby_token.insert("x-emby-token", HeaderValue::from_static("token-2"));
        assert_eq!(
            extract_virtual_token(&x_emby_token).as_deref(),
            Some("token-2")
        );

        let mut media_browser_token = HeaderMap::new();
        media_browser_token.insert("x-mediabrowser-token", HeaderValue::from_static("token-3"));
        assert_eq!(
            extract_virtual_token(&media_browser_token).as_deref(),
            Some("token-3")
        );
    }

    #[test]
    fn detects_android_tv_clients() {
        assert!(is_android_tv_client("Jellyfin Android TV"));
        assert!(is_android_tv_client("Jellyfin Android TV (debug)"));
        assert!(!is_android_tv_client("Jellyfin Android"));
        assert!(!is_android_tv_client("Jellyfin Web"));
    }

    #[test]
    fn derives_android_tv_device_id_from_uuid_user_id() {
        let base_device_id = "androidtv-device-123";
        let simple_user_id = "550e8400e29b41d4a716446655440000";
        let derived = derive_android_tv_user_device_id(base_device_id, simple_user_id);

        assert_eq!(derived, "7094964e96c0e66e00671463e263fbdffce6b001");
    }

    #[test]
    fn quick_connect_uses_android_tv_user_scoped_device_id() {
        let session = QuickConnectSession::new(
            "secret-1".to_string(),
            "123456".to_string(),
            "session-device".to_string(),
            "Android TV".to_string(),
            "Jellyfin Android TV".to_string(),
            "1.0.0".to_string(),
        );

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static(
                "MediaBrowser Client=\"Jellyfin Android TV\", Device=\"Chromecast\", DeviceId=\"androidtv-device-123\", Version=\"0.18.0\"",
            ),
        );

        let auth = effective_quick_connect_authorization(
            &headers,
            &session,
            "550e8400e29b41d4a716446655440000",
        );

        assert_eq!(auth.client, "Jellyfin Android TV");
        assert_eq!(auth.device, "Chromecast");
        assert_eq!(auth.version, "0.18.0");
        assert_eq!(auth.device_id, "7094964e96c0e66e00671463e263fbdffce6b001");
        assert!(auth.token.is_none());
    }

    #[test]
    fn quick_connect_falls_back_to_session_metadata_without_headers() {
        let session = QuickConnectSession::new(
            "secret-1".to_string(),
            "123456".to_string(),
            "session-device".to_string(),
            "Fallback Device".to_string(),
            "Unknown App".to_string(),
            "1.0.0".to_string(),
        );

        let headers = HeaderMap::new();
        let auth = effective_quick_connect_authorization(&headers, &session, "user-id");

        assert_eq!(auth.device_id, "session-device");
        assert_eq!(auth.device, "Fallback Device");
        assert_eq!(auth.client, "Unknown App");
    }
}
