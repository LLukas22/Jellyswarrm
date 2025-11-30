use jellyfin_api::JellyfinClient;

use crate::{server_storage::Server, AppState};

pub async fn authenticate_user_on_server(
    state: &AppState,
    user: &crate::ui::auth::User,
    server: &Server,
) -> Result<
    (
        JellyfinClient,
        jellyfin_api::models::User,
        jellyfin_api::models::PublicSystemInfo,
    ),
    String,
> {
    // Always check public system info first to get version and name
    let server_url = server.url.clone();
    let client_info = crate::config::CLIENT_INFO.clone();

    let (public_info, client) = match JellyfinClient::new(server_url.as_str(), client_info) {
        Ok(c) => match c.get_public_system_info().await {
            Ok(info) => (info, c),
            Err(_) => return Err("Server offline or unreachable".to_string()),
        },
        Err(e) => return Err(format!("Failed to create jellyfin client: {}", e)),
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
