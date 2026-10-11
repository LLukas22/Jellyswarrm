use axum::{extract::State, Json};
use hyper::StatusCode;
use serde_json::{json, Value};

use super::{
    library_resolution::{resolve_catalog_plan, CatalogFetchTarget, CatalogPlan},
    request_policy::replace_aggregate_parent_id,
};
use crate::{
    extractors::CatalogPreprocessed,
    handlers::common::execute_processed_json_request,
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{apply_to_request, remap_authorization},
    AppState,
};

/// Facet IDs remain source-owned. Partition OR lists for each backend instead
/// of sending another server's virtual IDs (or accidentally dropping a filter).
pub(super) async fn restrict_named_ids(
    state: &AppState,
    url: &mut url::Url,
    server_id: crate::server_id::ServerId,
) -> Result<bool, StatusCode> {
    for key in ["GenreIds", "StudioIds"] {
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
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
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
pub async fn get_filters(
    State(state): State<AppState>,
    CatalogPreprocessed(preprocessed): CatalogPreprocessed,
) -> Result<Json<Value>, StatusCode> {
    let targets = match resolve_catalog_plan(&state, &preprocessed).await? {
        CatalogPlan::EmptyVirtual => {
            return Ok(Json(
                json!({"Genres":[],"Tags":[],"Years":[],"OfficialRatings":[]}),
            ))
        }
        CatalogPlan::SingleServer => vec![CatalogFetchTarget {
            session: preprocessed
                .session
                .clone()
                .ok_or(StatusCode::UNAUTHORIZED)?,
            server: preprocessed.server.clone(),
            parent_id: None,
            resolved_parent_id: None,
        }],
        CatalogPlan::Virtual { targets, .. }
        | CatalogPlan::Interleaved(targets)
        | CatalogPlan::AutomaticRoot(targets)
        | CatalogPlan::ConfiguredRoot(targets) => targets,
    };
    let token = preprocessed.auth.as_ref().and_then(|auth| auth.token_ref());
    let timeout = std::time::Duration::from_secs(state.config.read().await.timeout.clamp(1, 300));
    let fetches = targets.into_iter().map(|target| {
        let state = &state;
        let preprocessed = &preprocessed;
        async move {
            let mut request = preprocessed
                .original_request
                .try_clone()
                .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
            if let (Some(parent), Some(resolved)) = (&target.parent_id, &target.resolved_parent_id)
            {
                *request.url_mut() = replace_aggregate_parent_id(request.url(), resolved, parent);
            }
            let session = Some(target.session);
            let auth = remap_authorization(&preprocessed.auth, &session)
                .await
                .map_err(|_| StatusCode::UNAUTHORIZED)?;
            apply_to_request(
                &mut request,
                &target.server,
                &session,
                &auth,
                state,
                preprocessed.access_scope.as_ref(),
            )
            .await;
            tokio::time::timeout(
                timeout,
                execute_processed_json_request(
                    state,
                    request,
                    &target.server,
                    ResponseProcessingProfile::Media,
                    token,
                ),
            )
            .await
            .map_err(|_| StatusCode::GATEWAY_TIMEOUT)?
        }
    });
    let mut merged = serde_json::Map::new();
    let mut successful = false;
    for result in futures_util::future::join_all(fetches).await {
        let Ok(Value::Object(fields)) = result else {
            continue;
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
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    Ok(Json(Value::Object(merged)))
}
