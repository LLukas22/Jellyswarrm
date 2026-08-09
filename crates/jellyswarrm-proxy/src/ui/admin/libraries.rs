use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use jellyfin_api::JellyfinClient;
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    config::{save_config, CLIENT_INFO},
    encryption::{decrypt_password, HashedPassword},
    server_id::ServerId,
    virtual_library_service::{
        normalize_library_id, AssignLibraryError, DiscoveredLibrary as StoredLibrary,
        LibraryGroupMemberRecord,
    },
    AppState,
};

#[derive(Template)]
#[template(path = "admin/libraries.html")]
pub struct LibrariesPageTemplate {
    pub merge_libraries: bool,
    pub has_merge_libraries_error: bool,
    pub merge_libraries_error: String,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/merge_libraries_control.html")]
struct MergeLibrariesControlTemplate {
    merge_libraries: bool,
    has_merge_libraries_error: bool,
    merge_libraries_error: String,
    ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/library_groups_list.html")]
pub struct LibraryGroupsListTemplate {
    pub groups: Vec<LibraryGroupView>,
    pub discovered_libraries: Vec<DiscoveredLibraryView>,
    pub ui_route: String,
}

pub struct LibraryGroupView {
    pub virtual_id: String,
    pub name: String,
    pub collection_type: Option<String>,
    pub members: Vec<LibraryGroupMemberView>,
}

pub struct LibraryGroupOptionView {
    pub virtual_id: String,
    pub name: String,
}

pub struct LibraryGroupMemberView {
    pub server_id: i64,
    pub server_name: String,
    pub original_library_id: String,
    pub library_name: String,
}

pub struct DiscoveredLibraryView {
    pub server_id: i64,
    pub server_name: String,
    pub library_id: String,
    pub library_name: String,
    pub collection_type: String,
    pub assigned_group: Option<String>,
    pub assignable_groups: Vec<LibraryGroupOptionView>,
}

#[derive(Deserialize)]
pub struct CreateGroupForm {
    pub name: String,
}

#[derive(Deserialize)]
pub struct AssignLibraryForm {
    pub group_virtual_id: String,
    pub server_id: i64,
    pub library_id: String,
}

#[derive(Deserialize)]
pub struct RemoveMemberForm {
    pub server_id: i64,
    pub library_id: String,
}

#[derive(Deserialize)]
pub struct RenameGroupForm {
    pub name: String,
}

#[derive(Deserialize)]
pub struct UpdateMergeLibrariesForm {
    #[serde(default)]
    pub merge_libraries: bool,
}

pub async fn libraries_page(State(state): State<AppState>) -> impl IntoResponse {
    let merge_libraries = state.merge_libraries_enabled().await;
    let template = LibrariesPageTemplate {
        merge_libraries,
        has_merge_libraries_error: false,
        merge_libraries_error: String::new(),
        ui_route: state.get_ui_route().await,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            error!("Failed to render libraries page: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

fn render_merge_libraries_control(
    merge_libraries: bool,
    ui_route: String,
    error_message: Option<&str>,
) -> Response {
    let template = MergeLibrariesControlTemplate {
        merge_libraries,
        has_merge_libraries_error: error_message.is_some(),
        merge_libraries_error: error_message.unwrap_or_default().to_string(),
        ui_route,
    };
    match template.render() {
        Ok(html) => Html(html).into_response(),
        Err(error) => {
            error!("Failed to render automatic merge control: {error}");
            (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
        }
    }
}

pub async fn update_merge_libraries(
    State(state): State<AppState>,
    Form(form): Form<UpdateMergeLibrariesForm>,
) -> Response {
    let ui_route = state.get_ui_route().await;
    let save_result = {
        let mut config = state.config.write().await;
        let mut updated = config.clone();
        updated.merge_libraries = form.merge_libraries;
        match save_config(&updated) {
            Ok(()) => {
                *config = updated;
                Ok(())
            }
            Err(error) => Err(error),
        }
    };

    match save_result {
        Ok(()) => render_merge_libraries_control(form.merge_libraries, ui_route, None),
        Err(error) => {
            error!("Failed to save automatic library merging setting: {error}");
            let current_value = state.merge_libraries_enabled().await;
            render_merge_libraries_control(
                current_value,
                ui_route,
                Some("Could not save this setting. The previous behavior is still active."),
            )
        }
    }
}

pub async fn library_groups_list(State(state): State<AppState>) -> impl IntoResponse {
    match render_library_groups_list(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<div class=\"alert alert-error\">{message}</div>")),
        )
            .into_response(),
    }
}

async fn render_library_groups_list(state: &AppState) -> Result<String, String> {
    let groups = state
        .virtual_library_service
        .list_groups()
        .await
        .map_err(|e| format!("Failed to load groups: {e}"))?;

    let mut group_views = Vec::new();
    for group in groups {
        let members = state
            .virtual_library_service
            .list_members(&group.virtual_id)
            .await
            .map_err(|e| format!("Failed to load group members: {e}"))?;

        let member_views = members
            .into_iter()
            .map(|member: LibraryGroupMemberRecord| LibraryGroupMemberView {
                server_id: member.server_id.as_i64(),
                server_name: String::new(),
                original_library_id: member.original_library_id,
                library_name: member.library_name,
            })
            .collect::<Vec<_>>();

        group_views.push(LibraryGroupView {
            virtual_id: group.virtual_id,
            name: group.name,
            collection_type: group.collection_type,
            members: member_views,
        });
    }

    let discovered = discover_libraries(state).await;
    let assignments = state
        .virtual_library_service
        .get_assignments()
        .await
        .unwrap_or_default();

    for group in &mut group_views {
        for member in &mut group.members {
            if let Ok(Some(server)) = state
                .server_storage
                .get_server_by_id(ServerId::new(member.server_id))
                .await
            {
                member.server_name = server.name;
            }
        }
    }

    let discovered_views = discovered
        .into_iter()
        .map(|library| {
            let collection_type = library.collection_type.trim().to_ascii_lowercase();
            let assigned_group = assignments
                .get(&(library.server_id, normalize_library_id(&library.library_id)))
                .map(|assignment| assignment.group_name.clone());
            let assignable_groups = group_views
                .iter()
                .filter(|group| {
                    accepts_collection_type(group.collection_type.as_deref(), &collection_type)
                })
                .map(|group| LibraryGroupOptionView {
                    virtual_id: group.virtual_id.clone(),
                    name: group.name.clone(),
                })
                .collect();

            DiscoveredLibraryView {
                server_id: library.server_id.as_i64(),
                server_name: library.server_name,
                library_id: library.library_id,
                library_name: library.library_name,
                collection_type,
                assigned_group,
                assignable_groups,
            }
        })
        .collect();

    let template = LibraryGroupsListTemplate {
        groups: group_views,
        discovered_libraries: discovered_views,
        ui_route: state.get_ui_route().await,
    };

    template
        .render()
        .map_err(|e| format!("Template error: {e}"))
}

fn accepts_collection_type(group_type: Option<&str>, library_type: &str) -> bool {
    !library_type.trim().is_empty()
        && group_type.is_none_or(|group_type| group_type.eq_ignore_ascii_case(library_type))
}

struct AvailableLibrary {
    server_id: ServerId,
    server_name: String,
    library_id: String,
    library_name: String,
    collection_type: String,
}

async fn discover_libraries(state: &AppState) -> Vec<AvailableLibrary> {
    let servers = match state.server_storage.list_servers().await {
        Ok(servers) => servers,
        Err(e) => {
            error!("Failed to list servers for library discovery: {}", e);
            return Vec::new();
        }
    };

    let server_names = servers
        .iter()
        .map(|server| (server.id, server.name.clone()))
        .collect::<HashMap<_, _>>();
    let mut discovered = state
        .virtual_library_service
        .list_discovered_libraries()
        .await
        .unwrap_or_else(|error| {
            error!("Failed to load user-observed libraries: {error}");
            Vec::new()
        })
        .into_iter()
        .filter_map(|library| {
            let server_name = server_names.get(&library.server_id)?.clone();
            Some((
                (library.server_id, library.original_library_id.clone()),
                AvailableLibrary {
                    server_id: library.server_id,
                    server_name,
                    library_id: library.original_library_id,
                    library_name: library.name,
                    collection_type: library.collection_type,
                },
            ))
        })
        .collect::<HashMap<_, _>>();

    let config = state.config.read().await;
    let admin_password: HashedPassword = config.password.clone().into();
    drop(config);

    for server in servers {
        let Some(admin) = state
            .server_storage
            .get_server_admin(server.id)
            .await
            .unwrap_or(None)
        else {
            continue;
        };

        let decrypted_password = match decrypt_password(&admin.password, &admin_password) {
            Ok(password) => password,
            Err(e) => {
                error!(
                    "Failed to decrypt admin password for server {}: {}",
                    server.name, e
                );
                continue;
            }
        };

        let client = match JellyfinClient::new(server.url.as_str(), CLIENT_INFO.clone()) {
            Ok(client) => client,
            Err(e) => {
                error!(
                    "Failed to create Jellyfin client for {}: {}",
                    server.name, e
                );
                continue;
            }
        };

        if client
            .authenticate_by_name(&admin.username, decrypted_password.as_str())
            .await
            .is_err()
        {
            error!("Failed to authenticate as admin on server {}", server.name);
            continue;
        }

        let folders = match client.get_media_folders(None).await {
            Ok(folders) => folders,
            Err(e) => {
                error!("Failed to list libraries on server {}: {}", server.name, e);
                continue;
            }
        };

        let mut authoritative_libraries = Vec::new();
        for folder in folders {
            if folder
                .collection_type
                .as_deref()
                .is_some_and(|collection_type| collection_type.eq_ignore_ascii_case("livetv"))
            {
                continue;
            }

            authoritative_libraries.push(StoredLibrary {
                server_id: server.id,
                original_library_id: folder.id,
                name: folder.name,
                collection_type: folder.collection_type.unwrap_or_default(),
            });
        }

        if let Err(error) = state
            .virtual_library_service
            .replace_discovered_libraries(server.id, &authoritative_libraries)
            .await
        {
            error!(
                "Failed to cache admin-discovered libraries for server {}: {error}",
                server.name
            );
        }
        discovered.retain(|(server_id, _), _| *server_id != server.id);
        for library in authoritative_libraries {
            let library_id = normalize_library_id(&library.original_library_id);
            discovered.insert(
                (server.id, library_id.clone()),
                AvailableLibrary {
                    server_id: server.id,
                    server_name: server.name.clone(),
                    library_id,
                    library_name: library.name,
                    collection_type: library.collection_type,
                },
            );
        }
    }

    let mut discovered = discovered.into_values().collect::<Vec<_>>();
    discovered.sort_by(|left, right| {
        left.server_name
            .cmp(&right.server_name)
            .then_with(|| left.library_name.cmp(&right.library_name))
    });

    discovered
}

pub async fn create_group(
    State(state): State<AppState>,
    Form(form): Form<CreateGroupForm>,
) -> Response {
    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Group name is required</div>"),
        )
            .into_response();
    }

