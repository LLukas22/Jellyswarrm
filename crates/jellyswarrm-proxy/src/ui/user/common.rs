use std::sync::Arc;

use jellyfin_api::JellyfinClient;
use tracing::info;

use crate::{
    config::CLIENT_STORAGE, encryption::HashedPassword, server_storage::Server,
    user_authorization_service::MappingAuth, AppState,
};

pub async fn authenticate_user_on_server(
    state: &AppState,
    user: &crate::ui::auth::User,
    server: &Server,
) -> Result<
    (
        Arc<JellyfinClient>,
        jellyfin_api::models::User,
        jellyfin_api::models::PublicSystemInfo,
    ),
    String,
> {
    let client_info = crate::config::CLIENT_INFO.clone();
    let server_url = server.url.clone();

    // Check cache first
    let client = CLIENT_STORAGE
        .get(
            server_url.as_ref(),
            client_info,
            Some(user.username.as_str()),
        )
        .await
        .map_err(|e| format!("Failed to get client from storage: {}", e))?;

    // Always check public system info first to get version and name
    let public_info = match client.get_public_system_info().await {
        Ok(info) => info,
        Err(_) => return Err("Server offline or unreachable".to_string()),
    };

    // Check for mapping and try to authenticate
    let mapping = match state
        .user_authorization
        .get_server_mapping(&user.id, server)
        .await
    {
        Ok(Some(m)) => m,
        Ok(None) => return Err("No mapping found for user on this server".to_string()),
        Err(e) => return Err(format!("Database error: {}", e)),
    };

    let admin_password = state.get_admin_password().await;
    let admin_password_hash: HashedPassword = (&admin_password).into();

    let mapping_key = user.local_credential.mapping_key();
    if let MappingAuth::QuickConnect {
        backend_user_id, ..
    } = &mapping.auth
    {
        let local_user = state
            .user_authorization
            .get_user_by_id(&user.id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("Local user not found")?;
        let (token, remote_user) = crate::mapping_auth::validated_quick_connect_token(
            state,
            &local_user,
            server,
            &mapping,
        )
        .await?;
        // Use a fresh client: the shared cache evicts by calling Logout, which
        // would revoke this mapping's persistent credential.
        let qc_client = Arc::new(
            JellyfinClient::new(
                server.url.as_str(),
                jellyfin_api::ClientInfo {
                    device_id: crate::mapping_auth::mapping_device_id(server, &user.id),
                    ..crate::config::CLIENT_INFO.clone()
                },
            )
            .map_err(|e| e.to_string())?,
        );
        qc_client.with_token(token).await;
        return Ok((
            qc_client,
            jellyfin_api::models::User {
                id: backend_user_id.clone(),
                name: remote_user.name,
                server_id: Some(remote_user.server_id),
                policy: None,
            },
            public_info,
        ));
    }
    let password = state.user_authorization.decrypt_server_mapping_password(
        &mapping,
        &mapping_key,
        &admin_password_hash,
        None,
        Some(&admin_password),
    );

    if client.get_token().await.is_some() {
        // Try to validate existing session
        info!(
            "Validating existing session for user '{}' on server '{}'.",
            user.username, server.name
        );
        match client.get_me().await {
            Ok(jellyfin_user) => {
                return Ok((client, jellyfin_user, public_info));
            }
            Err(e) => {
                tracing::warn!("Existing session invalid for server {}: {}", server.id, e);
                // Fall through to re-authenticate
            }
        }
    }

    info!(
        "Authenticating user '{}' on server '{}'.",
        user.username, server.name
    );

    match client
        .authenticate_by_name(mapping.auth.username(), password.as_str())
        .await
    {
        Ok(jellyfin_user) => Ok((client, jellyfin_user, public_info)),
        Err(e) => {
            // Auth failed, log it but continue to check existing session
            tracing::warn!(
                "Failed to authenticate with mapped credentials for server {}: {}",
                server.id,
                e
            );
            Err("Failed to log in with provided credentials".to_string())
        }
    }
}
