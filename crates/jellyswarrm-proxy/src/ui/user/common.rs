//! Shared user-page helpers for mapping validation, credentials, and rendering context.
use crate::{auth::encryption::Password, servers::storage::Server, AppState};

/// Validate ambiguous legacy values before the local password transaction.
/// Do not accept a cached token as proof of the saved password's correctness.
pub async fn prepare_mappings_for_password_change(
    state: &AppState,
    user: &crate::ui::auth::User,
    verified_password: &Password,
) -> Result<(), String> {
    let local_user = state
        .user_authorization
        .get_user_by_id(&user.id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Local user not found")?;
    state
        .mapping_authenticator()
        .await
        .prepare_password_change(&local_user, &state.server_storage, verified_password)
        .await
        .map_err(|error| error.to_string())
}

pub async fn authenticate_user_on_server(
    state: &AppState,
    user: &crate::ui::auth::User,
    server: &Server,
) -> Result<crate::auth::mapping_authentication::AuthenticatedMapping, String> {
    let mapping = state
        .user_authorization
        .get_server_mapping(&user.id, server)
        .await
        .map_err(|error| format!("Database error: {error}"))?
        .ok_or("No mapping found for user on this server")?;
    let local_user = state
        .user_authorization
        .get_user_by_id(&user.id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or("Local user not found")?;
    state
        .mapping_authenticator()
        .await
        .authenticate(&local_user, server, &mapping)
        .await
        .map_err(|error| error.to_string())
}
