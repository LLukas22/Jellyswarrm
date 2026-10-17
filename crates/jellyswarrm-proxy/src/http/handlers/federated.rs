//! HTTP adapters for the extracted catalog workflows.
use axum::{extract::State, Json};
use hyper::StatusCode;

#[cfg(test)]
use crate::http::preprocessing::PreprocessedRequest;
use crate::{application::catalog, http::extractors::CatalogInput, AppState};

#[cfg(test)]
#[allow(non_snake_case)]
fn Preprocessed(request: PreprocessedRequest) -> CatalogInput {
    CatalogInput(request.into())
}

mod live_tv;
pub use live_tv::post_live_tv_programs;

pub async fn get_live_tv_programs(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_live_tv_programs(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_items_from_all_servers_if_not_restricted(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_items_from_all_servers(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_show_children_from_all_servers(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_show_children_from_all_servers(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_media_folders(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_media_folders(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

pub async fn get_filters(
    State(state): State<AppState>,
    CatalogInput(request): CatalogInput,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::filters::get_filters_typed(&state.catalog_service(), request)
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

#[cfg(test)]
#[path = "../../unit_tests/federated_reliability.rs"]
mod reliability_tests;
#[cfg(test)]
#[path = "../../unit_tests/federated_snapshots.rs"]
mod snapshot_tests;
#[cfg(test)]
#[path = "../../unit_tests/federated_catalog.rs"]
mod tests;

#[cfg(test)]
async fn get_items_from_all_servers_preprocessed(
    state: &AppState,
    request: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    catalog::get_items_from_all_servers_preprocessed(&state.catalog_service(), request.into())
        .await
        .map(Json)
        .map_err(|error| error.detail_status())
}

#[cfg(test)]
use catalog::request_policy::CatalogRequestPolicy;
#[cfg(test)]
use catalog::{library_resolution, postprocessing, scan, upstream};
#[cfg(test)]
use library_resolution::{CatalogFetchTarget, CatalogPlan};

#[cfg(test)]
async fn resolve_catalog_plan(
    state: &AppState,
    request: &PreprocessedRequest,
) -> Result<CatalogPlan, StatusCode> {
    let context = crate::application::context::RequestContext::new(
        &request.original_request,
        request.auth.clone(),
        request.user.clone(),
        request.sessions.clone(),
        request.access_scope.clone(),
    );
    let input = catalog::input::CatalogRequest::new(
        request.original_request.try_clone().unwrap(),
        context,
        None,
    );
    library_resolution::resolve_catalog_plan(&state.catalog_service(), &input)
        .await
        .map_err(|error| error.detail_status())
}
#[cfg(test)]
use media_reconciliation::{get_aggregate_show_items, get_virtual_library_items};
#[cfg(test)]
use upstream::{fetch_catalog, FetchMode};

#[cfg(test)]
mod media_reconciliation {
    use super::*;

    pub(super) async fn get_virtual_library_items(
        state: &AppState,
        request: PreprocessedRequest,
        scope: String,
        targets: Vec<CatalogFetchTarget>,
        skipped: usize,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        catalog::media_reconciliation::get_virtual_library_items(
            &state.catalog_service(),
            request.into(),
            scope,
            targets,
            skipped,
        )
        .await
        .map(Json)
    }

    pub(super) async fn get_aggregate_show_items(
        state: &AppState,
        request: PreprocessedRequest,
        scope: String,
        targets: Vec<CatalogFetchTarget>,
        skipped: usize,
    ) -> Result<Json<serde_json::Value>, StatusCode> {
        catalog::media_reconciliation::get_aggregate_show_items(
            &state.catalog_service(),
            request.into(),
            scope,
            targets,
            skipped,
        )
        .await
        .map(Json)
    }
}
