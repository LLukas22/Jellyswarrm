use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use tracing::error;

use crate::{
    library_sync_service::UnifiedBrowseQuery,
    ui::auth::{AuthenticatedUser, UserRole},
    unified_library_service::DedupPolicy,
    AppState,
};

#[derive(Debug, Deserialize)]
pub struct CreateUnifiedLibraryRequest {
    pub name: String,
    pub collection_type: String,
    #[serde(default)]
    pub dedup_policy: DedupPolicy,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUnifiedLibraryRequest {
    pub name: String,
    pub collection_type: String,
    #[serde(default)]
    pub dedup_policy: DedupPolicy,
    pub sort_order: i32,
}

#[derive(Debug, Deserialize)]
pub struct AddUnifiedLibraryMemberRequest {
    pub server_id: i64,
    pub original_library_id: String,
    pub original_library_name: String,
}

#[derive(Debug, Deserialize)]
pub struct ReorderUnifiedLibrariesRequest {
    pub ids: Vec<i64>,
}

fn ensure_admin(user: &AuthenticatedUser) -> Result<(), StatusCode> {
    if user.0.role == UserRole::Admin {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

pub async fn list_unified_libraries(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<crate::unified_library_service::UnifiedLibrary>>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .list_all()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn get_unified_library(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: AuthenticatedUser,
) -> Result<Json<crate::unified_library_service::UnifiedLibrary>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .get(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

pub async fn create_unified_library(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(payload): Json<CreateUnifiedLibraryRequest>,
) -> Result<Json<crate::unified_library_service::UnifiedLibrary>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .create(&payload.name, &payload.collection_type, payload.dedup_policy)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn update_unified_library(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: AuthenticatedUser,
    Json(payload): Json<UpdateUnifiedLibraryRequest>,
) -> Result<StatusCode, StatusCode> {
    ensure_admin(&user)?;
    let updated = state
        .unified_libraries
        .update(
            id,
            &payload.name,
            &payload.collection_type,
            payload.dedup_policy,
            payload.sort_order,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if updated {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn delete_unified_library(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: AuthenticatedUser,
) -> Result<StatusCode, StatusCode> {
    ensure_admin(&user)?;
    let deleted = state
        .unified_libraries
        .delete(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn add_unified_library_member(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: AuthenticatedUser,
    Json(payload): Json<AddUnifiedLibraryMemberRequest>,
) -> Result<Json<crate::unified_library_service::UnifiedLibraryMember>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .add_member(
            id,
            payload.server_id,
            &payload.original_library_id,
            &payload.original_library_name,
        )
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn remove_unified_library_member(
    State(state): State<AppState>,
    Path((_id, member_id)): Path<(i64, i64)>,
    user: AuthenticatedUser,
) -> Result<StatusCode, StatusCode> {
    ensure_admin(&user)?;
    let deleted = state
        .unified_libraries
        .remove_member(member_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn list_unified_library_members(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<crate::unified_library_service::UnifiedLibraryMember>>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .list_members(id)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn reorder_unified_libraries(
    State(state): State<AppState>,
    user: AuthenticatedUser,
    Json(payload): Json<ReorderUnifiedLibrariesRequest>,
) -> Result<StatusCode, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .reorder(&payload.ids)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_available_libraries(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<Vec<crate::unified_library_service::AvailableLibrary>>, StatusCode> {
    ensure_admin(&user)?;
    state
        .unified_libraries
        .get_available_libraries(&state.server_storage, &state.library_sync)
        .await
        .map(Json)
        .map_err(|err| {
            error!("Failed to load available libraries: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn sync_status(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<crate::library_sync_service::SyncOverview>, StatusCode> {
    ensure_admin(&user)?;
    state
        .library_sync
        .sync_overview()
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

pub async fn trigger_sync(
    State(state): State<AppState>,
    user: AuthenticatedUser,
) -> Result<Json<crate::library_sync_service::SyncRunSummary>, StatusCode> {
    ensure_admin(&user)?;
    state
        .library_sync
        .sync_all_servers()
        .await
        .map(Json)
        .map_err(|err| {
            error!("Failed to run library sync: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

pub async fn browse_unified_items(
    State(state): State<AppState>,
    Path(virtual_library_id): Path<String>,
    Query(mut query): Query<UnifiedBrowseQuery>,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let library = state
        .library_sync
        .unified_library_by_virtual_id(&virtual_library_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    query.parent_id = Some(virtual_library_id);
    let response = state
        .library_sync
        .browse_unified_library(&library, &query)
        .await
        .map_err(|err| {
            error!("Failed to browse unified library: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    Ok(Json(crate::models::ItemsResponseVariants::WithCount(response)))
}

pub async fn search_unified_items(
    State(state): State<AppState>,
    Query(query): Query<UnifiedBrowseQuery>,
) -> Result<Json<crate::models::ItemsResponseVariants>, StatusCode> {
    let search_term = query.search_term.as_deref().unwrap_or_default().trim().to_string();
    if search_term.is_empty() {
        return Ok(Json(crate::models::ItemsResponseVariants::WithCount(
            crate::models::ItemsResponseWithCount {
                items: Vec::new(),
                total_record_count: 0,
                start_index: 0,
            },
        )));
    }
    let response = state
        .library_sync
        .search_items(&search_term, query.user_id.as_deref(), query.limit.unwrap_or(100))
        .await
        .map_err(|err| {
            error!("Failed to search unified items: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(crate::models::ItemsResponseVariants::WithCount(response)))
}
