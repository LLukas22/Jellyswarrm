use axum::{
    extract::{Path, Request, State},
    Json,
};
use hyper::{HeaderMap, StatusCode};
use tracing::{debug, error, info, warn};

use crate::{
    handlers::common::execute_json_request,
    models::{AuthenticateRequest, AuthenticateResponse, Authorization, SyncPlayUserAccessType},
    rate_limiter::extract_client_ip,
    request_preprocessing::preprocess_request,
    url_helper::join_server_url,
    AppState,
};

use anyhow::Result;

async fn process_user(
    server_user: crate::models::User,
    user: &crate::user_authorization_service::User,
    state: &AppState,
) -> Result<crate::models::User> {
    let mut server_user = server_user;

    server_user.id = user.id.clone();
    server_user.name = user.original_username.clone();
    server_user.policy.is_administrator = false;

    server_user.server_id = state.config.read().await.server_id.clone();

    Ok(server_user)
}

// http://foo:3000/users/public?)
pub async fn handle_public(
    _state: State<AppState>,
) -> Result<Json<Vec<crate::models::User>>, StatusCode> {
    // For now, return an empty list
    Ok(Json(vec![]))
}

pub async fn handle_get_me(
    State(state): State<AppState>,
    req: Request,
) -> Result<Json<crate::models::User>, StatusCode> {
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let user = preprocessed.user.ok_or(StatusCode::UNAUTHORIZED)?;

    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.reqwest_client, preprocessed.request).await?;

    let server_user = process_user(server_user, &user, &state)
        .await
        .map_err(|e| {
            error!("Failed to process user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(server_user))
}

pub async fn handle_get_user_by_id(
    State(state): State<AppState>,
    Path(_user_id): Path<String>,
    req: Request,
) -> Result<Json<crate::models::User>, StatusCode> {
    // Preprocess request and extract required data
    let preprocessed = preprocess_request(req, &state).await.map_err(|e| {
        error!("Failed to preprocess request: {}", e);
        StatusCode::BAD_REQUEST
    })?;

    let session = preprocessed.session.ok_or(StatusCode::UNAUTHORIZED)?;
    let user: crate::user_authorization_service::User =
        preprocessed.user.ok_or(StatusCode::UNAUTHORIZED)?;

    // Build request URL using helper function to preserve subdirectories
    let user_path = format!("/Users/{}", session.original_user_id);
    let user_url = join_server_url(&preprocessed.server.url, &user_path);

    let mut request = preprocessed.request;
    *request.url_mut() = user_url;

    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.reqwest_client, request).await?;

    let server_user = process_user(server_user, &user, &state)
        .await
        .map_err(|e| {
            error!("Failed to process user: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(server_user))
}

// Authenticates a user by trying all configured servers in parallel
pub async fn handle_authenticate_by_name(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<AuthenticateRequest>,
) -> Result<Json<AuthenticateResponse>, StatusCode> {
    // Track auth attempt for statistics
    state.statistics.counters().increment_auth_attempts();

    // Rate limiting check (use headers for client IP, no direct socket info available)
    if let Some(client_ip) = extract_client_ip(&headers, None) {
        if !state.rate_limiter.check(client_ip).await {
            state.statistics.counters().increment_rate_limited();
            warn!("Rate limit exceeded for authentication from IP: {}", client_ip);
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }
    }

    let mut servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if servers.is_empty() {
        tracing::warn!("No servers configured for authentication");
        return Err(StatusCode::NOT_FOUND);
    }

    let authentication = extract_auth_header(&headers).map_err(|_| {
        error!(
            "No valid 'Authorization' header found in authentication request! Headers: {:?}",
            headers
        );
        state.statistics.counters().increment_auth_failures();
        StatusCode::BAD_REQUEST
    })?;

    info!(
        "Got login request with authentication header: {}",
        authentication.to_redacted_header_value()
    );

    info!(
        "Attempting authentication for user '{}' across {} servers",
        payload.username,
        servers.len()
    );

    let mut auth_tasks = Vec::with_capacity(servers.len());

    if let Some(user) = state
        .user_authorization
        .get_user_by_credentials(&payload.username, &payload.password)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        let server_mappings = state
            .user_authorization
            .list_server_mappings(&user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if !server_mappings.is_empty() {
            for server_mapping in server_mappings {
                if let Some(pos) = servers.iter().position(|s| {
                    s.url.as_str().trim_end_matches('/')
                        == server_mapping.server_url.trim_end_matches('/')
                }) {
                    let server = servers.remove(pos);
                    info!(
                        "Using server mapping for user '{}' on server '{}'",
                        &payload.username, server.name
                    );
                    {
                        let state = state.clone();
                        let authentication = authentication.clone();
                        let payload = payload.clone();
                        auth_tasks.push(tokio::spawn(async move {
                            authenticate_on_server(
                                state.clone(),
                                authentication.clone(),
                                payload.clone(),
                                server,
                                Some(server_mapping),
                            )
                            .await
                        }));
                    }
                }
            }
        }
    }

    // also try to authenticate on leftover servers without a mapping
    let mut leftover_tasks: Vec<_> = servers
        .into_iter()
        .map(|server| {
            let state = state.clone();
            let authentication = authentication.clone();
            let payload = payload.clone();
            info!(
                "No server mapping found for user '{}' on server '{}'",
                payload.username, server.name
            );

            tokio::spawn(async move {
                authenticate_on_server(state, authentication, payload, server, None).await
            })
        })
        .collect();

    auth_tasks.append(&mut leftover_tasks);

    // Wait for all authentication attempts to complete
    let mut successful_auths = Vec::new();
    let total_servers = auth_tasks.len();

    for task in auth_tasks {
        match task.await {
            Ok(Ok(auth_response)) => {
                info!("Successfully authenticated user: {}", payload.username);
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

    if successful_auths.is_empty() {
        tracing::warn!(
            "All authentication attempts failed for user: {}",
            payload.username
        );
        Err(StatusCode::UNAUTHORIZED)
    } else {
        info!(
            "User '{}' successfully authenticated on {} out of {} servers and stored in authorization storage",
            payload.username,
            successful_auths.len(),
            total_servers
        );
        // Return the first successful authentication (you could also implement priority logic here)
        Ok(Json(successful_auths[0].clone()))
    }
}

/// Authenticates a user on a specific server
async fn authenticate_on_server(
    state: AppState,
    authorization: Authorization,
    payload: AuthenticateRequest,
    server: crate::server_storage::Server,
    server_mapping: Option<crate::user_authorization_service::ServerMapping>,
) -> Result<AuthenticateResponse, AuthError> {
    let auth_url = join_server_url(&server.url, "/Users/AuthenticateByName");

    info!(
        "Authenticating user '{}' at server '{}' ({})",
        payload.username, server.name, auth_url
    );

    // Get user mapping for this server
    let config = state.config.read().await;
    let admin_password = &config.password;

    let given_password = payload.password.clone();

    let (final_username, final_password) = if let Some(mapping) = &server_mapping {
        (
            mapping.mapped_username.clone(),
            state.user_authorization.decrypt_server_mapping_password(
                mapping,
                &given_password.clone().into(),
                &admin_password.into(),
            ),
        )
    } else {
        (payload.username.clone(), payload.password.clone())
    };

    // Create authentication payload
    let auth_payload = AuthenticateRequest {
        username: final_username.clone(),
        password: final_password.clone(),
    };

    // Make authentication request
    let response = state
        .reqwest_client
        .post(auth_url.as_str())
        .header("Authorization", authorization.to_header_value())
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .json(&auth_payload)
        .send()
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to send authentication request to {}: {}",
                server.name,
                e
            );
            AuthError::NetworkError(e.to_string())
        })?;

    // Check response status
    if !response.status().is_success() {
        tracing::warn!(
            "Authentication failed for server '{}' with status: {}",
            server.name,
            response.status()
        );
        return Err(AuthError::InvalidCredentials);
    }

    // Parse response
    let response_text = response.text().await.map_err(|e| {
        tracing::error!(
            "Failed to read authentication response from {}: {}",
            server.name,
            e
        );
        AuthError::NetworkError(e.to_string())
    })?;

    tracing::trace!("Raw response from {}: {}", server.name, response_text);

    let auth_response =
        serde_json::from_str::<AuthenticateResponse>(&response_text).map_err(|e| {
            tracing::error!(
                "Failed to parse authentication response from {}: {}. Response body: {}",
                server.name,
                e,
                response_text
            );
            AuthError::ParseError(e.to_string())
        })?;

    let mut auth_response = auth_response;

    // We authenticated sucessfully, now we need to get the user or create it
    let user = state
        .user_authorization
        .get_or_create_user(&payload.username, &given_password)
        .await
        .map_err(|e| {
            tracing::error!("Error getting user: {}", e);
            AuthError::InternalError
        })?;

    // Update or create server mapping to ensure it's encrypted
    // This handles creating new mappings and upgrading legacy plaintext mappings
    info!(
        "Updating server mapping for user '{}' on server '{}'",
        payload.username, server.name
    );
    state
        .user_authorization
        .add_server_mapping(
            &user.id,
            server.url.as_str(),
            &final_username,
            &final_password,
            Some(&given_password.into()),
        )
        .await
        .map_err(|e| {
            tracing::error!("Error updating server mapping: {}", e);
            AuthError::InternalError
        })?;

    let auth_token = auth_response.access_token.clone();

    let original_user_id = auth_response.user.id.clone();

    let server_id = state.config.read().await.server_id.clone();
    auth_response.server_id = server_id.clone();
    auth_response.user.server_id = server_id.clone();
    auth_response.session_info.server_id = server_id.clone();

    auth_response.session_info.user_id = user.id.clone();

    // Restore original username in response
    auth_response.user.name = payload.username.clone();
    auth_response.session_info.user_name = payload.username.clone();

    // Modify admin status (security measure)
    auth_response.user.policy.is_administrator = false;
    // Disable SyncPlay access
    auth_response.user.policy.sync_play_access = SyncPlayUserAccessType::None;

    // Generate a unique access token for this authentication
    auth_response.access_token = user.virtual_key.clone();

    // Use our user id as the user ID in the response
    auth_response.user.id = user.id.clone();

    // Store authorization data with the new access token
    let mut auth_to_store = authorization.clone();
    auth_to_store.token = Some(auth_token.clone());

    // Store authorization session
    state
        .user_authorization
        .store_authorization_session(
            &user.id,
            server.url.as_str(),
            &auth_to_store,
            auth_token.clone(),
            original_user_id, // Store the original Jellyfin user ID
            None,             // No expiration for now
        )
        .await
        .map_err(|e| {
            tracing::error!("Error storing authorization session: {}", e);
            AuthError::InternalError
        })?;

    info!(
        "Successfully authenticated user '{}' on server '{}' and stored authorization data with token: {}",
        payload.username, server.name, auth_token
    );
    Ok(auth_response)
}

/// Extracts authorization header
fn extract_auth_header(headers: &HeaderMap) -> Result<Authorization, AuthError> {
    if let Some(raw_auth) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse(raw_auth) {
            debug!("Extracted 'Authorization' header: {}", raw_auth);
            Ok(auth)
        } else {
            warn!("Invalid 'Authorization' header format: {}", raw_auth);
            Err(AuthError::ParseError(
                "Invalid 'Authorization' header format".to_string(),
            ))
        }
    } else if let Some(raw_auth) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse_with_legacy(raw_auth, true) {
            debug!("Extracted 'X-Emby-Authorization' header: {}", raw_auth);
            Ok(auth)
        } else {
            warn!("Invalid 'Authorization' header format: {}", raw_auth);
            Err(AuthError::ParseError(
                "Invalid 'X-Emby-Authorization' header format".to_string(),
            ))
        }
    } else {
        error!(
            "No 'Authorization' header found in login request! Headers: {:?}",
            headers
        );

        Err(AuthError::ParseError(
            "No 'Authorization' header found in login request!".to_string(),
        ))
    }
}

/// Custom error type for authentication operations
#[derive(Debug)]
#[allow(dead_code)]
enum AuthError {
    NetworkError(String),
    InvalidCredentials,
    ParseError(String),
    InternalError,
}
