//! Manage local users and backend federation from the administrative UI.
use askama::Template;
use axum::{
    extract::{Path, State},
    http::{header::HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Form,
};
use jellyfin_api::JellyfinClient;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{error, info};

use crate::{
    auth::federation::ServerSyncResult,
    auth::service::{ServerMapping, User},
    domain::ids::ServerId,
    domain::security::Password,
    http::state::ManagementState,
    servers::storage::Server,
};

#[derive(Template)]
#[template(path = "admin/users.html")]
pub(crate) struct UsersPageTemplate {
    pub(crate) ui_route: String,
}

pub(crate) struct UserWithMappings {
    pub(crate) user: User,
    pub(crate) mappings: Vec<(ServerMapping, Server, i64)>, // per mapping session count
    pub(crate) available_servers: Vec<Server>,              // servers not yet mapped
    pub(crate) total_sessions: i64,
}

#[derive(Template)]
#[template(path = "admin/user_list.html")]
pub(crate) struct UserListTemplate {
    pub(crate) users: Vec<UserWithMappings>,
    pub(crate) ui_route: String,
    pub(crate) sync_report: Option<Vec<ServerSyncResult>>,
}

#[derive(Template)]
#[template(path = "admin/user_item.html")]
pub(crate) struct UserItemTemplate {
    pub(crate) uwm: UserWithMappings,
    pub(crate) ui_route: String,
}

#[derive(Deserialize)]
pub(crate) struct AddUserForm {
    pub(crate) username: String,
    pub(crate) password: Password,
    #[serde(default)]
    pub(crate) enable_federation: bool,
}

#[derive(Deserialize)]
pub(crate) struct AddMappingForm {
    pub(crate) user_id: String,
    pub(crate) server_id: ServerId,
    pub(crate) mapped_username: String,
    pub(crate) mapped_password: Password,
}

pub(crate) async fn create_user_with_mappings(
    state: &ManagementState,
    user: User,
    servers: &[Server],
) -> UserWithMappings {
    // Session counts keyed by canonical server URL for template display.
    let mut session_counts: HashMap<String, i64> = HashMap::new();
    if let Ok(rows) = state
        .user_authorization
        .session_counts_by_server(&user.id)
        .await
    {
        for (url, cnt) in rows {
            session_counts.insert(url, cnt);
        }
    }

    let mappings_fetch = state
        .user_authorization
        .list_server_mappings(&user.id)
        .await;
    let mut mappings_vec: Vec<(ServerMapping, Server, i64)> = Vec::new();
    let mut mapped_server_ids: Vec<ServerId> = Vec::new();
    match mappings_fetch {
        Ok(mappings) => {
            for mapping in mappings {
                if let Some(server) = servers.iter().find(|srv| srv.id == mapping.server_id) {
                    let count = session_counts
                        .get(server.url.as_str())
                        .cloned()
                        .unwrap_or(0);
                    mappings_vec.push((mapping, server.clone(), count));
                    mapped_server_ids.push(server.id);
                }
            }
        }
        Err(e) => {
            error!("Failed to list mappings: {}", e);
        }
    }
    let available_servers: Vec<Server> = servers
        .iter()
        .filter(|srv| !mapped_server_ids.contains(&srv.id))
        .cloned()
        .collect();
    let user_total_sessions: i64 = mappings_vec.iter().map(|(_, _, c)| *c).sum();
    UserWithMappings {
        user,
        mappings: mappings_vec,
        available_servers,
        total_sessions: user_total_sessions,
    }
}

/// Main users page
pub(crate) async fn users_page(State(state): State<ManagementState>) -> impl IntoResponse {
    let template = UsersPageTemplate {
        ui_route: state.paths.ui_route(),
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render users template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub(crate) async fn get_user_item(state: &ManagementState, user_id: &str) -> impl IntoResponse {
    let servers = match state.server_storage.list_servers().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    let user = match state.user_authorization.get_user_by_id(user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">User not found</div>"),
            )
                .into_response();
        }
        Err(e) => {
            error!("Failed to fetch user by id {}: {}", user_id, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    // Build UserWithMappings and render single item template
    let uwm = create_user_with_mappings(state, user, &servers).await;
    let template = UserItemTemplate {
        uwm,
        ui_route: state.paths.ui_route(),
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Render error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

fn with_popup(mut response: Response, message: String) -> Response {
    let payload = serde_json::json!({
        "admin-popup": {
            "message": message,
        }
    })
    .to_string();

    match HeaderValue::from_str(&payload) {
        Ok(value) => {
            response.headers_mut().insert("HX-Trigger", value);
        }
        Err(e) => {
            error!("Failed to set HX-Trigger popup header: {}", e);
        }
    }

    response
}

async fn user_item_with_popup(state: &ManagementState, user_id: &str, message: String) -> Response {
    let response = get_user_item(state, user_id).await.into_response();
    with_popup(response, message)
}

async fn get_user_list_impl(
    state: &ManagementState,
    report: Option<Vec<ServerSyncResult>>,
) -> impl IntoResponse {
    // Fetch servers once for mapping lookup
    let servers = match state.server_storage.list_servers().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list servers: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response();
        }
    };

    match state.user_authorization.list_users().await {
        Ok(users) => {
            let mut result = Vec::new();
            for user in users {
                result.push(create_user_with_mappings(state, user, &servers).await);
            }

            let template = UserListTemplate {
                users: result,
                ui_route: state.paths.ui_route(),
                sync_report: report,
            };
            match template.render() {
                Ok(html) => Html(html).into_response(),
                Err(e) => {
                    error!("Render error: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
                }
            }
        }
        Err(e) => {
            error!("Failed to list users: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// List users with mappings
pub(crate) async fn get_user_list(State(state): State<ManagementState>) -> impl IntoResponse {
    get_user_list_impl(&state, None).await
}

/// Add user
pub(crate) async fn add_user(
    State(state): State<ManagementState>,
    Form(form): Form<AddUserForm>,
) -> Response {
    if form.username.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Username required</div>"),
        )
            .into_response();
    }
    match state
        .user_authorization
        .create_user(&form.username, &form.password)
        .await
    {
        Ok(user) => {
            info!("Created user {}", form.username);

            // Sync to all servers if enabled
            let report = if form.enable_federation {
                Some(
                    state
                        .federated_users
                        .sync_user_to_all_servers(&form.username, &form.password, &user.id)
                        .await,
                )
            } else {
                None
            };

            get_user_list_impl(&state, report).await.into_response()
        }
        Err(sqlx::Error::Database(db_err)) if db_err.is_unique_violation() => (
            StatusCode::CONFLICT,
            Html("<div class=\"alert alert-error\">User already exists</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to create user: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to create user</div>"),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct DeleteUserForm {
    #[serde(default)]
    pub(crate) delete_federated: bool,
}

/// Delete user
pub(crate) async fn delete_user(
    State(state): State<ManagementState>,
    Path(user_id): Path<String>,
    Form(form): Form<DeleteUserForm>,
) -> Response {
    // 1. Get user to get username for remote deletion
    let username = match state.user_authorization.get_user_by_id(&user_id).await {
        Ok(Some(u)) => u.original_username,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">User not found</div>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to fetch user by id {}: {}", user_id, e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Database error</div>"),
            )
                .into_response();
        }
    };

    // 2. Delete from federated servers if requested
    let report = if form.delete_federated {
        Some(
            state
                .federated_users
                .delete_user_from_all_servers(&username)
                .await,
        )
    } else {
        None
    };

    // 3. Delete locally
    match state.user_authorization.delete_user(&user_id).await {
        Ok(true) => {
            state
                .sessions
                .session_operations()
                .end_user_sessions(&user_id)
                .await;
            get_user_list_impl(&state, report).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">User not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Delete user error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete user</div>"),
            )
                .into_response()
        }
    }
}

/// Add mapping
pub(crate) async fn add_mapping(
    State(state): State<ManagementState>,
    Form(form): Form<AddMappingForm>,
) -> Response {
    if form.mapped_username.trim().is_empty() {
        return user_item_with_popup(
            &state,
            &form.user_id,
            "Mapping username is required.".to_string(),
        )
        .await;
    }

    info!(
        "Validating mapping credentials for local user '{}' on '{}' as mapped user '{}'.",
        form.user_id, form.server_id, form.mapped_username
    );

    let all_servers = match state.server_storage.list_servers().await {
        Ok(servers) => servers,
        Err(e) => {
            error!("Failed to list servers while adding mapping: {}", e);
            return user_item_with_popup(
                &state,
                &form.user_id,
                "Could not load servers while validating this mapping.".to_string(),
            )
            .await;
        }
    };

    let server = match all_servers.into_iter().find(|s| s.id == form.server_id) {
        Some(server) => server,
        None => {
            return user_item_with_popup(
                &state,
                &form.user_id,
                "Selected server was not found. Please refresh and try again.".to_string(),
            )
            .await;
        }
    };

    let client = match JellyfinClient::new(server.url.as_str(), crate::config::CLIENT_INFO.clone())
    {
        Ok(client) => client,
        Err(e) => {
            error!(
                "Failed to create jellyfin client for {}: {}",
                server.name, e
            );
            return user_item_with_popup(
                &state,
                &form.user_id,
                format!("Failed to connect to selected server '{}'.", server.name),
            )
            .await;
        }
    };

    match client
        .authenticate_by_name(&form.mapped_username, form.mapped_password.as_str())
        .await
    {
        Ok(_) => {
            info!(
                "Mapping credentials validated for local user '{}' on server '{}' as mapped user '{}'.",
                form.user_id, server.name, form.mapped_username
            );
        }
        Err(jellyfin_api::error::Error::AuthenticationFailed(_)) => {
            info!(
                "Mapping validation failed for local user '{}' on server '{}' as mapped user '{}': invalid credentials.",
                form.user_id, server.name, form.mapped_username
            );
            return user_item_with_popup(
                &state,
                &form.user_id,
                format!(
                    "Validation failed on server '{}': username or password is incorrect.",
                    server.name
                ),
            )
            .await;
        }
        Err(e) => {
            error!(
                "Failed to validate mapping credentials for user '{}' on server '{}': {}",
                form.user_id, server.name, e
            );
            return user_item_with_popup(
                &state,
                &form.user_id,
                format!(
                    "Could not validate credentials on server '{}': {}",
                    server.name, e
                ),
            )
            .await;
        }
    }

    let admin_password = { (&state.credentials.password()).into() };

    match state
        .user_authorization
        .add_server_mapping(
            &form.user_id,
            &server,
            &form.mapped_username,
            &form.mapped_password,
            Some(&admin_password),
        )
        .await
    {
        Ok(mapping_id) => {
            info!(
                "Saved mapping {} for local user '{}' to server '{}' as mapped user '{}'.",
                mapping_id, form.user_id, server.name, form.mapped_username
            );
            get_user_item(&state, &form.user_id).await.into_response()
        }
        Err(e) => {
            error!(
                "Failed to save mapping for local user '{}' to server '{}': {}",
                form.user_id, server.name, e
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to add mapping</div>"),
            )
                .into_response()
        }
    }
}

/// Delete mapping
pub(crate) async fn delete_mapping(
    State(state): State<ManagementState>,
    Path((user_id, mapping_id)): Path<(String, i64)>,
) -> Response {
    match state
        .user_authorization
        .delete_server_mapping(mapping_id)
        .await
    {
        Ok(true) => get_user_item(&state, &user_id).await.into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Mapping not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Delete mapping error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete mapping</div>"),
            )
                .into_response()
        }
    }
}

/// Delete sessions
pub(crate) async fn delete_sessions(
    State(state): State<ManagementState>,
    Path(user_id): Path<String>,
) -> Response {
    match state
        .user_authorization
        .delete_all_sessions_for_user(&user_id)
        .await
    {
        Ok(_) => {
            state
                .sessions
                .session_operations()
                .end_user_sessions(&user_id)
                .await;
            get_user_item(&state, &user_id).await.into_response()
        }
        Err(e) => {
            error!("Delete user error: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete usersessions</div>"),
            )
                .into_response()
        }
    }
}
