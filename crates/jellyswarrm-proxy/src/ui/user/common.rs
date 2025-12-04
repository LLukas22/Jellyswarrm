use std::sync::Arc;

use jellyfin_api::JellyfinClient;

use crate::{config::CLIENT_STORAGE, server_storage::Server, AppState};

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
            &server_url.to_string(),
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
        .get_server_mapping(&user.id, server.url.as_str())
        .await
    {
        Ok(Some(m)) => m,
        Ok(None) => return Err("No mapping found for user on this server".to_string()),
        Err(e) => return Err(format!("Database error: {}", e)),
    };

    let admin_password = state.get_admin_password().await;

    let password = state.user_authorization.decrypt_server_mapping_password(
        &mapping,
        &user.password,
        &admin_password,
    );

    if client.get_token().is_some() {
        // Try to validate existing session
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

    match client
        .authenticate_by_name(&mapping.mapped_username, &password)
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
