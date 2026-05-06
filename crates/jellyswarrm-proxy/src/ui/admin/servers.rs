use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::{
    config::MediaStreamingMode,
    encryption::{encrypt_password, Password},
    models::enums::CollectionType,
    server_storage::Server,
    unified_library_service::{GroupMode, UnifiedLibraryGroup},
    AppState,
};

#[derive(Template)]
#[template(path = "admin/servers.html")]
pub struct ServersPageTemplate {
    pub ui_route: String,
}

pub struct ServerWithAdmin {
    pub server: Server,
    pub has_admin: bool,
    pub is_redirect: bool,
    pub is_proxy: bool,
}

#[derive(Template)]
#[template(path = "admin/server_list.html")]
pub struct ServerListTemplate {
    pub servers: Vec<ServerWithAdmin>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddServerForm {
    pub name: String,
    pub url: String,
    pub priority: i32,
    pub media_streaming_mode: String,
}

#[derive(Deserialize)]
pub struct UpdatePriorityForm {
    pub priority: i32,
}

#[derive(Deserialize)]
pub struct UpdateMediaStreamingModeForm {
    pub media_streaming_mode: String,
}

#[derive(Deserialize)]
pub struct AddServerAdminForm {
    pub username: String,
    pub password: Password,
}

async fn render_server_list(state: &AppState) -> Result<String, String> {
    match state.server_storage.list_servers().await {
        Ok(servers) => {
            let mut servers_with_admin = Vec::new();
            for server in servers {
                let has_admin = state
                    .server_storage
                    .get_server_admin(server.id)
                    .await
                    .unwrap_or(None)
                    .is_some();
                let is_redirect = server.media_streaming_mode == MediaStreamingMode::Redirect;
                servers_with_admin.push(ServerWithAdmin {
                    server,
                    has_admin,
                    is_redirect,
                    is_proxy: !is_redirect,
                });
            }

            let template = ServerListTemplate {
                servers: servers_with_admin,
                ui_route: state.get_ui_route().await,
            };

            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Main servers management page
pub async fn servers_page(State(state): State<AppState>) -> impl IntoResponse {
    let template = ServersPageTemplate {
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render servers template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// Get server list partial (for HTMX)
pub async fn get_server_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_server_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render server list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Add a new server
pub async fn add_server(
    State(state): State<AppState>,
    Form(form): Form<AddServerForm>,
) -> Response {
    // Validate the form data
    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Server name cannot be empty</div>"),
        )
            .into_response();
    }

    if form.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Server URL cannot be empty</div>"),
        )
            .into_response();
    }

    if form.priority < 1 || form.priority > 999 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Priority must be between 1 and 999</div>"),
        )
            .into_response();
    }

    let media_streaming_mode = match form.media_streaming_mode.parse::<MediaStreamingMode>() {
        Ok(mode) => mode,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid streaming mode</div>"),
            )
                .into_response()
        }
    };

    // Try to add the server
    match state
        .server_storage
        .add_server(
            form.name.trim(),
            form.url.trim(),
            form.priority,
            media_streaming_mode,
        )
        .await
    {
        Ok(server_id) => {
            info!(
                "Added new server: {} ({}) with ID: {}",
                form.name, form.url, server_id
            );

            // Force Update server state
            state.server_storage.check_servers_health().await;

            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Err(e) => {
            error!("Failed to add server: {}", e);

            let error_message = if e.to_string().contains("UNIQUE constraint failed") {
                "A server with that name already exists"
            } else if e.to_string().contains("Invalid URL") {
                "Invalid URL format"
            } else {
                "Failed to add server"
            };

            (
                StatusCode::BAD_REQUEST,
                Html(format!(
                    "<div class=\"alert alert-error\">{error_message}</div>"
                )),
            )
                .into_response()
        }
    }
}

