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
    encryption::Password,
    federated_users::ServerSyncResult,
    server_storage::Server,
    user_authorization_service::{ServerMapping, User},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/users.html")]
pub struct UsersPageTemplate {
    pub ui_route: String,
}

pub struct UserWithMappings {
    pub user: User,
    pub mappings: Vec<(ServerMapping, Server, i64)>, // per mapping session count
    pub available_servers: Vec<Server>,              // servers not yet mapped
    pub total_sessions: i64,
}

#[derive(Template)]
#[template(path = "admin/user_list.html")]
pub struct UserListTemplate {
    pub users: Vec<UserWithMappings>,
    pub ui_route: String,
    pub sync_report: Option<Vec<ServerSyncResult>>,
}

#[derive(Template)]
#[template(path = "admin/user_item.html")]
pub struct UserItemTemplate {
    pub uwm: UserWithMappings,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddUserForm {
    pub username: String,
    pub password: Password,
    #[serde(default)]
    pub enable_federation: bool,
}

#[derive(Deserialize)]
pub struct AddMappingForm {
    pub user_id: String,
    pub server_url: String,
    pub mapped_username: String,
    pub mapped_password: Password,
}

pub async fn create_user_with_mappings(
    state: &AppState,
    user: User,
    servers: &[Server],
) -> UserWithMappings {
    // session counts per server_url (normalized)
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
    let mut mapped_urls: Vec<String> = Vec::new();
    match mappings_fetch {
        Ok(mappings) => {
            for mapping in mappings {
                if let Some(server) = servers.iter().find(|srv| {
                    srv.url.as_str().trim_end_matches('/')
                        == mapping.server_url.trim_end_matches('/')
                }) {
                    let count = session_counts
                        .get(mapping.server_url.trim_end_matches('/'))
                        .cloned()
                        .unwrap_or(0);
                    mappings_vec.push((mapping, server.clone(), count));
                    mapped_urls.push(server.url.as_str().trim_end_matches('/').to_string());
                }
            }
        }
        Err(e) => {
            error!("Failed to list mappings: {}", e);
        }
    }
    let available_servers: Vec<Server> = servers
        .iter()
        .filter(|srv| {
            !mapped_urls
                .iter()
                .any(|u| u == srv.url.as_str().trim_end_matches('/'))
        })
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
pub async fn users_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = UsersPageTemplate {
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render users template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn get_user_item(state: &AppState, user_id: &str) -> impl IntoResponse {
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
        ui_route: state.get_ui_route().await,
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

async fn user_item_with_popup(state: &AppState, user_id: &str, message: String) -> Response {
    let response = get_user_item(state, user_id).await.into_response();
    with_popup(response, message)
}

async fn get_user_list_impl(
    state: &AppState,
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
                ui_route: state.get_ui_route().await,
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
pub async fn get_user_list(State(state): State<AppState>) -> impl IntoResponse {
    get_user_list_impl(&state, None).await
}

/// Add user
pub async fn add_user(State(state): State<AppState>, Form(form): Form<AddUserForm>) -> Response {
    if form.username.trim().is_empty() || form.password.as_str().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Username and password required</div>"),
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
pub struct DeleteUserForm {
    #[serde(default)]
    pub delete_federated: bool,
}

/// Delete user
pub async fn delete_user(
    State(state): State<AppState>,
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
        Ok(true) => get_user_list_impl(&state, report).await.into_response(),
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
pub async fn add_mapping(
    State(state): State<AppState>,
    Form(form): Form<AddMappingForm>,
) -> Response {
    if form.mapped_username.trim().is_empty() || form.mapped_password.as_str().is_empty() {
        return user_item_with_popup(
            &state,
            &form.user_id,
            "Mapping username and password are required.".to_string(),
        )
        .await;
    }

    info!(
        "Validating mapping credentials for local user '{}' on '{}' as mapped user '{}'.",
        form.user_id, form.server_url, form.mapped_username
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

    let normalized_target = form.server_url.trim_end_matches('/');
    let server = match all_servers
        .into_iter()
        .find(|s| s.url.as_str().trim_end_matches('/') == normalized_target)
    {
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

    let config = state.config.read().await;
    let admin_password = &config.password;

    match state
        .user_authorization
        .add_server_mapping(
            &form.user_id,
            &form.server_url,
            &form.mapped_username,
            &form.mapped_password,
            Some(&admin_password.into()),
        )
        .await
    {
        Ok(mapping_id) => {
            info!(
                "Saved mapping {} for local user '{}' to server '{}' as mapped user '{}'.",
                mapping_id, form.user_id, form.server_url, form.mapped_username
            );
            match state
                .user_authorization
                .delete_sessions_for_mapping(mapping_id)
                .await
            {
                Ok(deleted) => info!(
                    "Deleted {} sessions for mapping {} (user {})",
                    deleted, mapping_id, form.user_id
                ),
                Err(e) => error!(
                    "Failed to delete sessions for mapping {} (user {}): {}",
                    mapping_id, form.user_id, e
                ),
            }
            get_user_item(&state, &form.user_id).await.into_response()
        }
        Err(e) => {
            error!(
                "Failed to save mapping for local user '{}' to server '{}': {}",
                form.user_id, form.server_url, e
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
pub async fn delete_mapping(
    State(state): State<AppState>,
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
pub async fn delete_sessions(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Response {
    match state
        .user_authorization
        .delete_all_sessions_for_user(&user_id)
        .await
    {
        Ok(_) => get_user_item(&state, &user_id).await.into_response(),
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
