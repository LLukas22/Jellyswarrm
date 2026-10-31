//! Catalog workflow entry points and dispatch. Start with `get_catalog`.
//!
//! Flow: input → library_resolution → snapshots/fetch → media_reconciliation
//! → postprocessing. Roots, batch lookup, and facets branch to their named modules.
//! See `../../../DATA_FLOW.md`; dependencies come from `application::services`.
use crate::application::details::get_item_detail;
use crate::application::error::{ApplicationError, FailureStage};
use tracing::debug;

use crate::application::{request::input::OperationRequest, services::MediaServices};

mod query;
pub(crate) mod refresh;

mod fetch;
mod filters;
mod item_lookup;
mod library_resolution;
mod library_root;
mod media_reconciliation;
mod postprocessing;
mod request_policy;
pub(crate) mod scan;
pub(crate) mod snapshots;

use fetch::{fetch_catalog, FetchMode, FetchedCatalog};
use library_resolution::{
    resolve_catalog_plan, CatalogFetchTarget, CatalogPlan, ResolvedCatalogParent,
};
use library_root::{get_automatic_library_root, get_configured_library_root};
use postprocessing::{FederatedItems, ResponseShape};
use request_policy::{has_query_key, CatalogRequestPolicy};

/// Series detail and single-channel/genre guides identify one upstream item.
/// Query only its owning server so unrelated programs cannot enter the result.
pub(crate) async fn get_live_tv_programs(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let url = preprocessed.original_request.url();
    let single_scoped_id = url.query_pairs().any(|(key, value)| {
        (key.eq_ignore_ascii_case("ChannelIds") || key.eq_ignore_ascii_case("GenreIds"))
            && value.split(',').filter(|id| !id.trim().is_empty()).count() == 1
    });
    if has_query_key(url, &["LibrarySeriesId"]) || single_scoped_id {
        get_item_detail(state, preprocessed, false).await
    } else {
        get_catalog(state, preprocessed).await
    }
}

async fn is_aggregate_id(state: &MediaServices, url: &url::Url) -> Result<bool, ApplicationError> {
    let series_id = url.query_pairs().find_map(|(key, value)| {
        (key.eq_ignore_ascii_case("SeriesId") || key.eq_ignore_ascii_case("SeasonId"))
            .then(|| value.into_owned())
    });
    let Some(series_id) = series_id else {
        return Ok(false);
    };
    Ok(state
        .media_storage
        .get_media_version_group(&series_id)
        .await
        .map_err(ApplicationError::Storage)?
        .is_some())
}

pub(crate) async fn get_items_from_all_servers_if_not_restricted(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let original_request = &preprocessed.original_request;

    if has_query_key(original_request.url(), &["SeriesId"])
        && !is_aggregate_id(state, original_request.url()).await?
    {
        return get_item_detail(state, preprocessed, false).await;
    }

    get_catalog(state, preprocessed).await
}

/// Federates `/Shows/{seriesId}/Seasons|Episodes` when `seriesId` is a
/// collapsed show aggregate; otherwise falls back to single-server proxying.
pub(crate) async fn get_show_children(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<serde_json::Value, ApplicationError> {
    use crate::protocol::url::contains_id;

    let aggregate_id = contains_id(preprocessed.original_request.url(), "Shows");
    let Some(aggregate_id) = aggregate_id else {
        return get_item_detail(state, preprocessed, false).await;
    };
    if !state.deduplicate_media_enabled().await {
        return get_item_detail(state, preprocessed, false).await;
    }
    let Some(group) = state
        .media_storage
        .get_media_version_group(&aggregate_id)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return get_item_detail(state, preprocessed, false).await;
    };
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?;
    let Some(sessions) = preprocessed.context.backends().sessions.clone() else {
        return Err(ApplicationError::MissingIdentity("show catalog sessions")
            .at(FailureStage::CallerAuthentication));
    };
    let mut skipped_targets = 0;
    let mut targets = Vec::new();
    for member in members {
        if preprocessed
            .context
            .access_scope()
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
            parent: Some(ResolvedCatalogParent {
                backend: member.mapping.original_media_id.into(),
                member: member.mapping.virtual_media_id.into(),
                requested: aggregate_id.clone().into(),
            }),
        });
    }
    if targets.is_empty() {
        return get_item_detail(state, preprocessed, false).await;
    }
    media_reconciliation::get_aggregate_show_items(
        state,
        preprocessed,
        group.virtual_media_id,
        targets,
        skipped_targets,
    )
    .await
}