/// Update server media streaming mode
pub async fn update_server_media_streaming_mode(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
    Form(form): Form<UpdateMediaStreamingModeForm>,
) -> Response {
    let media_streaming_mode = match form.media_streaming_mode.parse::<MediaStreamingMode>() {
        Ok(mode) => mode,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid streaming mode</div>"),
            )
                .into_response()
        }
    };

    match state
        .server_storage
        .update_server_media_streaming_mode(server_id, media_streaming_mode)
        .await
    {
        Ok(true) => {
            info!(
                "Updated server {} media streaming mode to {}",
                server_id, media_streaming_mode
            );
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update server media streaming mode: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update streaming mode</div>"),
            )
                .into_response()
        }
    }
}

/// Delete a server
pub async fn delete_server(State(state): State<AppState>, Path(server_id): Path<i64>) -> Response {
    match state.server_storage.delete_server(server_id).await {
        Ok(true) => {
            info!("Deleted server with ID: {}", server_id);
            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete server: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete server</div>"),
            )
                .into_response()
        }
    }
}

/// Update server priority
pub async fn update_server_priority(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
    Form(form): Form<UpdatePriorityForm>,
) -> Response {
    if form.priority < 1 || form.priority > 999 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Priority must be between 1 and 999</div>"),
        )
            .into_response();
    }

    match state
        .server_storage
        .update_server_priority(server_id, form.priority)
        .await
    {
        Ok(true) => {
            info!("Updated server {} priority to {}", server_id, form.priority);
            // Return updated server list
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Server not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update server priority: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update priority</div>"),
            )
                .into_response()
        }
    }
}

/// Add server admin
pub async fn add_server_admin(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
    Form(form): Form<AddServerAdminForm>,
) -> Response {
    // 1. Get server details
    let server = match state.server_storage.get_server_by_id(server_id).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Server not found</div>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to get server: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Database error</div>"),
            )
                .into_response();
        }
    };

    // 2. Verify credentials with upstream Jellyfin and check admin status
    let client_info = crate::config::CLIENT_INFO.clone();

    let client = match jellyfin_api::JellyfinClient::new(server.url.as_str(), client_info) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to create jellyfin client: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Client error</div>"),
            )
                .into_response();
        }
    };

    match client
        .authenticate_by_name(&form.username, form.password.as_str())
        .await
    {
        Ok(user) => {
            // Check if user is admin
            let is_admin = user.policy.map(|p| p.is_administrator).unwrap_or(false);

            if !is_admin {
                return (
                    StatusCode::OK,
                    Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">User is not an administrator on this server</div>"),
                )
                    .into_response();
            }

            // 3. Encrypt password with admin master password
            let config = state.config.read().await;
            let encrypted_password = match encrypt_password(&form.password, &config.password.clone().into()) {
                Ok(p) => p,
                Err(e) => {
                    error!("Encryption failed: {}", e);
                    return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Encryption failed</div>"),
                        )
                            .into_response();
                }
            };

            // 4. Save to database
            match state
                .server_storage
                .add_server_admin(server_id, &form.username, &encrypted_password)
                .await
            {
                Ok(_) => {
                    info!("Added admin for server {}", server.name);
                    match render_server_list(&state).await {
                        Ok(html) => Html(format!(
                            r#"<div id="server-list" hx-swap-oob="innerHTML">{}</div>"#,
                            html
                        ))
                        .into_response(),
                        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
                    }
                }
                Err(e) => {
                    error!("Failed to add server admin: {}", e);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Database error</div>"),
                    )
                        .into_response()
                }
            }
        }
        Err(jellyfin_api::error::Error::AuthenticationFailed(_)) => {
            (
                StatusCode::OK,
                Html("<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Invalid credentials</div>"),
            )
                .into_response()
        }
        Err(e) => {
            error!("Failed to authenticate with upstream: {}", e);
            (
                StatusCode::OK,
                Html(format!(
                    "<div style=\"background-color: #e74c3c; color: white; padding: 0.75rem; border-radius: 0.25rem; margin-bottom: 1rem;\">Connection error: {}</div>",
                    e
                )),
            )
                .into_response()
        }
    }
}

