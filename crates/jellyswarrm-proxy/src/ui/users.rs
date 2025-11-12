use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use std::collections::HashMap;
use tracing::{error, info};

use crate::{
    server_storage::Server,
    user_authorization_service::{ServerMapping, User},
    AppState,
};

#[derive(Template)]
#[template(path = "users.html")]
pub struct UsersPageTemplate {
    pub ui_route: String,
}

pub struct UserWithMappings {
    pub user: User,
    pub mappings: Vec<(ServerMapping, Server, i64)>, // per mapping session count
    pub available_servers: Vec<Server>,              // servers not yet mapped
    pub total_sessions: i64,
    pub open_mappings: bool, // controls <details open> in template
}

#[derive(Template)]
#[template(path = "user_list.html")]
pub struct UserListTemplate {
    pub users: Vec<UserWithMappings>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "user_item.html")]
pub struct UserItememplate {
    pub uwm: UserWithMappings,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddUserForm {
    pub username: String,
    pub password: String,
}

#[derive(Deserialize)]
pub struct AddMappingForm {
    pub user_id: String,
    pub server_url: String,
    pub mapped_username: String,
    pub mapped_password: String,
    /// The user's master password for encrypting the mapping password
    pub master_password: Option<String>,
}

pub async fn create_user_with_mappings(
    state: &AppState,
    user: User,
    servers: &[Server],
    open_mappings: bool,
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
        open_mappings,
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

pub async fn get_user_item(
    state: &AppState,
    user_id: &str,
    open_mappings: bool,
) -> impl IntoResponse {
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
    let uwm = create_user_with_mappings(state, user, &servers, open_mappings).await;
    let template = UserItememplate {
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

/// List users with mappings
pub async fn get_user_list(State(state): State<AppState>) -> impl IntoResponse {
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
                result.push(create_user_with_mappings(&state, user, &servers, false).await);
            }

            let template = UserListTemplate {
                users: result,
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
        Err(e) => {
            error!("Failed to list users: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Database error").into_response()
        }
    }
}

/// Add user
pub async fn add_user(State(state): State<AppState>, Form(form): Form<AddUserForm>) -> Response {
    if form.username.trim().is_empty() || form.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Username and password required</div>"),
        )
            .into_response();
    }
    match state
        .user_authorization
        .get_or_create_user(&form.username, &form.password)
        .await
    {
        Ok(_user) => {
            info!("Created user {}", form.username);
            get_user_list(State(state)).await.into_response()
        }
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

/// Delete user
pub async fn delete_user(State(state): State<AppState>, Path(user_id): Path<String>) -> Response {
    match state.user_authorization.delete_user(&user_id).await {
        Ok(true) => get_user_list(State(state)).await.into_response(),
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
    if form.mapped_username.trim().is_empty() || form.mapped_password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Mapping credentials required</div>"),
        )
            .into_response();
    }
    
    // For UI-added mappings, we'll encrypt the password using the user's master password
        match state
            .user_authorization
            .add_server_mapping(
                &form.user_id,
                &form.server_url,
                &form.mapped_username,
                &form.mapped_password,
                form.master_password.as_deref(),
            )
            .await
    {
        Ok(_id) => {
            match state
                .user_authorization
                .delete_all_sessions_for_user(&form.user_id)
                .await
            {
                Ok(_) => info!("Deleted all sessions for user {}", form.user_id),
                Err(e) => error!("Failed to delete sessions for user {}: {}", form.user_id, e),
            }
            get_user_item(&state, &form.user_id, true)
                .await
                .into_response()
        }
        Err(e) => {
            error!("Add mapping error: {}", e);
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
        Ok(true) => get_user_item(&state, &user_id, true).await.into_response(),
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
        Ok(_) => get_user_item(&state, &user_id, false).await.into_response(),
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
