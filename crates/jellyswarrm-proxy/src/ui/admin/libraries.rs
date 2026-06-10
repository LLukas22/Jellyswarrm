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
    config::CLIENT_INFO,
    duplicate_policy::DuplicatePolicy,
    encryption::{decrypt_password, HashedPassword},
    library_group_service::{normalize_library_id, LibraryGroupMemberRecord},
    server_id::ServerId,
    AppState,
};

#[derive(Template)]
#[template(path = "admin/libraries.html")]
pub struct LibrariesPageTemplate {
    pub merge_libraries: bool,
    pub ui_route: String,
}

#[derive(Template)]
#[template(path = "admin/library_groups_list.html")]
pub struct LibraryGroupsListTemplate {
    pub groups: Vec<LibraryGroupView>,
    pub discovered_libraries: Vec<DiscoveredLibraryView>,
    pub servers: Vec<ServerOptionView>,
    pub duplicate_policies: Vec<DuplicatePolicyOptionView>,
    pub ui_route: String,
}

pub struct ServerOptionView {
    pub id: i64,
    pub name: String,
}

pub struct DuplicatePolicyOptionView {
    pub value: String,
    pub label: String,
}

pub struct LibraryGroupView {
    pub virtual_id: String,
    pub name: String,
    pub duplicate_policy: String,
    pub preferred_server_id: Option<i64>,
    pub members: Vec<LibraryGroupMemberView>,
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
    pub library_name: String,
}

#[derive(Deserialize)]
pub struct RemoveMemberForm {
    pub server_id: i64,
    pub library_id: String,
}

#[derive(Deserialize)]
pub struct UpdateGroupPolicyForm {
    pub duplicate_policy: String,
    pub preferred_server_id: Option<String>,
}

#[derive(Deserialize)]
pub struct RenameGroupForm {
    pub name: String,
}

pub async fn libraries_page(State(state): State<AppState>) -> impl IntoResponse {
    let merge_libraries = state.merge_libraries_enabled().await;
    let template = LibrariesPageTemplate {
        merge_libraries,
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
    if state.merge_libraries_enabled().await {
        return Ok(
            "<article><p>Disable <strong>Merge Libraries Across Servers</strong> in Settings to use custom library groups.</p></article>".to_string(),
        );
    }

    let groups = state
        .library_group_service
        .list_groups()
        .await
        .map_err(|e| format!("Failed to load groups: {e}"))?;

    let mut group_views = Vec::new();
    for group in groups {
        let members = state
            .library_group_service
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
            duplicate_policy: group.duplicate_policy.to_string(),
            preferred_server_id: group.preferred_server_id.map(|id| id.as_i64()),
            members: member_views,
        });
    }

    let servers = state
        .server_storage
        .list_servers()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|server| ServerOptionView {
            id: server.id.as_i64(),
            name: server.name,
        })
        .collect::<Vec<_>>();

    let duplicate_policies = [
        DuplicatePolicy::ShowAll,
        DuplicatePolicy::LargestSize,
        DuplicatePolicy::SmallestSize,
        DuplicatePolicy::BestQuality,
        DuplicatePolicy::LowestQuality,
        DuplicatePolicy::PreferServer,
        DuplicatePolicy::ServerPriority,
    ]
    .into_iter()
    .map(|policy| DuplicatePolicyOptionView {
        value: policy.to_string(),
        label: policy.label().to_string(),
    })
    .collect();

    let discovered = discover_libraries(state).await;
    let assignments = state
        .library_group_service
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
            let assigned_group = assignments
                .get(&(library.server_id, normalize_library_id(&library.library_id)))
                .map(|assignment| assignment.group_name.clone());

            DiscoveredLibraryView {
                server_id: library.server_id.as_i64(),
                server_name: library.server_name,
                library_id: library.library_id,
                library_name: library.library_name,
                collection_type: library.collection_type,
                assigned_group,
            }
        })
        .collect();

    let template = LibraryGroupsListTemplate {
        groups: group_views,
        discovered_libraries: discovered_views,
        servers,
        duplicate_policies,
        ui_route: state.get_ui_route().await,
    };

    template.render().map_err(|e| format!("Template error: {e}"))
}