    match state
        .virtual_library_service
        .create_group(form.name.trim())
        .await
    {
        Ok(group) => {
            info!("Created library group: {}", group.name);
            match render_library_groups_list(&state).await {
                Ok(html) => Html(html).into_response(),
                Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
            }
        }
        Err(e) => {
            error!("Failed to create library group: {}", e);
            let message = if e.to_string().contains("UNIQUE constraint failed") {
                "A group with that name already exists"
            } else {
                "Failed to create library group"
            };
            (
                StatusCode::BAD_REQUEST,
                Html(format!("<div class=\"alert alert-error\">{message}</div>")),
            )
                .into_response()
        }
    }
}

pub async fn delete_group(
    State(state): State<AppState>,
    Path(virtual_id): Path<String>,
) -> Response {
    match state
        .virtual_library_service
        .delete_group(&virtual_id)
        .await
    {
        Ok(true) => match render_library_groups_list(&state).await {
            Ok(html) => Html(html).into_response(),
            Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        },
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

pub async fn assign_library(
    State(state): State<AppState>,
    Form(form): Form<AssignLibraryForm>,
) -> Response {
    let server_id = ServerId::new(form.server_id);
    let library = match state
        .virtual_library_service
        .get_discovered_library(server_id, &form.library_id)
        .await
    {
        Ok(Some(library)) => library,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html("<div class=\"alert alert-error\">Real library not found.</div>"),
            )
                .into_response();
        }
        Err(error) => {
            error!("Failed to load library before assignment: {error}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to assign library.</div>"),
            )
                .into_response();
        }
    };