pub(crate) async fn get_media_folders(
    state: &MediaServices,
    mut preprocessed: OperationRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let user_id = preprocessed
        .context
        .user()
        .map(|user| user.id.clone())
        .ok_or_else(|| {
            ApplicationError::MissingIdentity("media folder viewer")
                .at(FailureStage::CallerAuthentication)
        })?;
    let path = preprocessed.original_request.url().path().to_string();
    let path = path
        .strip_suffix("/Library/MediaFolders")
        .or_else(|| path.strip_suffix("/library/mediafolders"))
        .unwrap_or_default();
    preprocessed
        .original_request
        .url_mut()
        .set_path(&format!("{path}/Users/{user_id}/Views"));

    get_catalog(state, preprocessed).await
}

/// Cause-preserving catalog service boundary. Only HTTP adapters reduce errors
/// to the endpoint's established status contract.
pub(crate) async fn get_catalog(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<serde_json::Value, ApplicationError> {
    if preprocessed.catalog_query().kind == query::CatalogQueryKind::Batch {
        if let Some(lookup) = item_lookup::RequestedItems::resolve(state, &preprocessed).await? {
            return lookup.fetch(state, preprocessed).await;
        }
    }
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
                &preprocessed.catalog_query().policy(false),
                response_shape,
            )
            .await
        }
        CatalogPlan::SingleServer => get_item_detail(state, preprocessed, false).await,
        CatalogPlan::Virtual {
            catalog_scope_key,
            targets,
            skipped_targets,
        } => {
            media_reconciliation::get_virtual_library_items(
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
    state: &MediaServices,
    preprocessed: OperationRequest,
    targets: Vec<CatalogFetchTarget>,
) -> Result<serde_json::Value, ApplicationError> {
    let snapshot =
        snapshots::SnapshotRequest::prepare(state, &preprocessed, &targets, "listing").await;
    let policy = snapshot.materialization_policy(&preprocessed);
    snapshot
        .serve(
            state,
            get_interleaved_root_uncached(state, preprocessed, targets, policy),
        )
        .await
}

async fn get_interleaved_root_uncached(
    state: &MediaServices,
    preprocessed: OperationRequest,
    targets: Vec<CatalogFetchTarget>,
    policy: CatalogRequestPolicy,
) -> Result<snapshots::SnapshotResponse, ApplicationError> {
    let skipped_targets = library_resolution::unavailable_source_count(&preprocessed, &targets);
    let original_request = preprocessed.original_request;
    let FetchedCatalog {
        server_items,
        response_shape,
        report,
    } = fetch_catalog(
        state,
        &original_request,
        &policy,
        targets,
        FetchMode::Listing,
        skipped_targets,
    )
    .await?;
    let server_count = server_items.len();
    let name_policy = if state.config.read().await.include_server_name_in_media {
        crate::media::presentation::ItemNamePolicy::IncludeServerName
    } else {
        crate::media::presentation::ItemNamePolicy::Preserve
    };
    let items = FederatedItems::from_servers(
        server_items
            .into_iter()
            .map(|items| items.server_items)
            .collect(),
        name_policy,
    );

    debug!("Combined items from {server_count} servers");

    Ok(snapshots::SnapshotResponse {
        response: finalize_items_response(state, &original_request, items, &policy, response_shape)
            .await?,
        report,
    })
}

pub(super) async fn finalize_items_response(
    state: &MediaServices,
    request: &reqwest::Request,
    items: FederatedItems,
    policy: &CatalogRequestPolicy,
    response_shape: ResponseShape,
) -> Result<serde_json::Value, ApplicationError> {
    let response = serde_json::to_value(items.into_response_with_policy(policy, response_shape))
        .map_err(ApplicationError::Serialization)?;
    if library_resolution::is_library_root_request(request.url(), state.url_prefix()) {
        if let Some(auth) = crate::protocol::identity::JellyfinAuthorization::from_headers_url(
            request.headers(),
            request.url(),
        ) {
            if let Some(token) = auth.token_ref() {
                state
                    .client_sessions
                    .cache_media_response(token, &response)
                    .await;
            }
        }
    }
    Ok(response)
}

#[cfg(test)]
#[path = "../../unit_tests/federated_reliability.rs"]
mod reliability_tests;
#[cfg(test)]
#[path = "../../unit_tests/federated_snapshots.rs"]
mod snapshot_tests;
#[cfg(test)]
#[path = "../../unit_tests/federated_catalog.rs"]
pub(crate) mod tests;

pub(crate) use filters::get_filters;
