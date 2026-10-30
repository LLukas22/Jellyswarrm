//! Apply access-scoped catalog filters and translate named or requested media IDs.
use crate::application::error::{ApplicationError, FailureStage};
use serde_json::{json, Value};

use super::{
    library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan},
    request_policy::replace_aggregate_parent_id,
};
use crate::application::services::MediaServices;
use crate::{
    application::input::OperationRequest, processors::response_processor::ResponseProcessingProfile,
};

/// Facet IDs remain source-owned. Partition OR lists for each backend instead
/// of sending another server's virtual IDs (or accidentally dropping a filter).
pub(super) async fn restrict_named_ids(
    state: &MediaServices,
    url: &mut url::Url,
    server_id: crate::domain::server_id::ServerId,
) -> Result<bool, ApplicationError> {
    let guide = url
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with("/livetv/programs");
    for key in ["GenreIds", "StudioIds"]
        .into_iter()
        .chain(guide.then_some("ChannelIds"))
    {
        let ids = url
            .query_pairs()
            .filter(|(name, _)| name.eq_ignore_ascii_case(key))
            .flat_map(|(_, value)| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        if ids.is_empty() {
            continue;
        }
        let mut selected = Vec::new();
        for id in ids {
            if state
                .media_storage
                .get_media_mapping_by_virtual(&id)
                .await
                .map_err(ApplicationError::Storage)?
                .is_some_and(|mapping| mapping.server_id == server_id)
            {
                selected.push(id);
            }
        }
        if selected.is_empty() {
            return Ok(false);
        }
        let pairs = url
            .query_pairs()
            .filter(|(name, _)| !name.eq_ignore_ascii_case(key))
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(pairs)
            .append_pair(key, &selected.join(","));
    }
    Ok(true)
}

/// Filter DTOs contain arrays of options rather than paged BaseItemDto lists.
/// Resolve library members as for browsing, then union their visible options.
pub(crate) async fn get_filters(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<Value, ApplicationError> {
    let targets = match resolve_catalog_plan(state, &preprocessed).await? {
        CatalogPlan::EmptyVirtual => {
            return Ok(json!({"Genres":[],"Tags":[],"Years":[],"OfficialRatings":[]}))
        }
        CatalogPlan::SingleServer => {
            let (server, session, _) = crate::application::routing::resolve_server(
                &preprocessed.context.backends().sessions,
                &preprocessed.body_analysis,
                state,
                &preprocessed.original_request,
                preprocessed.context.access_scope(),
            )
            .await
            .map_err(ApplicationError::from)?;
            vec![CatalogFetchTarget {
                session: session.ok_or_else(|| {
                    ApplicationError::MissingIdentity("facet session")
                        .at(FailureStage::CallerAuthentication)
                })?,
                server,
                parent: None,
            }]
        }
        CatalogPlan::Virtual { targets, .. }
        | CatalogPlan::Interleaved(targets)
        | CatalogPlan::AutomaticRoot(targets)
        | CatalogPlan::ConfiguredRoot(targets) => targets,
    };
    let token = preprocessed
        .context
        .authorization()
        .and_then(|auth| auth.token_ref());
    let timeout = std::time::Duration::from_secs(state.config.read().await.timeout.clamp(1, 300));
    let fetches = targets.into_iter().map(|target| {
        let preprocessed = &preprocessed;
        async move {
            let mut request = preprocessed
                .original_request
                .try_clone()
                .ok_or(ApplicationError::MissingIdentity("cloneable facet request"))?;
            if let Some(parent) = &target.parent {
                *request.url_mut() =
                    replace_aggregate_parent_id(request.url(), &parent.requested, &parent.member);
            }
            let authorized = preprocessed
                .context
                .target(target.server.id)
                .map_err(ApplicationError::TargetSelection)?;
            let request = state.request_builder().build(&authorized, &request).await?;
            tokio::time::timeout(
                timeout,
                state.execute_processed_json(
                    request,
                    &target.server,
                    ResponseProcessingProfile::Media,
                    token,
                ),
            )
            .await
            .map_err(|error| {
                ApplicationError::Internal(error.into()).at(FailureStage::CatalogDeadline)
            })?
        }
    });
    let mut merged = serde_json::Map::new();
    let mut successful = false;
    let mut failures = Vec::new();
    for result in futures_util::future::join_all(fetches).await {
        let fields = match result {
            Ok(Value::Object(fields)) => fields,
            Ok(_) => {
                failures.push(ApplicationError::InvalidUpstreamPayload(
                    "facet response must be an object",
                ));
                continue;
            }
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        successful = true;
        for (key, value) in fields {
            match value {
                Value::Array(options) => {
                    let entry = merged
                        .entry(key)
                        .or_insert_with(|| Value::Array(Vec::new()));
                    if let Some(values) = entry.as_array_mut() {
                        for option in options {
                            if !values.contains(&option) {
                                values.push(option);
                            }
                        }
                    }
                }
                value => {
                    merged.entry(key).or_insert(value);
                }
            }
        }
    }
    if !successful {
        return Err(ApplicationError::SourcesFailed(failures));
    }
    Ok(Value::Object(merged))
}