    match state
        .virtual_library_service
        .add_member(
            &form.group_virtual_id,
            server_id,
            &form.library_id,
            &library.name,
            &library.collection_type,
        )
        .await
    {
        Ok(()) => match render_library_groups_list(&state).await {
            Ok(html) => Html(html).into_response(),
            Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        },
        Err(AssignLibraryError::CollectionTypeMismatch {
            group_type,
            library_type,
        }) => (
            StatusCode::CONFLICT,
            Html(format!(
                "<div class=\"alert alert-error\">This virtual library contains {group_type} libraries, not {library_type} libraries.</div>"
            )),
        )
            .into_response(),
        Err(AssignLibraryError::MissingCollectionType) => (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">The library type is required.</div>".to_string()),
        )
            .into_response(),
        Err(AssignLibraryError::GroupNotFound) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Virtual library not found.</div>".to_string()),
        )
            .into_response(),
        Err(AssignLibraryError::Database(error)) => {
            error!("Failed to assign library to group: {error}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to assign library.</div>".to_string()),
            )
                .into_response()
        }
    }
}

pub async fn rename_group(
    State(state): State<AppState>,
    Path(virtual_id): Path<String>,
    Form(form): Form<RenameGroupForm>,
) -> Response {
    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Display name is required</div>"),
        )
            .into_response();
    }

    match state
        .virtual_library_service
        .rename_group(&virtual_id, form.name.trim())
        .await
    {
        Ok(true) => match render_library_groups_list(&state).await {
            Ok(html) => Html(html).into_response(),
            Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        },
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Group not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to rename library group: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to rename group</div>"),
            )
                .into_response()
        }
    }
}

