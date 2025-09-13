use crate::{
    handlers::users::authenticate_on_server,
    models::{AuthenticateResponse, Authorization},
    AppState,
};
use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use hyper::StatusCode;
use jellyswarrm_macros::multi_case_struct;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

// Types for Quick Connect
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
            expires_at: now + chrono::Duration::minutes(10), // Sessions expire after 10 minutes
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
pub struct AuthenticateRequest {
    pub secret: String,
}

// In-memory storage for Quick Connect sessions
pub struct QuickConnectStorage {
    sessions: Arc<Mutex<HashMap<String, QuickConnectSession>>>,
}

impl QuickConnectStorage {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Store a new session, indexed by both secret and code
    pub fn store_session(&self, session: QuickConnectSession) {
        let mut sessions = self.sessions.lock().unwrap();
        sessions.insert(session.secret.clone(), session.clone());
        sessions.insert(session.code.clone(), session);
    }

    /// Get session by secret or code, automatically cleaning up expired sessions
    pub fn get_session(&self, key: &str) -> Option<QuickConnectSession> {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.get(key) {
            if session.is_expired() {
                // Clean up expired session
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

    /// Update an existing session (by code), automatically handles dual indexing
    pub fn update_session_by_code(
        &self,
        code: &str,
        mut updater: impl FnMut(&mut QuickConnectSession),
    ) -> bool {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.get(code).cloned() {
            if session.is_expired() {
                // Clean up expired session
                let secret = session.secret.clone();
                sessions.remove(&secret);
                sessions.remove(code);
                return false;
            }

            let mut updated_session = session;
            updater(&mut updated_session);

            // Update both secret and code entries
            let secret = updated_session.secret.clone();
            sessions.insert(secret, updated_session.clone());
            sessions.insert(code.to_string(), updated_session);

            return true;
        }
        false
    }

    /// Remove a session by secret, also removes the code entry
    pub fn remove_session(&self, secret: &str) -> Option<QuickConnectSession> {
        let mut sessions = self.sessions.lock().unwrap();

        if let Some(session) = sessions.remove(secret) {
            sessions.remove(&session.code);
            return Some(session);
        }
        None
    }

    /// Clean up all expired sessions (can be called periodically)
    pub fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.lock().unwrap();
        let mut to_remove = Vec::new();

        // Find all expired sessions
        for (key, session) in sessions.iter() {
            if session.is_expired() {
                to_remove.push((key.clone(), session.secret.clone(), session.code.clone()));
            }
        }

        // Remove expired sessions
        let count = to_remove.len() / 2; // Each session is stored twice (by secret and code)
        for (_, secret, code) in to_remove {
            sessions.remove(&secret);
            sessions.remove(&code);
        }

        count
    }

    /// Get current session count (for monitoring/debugging)
    pub fn session_count(&self) -> usize {
        let sessions = self.sessions.lock().unwrap();
        sessions.len() / 2 // Each session is stored twice
    }

    /// Start a background task to periodically clean up expired sessions
    pub fn start_cleanup_task(storage: QuickConnectStorage) {
        use std::time::Duration;
        use tokio::time;

        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(60)); // Clean up every minute
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
    use rand::distributions::Uniform;

    rand::thread_rng()
        .sample_iter(Uniform::new(0, 10))
        .take(6)
        .map(|n| char::from_digit(n, 10).unwrap())
        .collect::<String>()
}

fn parse_client_info(headers: &HeaderMap) -> (String, String, String, String) {
    if let Some(auth_header) = headers.get("authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            match Authorization::parse(auth_str) {
                Ok(auth) => {
                    return (auth.device_id, auth.device, auth.client, auth.version);
                }
                Err(e) => {
                    // Log the error but continue with fallback
                    warn!("Failed to parse authorization header: {}", e);
                }
            }
        } else {
            warn!("Authorization header contains invalid UTF-8");
        }
    }

    // Fallback values when no valid authorization header is present
    (
        Uuid::new_v4().to_string(),
        "Unknown Device".to_string(),
        "Unknown App".to_string(),
        "1.0.0".to_string(),
    )
}

