use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    server_storage::Server,
    user_authorization_service::User,
    user_permissions::{PermissionType, UserPermissionWithServer},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/permissions.html")]
pub struct PermissionsTemplate {
    pub ui_route: String,
    pub users: Vec<UserWithPermissions>,
    pub servers: Vec<Server>,
}

pub struct UserWithPermissions {
    pub user: User,
    pub permissions: Vec<UserPermissionWithServer>,
}

#[derive(Template)]
#[template(path = "admin/user_permissions.html")]
pub struct UserPermissionsTemplate {
    pub ui_route: String,
    pub user: User,
    pub permissions: Vec<UserPermissionWithServer>,
    pub servers: Vec<Server>,
}

#[derive(Deserialize)]
pub struct SetPermissionForm {
    pub user_id: String,
    pub server_id: i64,
    pub permission_type: String, // "allow" or "deny"
}

/// GET /admin/permissions - User permissions management
pub async fn get_permissions_page(State(state): State<AppState>) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    let users = match state.user_authorization.list_users().await {
        Ok(u) => u,
        Err(e) => {
            error!("Failed to list users: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let servers = match state.server_storage.list_servers().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let mut users_with_perms = Vec::new();
    for user in users {
        let permissions = state
            .user_permissions
            .get_user_permissions(&user.id)
            .await
            .unwrap_or_default();

        users_with_perms.push(UserWithPermissions { user, permissions });
    }

    let template = PermissionsTemplate {
        ui_route,
        users: users_with_perms,
        servers,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// GET /admin/permissions/:user_id - Get permissions for a specific user
pub async fn get_user_permissions(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> impl IntoResponse {
    let ui_route = state.get_ui_route().await;

    let user = match state.user_authorization.get_user_by_id(&user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "User not found").into_response();
        }
        Err(e) => {
            error!("Failed to get user: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let permissions = state
        .user_permissions
        .get_user_permissions(&user_id)
        .await
        .unwrap_or_default();

    let servers = state.server_storage.list_servers().await.unwrap_or_default();

    let template = UserPermissionsTemplate {
        ui_route,
        user,
        permissions,
        servers,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Template render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// POST /admin/permissions - Set a permission
pub async fn set_permission(
    State(state): State<AppState>,
    Form(form): Form<SetPermissionForm>,
) -> impl IntoResponse {
    let permission_type = match PermissionType::from_str(&form.permission_type) {
        Some(pt) => pt,
        None => {
            return (StatusCode::BAD_REQUEST, "Invalid permission type").into_response();
        }
    };

    let created_by = "admin"; // TODO: Get from session

    match state
        .user_permissions
        .set_permission(&form.user_id, form.server_id, permission_type, created_by)
        .await
    {
        Ok(_) => {
            info!(
                "Set permission {} for user {} on server {}",
                form.permission_type, form.user_id, form.server_id
            );

            // Return updated permissions for user
            get_user_permissions(State(state), Path(form.user_id)).await.into_response()
        }
        Err(e) => {
            error!("Failed to set permission: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// DELETE /admin/permissions/:user_id/:server_id - Remove a permission
pub async fn remove_permission(
    State(state): State<AppState>,
    Path((user_id, server_id)): Path<(String, i64)>,
) -> impl IntoResponse {
    match state
        .user_permissions
        .remove_permission(&user_id, server_id)
        .await
    {
        Ok(_) => {
            info!(
                "Removed permission for user {} on server {}",
                user_id, server_id
            );

            // Return updated permissions for user
            get_user_permissions(State(state), Path(user_id)).await.into_response()
        }
        Err(e) => {
            error!("Failed to remove permission: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}
