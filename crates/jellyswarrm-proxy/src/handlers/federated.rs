use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::{debug, error};

use crate::{
    extractors::Preprocessed, handlers::items::get_items,
    request_preprocessing::PreprocessedRequest, AppState,
};

mod item_policy;
mod library_resolution;
mod library_root;
mod movie_reconciliation;
mod postprocessing;
mod request_policy;
mod upstream;

use library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan};
use library_root::{get_automatic_library_root, get_configured_library_root};
use movie_reconciliation::get_virtual_library_items;
use postprocessing::{FederatedItems, Pagination, ResponseShape};
use request_policy::has_query_key;
use upstream::{fetch_catalog, FetchMode, FetchedCatalog};

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = &preprocessed.original_request;

    if has_query_key(original_request.url(), &["SeriesId"]) {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    }

    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

pub async fn get_items_from_all_servers(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

pub async fn get_media_folders(
    State(state): State<AppState>,
    Preprocessed(mut preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let user_id = preprocessed
        .user
        .as_ref()
        .map(|user| user.id.clone())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let path = preprocessed.original_request.url().path().to_string();
    let path = path
        .strip_suffix("/Library/MediaFolders")
        .or_else(|| path.strip_suffix("/library/mediafolders"))
        .unwrap_or_default();
    preprocessed
        .original_request
        .url_mut()
        .set_path(&format!("{path}/Users/{user_id}/Views"));

    get_items_from_all_servers_preprocessed(&state, preprocessed).await
}

async fn get_items_from_all_servers_preprocessed(
    state: &AppState,
    preprocessed: PreprocessedRequest,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let plan = resolve_catalog_plan(state, &preprocessed).await?;
    match plan {
        CatalogPlan::EmptyVirtual => {
            let response_shape = if preprocessed
                .original_request
                .url()
                .path()
                .to_ascii_lowercase()
                .ends_with("/latest")
            {
                ResponseShape::Bare
            } else {
                ResponseShape::Counted
            };
            finalize_items_response(
                FederatedItems::default(),
                preprocessed.original_request.url(),
                response_shape,
            )
        }
        CatalogPlan::SingleServer => {
            get_items(State(state.clone()), Preprocessed(preprocessed)).await
        }
        CatalogPlan::Virtual {
            catalog_scope_key,
            targets,
            skipped_targets,
        } => {
            get_virtual_library_items(
                state,
                preprocessed,
                catalog_scope_key,
                targets,
                skipped_targets,
            )
            .await
        }
        CatalogPlan::Interleaved(targets) => {
            get_interleaved_root(state, preprocessed, targets).await
        }
        CatalogPlan::AutomaticRoot(targets) => {
            get_automatic_library_root(state, preprocessed, targets).await
        }
        CatalogPlan::ConfiguredRoot(targets) => {
            get_configured_library_root(state, preprocessed, targets).await
        }
    }
}

async fn get_interleaved_root(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let pagination = Pagination::from_url(original_request.url());
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
    } = fetch_catalog(
        state,
        &original_request,
        targets,
        FetchMode::ClientWindow(pagination),
        0,
    )
    .await?;
    let server_count = server_items.len();
    let responses = server_items
        .into_iter()
        .map(|items| items.server_items.response)
        .collect::<Vec<_>>();
    let items = FederatedItems::interleaved(responses);

    debug!("Combined items from {server_count} servers");

    finalize_items_response(items, original_request.url(), response_shape)
}

fn finalize_items_response(
    items: FederatedItems,
    url: &url::Url,
    response_shape: ResponseShape,
) -> Result<Json<serde_json::Value>, StatusCode> {
    serde_json::to_value(items.into_response(url, response_shape))
        .map(Json)
        .map_err(|e| {
            error!("Failed to serialize federated items response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })
}