struct DiscoveredLibrary {
    server_id: ServerId,
    server_name: String,
    library_id: String,
    library_name: String,
    collection_type: String,
}

async fn discover_libraries(state: &AppState) -> Vec<DiscoveredLibrary> {
    let servers = match state.server_storage.list_servers().await {
        Ok(servers) => servers,
        Err(e) => {
            error!("Failed to list servers for library discovery: {}", e);
            return Vec::new();
        }
    };

    let config = state.config.read().await;
    let admin_password: HashedPassword = config.password.clone().into();
    drop(config);

    let mut discovered = Vec::new();

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
                error!("Failed to create Jellyfin client for {}: {}", server.name, e);
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

        for folder in folders {
            if folder
                .collection_type
                .as_deref()
                .is_some_and(|collection_type| collection_type.eq_ignore_ascii_case("livetv"))
            {
                continue;
            }

            discovered.push(DiscoveredLibrary {
                server_id: server.id,
                server_name: server.name.clone(),
                library_id: folder.id,
                library_name: folder.name,
                collection_type: folder.collection_type.unwrap_or_default(),
            });
        }
    }

    discovered.sort_by(|left, right| {
        left.server_name
            .cmp(&right.server_name)
            .then_with(|| left.library_name.cmp(&right.library_name))
    });

    discovered
}

fn library_groups_blocked_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Html("<div class=\"alert alert-error\">Disable <strong>Merge Libraries Across Servers</strong> in Settings to use custom library groups.</div>"),
    )
        .into_response()
}

pub async fn create_group(
    State(state): State<AppState>,
    Form(form): Form<CreateGroupForm>,
) -> Response {
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Group name is required</div>"),
        )
            .into_response();
    }

    match state
        .library_group_service
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
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    match state.library_group_service.delete_group(&virtual_id).await {
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
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    let server_id = ServerId::new(form.server_id);

    match state
        .library_group_service
        .add_member(
            &form.group_virtual_id,
            server_id,
            &form.library_id,
            &form.library_name,
        )
        .await
    {
        Ok(()) => match render_library_groups_list(&state).await {
            Ok(html) => Html(html).into_response(),
            Err(message) => (StatusCode::INTERNAL_SERVER_ERROR, message).into_response(),
        },
        Err(e) => {
            error!("Failed to assign library to group: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to assign library</div>"),
            )
                .into_response()
        }
    }
}

pub async fn update_group_policy(
    State(state): State<AppState>,
    Path(virtual_id): Path<String>,
    Form(form): Form<UpdateGroupPolicyForm>,
) -> Response {
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    let policy = match form.duplicate_policy.parse::<DuplicatePolicy>() {
        Ok(policy) => policy,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Html("<div class=\"alert alert-error\">Invalid duplicate policy</div>"),
            )
                .into_response();
        }
    };

    let preferred_server_id = form
        .preferred_server_id
        .filter(|value| !value.trim().is_empty())
        .and_then(|value| value.parse::<i64>().ok())
        .map(ServerId::new);

    match state
        .library_group_service
        .update_group_policy(&virtual_id, policy, preferred_server_id)
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
            error!("Failed to update library group policy: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<div class=\"alert alert-error\">Failed to update duplicate policy</div>"),
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
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    if form.name.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Html("<div class=\"alert alert-error\">Display name is required</div>"),
        )
            .into_response();
    }

    match state
        .library_group_service
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
    if state.merge_libraries_enabled().await {
        return library_groups_blocked_response();
    }

    let server_id = ServerId::new(form.server_id);

    match state
        .library_group_service
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