/// Delete server admin
pub async fn delete_server_admin(
    State(state): State<AppState>,
    Path(server_id): Path<i64>,
) -> Response {
    match state.server_storage.delete_server_admin(server_id).await {
        Ok(true) => {
            info!("Deleted admin for server ID: {}", server_id);
            get_server_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Admin not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete server admin: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete admin</div>"),
            )
                .into_response()
        }
    }
}

// ─── Unified Library Group handlers ──────────────────────────────────────────

pub struct LibraryGroupView {
    pub id: i64,
    pub name: String,
    pub library_type_display: String,
    pub virtual_id_short: String,
    pub virtual_id_full: String,
    pub mode: String,
    pub mode_display: &'static str,
}

#[derive(Template)]
#[template(path = "admin/library_list.html")]
pub struct LibraryListTemplate {
    pub groups: Vec<LibraryGroupView>,
    pub ui_route: String,
}

#[derive(Deserialize)]
pub struct AddLibraryGroupForm {
    pub name: String,
    pub library_type: String,
    pub mode: Option<String>,
}

fn map_library_type_display(ct: &CollectionType) -> &'static str {
    match ct {
        CollectionType::Movies => "Movies",
        CollectionType::TvShows => "TV Shows",
        CollectionType::Music => "Music",
        CollectionType::MusicVideos => "Music Videos",
        CollectionType::Trailers => "Trailers",
        CollectionType::HomeVideos => "Home Videos",
        CollectionType::BoxSets => "Box Sets",
        CollectionType::Books => "Books",
        CollectionType::Photos => "Photos",
        CollectionType::LiveTv => "Live TV",
        CollectionType::Playlists => "Playlists",
        CollectionType::Folders => "Folders",
        _ => "Unknown",
    }
}

fn to_library_group_view(g: UnifiedLibraryGroup) -> LibraryGroupView {
    let mode_display = match g.mode {
        GroupMode::Auto => "Auto",
        GroupMode::Manual => "Manual",
    };
    LibraryGroupView {
        id: g.id,
        name: g.name,
        library_type_display: map_library_type_display(&g.library_type).to_string(),
        virtual_id_short: g.virtual_id[..8.min(g.virtual_id.len())].to_string(),
        virtual_id_full: g.virtual_id,
        mode: g.mode.to_string(),
        mode_display,
    }
}

async fn render_library_list(state: &AppState) -> Result<String, String> {
    match state.unified_library.list_groups().await {
        Ok(groups) => {
            let views: Vec<LibraryGroupView> = groups.into_iter().map(to_library_group_view).collect();
            let template = LibraryListTemplate {
                groups: views,
                ui_route: state.get_ui_route().await,
            };
            template.render().map_err(|e| e.to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Get library group list partial (HTMX)
pub async fn get_library_group_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_library_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render library group list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Error").into_response()
        }
    }
}

/// Create a new unified library group
pub async fn create_library_group(
    State(state): State<AppState>,
    Form(form): Form<AddLibraryGroupForm>,
) -> Response {
    let name = form.name.trim().to_string();

    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Name cannot be empty</div>"),
        )
            .into_response();
    }

    if name.len() > 100 {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Name must be 100 characters or fewer</div>"),
        )
            .into_response();
    }

    let library_type: CollectionType =
        match serde_json::from_value(serde_json::Value::String(form.library_type.clone())) {
            Ok(ct) => ct,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Html("<div class=\"alert alert-error\">Invalid library type</div>"),
                )
                    .into_response()
            }
        };

    if matches!(library_type, CollectionType::Unknown | CollectionType::UnknownVariant(_)) {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Invalid library type</div>"),
        )
            .into_response();
    }

    let mode_str = form.mode.as_deref().unwrap_or("auto");
    let mode = match GroupMode::try_from(mode_str.to_string()) {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid mode value</div>"),
            )
                .into_response()
        }
    };

    match state.unified_library.create_group(&name, library_type, mode).await {
        Ok(_) => {
            info!("Created unified library group: {}", name);
            match render_library_list(&state).await {
                Ok(html) => Html(format!(
                    r#"<div id="library-list" hx-swap-oob="innerHTML">{html}</div>"#
                ))
                .into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to create library group: {}", e);
            let msg = if e.to_string().contains("UNIQUE constraint failed") {
                "A group with that name already exists"
            } else {
                "Failed to create group"
            };
            (
                StatusCode::BAD_REQUEST,
                Html(format!("<div class=\"alert alert-error\">{msg}</div>")),
            )
                .into_response()
        }
    }
}

