//! HTTP adapters for the extracted catalog workflows.
use axum::{extract::State, Json};
use hyper::StatusCode;

use crate::{application::catalog, http::extractors::CatalogInput, AppState};

mod live_tv;
pub use live_tv::post_live_tv_programs;

pub async fn get_live_tv_programs(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_live_tv_programs(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_items_from_all_servers_if_not_restricted(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_items_from_all_servers(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub async fn get_show_children_from_all_servers(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_show_children_from_all_servers(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub async fn get_media_folders(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_media_folders(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}

pub async fn get_filters(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_filters_typed(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| crate::http::handlers::pipeline_error::detail_status(&error))
}