// POST /QuickConnect/Initiate
pub async fn handle_quick_connect_initiate(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<QuickConnectSession>, StatusCode> {
    let secret = Uuid::new_v4().to_string();
    let code = generate_code();
    let (device_id, device_name, app_name, app_version) = parse_client_info(&headers);
    info!("Initiating Quick Connect session: device_id={}, device_name={}, app_name={}, app_version={}", device_id, device_name, app_name, app_version);

    let session = QuickConnectSession::new(
        secret.clone(),
        code.clone(),
        device_id,
        device_name,
        app_name,
        app_version,
    );

    // Store session using the new storage API
    state.quick_connect.store_session(session.clone());
    state.quick_connect.cleanup_expired(); // Clean up expired sessions on each initiation

    Ok(Json(session))
}

// POST /QuickConnect/Authorize?code={CODE}&userId={UUID}
pub async fn handle_quick_connect_authorize(
    Query(params): Query<AuthorizeQuery>,
    State(state): State<AppState>,
) -> Result<Json<bool>, StatusCode> {
    let success = state
        .quick_connect
        .update_session_by_code(&params.code, |session| {
            session.authenticated = true;
            session.user_id = Some(params.user_id.clone());
            info!(
                "Authorized Quick Connect session `{}` for user_id={}",
                params.code, params.user_id
            );
        });

    if success {
        Ok(Json(true))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

// GET /QuickConnect/Connect?secret={SECRET}
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

// POST /Users/AuthenticateWithQuickConnect
pub async fn handle_authenticate_with_quick_connect(
    State(state): State<AppState>,
    Json(request): Json<AuthenticateRequest>,
) -> Result<Json<AuthenticateResponse>, StatusCode> {
    if let Some(session) = state.quick_connect.get_session(&request.secret) {
        if session.authenticated && session.user_id.is_some() {
            let user_id = session.user_id.as_ref().unwrap().clone();
            if let Some(user) = state
                .user_authorization
                .get_user_by_id(&user_id)
                .await
                .map_err(|e| {
                    warn!(
                        "Database error while fetching user by ID {}: {}",
                        user_id, e
                    );
                    StatusCode::INTERNAL_SERVER_ERROR
                })?
            {
                info!(
                    "Authenticating Quick Connect session for user: {}",
                    user.original_username
                );

                let mut servers = state
                    .server_storage
                    .list_servers()
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                if servers.is_empty() {
                    tracing::warn!("No servers configured for authentication");
                    return Err(StatusCode::NOT_FOUND);
                }

                let server_mappings = state
                    .user_authorization
                    .list_server_mappings(&user.id)
                    .await
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

                if server_mappings.is_empty() {
                    warn!("User {} has no server mappings!", user.original_username);
                    return Err(StatusCode::UNAUTHORIZED);
                }

                let mut auth_tasks = Vec::with_capacity(server_mappings.len());

                let authorization = Authorization {
                    client: session.app_name.clone(),
                    device: session.device_name.clone(),
                    device_id: session.device_id.clone(),
                    version: session.app_version.clone(),
                    token: None,
                };

                for server_mapping in server_mappings {
                    if let Some(pos) = servers.iter().position(|s| {
                        s.url.as_str().trim_end_matches('/')
                            == server_mapping.server_url.trim_end_matches('/')
                    }) {
                        let server = servers.remove(pos);
                        info!(
                            "Using server mapping for user '{}' on server '{}'",
                            &user.original_username, server.name
                        );
                        {
                            let state = state.clone();
                            let authorization = authorization.clone();
                            let username = user.original_username.clone();
                            let password = "UNKNOWN".to_string(); // Password is not used in Quick Connect
                            auth_tasks.push(tokio::spawn(async move {
                                authenticate_on_server(
                                    state.clone(),
                                    authorization,
                                    username,
                                    password,
                                    server,
                                    Some(server_mapping),
                                )
                                .await
                            }));
                        }
                    }
                }

                // Wait for all authentication attempts to complete
                let mut successful_auths = Vec::new();
                let total_servers = auth_tasks.len();

                for task in auth_tasks {
                    match task.await {
                        Ok(Ok(auth_response)) => {
                            info!(
                                "Successfully authenticated user: {}",
                                user.original_username
                            );
                            successful_auths.push(auth_response);
                        }
                        Ok(Err(e)) => {
                            tracing::debug!("Authentication attempt failed: {:?}", e);
                        }
                        Err(join_err) => {
                            tracing::error!("Authentication task failed: {}", join_err);
                        }
                    }
                }

                state.quick_connect.remove_session(&request.secret);

                if successful_auths.is_empty() {
                    tracing::warn!(
                        "All authentication attempts failed for user: {}",
                        user.original_username
                    );
                    return Err(StatusCode::UNAUTHORIZED);
                } else {
                    info!(
                    "User '{}' successfully authenticated on {} out of {} servers and stored in authorization storage",
                    user.original_username,
                    successful_auths.len(),
                    total_servers
                );
                    // Return the first successful authentication (you could also implement priority logic here)
                    return Ok(Json(successful_auths[0].clone()));
                }
            } else {
                warn!("User ID from Quick Connect session not found: {}", user_id);
                return Err(StatusCode::UNAUTHORIZED);
            }
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn handle_quick_connect_enabled() -> Result<Json<bool>, StatusCode> {
    Ok(Json(true))
}