// ─── Edit Sources — view structs, form types ─────────────────────────────────

pub struct ServerOption {
    pub id: i64,
    pub name: String,
}

pub struct LibrarySourceView {
    pub id: i64,
    pub server_id: i64,
    pub server_name: String,
    pub jellyfin_library_id: String,
    pub jellyfin_library_name: String,
    pub tag_filter_display: String,
}

pub struct CachedLibraryOption {
    pub jellyfin_library_id: String,
    pub jellyfin_library_name: String,
}

#[derive(Template)]
#[template(path = "admin/library_sources.html")]
pub struct LibrarySourcesPageTemplate {
    pub group_id: i64,
    pub group_name: String,
    pub group_type_display: String,
    pub group_mode: String,
    pub global_tag_filter: String,
    pub sources: Vec<LibrarySourceView>,
    pub servers: Vec<ServerOption>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/library_source_list.html")]
pub struct SourceListTemplate {
    pub group_id: i64,
    pub sources: Vec<LibrarySourceView>,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/library_picker_options.html")]
pub struct LibraryPickerOptionsTemplate {
    pub libraries: Vec<CachedLibraryOption>,
}

#[derive(Deserialize)]
pub struct AddSourceForm {
    pub server_id: i64,
    pub jellyfin_library_id: String,
    pub jellyfin_library_name: String,
    pub tag_filter: String,
}

#[derive(Deserialize)]
pub struct SetModeForm {
    pub mode: String,
}

#[derive(Deserialize)]
pub struct SetGlobalTagFilterForm {
    pub global_tag_filter: String,
}

#[derive(Deserialize)]
pub struct LibraryPickerQuery {
    pub server_id: i64,
}

// ─── Delete a unified library group ──────────────────────────────────────────

/// Delete a unified library group
pub async fn delete_library_group(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    match state.unified_library.delete_group(id).await {
        Ok(true) => {
            info!("Deleted unified library group ID: {}", id);
            get_library_group_list(State(state)).await.into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Group not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to delete library group: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to delete group</div>"),
            )
                .into_response()
        }
    }
}

// ─── Edit Sources — helpers ───────────────────────────────────────────────────

fn parse_tag_filter_string(raw: &str) -> Option<Vec<String>> {
    let tags: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if tags.is_empty() { None } else { Some(tags) }
}

async fn render_source_list(state: &AppState, group_id: i64) -> Result<String, String> {
    let sources = state
        .unified_library
        .list_sources_for_group(group_id)
        .await
        .map_err(|e| e.to_string())?;

    let servers = state
        .server_storage
        .list_servers()
        .await
        .map_err(|e| e.to_string())?;

    let server_map: std::collections::HashMap<i64, String> =
        servers.into_iter().map(|s| (s.id, s.name)).collect();

    let source_views: Vec<LibrarySourceView> = sources
        .into_iter()
        .map(|src| {
            let server_name = server_map
                .get(&src.server_id)
                .cloned()
                .unwrap_or_else(|| format!("Server {}", src.server_id));
            let tag_filter_display = src
                .tag_filter
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_default();
            LibrarySourceView {
                id: src.id,
                server_id: src.server_id,
                server_name,
                jellyfin_library_id: src.jellyfin_library_id,
                jellyfin_library_name: src.jellyfin_library_name,
                tag_filter_display,
            }
        })
        .collect();

    let template = SourceListTemplate {
        group_id,
        sources: source_views,
        ui_route: state.get_ui_route().await,
    };
    template.render().map_err(|e| e.to_string())
}

// ─── Edit Sources — handlers ──────────────────────────────────────────────────

/// Edit Sources page for a unified library group
pub async fn library_sources_page(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let group = match state.unified_library.get_group_by_id(group_id).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Html("Group not found".to_string())).into_response()
        }
        Err(e) => {
            error!("Failed to load group {}: {}", group_id, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Html("Database error".to_string()))
                .into_response();
        }
    };

    let sources = state
        .unified_library
        .list_sources_for_group(group_id)
        .await
        .unwrap_or_default();

    let servers = state
        .server_storage
        .list_servers()
        .await
        .unwrap_or_default();

    let server_map: std::collections::HashMap<i64, String> =
        servers.iter().map(|s| (s.id, s.name.clone())).collect();

    let source_views: Vec<LibrarySourceView> = sources
        .into_iter()
        .map(|src| {
            let server_name = server_map
                .get(&src.server_id)
                .cloned()
                .unwrap_or_else(|| format!("Server {}", src.server_id));
            let tag_filter_display = src
                .tag_filter
                .as_ref()
                .map(|v| v.join(", "))
                .unwrap_or_default();
            LibrarySourceView {
                id: src.id,
                server_id: src.server_id,
                server_name,
                jellyfin_library_id: src.jellyfin_library_id,
                jellyfin_library_name: src.jellyfin_library_name,
                tag_filter_display,
            }
        })
        .collect();

    let server_options: Vec<ServerOption> = servers
        .into_iter()
        .map(|s| ServerOption { id: s.id, name: s.name })
        .collect();

    let global_tag_filter = group
        .global_tag_filter
        .as_ref()
        .map(|v| v.join(", "))
        .unwrap_or_default();

    let template = LibrarySourcesPageTemplate {
        group_id,
        group_name: group.name.clone(),
        group_type_display: map_library_type_display(&group.library_type).to_string(),
        group_mode: group.mode.to_string(),
        global_tag_filter,
        sources: source_views,
        servers: server_options,
        ui_route: state.get_ui_route().await,
    };

    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render library sources template: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

