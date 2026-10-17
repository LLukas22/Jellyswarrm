//! Account, authentication, and server identity application workflows.
//! HTTP adapters own extraction and rendering; backend requests are built once.
use hyper::{HeaderMap, StatusCode};
use tracing::{debug, error, info, warn};

use crate::{
    application::input::OperationRequest,
    encryption::{HashedPassword, Password},
    models::{AuthenticateRequest, AuthenticateResponse, Authorization, SyncPlayUserAccessType},
    upstream::jellyfin::{json::execute_json_request, request::UpstreamRequestBuilder},
    url_helper::join_server_url,
    user_authorization_service::{LocalCredential, MappingAuth},
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
pub(crate) fn public_users() -> Vec<crate::models::User> {
    // For now, return an empty list
    vec![]
}

pub(crate) async fn get_me(
    state: &AppState,
    input: OperationRequest,
) -> Result<crate::models::User, StatusCode> {
    // Preserve routing failures before the legacy RequireUser rejection.
    let resolved = input.resolve_target(state).await?;
    let user = input.context.user().ok_or(StatusCode::UNAUTHORIZED)?;
    let request = build_request(state, &resolved.target, &input.original_request).await?;
    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.reqwest_client, request).await?;

    let server_user = process_user(server_user, user, state).await.map_err(|e| {
        error!("Failed to process user: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(server_user)
}

pub(crate) async fn get_user_by_id(
    state: &AppState,
    mut input: OperationRequest,
) -> Result<crate::models::User, StatusCode> {
    let resolved = input.resolve_target(state).await?;
    let user = input.context.user().ok_or(StatusCode::UNAUTHORIZED)?;
    resolved
        .target
        .session()
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    // This endpoint always addresses the caller, not the requested path ID.
    // Normalize virtual input before translation; the old URL replacement also
    // deliberately discarded the incoming query.
    normalize_user_path(&mut input.original_request, &user.id);
    let request = build_request(state, &resolved.target, &input.original_request).await?;

    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.reqwest_client, request).await?;

    let server_user = process_user(server_user, user, state).await.map_err(|e| {
        error!("Failed to process user: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(server_user)
}

fn normalize_user_path(request: &mut reqwest::Request, user_id: &str) {
    request.url_mut().set_path(&format!("/Users/{user_id}"));
    request.url_mut().set_query(None);
}

async fn build_request(
    state: &AppState,
    target: &crate::application::context::AuthorizedTarget,
    request: &reqwest::Request,
) -> Result<reqwest::Request, StatusCode> {
    let prefix = state.get_url_prefix().await;
    UpstreamRequestBuilder::new(&state.processors.url_processor, prefix.as_deref())
        .build(target, request)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)
}

fn reported_server_version(ui_version: &str) -> String {
    // Jellyfin Web refuses empty/unknown versions ("Update Required").
    let version = ui_version.trim();
    if version.is_empty() || version.eq_ignore_ascii_case("unknown") {
        "12.0.0".to_string()
    } else {
        version.to_string()
    }
}

pub(crate) async fn public_server_info(
    state: &AppState,
    ui_version: &str,
) -> crate::models::PublicServerInfo {
    let cfg = state.config.read().await;
    crate::models::PublicServerInfo {
        id: cfg.server_id.clone(),
        server_name: cfg.server_name.clone(),
        local_address: cfg.public_address.clone(),
        version: reported_server_version(ui_version),
        product_name: "Jellyfin Server".to_string(),
        operating_system: std::env::consts::OS.to_string(),
        startup_wizard_completed: true,
    }
}

pub(crate) async fn server_info(
    state: &AppState,
    input: OperationRequest,
    ui_version: &str,
) -> Result<crate::models::ServerInfo, StatusCode> {
    let resolved = input.resolve_target(state).await?;
    input.context.user().ok_or(StatusCode::UNAUTHORIZED)?;
    // Match the legacy original authorization: only the Authorization variant
    // enabled this response, not X-Emby-Authorization after remapping.
    let is_seerr = matches!(
        input.context.authorization(),
        Some(crate::request_preprocessing::JellyfinAuthorization::Authorization(authorization))
            if is_seerr_client(authorization)
    );
    if !is_seerr {
        let request = build_request(state, &resolved.target, &input.original_request).await?;
        let mut server_info =
            execute_json_request::<crate::models::ServerInfo>(&state.reqwest_client, request)
                .await
                .inspect_err(|status| {
                    error!("Failed to get upstream server info: {status}");
                })?;
        let cfg = state.config.read().await;
        server_info.id = cfg.server_id.clone();
        server_info.server_name = cfg.server_name.clone();
        server_info.local_address = cfg.public_address.clone();
        server_info.version = Some(reported_server_version(ui_version));
        return Ok(server_info);
    }

    let cfg = state.config.read().await;
    Ok(crate::models::ServerInfo {
        operating_system_display_name: Some(std::env::consts::OS.to_string()),
        has_pending_restart: Some(false),
        is_shutting_down: Some(false),
        supports_library_monitor: Some(false),
        web_socket_port_number: None,
        completed_installations: None,
        can_self_restart: Some(false),
        can_launch_web_browser: Some(false),
        program_data_path: None,
        web_path: None,
        items_by_name_path: None,
        cache_path: None,
        log_path: None,
        internal_metadata_path: None,
        transcoding_temp_path: None,
        cast_receiver_applications: None,
        has_update_available: Some(false),
        encoder_location: None,
        system_architecture: Some(std::env::consts::ARCH.to_string()),
        local_address: cfg.public_address.clone(),
        server_name: cfg.server_name.clone(),
        version: Some(reported_server_version(ui_version)),
        operating_system: Some(std::env::consts::OS.to_string()),
        id: cfg.server_id.clone(),
        startup_wizard_completed: Some(true),
    })
}

// Authenticates a user by trying all configured servers in parallel
pub(crate) async fn authenticate_by_name(
    state: &AppState,
    headers: HeaderMap,
    payload: AuthenticateRequest,
) -> Result<AuthenticateResponse, StatusCode> {
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
        error!("No valid authorization header found in authentication request");
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

    let existing_user = state
        .user_authorization
        .get_user_by_username(&payload.username)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if existing_user.is_none() && !state.auto_create_users_on_login().await {
        warn!(
            "Auto user creation disabled; rejecting login for non-existing local user '{}'",
            payload.username
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    if !local_credential_allows_login(
        existing_user.as_ref().map(|user| &user.local_credential),
        &payload.password,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    {
        warn!(
            "Rejecting login for existing local user '{}' with invalid credentials",
            payload.username
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    let is_existing_user = existing_user.is_some();

    if let Some(user) = existing_user {
        let server_mappings = state
            .user_authorization
            .list_server_mappings(&user.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        if !server_mappings.is_empty() {
            for server_mapping in server_mappings {
                if let Some(pos) = servers
                    .iter()
                    .position(|s| s.id == server_mapping.server_id)
                {
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

    if is_existing_user {
        if !servers.is_empty() {
            info!(
                "Skipping {} unmapped servers for existing user '{}' during login",
                servers.len(),
                payload.username
            );
        }
    } else {
        // For first-time users we still probe all configured servers so mappings can be created.
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
    }

    // Wait for all authentication attempts to complete
    let mut successful_auths: Vec<SuccessfulServerAuth> = Vec::new();
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
        let user =
            resolve_or_create_login_user(state, &payload.username, &payload.password).await?;

        persist_successful_auths(state, &user, &authentication, &successful_auths).await?;

        let auth_response = decorate_auth_response(
            state,
            &user,
            &payload.username,
            &authentication,
            &successful_auths[0],
        )
        .await;

        info!(
            "User '{}' successfully authenticated on {} out of {} servers and stored in authorization storage",
            payload.username,
            successful_auths.len(),
            total_servers
        );
        Ok(auth_response)
    }
}

async fn local_credential_allows_login(
    local_credential: Option<&LocalCredential>,
    password: &Password,
) -> Result<bool, sqlx::Error> {
    match local_credential {
        Some(credential) => credential.verify_async(password).await,
        None => Ok(true),
    }
}

async fn resolve_or_create_login_user(
    state: &AppState,
    username: &str,
    password: &Password,
) -> Result<crate::user_authorization_service::User, StatusCode> {
    state
        .user_authorization
        .resolve_or_create_login_user(username, password)
        .await
        .map_err(|e| {
            tracing::error!("Error resolving local user for login: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::UNAUTHORIZED)
}

async fn persist_successful_auths(
    state: &AppState,
    user: &crate::user_authorization_service::User,
    login_authorization: &Authorization,
    successful_auths: &[SuccessfulServerAuth],
) -> Result<(), StatusCode> {
    let mapping_key = user.local_credential.mapping_key();

    for successful in successful_auths {
        if let Some(final_password) = &successful.final_password {
            state
                .user_authorization
                .add_server_mapping(
                    &user.id,
                    &successful.server,
                    &successful.final_username,
                    final_password,
                    Some(&mapping_key),
                )
                .await
                .map_err(|e| {
                    tracing::error!("Error updating server mapping after authentication: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
        }

        let mut auth_to_store = login_authorization.clone();
        auth_to_store.token = Some(successful.auth_response.access_token.clone());

        state
            .user_authorization
            .store_authorization_session(
                &user.id,
                &successful.server,
                &auth_to_store,
                successful.auth_response.access_token.clone(),
                successful.auth_response.user.id.clone(),
                None,
            )
            .await
            .map_err(|e| {
                tracing::error!("Error storing authorization session: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
    }

    Ok(())
}

async fn decorate_auth_response(
    state: &AppState,
    user: &crate::user_authorization_service::User,
    login_username: &str,
    login_authorization: &Authorization,
    successful_auth: &SuccessfulServerAuth,
) -> AuthenticateResponse {
    let mut auth_response = successful_auth.auth_response.clone();
    let server_id = state.config.read().await.server_id.clone();
    auth_response.server_id = server_id.clone();
    auth_response.user.server_id = server_id.clone();
    auth_response.session_info.server_id = server_id;
    auth_response.session_info.user_id = user.id.clone();
    auth_response.user.name = login_username.to_string();
    auth_response.session_info.user_name = login_username.to_string();
    // Seerr requires this flag to finish setup. Authorization remains enforced by
    // Jellyswarrm and the user's real upstream sessions.
    auth_response.user.policy.is_administrator = is_seerr_client(login_authorization);
    auth_response.user.policy.sync_play_access = SyncPlayUserAccessType::CreateAndJoinGroups;
    auth_response.access_token = user.virtual_key.clone();
    auth_response.user.id = user.id.clone();
    crate::sessions::decorate_authentication(state, user, login_authorization, &mut auth_response)
        .await;
    auth_response
}

fn is_seerr_client(authorization: &Authorization) -> bool {
    authorization.client.eq_ignore_ascii_case("Seerr")
        && authorization.device.eq_ignore_ascii_case("Seerr")
}

#[cfg(test)]
mod tests {
    use super::{
        extract_auth_header, is_seerr_client, local_credential_allows_login, normalize_user_path,
        reported_server_version,
    };
    use crate::{
        encryption::{HashedPassword, Password},
        models::Authorization,
        user_authorization_service::LocalCredential,
    };

    fn authorization(client: &str, device: &str) -> Authorization {
        Authorization {
            client: client.to_string(),
            device: device.to_string(),
            device_id: "device-id".to_string(),
            version: "1.0.0".to_string(),
            token: None,
        }
    }

    #[test]
    fn seerr_client_requires_matching_client_and_device() {
        assert!(is_seerr_client(&authorization("Seerr", "Seerr")));
    }

    #[test]
    fn seerr_client_rejects_spoofed_device_from_other_client() {
        assert!(!is_seerr_client(&authorization("Jellyfin Web", "Seerr")));
    }

    #[test]
    fn user_by_id_normalizes_virtual_input_and_drops_query() {
        let mut request = reqwest::Request::new(
            hyper::Method::GET,
            "http://proxy.example/prefix/Users/requested?Fields=Policy&api_key=virtual"
                .parse()
                .unwrap(),
        );
        request.headers_mut().insert(
            "x-emby-token",
            hyper::header::HeaderValue::from_static("virtual"),
        );
        normalize_user_path(&mut request, "caller");
        assert_eq!(request.url().path(), "/Users/caller");
        assert_eq!(request.url().query(), None);
        // Credentials stay untouched until the single target-bound build.
        assert_eq!(request.headers()["x-emby-token"], "virtual");
    }

    #[test]
    fn reported_version_keeps_ui_compatible_fallback() {
        assert_eq!(reported_server_version(""), "12.0.0");
        assert_eq!(reported_server_version(" UNKNOWN "), "12.0.0");
        assert_eq!(reported_server_version(" 10.11.2 "), "10.11.2");
    }

    #[test]
    fn login_header_prefers_authorization_and_does_not_fall_back_if_invalid() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("authorization", "invalid".parse().unwrap());
        headers.insert(
            "x-emby-authorization",
            authorization("Seerr", "Seerr")
                .to_header_value()
                .parse()
                .unwrap(),
        );
        assert!(extract_auth_header(&headers).is_err());
        headers.remove("authorization");
        assert!(is_seerr_client(&extract_auth_header(&headers).unwrap()));
    }

    #[test]
    fn login_rejects_missing_authorization() {
        assert!(extract_auth_header(&hyper::HeaderMap::new()).is_err());
    }

    #[tokio::test]
    async fn existing_local_credential_must_match_before_upstream_login() {
        let passwordless = LocalCredential::Passwordless;
        assert!(
            local_credential_allows_login(Some(&passwordless), &Password::from(""))
                .await
                .unwrap()
        );
        assert!(
            !local_credential_allows_login(Some(&passwordless), &Password::from("anything"))
                .await
                .unwrap()
        );

        let protected = LocalCredential::Password(HashedPassword::from_password("correct"));
        assert!(
            local_credential_allows_login(Some(&protected), &Password::from("correct"))
                .await
                .unwrap()
        );
        assert!(
            !local_credential_allows_login(Some(&protected), &Password::from("wrong"))
                .await
                .unwrap()
        );
        assert!(
            local_credential_allows_login(None, &Password::from("upstream-password"))
                .await
                .unwrap()
        );
    }
}

/// Authenticates a user on a specific server
async fn authenticate_on_server(
    state: AppState,
    authorization: Authorization,
    payload: AuthenticateRequest,
    server: crate::server_storage::Server,
    server_mapping: Option<crate::user_authorization_service::ServerMapping>,
) -> Result<SuccessfulServerAuth, AuthError> {
    let auth_url = join_server_url(&server.url, "/Users/AuthenticateByName");

    info!(
        "Authenticating user '{}' at server '{}' ({})",
        payload.username, server.name, auth_url
    );

    // Get user mapping for this server
    let config = state.config.read().await;
    let admin_password = &config.password;

    let given_password = payload.password.clone();
    // Only old mappings use a password-derived key; modern mappings use the
    // persistent server secret. Never generate a salted verifier here.
    let user_mapping_key = HashedPassword::from_password(given_password.as_str());

    if let Some(mapping) = &server_mapping {
        if matches!(mapping.auth, MappingAuth::QuickConnect { .. }) {
            let local_user = state
                .user_authorization
                .get_user_by_username(&payload.username)
                .await
                .map_err(|_| AuthError::InternalError)?
                .ok_or(AuthError::InvalidCredentials)?;
            let (token, remote_user) = crate::mapping_auth::validated_quick_connect_token(
                &state,
                &local_user,
                &server,
                mapping,
            )
            .await
            .map_err(|_| AuthError::InvalidCredentials)?;
            let server_id = state.config.read().await.server_id.clone();
            return Ok(SuccessfulServerAuth {
                server,
                auth_response: AuthenticateResponse {
                    user: remote_user,
                    session_info: crate::models::SessionInfo {
                        user_id: local_user.id,
                        user_name: payload.username.clone(),
                        server_id: server_id.clone(),
                        extra: Default::default(),
                    },
                    access_token: token,
                    server_id,
                },
                final_username: mapping.auth.username().to_string(),
                final_password: None,
            });
        }
    }
    let (final_username, final_password) = if let Some(mapping) = &server_mapping {
        (
            mapping.auth.username().to_string(),
            state
                .user_authorization
                .decrypt_server_mapping_password(
                    mapping,
                    &user_mapping_key,
                    &admin_password.into(),
                    Some(&given_password),
                    Some(admin_password),
                )
                .map_err(|_| AuthError::InvalidCredentials)?,
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

    tracing::trace!(
        "Received authentication response from {} ({} bytes)",
        server.name,
        response_text.len()
    );

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

    info!(
        "Successfully authenticated user '{}' on server '{}'",
        payload.username, server.name
    );
    Ok(SuccessfulServerAuth {
        server,
        auth_response,
        final_username,
        final_password: Some(final_password),
    })
}

/// Extracts authorization header
fn extract_auth_header(headers: &HeaderMap) -> Result<Authorization, AuthError> {
    if let Some(raw_auth) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse(raw_auth) {
            debug!("Extracted 'Authorization' header: {}", auth);
            Ok(auth)
        } else {
            warn!("Invalid 'Authorization' header format");
            Err(AuthError::ParseError(
                "Invalid 'Authorization' header format".to_string(),
            ))
        }
    } else if let Some(raw_auth) = headers
        .get("x-emby-authorization")
        .and_then(|value| value.to_str().ok())
    {
        if let Ok(auth) = Authorization::parse_with_legacy(raw_auth, true) {
            debug!("Extracted 'X-Emby-Authorization' header: {}", auth);
            Ok(auth)
        } else {
            warn!("Invalid 'X-Emby-Authorization' header format");
            Err(AuthError::ParseError(
                "Invalid 'X-Emby-Authorization' header format".to_string(),
            ))
        }
    } else {
        error!("No 'Authorization' header found in login request");

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

#[derive(Debug, Clone)]
struct SuccessfulServerAuth {
    server: crate::server_storage::Server,
    auth_response: AuthenticateResponse,
    final_username: String,
    final_password: Option<crate::encryption::Password>,
}
