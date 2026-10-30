//! Compose authenticated user profile, backend mapping, and media management routes.
pub(crate) mod media;
pub(crate) mod profile;
pub(crate) mod servers;

use crate::{http::state::ManagementState, servers::storage::Server};

/// Authenticate the caller's saved mapping on one server, for pages that query it live.
pub(crate) async fn authenticate_user_on_server(
    state: &ManagementState,
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
        .mapping_authenticator
        .authenticate(&local_user, server, &mapping)
        .await
        .map_err(|error| error.to_string())
}
