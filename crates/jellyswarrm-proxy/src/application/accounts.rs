//! Account, authentication, and server identity application workflows.
//! HTTP adapters own extraction and rendering; backend requests are built once.
use crate::application::error::{ApplicationError, FailureStage};
use hyper::HeaderMap;
use tracing::{debug, error, info, warn};

use crate::{
    application::input::OperationRequest,
    auth::encryption::Password,
    auth::service::LocalCredential,
    models::{AuthenticateRequest, AuthenticateResponse, Authorization, SyncPlayUserAccessType},
    upstream::jellyfin::{
        json::execute_json_request_typed as execute_json_request, request::UpstreamRequestBuilder,
    },
};

use anyhow::Result;
pub(crate) mod services;
use services::{AccountService, AuthenticationService};

async fn process_user(
    server_user: crate::models::User,
    user: &crate::auth::service::User,
    state: &AuthenticationService,
) -> Result<crate::models::User> {
    let mut server_user = server_user;

    server_user.id = user.id.to_string();
    server_user.name = user.original_username.clone();
    server_user.policy.is_administrator = false;

    server_user.server_id = state.settings.read().await.server_id.clone();

    Ok(server_user)
}

// http://foo:3000/users/public?)
pub(crate) fn public_users() -> Vec<crate::models::User> {
    // For now, return an empty list
    vec![]
}

pub(crate) async fn get_me(
    state: &AccountService,
    input: OperationRequest,
) -> Result<crate::models::User, ApplicationError> {
    // Preserve routing failures before the legacy RequireUser rejection.
    let resolved = input
        .resolve_target_using(&state.routing_services())
        .await?;
    let user = input.context.user().ok_or_else(unauthenticated)?;
    let request = build_request(state, &resolved.target, &input.original_request).await?;
    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.authentication.reqwest_client, request).await?;

    let server_user = process_user(server_user, user, &state.authentication)
        .await
        .map_err(|e| {
            error!("Failed to process user: {}", e);
            ApplicationError::Internal(e)
        })?;

    Ok(server_user)
}

pub(crate) async fn get_user_by_id(
    state: &AccountService,
    mut input: OperationRequest,
) -> Result<crate::models::User, ApplicationError> {
    let resolved = input
        .resolve_target_using(&state.routing_services())
        .await?;
    let user = input.context.user().ok_or_else(unauthenticated)?;
    resolved.target.session().ok_or_else(unauthenticated)?;
    // This endpoint always addresses the caller, not the requested path ID.
    // Normalize virtual input before translation; the old URL replacement also
    // deliberately discarded the incoming query.
    normalize_user_path(&mut input.original_request, &user.id);
    let request = build_request(state, &resolved.target, &input.original_request).await?;

    // Execute request and parse JSON response
    let server_user: crate::models::User =
        execute_json_request(&state.authentication.reqwest_client, request).await?;

    let server_user = process_user(server_user, user, &state.authentication)
        .await
        .map_err(|e| {
            error!("Failed to process user: {}", e);
            ApplicationError::Internal(e)
        })?;

    Ok(server_user)
}

fn normalize_user_path(request: &mut reqwest::Request, user_id: &str) {
    request.url_mut().set_path(&format!("/Users/{user_id}"));
    request.url_mut().set_query(None);
}