/// GET cached libraries for the source picker (HTMX)
pub async fn get_cached_libraries_for_picker(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Query(query): Query<LibraryPickerQuery>,
) -> impl IntoResponse {
    let group = match state.unified_library.get_group_by_id(group_id).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return Html(
                "<option disabled>Group not found</option>".to_string(),
            )
            .into_response()
        }
        Err(e) => {
            error!("Failed to load group {}: {}", group_id, e);
            return Html("<option disabled>Error loading group</option>".to_string())
                .into_response();
        }
    };

    let libraries = state
        .unified_library
        .get_cached_libraries(query.server_id, Some(&group.library_type))
        .await
        .unwrap_or_default();

    let lib_options: Vec<CachedLibraryOption> = libraries
        .into_iter()
        .map(|l| CachedLibraryOption {
            jellyfin_library_id: l.jellyfin_library_id,
            jellyfin_library_name: l.jellyfin_library_name,
        })
        .collect();

    let template = LibraryPickerOptionsTemplate { libraries: lib_options };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render library picker options: {}", e);
            Html("<option disabled>Error loading libraries</option>".to_string()).into_response()
        }
    }
}

/// POST add a source to a manual-mode group (HTMX)
pub async fn add_source_handler(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Form(form): Form<AddSourceForm>,
) -> Response {
    if form.jellyfin_library_id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Library must be selected</div>"),
        )
            .into_response();
    }
    if form.jellyfin_library_name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Library name is missing</div>"),
        )
            .into_response();
    }

    let tag_filter = parse_tag_filter_string(&form.tag_filter);

    match state
        .unified_library
        .add_source(
            group_id,
            form.server_id,
            &form.jellyfin_library_id,
            &form.jellyfin_library_name,
            tag_filter,
        )
        .await
    {
        Ok(_) => {
            info!(
                "Added source {} to group {}",
                form.jellyfin_library_name, group_id
            );
            match render_source_list(&state, group_id).await {
                Ok(html) => Html(html).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to add source to group {}: {}", group_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to add source</div>"),
            )
                .into_response()
        }
    }
}

