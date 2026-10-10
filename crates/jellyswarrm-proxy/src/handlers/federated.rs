use axum::{extract::State, Json};
use hyper::StatusCode;
use tracing::{debug, error};

use crate::{
    extractors::CatalogPreprocessed as Preprocessed, request_preprocessing::PreprocessedRequest,
    AppState,
};

mod item_policy;
mod library_resolution;
mod library_root;
mod media_reconciliation;
mod postprocessing;
#[cfg(test)]
mod reliability_tests;
mod request_policy;
mod scan;
pub(crate) mod snapshots;
#[cfg(test)]
mod tests;
mod upstream;

use library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan};
use library_root::{get_automatic_library_root, get_configured_library_root};
use media_reconciliation::{get_aggregate_show_items, get_virtual_library_items};
use postprocessing::{FederatedItems, ResponseShape};
use request_policy::{has_query_key, CatalogRequestPolicy};
use upstream::{fetch_catalog, FetchMode, FetchedCatalog};

async fn get_items(
    State(state): State<AppState>,
    Preprocessed(mut request): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    crate::request_preprocessing::apply_to_request(
        &mut request.request,
        &request.server,
        &request.session,
        &request.new_auth,
        &state,
        request.access_scope.as_ref(),
    )
    .await;
    crate::handlers::items::get_items(State(state), crate::extractors::Preprocessed(request)).await
}

/// Series detail and single-channel/genre guides identify one upstream item.
/// Query only its owning server so unrelated programs cannot enter the result.
pub async fn get_live_tv_programs(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let url = preprocessed.original_request.url();
    let single_scoped_id = url.query_pairs().any(|(key, value)| {
        (key.eq_ignore_ascii_case("ChannelIds") || key.eq_ignore_ascii_case("GenreIds"))
            && value.split(',').filter(|id| !id.trim().is_empty()).count() == 1
    });
    if has_query_key(url, &["LibrarySeriesId"]) || single_scoped_id {
        get_items(State(state), Preprocessed(preprocessed)).await
    } else {
        get_items_from_all_servers_preprocessed(&state, preprocessed).await
    }
}

async fn is_aggregate_id(state: &AppState, url: &url::Url) -> bool {
    let series_id = url.query_pairs().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("SeriesId") || key.eq_ignore_ascii_case("SeasonId"))
            .then(|| value.into_owned())
    });
    let Some(series_id) = series_id else {
        return false;
    };
    state
        .media_storage
        .get_media_version_group(&series_id)
        .await
        .ok()
        .flatten()
        .is_some()
}

pub async fn get_items_from_all_servers_if_not_restricted(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = &preprocessed.original_request;

    if has_query_key(original_request.url(), &["SeriesId"])
        && !is_aggregate_id(&state, original_request.url()).await
    {
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

/// Federates `/Shows/{seriesId}/Seasons|Episodes` when `seriesId` is a
/// collapsed show aggregate; otherwise falls back to single-server proxying.
pub async fn get_show_children_from_all_servers(
    State(state): State<AppState>,
    Preprocessed(preprocessed): Preprocessed,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use crate::url_helper::contains_id;

    let aggregate_id = contains_id(preprocessed.original_request.url(), "Shows");
    let Some(aggregate_id) = aggregate_id else {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    };
    if !state.deduplicate_media_enabled().await {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    }
    let Some(group) = state
        .media_storage
        .get_media_version_group(&aggregate_id)
        .await
        .map_err(|error| {
            error!("Failed to resolve show aggregate {aggregate_id}: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
    else {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    };
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(|error| {
            error!("Failed to load show aggregate members: {error}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let Some(sessions) = preprocessed.sessions.clone() else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let mut skipped_targets = 0;
    let mut targets = Vec::new();
    for member in members {
        if preprocessed
            .access_scope
            .as_ref()
            .is_some_and(|scope| !scope.allows(member.server.id))
        {
            continue;
        }
        let Some((session, server)) = sessions
            .iter()
            .find(|(_, server)| server.id == member.server.id)
            .cloned()
        else {
            skipped_targets += 1;
            continue;
        };
        targets.push(CatalogFetchTarget {
            session,
            server,
            parent_id: Some(member.mapping.original_media_id),
            resolved_parent_id: Some(aggregate_id.clone()),
        });
    }
    if targets.is_empty() {
        return get_items(State(state), Preprocessed(preprocessed)).await;
    }
    get_aggregate_show_items(
        &state,
        preprocessed,
        group.virtual_media_id,
        targets,
        skipped_targets,
    )
    .await
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
                state,
                &preprocessed.original_request,
                FederatedItems::default(),
                &CatalogRequestPolicy::from_url(preprocessed.original_request.url()),
                response_shape,
            )
            .await
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
    mut preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let snapshot =
        snapshots::SnapshotRequest::prepare(state, &mut preprocessed, &targets, "listing").await;
    snapshot
        .serve(
            state,
            get_interleaved_root_uncached(state, preprocessed, targets),
        )
        .await
}

async fn get_interleaved_root_uncached(
    state: &AppState,
    preprocessed: PreprocessedRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let original_request = preprocessed.original_request;
    let policy = CatalogRequestPolicy::from_url(original_request.url());
    let FetchedCatalog {
        server_items,
        response_shape,
        ..
    } = fetch_catalog(
        state,
        &original_request,
        &policy,
        targets,
        FetchMode::Listing,
        0,
    )
    .await?;
    upstream::require_complete_sources(&server_items)?;
    let server_count = server_items.len();
    let name_policy = if state.config.read().await.include_server_name_in_media {
        crate::media_presentation::ItemNamePolicy::IncludeServerName
    } else {
        crate::media_presentation::ItemNamePolicy::Preserve
    };
    let items = FederatedItems::from_servers(
        server_items
            .into_iter()
            .map(|items| items.server_items)
            .collect(),
        name_policy,
    );

    debug!("Combined items from {server_count} servers");

    finalize_items_response(state, &original_request, items, &policy, response_shape).await
}

async fn finalize_items_response(
    state: &AppState,
    request: &reqwest::Request,
    items: FederatedItems,
    policy: &CatalogRequestPolicy,
    response_shape: ResponseShape,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let response = serde_json::to_value(items.into_response_with_policy(policy, response_shape))
        .map_err(|e| {
            error!("Failed to serialize federated items response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if library_resolution::is_library_root_request(
        request.url(),
        state.get_url_prefix().await.as_deref(),
    ) {
        if let Some(auth) =
            crate::request_preprocessing::JellyfinAuthorization::from_request(request)
        {
            if let Some(token) = auth.token_ref() {
                state
                    .client_sessions
                    .cache_media_response(token, &response)
                    .await;
            }
        }
    }
    Ok(Json(response))
}