async fn build_request(
    state: &AccountService,
    target: &crate::application::context::AuthorizedTarget,
    request: &reqwest::Request,
) -> Result<reqwest::Request, ApplicationError> {
    let prefix = state
        .authentication
        .settings
        .read()
        .await
        .url_prefix
        .as_ref()
        .map(ToString::to_string);
    UpstreamRequestBuilder::new(&state.urls, prefix.as_deref())
        .build(target, request)
        .await
        .map_err(|error| crate::application::input::routing_error(error.0))
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
    state: &AuthenticationService,
    ui_version: &str,
) -> crate::models::PublicServerInfo {
    let cfg = state.settings.read().await;
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
    state: &AccountService,
    input: OperationRequest,
    ui_version: &str,
) -> Result<crate::models::ServerInfo, ApplicationError> {
    let resolved = input
        .resolve_target_using(&state.routing_services())
        .await?;
    input.context.user().ok_or_else(unauthenticated)?;
    // Match the legacy original authorization: only the Authorization variant
    // enabled this response, not X-Emby-Authorization after remapping.
    let is_seerr = matches!(
        input.context.authorization(),
        Some(crate::auth::identity::JellyfinAuthorization::Authorization(authorization))
            if is_seerr_client(authorization)
    );
    if !is_seerr {
        let request = build_request(state, &resolved.target, &input.original_request).await?;
        let mut server_info = execute_json_request::<crate::models::ServerInfo>(
            &state.authentication.reqwest_client,
            request,
        )
        .await
        .inspect_err(|status| {
            error!("Failed to get upstream server info: {status}");
        })?;
        let cfg = state.authentication.settings.read().await;
        server_info.id = cfg.server_id.clone();
        server_info.server_name = cfg.server_name.clone();
        server_info.local_address = cfg.public_address.clone();
        server_info.version = Some(reported_server_version(ui_version));
        return Ok(server_info);
    }

    let cfg = state.authentication.settings.read().await;
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
    state: &AuthenticationService,
    headers: HeaderMap,
    payload: AuthenticateRequest,
) -> Result<AuthenticateResponse, ApplicationError> {
    let mut servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(ApplicationError::Storage)?;

    if servers.is_empty() {
        tracing::warn!("No servers configured for authentication");
        return Err(ApplicationError::InvalidInput("no authentication servers")
            .at(FailureStage::ResourceLookup));
    }

    let authentication = extract_auth_header(&headers).map_err(|_| {
        error!("No valid authorization header found in authentication request");
        ApplicationError::InvalidInput("missing or invalid login authorization")
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

    // The operation owns all attempts. Cancellation aborts unfinished work
    // instead of detaching backend logins after the local request has ended.
    let mut auth_tasks = tokio::task::JoinSet::new();

    let existing_user = state
        .user_authorization
        .get_user_by_username(&payload.username)
        .await
        .map_err(ApplicationError::Storage)?;

    if existing_user.is_none() && !state.settings.read().await.auto_create_users_on_login {
        warn!(
            "Auto user creation disabled; rejecting login for non-existing local user '{}'",
            payload.username
        );
        return Err(unauthenticated());
    }

    if !local_credential_allows_login(
        existing_user.as_ref().map(|user| &user.local_credential),
        &payload.password,
    )
    .await
    .map_err(ApplicationError::Storage)?
    {
        warn!(
            "Rejecting login for existing local user '{}' with invalid credentials",
            payload.username
        );
        return Err(unauthenticated());
    }

    let is_existing_user = existing_user.is_some();

    if let Some(user) = existing_user {
        let server_mappings = state
            .user_authorization
            .list_server_mappings(&user.id)
            .await
            .map_err(ApplicationError::Storage)?;

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
                        let order = auth_tasks.len();
                        auth_tasks.spawn(async move {
                            (
                                order,
                                authenticate_on_server(
                                    state.clone(),
                                    authentication.clone(),
                                    payload.clone(),
                                    server,
                                    Some(server_mapping),
                                )
                                .await,
                            )
                        });
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
        for server in servers {
            let state = state.clone();
            let authentication = authentication.clone();
            let payload = payload.clone();
            info!(
                "No server mapping found for user '{}' on server '{}'",
                payload.username, server.name
            );

            let order = auth_tasks.len();
            auth_tasks.spawn(async move {
                (
                    order,
                    authenticate_on_server(state, authentication, payload, server, None).await,
                )
            });
        }
    }

    // Wait for all authentication attempts to complete
    let mut successful_auths = Vec::new();
    let mut failures = Vec::new();
    let total_servers = auth_tasks.len();

    while let Some(task) = auth_tasks.join_next().await {
        match task {
            Ok((order, Ok(auth_response))) => {
                info!("Successfully authenticated user: {}", payload.username);
                successful_auths.push((order, auth_response));
            }
            Ok((_, Err(e))) => {
                tracing::debug!("Authentication attempt failed: {:?}", e);
                failures.push(ApplicationError::Internal(e.into()));
            }
            Err(join_err) => {
                tracing::error!("Authentication task failed: {}", join_err);
                failures.push(ApplicationError::Internal(join_err.into()));
            }
        }
    }

    if successful_auths.is_empty() {
        tracing::warn!(
            "All authentication attempts failed for user: {}",
            payload.username
        );
        Err(ApplicationError::SourcesFailed(failures).at(FailureStage::CallerAuthentication))
    } else {
        // Completion order must not change which backend supplies the response.
        successful_auths.sort_by_key(|(order, _)| *order);
        let successful_auths: Vec<_> = successful_auths.into_iter().map(|(_, auth)| auth).collect();
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
    state: &AuthenticationService,
    username: &str,
    password: &Password,
) -> Result<crate::auth::service::User, ApplicationError> {
    state
        .user_authorization
        .resolve_or_create_login_user(username, password)
        .await
        .map_err(|e| {
            tracing::error!("Error resolving local user for login: {}", e);
            ApplicationError::Storage(e)
        })?
        .ok_or_else(unauthenticated)
}

async fn persist_successful_auths(
    state: &AuthenticationService,
    user: &crate::auth::service::User,
    login_authorization: &Authorization,
    successful_auths: &[SuccessfulServerAuth],
) -> Result<(), ApplicationError> {
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
                    ApplicationError::Storage(e)
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
                ApplicationError::Storage(e)
            })?;
    }

    Ok(())
}

