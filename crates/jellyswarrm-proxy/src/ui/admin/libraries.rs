//! Manage virtual-library grouping, discovery, and member assignments in the administrative UI.
use std::collections::HashMap;

use askama::Template;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Form,
};
use serde::Deserialize;
use tracing::{error, info};

use crate::{
    domain::ids::ServerId,
    http::state::ManagementState,
    libraries::service::{normalize_library_id, AssignLibraryError, LibraryGroupMemberRecord},
};

#[derive(Template)]
#[template(path = "admin/libraries.html")]
pub(crate) struct LibrariesPageTemplate {
    pub(crate) merge_libraries: bool,
    pub(crate) has_merge_libraries_error: bool,
    pub(crate) merge_libraries_error: String,
    pub(crate) ui_route: String,
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
pub(crate) struct LibraryGroupsListTemplate {
    pub(crate) groups: Vec<LibraryGroupView>,
    pub(crate) discovered_libraries: Vec<DiscoveredLibraryView>,
    pub(crate) ui_route: String,
    pub(crate) discovery_errors: Option<Vec<String>>,
}

pub(crate) struct LibraryGroupView {
    pub(crate) virtual_id: String,
    pub(crate) name: String,
    pub(crate) collection_type: Option<String>,
    pub(crate) members: Vec<LibraryGroupMemberView>,
}

pub(crate) struct LibraryGroupOptionView {
    pub(crate) virtual_id: String,
    pub(crate) name: String,
}

pub(crate) struct LibraryGroupMemberView {
    pub(crate) server_id: i64,
    pub(crate) server_name: String,
    pub(crate) original_library_id: String,
    pub(crate) library_name: String,
}

pub(crate) struct DiscoveredLibraryView {
    pub(crate) server_id: i64,
    pub(crate) server_name: String,
    pub(crate) library_id: String,
    pub(crate) library_name: String,
    pub(crate) collection_type: String,
    pub(crate) assigned_group: Option<String>,
    pub(crate) assignable_groups: Vec<LibraryGroupOptionView>,
}

impl DiscoveredLibraryView {
    pub(crate) fn assignment_block_reason(&self) -> Option<&str> {
        if !self.assignable_groups.is_empty() {
            None
        } else if self.collection_type.trim().is_empty() {
            Some("Library type unavailable. Refresh libraries; if it remains missing, check the content type on the Jellyfin server.")
        } else {
            Some("No compatible virtual library. Create an empty virtual library or one with the same library type.")
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct CreateGroupForm {
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct AssignLibraryForm {
    pub(crate) group_virtual_id: String,
    pub(crate) server_id: i64,
    pub(crate) library_id: String,
}

#[derive(Deserialize)]
pub(crate) struct RemoveMemberForm {
    pub(crate) server_id: i64,
    pub(crate) library_id: String,
}

#[derive(Deserialize)]
pub(crate) struct RenameGroupForm {
    pub(crate) name: String,
}

#[derive(Deserialize)]
pub(crate) struct UpdateMergeLibrariesForm {
    #[serde(default)]
    pub(crate) merge_libraries: bool,
}

pub(crate) async fn libraries_page(State(state): State<ManagementState>) -> impl IntoResponse {
    let merge_libraries = state.settings.read().await.merge_libraries;
    let template = LibrariesPageTemplate {
        merge_libraries,
        has_merge_libraries_error: false,
        merge_libraries_error: String::new(),
        ui_route: state.paths.ui_route(),
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

pub(crate) async fn update_merge_libraries(
    State(state): State<ManagementState>,
    Form(form): Form<UpdateMergeLibrariesForm>,
) -> Response {
    let ui_route = state.paths.ui_route();
    let save_result = state
        .configuration
        .set_library_merging(form.merge_libraries)
        .await;

    match save_result {
        Ok(()) => render_merge_libraries_control(form.merge_libraries, ui_route, None),
        Err(error) => {
            error!("Failed to save automatic library merging setting: {error}");
            let current_value = state.settings.read().await.merge_libraries;
            render_merge_libraries_control(
                current_value,
                ui_route,
                Some("Could not save this setting. The previous behavior is still active."),
            )
        }
    }
}

pub(crate) async fn library_groups_list(State(state): State<ManagementState>) -> impl IntoResponse {
    let errors = state
        .library_administration()
        .refresh()
        .await
        .into_iter()
        .map(discovery_message)
        .collect();
    match render_library_groups_list_with_errors(&state, Some(errors)).await {
        Ok(html) => Html(html).into_response(),
        Err(message) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(format!("<div class=\"alert alert-error\">{message}</div>")),
        )
            .into_response(),
    }
}

async fn render_library_groups_list(state: &ManagementState) -> Result<String, String> {
    render_library_groups_list_with_errors(state, None).await
}

async fn render_library_groups_list_with_errors(
    state: &ManagementState,
    discovery_errors: Option<Vec<String>>,
) -> Result<String, String> {
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

    let server_names = state
        .server_storage
        .list_servers()
        .await
        .map_err(|error| format!("Failed to load servers: {error}"))?
        .into_iter()
        .map(|server| (server.id, server.name))
        .collect::<HashMap<_, _>>();
    let discovered = state
        .virtual_library_service
        .list_discovered_libraries()
        .await
        .map_err(|error| format!("Failed to load discovered libraries: {error}"))?;
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

    let mut discovered_views = discovered
        .into_iter()
        .filter_map(|library| {
            let server_name = server_names.get(&library.server_id)?.clone();
            let collection_type = library.collection_type.trim().to_ascii_lowercase();
            let assigned_group = assignments
                .get(&(
                    library.server_id,
                    normalize_library_id(&library.original_library_id),
                ))
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

            Some(DiscoveredLibraryView {
                server_id: library.server_id.as_i64(),
                server_name,
                library_id: library.original_library_id,
                library_name: library.name,
                collection_type,
                assigned_group,
                assignable_groups,
            })
        })
        .collect::<Vec<_>>();
    discovered_views.sort_by(|left, right| {
        left.server_name
            .cmp(&right.server_name)
            .then_with(|| left.library_name.cmp(&right.library_name))
    });

    let template = LibraryGroupsListTemplate {
        groups: group_views,
        discovered_libraries: discovered_views,
        ui_route: state.paths.ui_route(),
        discovery_errors,
    };

    template
        .render()
        .map_err(|e| format!("Template error: {e}"))
}

fn accepts_collection_type(group_type: Option<&str>, library_type: &str) -> bool {
    !library_type.trim().is_empty()
        && group_type.is_none_or(|group_type| group_type.eq_ignore_ascii_case(library_type))
}

fn discovery_message(issue: crate::application::library_admin::DiscoveryIssue) -> String {
    use crate::application::library_admin::DiscoveryStage;
    let server = issue.server.unwrap_or_default();
    match issue.stage {
        DiscoveryStage::Servers => "Could not load servers. Library discovery is incomplete.".into(),
        DiscoveryStage::Users => "Could not load mapped users. Library discovery is incomplete.".into(),
        DiscoveryStage::AdminCredentials => format!("{server}: could not load administrator credentials. Library discovery is incomplete."),
        DiscoveryStage::Mapping => format!("{server}: could not load a user mapping. Library discovery is incomplete."),
        DiscoveryStage::Authentication | DiscoveryStage::Inventory => match issue.user {
            Some(user) => format!("{server}: could not refresh libraries for {user}. Showing previously discovered libraries where available."),
            None => format!("{server}: could not fetch a complete library inventory. Showing previously discovered libraries where available."),
        },
        DiscoveryStage::Persistence => format!("{server}: could not save discovered libraries."),
    }
}

pub(crate) async fn create_group(
    State(state): State<ManagementState>,
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

pub(crate) async fn delete_group(
    State(state): State<ManagementState>,
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

pub(crate) async fn assign_library(
    State(state): State<ManagementState>,
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

pub(crate) async fn rename_group(
    State(state): State<ManagementState>,
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

pub(crate) async fn remove_member(
    State(state): State<ManagementState>,
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
            discovery_errors: None,
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
            discovery_errors: None,
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
    fn library_board_offers_search_filters_and_pagination() {
        let html = render_library_board();
        assert!(html.contains("data-library-search"));
        assert!(html.contains("data-library-server-filter"));
        assert!(html.contains("data-library-assignment-filter"));
        assert!(html.contains("data-assignment=\"assigned\""));
        assert!(html.contains("data-library-page=\"next\""));
        assert!(html.contains("data-library-page-status role=\"status\""));
    }

    #[test]
    fn library_board_explains_missing_types_and_incompatible_groups() {
        let library = |id: &str, collection_type: &str| DiscoveredLibraryView {
            server_id: 1,
            server_name: "Primary".into(),
            library_id: id.into(),
            library_name: id.into(),
            collection_type: collection_type.into(),
            assigned_group: None,
            assignable_groups: Vec::new(),
        };
        let html = LibraryGroupsListTemplate {
            groups: Vec::new(),
            discovered_libraries: vec![library("missing", ""), library("typed", "movies")],
            ui_route: "admin".into(),
            discovery_errors: None,
        }
        .render()
        .unwrap();
        assert!(html.contains("Type unavailable"));
        assert!(html.contains("check the content type on the Jellyfin server"));
        assert!(html.contains("Create an empty virtual library"));
        assert_eq!(html.matches("data-can-drag=\"false\"").count(), 2);
        assert!(!html.contains("class=\"library-assignment-form\""));
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
