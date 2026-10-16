use crate::application::error::ApplicationError;
use hyper::StatusCode;
use serde_json::{json, Value};

use super::{
    library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan},
    request_policy::replace_aggregate_parent_id,
};
use crate::{
    application::catalog::input::CatalogRequest,
    application::json::execute_processed_json_request_typed,
    processors::response_processor::ResponseProcessingProfile, AppState,
};

/// Facet IDs remain source-owned. Partition OR lists for each backend instead
/// of sending another server's virtual IDs (or accidentally dropping a filter).
pub(super) async fn restrict_named_ids(
    state: &AppState,
    url: &mut url::Url,
    server_id: crate::server_id::ServerId,
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
pub(crate) async fn get_filters_typed(
    state: &AppState,
    preprocessed: CatalogRequest,
) -> Result<Value, ApplicationError> {
    let targets = match resolve_catalog_plan(state, &preprocessed).await? {
        CatalogPlan::EmptyVirtual => {
            return Ok(json!({"Genres":[],"Tags":[],"Years":[],"OfficialRatings":[]}))
        }
        CatalogPlan::SingleServer => {
            let (server, session, _) = crate::application::routing::resolve_server(
                &preprocessed.context.backends.sessions,
                &preprocessed.body_analysis,
                state,
                &preprocessed.original_request,
                preprocessed.context.access_scope.as_ref(),
            )
            .await
            .map_err(super::input::input_error_typed)?;
            vec![CatalogFetchTarget {
                session: session.ok_or_else(|| {
                    ApplicationError::MissingIdentity("facet session")
                        .with_status(StatusCode::UNAUTHORIZED)
                })?,
                server,
                parent_id: None,
                member_parent_id: None,
                resolved_parent_id: None,
            }]
        }
        CatalogPlan::Virtual { targets, .. }
        | CatalogPlan::Interleaved(targets)
        | CatalogPlan::AutomaticRoot(targets)
        | CatalogPlan::ConfiguredRoot(targets) => targets,
    };
    let token = preprocessed
        .context
        .auth
        .as_ref()
        .and_then(|auth| auth.token_ref());
    let timeout = std::time::Duration::from_secs(state.config.read().await.timeout.clamp(1, 300));
    let fetches = targets.into_iter().map(|target| {
        let preprocessed = &preprocessed;
        async move {
            let mut request = preprocessed
                .original_request
                .try_clone()
                .ok_or(ApplicationError::MissingIdentity("cloneable facet request"))?;
            if let (Some(parent), Some(resolved)) =
                (&target.member_parent_id, &target.resolved_parent_id)
            {
                *request.url_mut() = replace_aggregate_parent_id(request.url(), resolved, parent);
            }
            let authorized = preprocessed
                .context
                .target(target.server.id)
                .map_err(|error| {
                    let status = match &error {
                        crate::application::context::TargetError::UnavailableOwner => {
                            StatusCode::SERVICE_UNAVAILABLE
                        }
                        _ => StatusCode::FORBIDDEN,
                    };
                    ApplicationError::Internal(error.into()).with_status(status)
                })?;
            let prefix = state.get_url_prefix().await;
            let request = crate::upstream::jellyfin::request::UpstreamRequestBuilder::new(
                &state.processors.url_processor,
                prefix.as_deref(),
            )
            .build(&authorized, &request)
            .await
            .map_err(|error| ApplicationError::Internal(error.into()))?;
            tokio::time::timeout(
                timeout,
                execute_processed_json_request_typed(
                    state,
                    request,
                    &target.server,
                    ResponseProcessingProfile::Media,
                    token,
                ),
            )
            .await
            .map_err(|error| {
                ApplicationError::Internal(error.into()).with_status(StatusCode::GATEWAY_TIMEOUT)
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
