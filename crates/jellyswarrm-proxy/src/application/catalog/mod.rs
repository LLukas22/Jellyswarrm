//! Catalog workflows with unselected inputs and explicit target planning.
use crate::application::error::ApplicationError;
use tracing::debug;

use self::services::CatalogService;
pub(crate) mod services;
use input::CatalogRequest;

pub(crate) mod input;
pub(crate) mod query;
pub(crate) mod refresh;

pub(crate) mod filters;
mod item_lookup;
mod item_policy;
pub(crate) mod library_resolution;
mod library_root;
pub(crate) mod media_reconciliation;
pub(crate) mod postprocessing;
pub(crate) mod request_policy;
pub(crate) mod scan;
pub(crate) mod snapshots;
pub(crate) mod upstream;

use library_resolution::{
    resolve_catalog_plan, CatalogFetchTarget, CatalogPlan, ResolvedCatalogParent,
};
use library_root::{get_automatic_library_root, get_configured_library_root};
use postprocessing::{FederatedItems, ResponseShape};
use request_policy::{has_query_key, CatalogRequestPolicy};
use upstream::{fetch_catalog, FetchMode, FetchedCatalog};

async fn get_items(
    state: &CatalogService,
    request: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    crate::application::details::service::get_item_detail(state, request.into(), false).await
}

/// Series detail and single-channel/genre guides identify one upstream item.
/// Query only its owning server so unrelated programs cannot enter the result.
pub(crate) async fn get_live_tv_programs(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let url = preprocessed.original_request.url();
    let single_scoped_id = url.query_pairs().any(|(key, value)| {
        (key.eq_ignore_ascii_case("ChannelIds") || key.eq_ignore_ascii_case("GenreIds"))
            && value.split(',').filter(|id| !id.trim().is_empty()).count() == 1
    });
    if has_query_key(url, &["LibrarySeriesId"]) || single_scoped_id {
        get_items(state, preprocessed).await
    } else {
        get_items_from_all_servers_preprocessed(state, preprocessed).await
    }
}

async fn is_aggregate_id(state: &CatalogService, url: &url::Url) -> Result<bool, ApplicationError> {
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
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let original_request = &preprocessed.original_request;

    if has_query_key(original_request.url(), &["SeriesId"])
        && !is_aggregate_id(state, original_request.url()).await?
    {
        return get_items(state, preprocessed).await;
    }

    get_items_from_all_servers_preprocessed(state, preprocessed).await
}

pub(crate) async fn get_items_from_all_servers(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    get_items_from_all_servers_preprocessed(state, preprocessed).await
}

/// Federates `/Shows/{seriesId}/Seasons|Episodes` when `seriesId` is a
/// collapsed show aggregate; otherwise falls back to single-server proxying.
pub(crate) async fn get_show_children_from_all_servers(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    get_show_children(state, preprocessed).await
}

pub(crate) async fn get_show_children(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    use crate::url_helper::contains_id;

    let aggregate_id = contains_id(preprocessed.original_request.url(), "Shows");
    let Some(aggregate_id) = aggregate_id else {
        return crate::application::details::service::get_item_detail(
            state,
            preprocessed.into(),
            false,
        )
        .await;
    };
    if !state.deduplicate_media_enabled().await {
        return crate::application::details::service::get_item_detail(
            state,
            preprocessed.into(),
            false,
        )
        .await;
    }
    let Some(group) = state
        .media_storage
        .get_media_version_group(&aggregate_id)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return crate::application::details::service::get_item_detail(
            state,
            preprocessed.into(),
            false,
        )
        .await;
    };
    let members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?;
    let Some(sessions) = preprocessed.context.backends().sessions.clone() else {
        return Err(ApplicationError::MissingIdentity("show catalog sessions")
            .at(crate::application::error::FailureStage::CallerAuthentication));
    };
    let mut skipped_targets = 0;
    let mut targets = Vec::new();
    for member in members {
        if preprocessed
            .context
            .access_scope()
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
            parent: Some(ResolvedCatalogParent {
                backend: member.mapping.original_media_id.into(),
                member: member.mapping.virtual_media_id.into(),
                requested: aggregate_id.clone().into(),
            }),
        });
    }
    if targets.is_empty() {
        return crate::application::details::service::get_item_detail(
            state,
            preprocessed.into(),
            false,
        )
        .await;
    }
    media_reconciliation::get_aggregate_show_items_typed(
        state,
        preprocessed,
        group.virtual_media_id,
        targets,
        skipped_targets,
    )
    .await
}

pub(crate) async fn get_media_folders(
    state: &CatalogService,
    mut preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    let user_id = preprocessed
        .context
        .user()
        .map(|user| user.id.clone())
        .ok_or_else(|| {
            ApplicationError::MissingIdentity("media folder viewer")
                .at(crate::application::error::FailureStage::CallerAuthentication)
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

    get_items_from_all_servers_preprocessed(state, preprocessed).await
}

pub(crate) async fn get_items_from_all_servers_preprocessed(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    get_catalog(state, preprocessed).await
}

/// Cause-preserving catalog service boundary. Only HTTP adapters reduce errors
/// to the endpoint's established status contract.
pub(crate) async fn get_catalog(
    state: &CatalogService,
    preprocessed: CatalogRequest,
) -> Result<serde_json::Value, ApplicationError> {
    if preprocessed.query().kind == query::CatalogQueryKind::Batch {
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
            finalize_items_response_typed(
                state,
                &preprocessed.original_request,
                FederatedItems::default(),
                &preprocessed.query().policy(false),
                response_shape,
            )
            .await
        }
        CatalogPlan::SingleServer => {
            crate::application::details::service::get_item_detail(state, preprocessed.into(), false)
                .await
        }
        CatalogPlan::Virtual {
            catalog_scope_key,
            targets,
            skipped_targets,
        } => {
            media_reconciliation::get_virtual_library_items_typed(
                state,
                preprocessed,
                catalog_scope_key,
                targets,
                skipped_targets,
            )
            .await
        }
        CatalogPlan::Interleaved(targets) => {
            get_interleaved_root_typed(state, preprocessed, targets).await
        }
        CatalogPlan::AutomaticRoot(targets) => {
            get_automatic_library_root(state, preprocessed, targets).await
        }
        CatalogPlan::ConfiguredRoot(targets) => {
            get_configured_library_root(state, preprocessed, targets).await
        }
    }
}

async fn get_interleaved_root_typed(
    state: &CatalogService,
    preprocessed: CatalogRequest,
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
    state: &CatalogService,
    preprocessed: CatalogRequest,
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

    Ok(snapshots::SnapshotResponse {
        response: finalize_items_response_typed(
            state,
            &original_request,
            items,
            &policy,
            response_shape,
        )
        .await?,
        report,
    })
}

pub(super) async fn finalize_items_response_typed(
    state: &CatalogService,
    request: &reqwest::Request,
    items: FederatedItems,
    policy: &CatalogRequestPolicy,
    response_shape: ResponseShape,
) -> Result<serde_json::Value, ApplicationError> {
    let response = serde_json::to_value(items.into_response_with_policy(policy, response_shape))
        .map_err(ApplicationError::Serialization)?;
    if library_resolution::is_library_root_request(
        request.url(),
        state.get_url_prefix().await.as_deref(),
    ) {
        if let Some(auth) =
            crate::request_preprocessing::JellyfinAuthorization::from_request(request)
        {
            if let Some(token) = auth.token_ref() {
                crate::application::observation::media_response(
                    &state.client_sessions,
                    Some(token),
                    &response,
                )
                .await;
            }
        }
    }
    Ok(response)
}