async fn decorate_auth_response(
    state: &AuthenticationService,
    user: &crate::auth::service::User,
    login_username: &str,
    login_authorization: &Authorization,
    successful_auth: &SuccessfulServerAuth,
) -> AuthenticateResponse {
    let mut auth_response = successful_auth.auth_response.clone();
    let server_id = state.settings.read().await.server_id.clone();
    auth_response.server_id = server_id.clone();
    auth_response.user.server_id = server_id.clone();
    auth_response.session_info.server_id = server_id;
    auth_response.session_info.user_id = user.id.to_string();
    auth_response.user.name = login_username.to_string();
    auth_response.session_info.user_name = login_username.to_string();
    // Seerr requires this flag to finish setup. Authorization remains enforced by
    // Jellyswarrm and the user's real upstream sessions.
    auth_response.user.policy.is_administrator = is_seerr_client(login_authorization);
    auth_response.user.policy.sync_play_access = SyncPlayUserAccessType::CreateAndJoinGroups;
    auth_response.access_token = user.virtual_key.clone();
    auth_response.user.id = user.id.to_string();
    crate::sessions::decorate_authentication(
        &state.client_sessions,
        &state.settings.read().await.server_id,
        user,
        login_authorization,
        &mut auth_response,
    )
    .await;
    auth_response
}

fn is_seerr_client(authorization: &Authorization) -> bool {
    authorization.client.eq_ignore_ascii_case("Seerr")
        && authorization.device.eq_ignore_ascii_case("Seerr")
}

fn unauthenticated() -> ApplicationError {
    ApplicationError::MissingIdentity("local authentication").at(FailureStage::CallerAuthentication)
}

#[cfg(test)]
mod tests {
    use super::{
        extract_auth_header, is_seerr_client, local_credential_allows_login, normalize_user_path,
        reported_server_version,
    };
    use crate::{
        auth::encryption::{HashedPassword, Password},
        auth::service::LocalCredential,
        models::Authorization,
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
    state: AuthenticationService,
    authorization: Authorization,
    payload: AuthenticateRequest,
    server: crate::servers::storage::Server,
    server_mapping: Option<crate::auth::service::ServerMapping>,
) -> Result<SuccessfulServerAuth, AuthError> {
    let client_info = jellyfin_api::ClientInfo {
        client: authorization.client,
        device: authorization.device,
        device_id: authorization.device_id,
        version: authorization.version,
    };
    let authenticator = state.mapping_authenticator().await;
    let login = if let Some(mapping) = &server_mapping {
        let user = state
            .user_authorization
            .get_user_by_username(&payload.username)
            .await
            .map_err(AuthError::Storage)?
            .ok_or(AuthError::InvalidCredentials)?;
        authenticator
            .authenticate_login(
                &user,
                &server,
                mapping,
                Some(&payload.password),
                client_info,
            )
            .await
    } else {
        authenticator
            .authenticate_password(&server, &payload.username, &payload.password, client_info)
            .await
    }
    .map_err(AuthError::Mapping)?;
    Ok(SuccessfulServerAuth {
        server,
        auth_response: login.response,
        final_username: login.username,
        final_password: login.password,
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
                "Invalid 'Authorization' header format",
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
                "Invalid 'X-Emby-Authorization' header format",
            ))
        }
    } else {
        error!("No 'Authorization' header found in login request");

        Err(AuthError::ParseError(
            "No 'Authorization' header found in login request!",
        ))
    }
}

/// Custom error type for authentication operations
#[derive(Debug, thiserror::Error)]
enum AuthError {
    #[error("mapped backend authentication failed")]
    Mapping(#[from] crate::auth::mapping_authentication::MappingAuthenticationError),
    #[error("invalid local credentials")]
    InvalidCredentials,
    #[error("{0}")]
    ParseError(&'static str),
    #[error("authentication storage failed")]
    Storage(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
struct SuccessfulServerAuth {
    server: crate::servers::storage::Server,
    auth_response: AuthenticateResponse,
    final_username: String,
    final_password: Option<crate::auth::encryption::Password>,
}