pub async fn remove_member(
    State(state): State<AppState>,
    Path(group_virtual_id): Path<String>,
    Form(form): Form<RemoveMemberForm>,
) -> Response {
    let server_id = ServerId::new(form.server_id);

    match state
        .virtual_library_service
        .remove_member(&group_virtual_id, server_id, &form.library_id)
        .await
    {
        Ok(true) => match render_library_groups_list(&state).await {
            Ok(html) => Html(html).into_response(),
            Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        },
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Html("<div class=\"alert alert-error\">Library assignment not found</div>"),
        )
            .into_response(),
        Err(e) => {
            error!("Failed to remove library from group: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to remove library</div>"),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_control(enabled: bool) -> String {
        MergeLibrariesControlTemplate {
            merge_libraries: enabled,
            has_merge_libraries_error: false,
            merge_libraries_error: String::new(),
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap()
    }

    fn render_library_board() -> String {
        LibraryGroupsListTemplate {
            groups: vec![LibraryGroupView {
                virtual_id: "group-id".to_string(),
                name: "Movies".to_string(),
                collection_type: Some("movies".to_string()),
                members: vec![LibraryGroupMemberView {
                    server_id: 1,
                    server_name: "Primary".to_string(),
                    original_library_id: "library-id".to_string(),
                    library_name: "Movies".to_string(),
                }],
            }],
            discovered_libraries: vec![DiscoveredLibraryView {
                server_id: 1,
                server_name: "Primary".to_string(),
                library_id: "library-id".to_string(),
                library_name: "Movies".to_string(),
                collection_type: "movies".to_string(),
                assigned_group: Some("Movies".to_string()),
                assignable_groups: vec![LibraryGroupOptionView {
                    virtual_id: "group-id".to_string(),
                    name: "Movies".to_string(),
                }],
            }],
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap()
    }

    #[test]
    fn automatic_merge_control_is_checked_when_enabled() {
        let html = render_control(true);

        assert!(html.contains("name=\"merge_libraries\""));
        assert!(html.contains("checked"));
        assert!(html.contains("hx-patch=\"/admin/libraries/merge-libraries\""));
    }

    #[test]
    fn automatic_merge_control_is_unchecked_when_disabled() {
        let html = render_control(false);

        assert!(html.contains("name=\"merge_libraries\""));
        assert!(!html.contains("checked"));
        assert!(html.contains("Disabled"));
    }

    #[test]
    fn libraries_page_contains_one_automatic_merge_control() {
        let html = LibrariesPageTemplate {
            merge_libraries: true,
            has_merge_libraries_error: false,
            merge_libraries_error: String::new(),
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap();

        assert_eq!(html.matches("name=\"merge_libraries\"").count(), 1);
    }

    #[test]
    fn configured_groups_do_not_offer_duplicate_policies() {
        let html = LibraryGroupsListTemplate {
            groups: vec![LibraryGroupView {
                virtual_id: "group-id".to_string(),
                name: "Movies".to_string(),
                collection_type: None,
                members: Vec::new(),
            }],
            discovered_libraries: Vec::new(),
            ui_route: "admin".to_string(),
        }
        .render()
        .unwrap();

        assert!(!html.contains("duplicate_policy"));
        assert!(!html.contains("Preferred server"));
    }

    #[test]
    fn library_board_renders_draggable_libraries_and_drop_zones() {
        let html = render_library_board();

        assert!(html.contains("data-library-card"));
        assert!(html.contains("data-library-drag-handle"));
        assert!(html.contains("data-can-drag=\"true\""));
        assert!(html.contains("draggable=\"true\""));
        assert!(html.contains("data-library-dropzone"));
        assert!(html.contains("data-group-id=\"group-id\""));
    }

    #[test]
    fn library_board_keeps_an_accessible_assignment_form() {
        let html = render_library_board();

        assert!(html.contains("class=\"library-assignment-form\""));
        assert!(html.contains("name=\"group_virtual_id\""));
        assert!(html.contains("data-collection-type=\"movies\""));
        assert!(html.contains("hx-post=\"/admin/libraries/assign\""));
        assert!(html.contains("data-library-popover"));
        assert!(html.contains("aria-haspopup=\"true\""));
    }

    #[test]
    fn untyped_virtual_library_accepts_any_collection_type() {
        assert!(accepts_collection_type(None, "movies"));
    }

    #[test]
    fn typed_virtual_library_rejects_different_collection_type() {
        assert!(!accepts_collection_type(Some("movies"), "tvshows"));
    }

    #[test]
    fn virtual_library_rejects_library_without_collection_type() {
        assert!(!accepts_collection_type(None, ""));
    }
}
