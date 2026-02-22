use crate::{
    encryption::HashedPassword,
    models::{
        AuthenticateRequest as JellyfinAuthenticateRequest, AuthenticateResponse, Authorization,
        SyncPlayUserAccessType,
    },
    url_helper::join_server_url,
    AppState,
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Duration, Utc};
use hyper::StatusCode;
use jellyswarrm_macros::multi_case_struct;
use rand::Rng;
use serde::{Deserialize, Serialize};
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
    pub user_id: String,
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

fn parse_client_info(headers: &HeaderMap) -> (String, String, String, String) {
    if let Some(header) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse(header) {
            Ok(auth) => {
                return (auth.device_id, auth.device, auth.client, auth.version);
            }
            Err(e) => warn!("Failed to parse Authorization header: {}", e),
        }
    }

    if let Some(header) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        match Authorization::parse_with_legacy(header, true) {
            Ok(auth) => {
                return (auth.device_id, auth.device, auth.client, auth.version);
            }
            Err(e) => warn!("Failed to parse X-Emby-Authorization header: {}", e),
        }
    }

    (
        Uuid::new_v4().to_string(),
        "Unknown Device".to_string(),
        "Unknown App".to_string(),
        "1.0.0".to_string(),
    )
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
) -> Result<Json<bool>, StatusCode> {
    let success = state
        .quick_connect
        .update_session_by_code(&params.code, |session| {
            session.authenticated = true;
            session.user_id = Some(params.user_id.clone());
        });

    if success {
        info!(
            "Authorized Quick Connect code {} for user {}",
            params.code, params.user_id
        );
        Ok(Json(true))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
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

    let authorization = Authorization {
        client: session.app_name,
        device: session.device_name,
        device_id: session.device_id,
        version: session.app_version,
        token: None,
    };

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
    let auth_url = join_server_url(&server.url, "/Users/AuthenticateByName");

    let admin_password = state.get_admin_password().await;
    let admin_password_hash: HashedPassword = (&admin_password).into();

    let mapped_password = state.user_authorization.decrypt_server_mapping_password(
        &server_mapping,
        &user.original_password_hash,
        &admin_password_hash,
    );

    let auth_payload = JellyfinAuthenticateRequest {
        username: server_mapping.mapped_username.clone(),
        password: mapped_password.clone(),
    };

    let response = state
        .reqwest_client
        .post(auth_url.as_str())
        .header("Authorization", authorization.to_header_value())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&auth_payload)
        .send()
        .await
        .map_err(|e| QuickConnectAuthError::Network(e.to_string()))?;

    if !response.status().is_success() {
        return Err(QuickConnectAuthError::InvalidCredentials);
    }

    let response_text = response
        .text()
        .await
        .map_err(|e| QuickConnectAuthError::Network(e.to_string()))?;

    let mut auth_response = serde_json::from_str::<AuthenticateResponse>(&response_text)
        .map_err(|e| QuickConnectAuthError::Parse(e.to_string()))?;

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