/// DELETE remove a source from a group (HTMX)
pub async fn remove_source_handler(
    State(state): State<AppState>,
    Path((group_id, source_id)): Path<(i64, i64)>,
) -> Response {
    match state.unified_library.remove_source(source_id).await {
        Ok(true) => {
            info!("Removed source {} from group {}", source_id, group_id);
            match render_source_list(&state, group_id).await {
                Ok(html) => Html(html).into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
            }
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Source not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to remove source {}: {}", source_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to remove source</div>"),
            )
                .into_response()
        }
    }
}

/// PATCH set group mode — triggers HX-Redirect to reload page with correct sections
pub async fn set_group_mode_handler(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Form(form): Form<SetModeForm>,
) -> Response {
    let mode = match GroupMode::try_from(form.mode.clone()) {
        Ok(m) => m,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid mode</div>"),
            )
                .into_response()
        }
    };

    match state.unified_library.set_group_mode(group_id, mode).await {
        Ok(true) => {
            info!("Set group {} mode to {}", group_id, form.mode);
            let ui_route = state.get_ui_route().await;
            let redirect_url = format!(
                "/{}/servers/unified-libraries/{}/sources",
                ui_route, group_id
            );
            let mut response = StatusCode::OK.into_response();
            if let Ok(val) = HeaderValue::from_str(&redirect_url) {
                response.headers_mut().insert("HX-Redirect", val);
            }
            response
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Group not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to set group {} mode: {}", group_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update mode</div>"),
            )
                .into_response()
        }
    }
}

/// PATCH set global tag filter for an auto-mode group (HTMX)
pub async fn set_global_tag_filter_handler(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Form(form): Form<SetGlobalTagFilterForm>,
) -> Response {
    let tags = parse_tag_filter_string(&form.global_tag_filter);

    match state
        .unified_library
        .set_global_tag_filter(group_id, tags)
        .await
    {
        Ok(true) => {
            info!("Updated global tag filter for group {}", group_id);
            Html("<span style=\"color: var(--pico-ins-color);\">Filter saved</span>".to_string())
                .into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<span style=\"color: var(--pico-del-color);\">Group not found</span>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to update global tag filter for group {}: {}", group_id, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<span style=\"color: var(--pico-del-color);\">Failed to save filter</span>"),
            )
                .into_response()
        }
    }
}

/// POST refresh library discovery cache for all servers (HTMX)
pub async fn refresh_cache_handler(
    State(state): State<AppState>,
    Path(_group_id): Path<i64>,
) -> Response {
    let servers = match state.server_storage.list_servers().await {
        Ok(s) => s,
        Err(e) => {
            error!("Failed to list servers for cache refresh: {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to load servers</div>"),
            )
                .into_response();
        }
    };

    let mut results: Vec<String> = Vec::new();

    for server in &servers {
        let session = match state
            .user_authorization
            .get_any_session_for_server(server.url.as_str())
            .await
        {
            Ok(Some(s)) => s,
            Ok(None) => {
                results.push(format!(
                    "<li><strong>{}</strong>: skipped (no active user sessions — log in via a Jellyfin client first)</li>",
                    server.name
                ));
                continue;
            }
            Err(e) => {
                warn!("Failed to get session for server {}: {}", server.name, e);
                results.push(format!(
                    "<li><strong>{}</strong>: error loading session</li>",
                    server.name
                ));
                continue;
            }
        };

        match state
            .unified_library
            .refresh_library_cache(server, &session)
            .await
        {
            Ok(count) => {
                info!("Refreshed library cache for {}: {} libraries", server.name, count);
                results.push(format!(
                    "<li><strong>{}</strong>: {} libraries cached</li>",
                    server.name, count
                ));
            }
            Err(e) => {
                warn!("Cache refresh failed for {}: {}", server.name, e);
                results.push(format!(
                    "<li><strong>{}</strong>: refresh failed — {}</li>",
                    server.name, e
                ));
            }
        }
    }

    let body = if results.is_empty() {
        "<p style=\"color: var(--pico-muted-color);\">No servers configured.</p>".to_string()
    } else {
        format!("<ul style=\"margin: 0; padding-left: 1.25rem;\">{}</ul>", results.join(""))
    };

    Html(body).into_response()
}
