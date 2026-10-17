//! Item detail workflow: select owner, fetch, merge sources, and project the response.
//! Entry point: `get_item_detail`. Source-merging logic lives in `sources`.
pub(crate) mod sources;

#[cfg(test)]
use hyper::StatusCode;

#[cfg(test)]
mod tests;

use crate::application::catalog::services::CatalogService;
#[cfg(test)]
use crate::AppState;
use crate::{
    application::details::sources::{
        merge_media_detail, normalize_detail_member, DetailMergeContext,
    },
    application::error::ApplicationError,
    application::{context::TargetError, input::OperationRequest},
    processors::response_processor::ResponseProcessingProfile,
    upstream::jellyfin::request::UpstreamRequestBuilder,
    url_helper::{contains_id, ensure_query_list_value},
    virtual_library_service::VirtualLibraryResolution,
};

/// Retry ordered virtual-library members only for an upstream response failure.
/// Access and local application failures must never initiate fallback.
struct VirtualLibraryFallbackPolicy;

impl VirtualLibraryFallbackPolicy {
    fn eligible(error: &ApplicationError) -> bool {
        error.allows_detail_fallback()
    }

    fn order(members: &mut [crate::virtual_library_service::VirtualLibraryMember]) {
        members.sort_by(|left, right| {
            crate::virtual_library_service::compare_virtual_library_routes(
                &right.server,
                &right.mapping.original_media_id,
                &left.server,
                &left.mapping.original_media_id,
            )
        });
    }
}

/// Fetch item JSON, applying the established fallback and optional source merge.
pub(crate) async fn get_item_detail(
    state: &CatalogService,
    input: OperationRequest,
    merge_media_versions: bool,
) -> Result<serde_json::Value, ApplicationError> {
    let mut target = input
        .resolve_target_using(&state.routing_services())
        .await?
        .target;
    let virtual_library = input
        .original_request
        .url()
        .path_segments()
        .and_then(Iterator::last)
        .map(str::to_string);
    let requested_item_id = contains_id(input.original_request.url(), "Items");
    let mut normalized_request = input.original_request.try_clone().ok_or_else(|| {
        ApplicationError::Internal(anyhow::anyhow!("detail request is not cloneable"))
    })?;
    let mut selected_group = None;
    if let (true, Some(item_id), Some(scope)) = (
        merge_media_versions,
        requested_item_id.as_deref(),
        input.context.access_scope().as_ref(),
    ) {
        if let Some((group, owner)) = state
            .media_storage
            .get_media_version_detail_route(item_id, scope.user_id())
            .await
            .map_err(ApplicationError::Storage)?
        {
            target = input
                .context
                .target(owner.server_id)
                .map_err(target_error)?;
            // A source ID is not necessarily its owning item ID. Normalize with
            // the virtual member ID so the builder translates exactly once.
            normalize_detail_member(&mut normalized_request, &owner)?;
            selected_group = Some(group);
        }
    }
    let source_generation = if merge_media_versions {
        Some(
            state
                .media_storage
                .begin_media_reconciliation()
                .await
                .map_err(ApplicationError::Storage)?,
        )
    } else {
        None
    };
    let mut server = target.server().clone();
    let proxy_api_key = input
        .context
        .authorization()
        .and_then(|auth| auth.token_ref())
        .map(str::to_string);

    if merge_media_versions {
        ensure_query_list_value(normalized_request.url_mut(), "Fields", "MediaSources");
    }
    let prefix = state.get_url_prefix().await;
    let builder = UpstreamRequestBuilder::new(&state.processors.url_processor, prefix.as_deref());
    let request = builder
        .build(&target, &normalized_request)
        .await
        .map_err(|error| ApplicationError::Internal(error.into()))?;
    let failed_id = contains_id(request.url(), "Items");
    let result = state
        .execute_processed_json(
            request,
            &server,
            ResponseProcessingProfile::Media,
            proxy_api_key.as_deref(),
        )
        .await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            if !VirtualLibraryFallbackPolicy::eligible(&error) {
                return Err(error);
            }
            let Some(id) = virtual_library.as_deref() else {
                return Err(error);
            };
            let VirtualLibraryResolution::Resolved(mut library) = state
                .virtual_library_service
                .resolve(id, input.context.access_scope().as_ref())
                .await
                .map_err(ApplicationError::Storage)?
            else {
                return Err(error);
            };
            // Visibility can change after a root refresh (or a client may open
            // a saved ID first). Retry other authorized members on source failure.
            VirtualLibraryFallbackPolicy::order(&mut library.members);
            let mut fallback = None;
            for member in library.members {
                if member.server.id == server.id
                    && failed_id.as_deref() == Some(&member.mapping.original_media_id)
                {
                    continue;
                }
                let member_target = match input.context.target(member.server.id) {
                    Ok(target) => target,
                    Err(TargetError::UnavailableOwner) => continue,
                    Err(error) => return Err(target_error(error)),
                };
                let mut request = normalized_request.try_clone().ok_or_else(|| {
                    ApplicationError::Internal(anyhow::anyhow!("detail request is not cloneable"))
                })?;
                normalize_detail_member(&mut request, &member.mapping)?;
                let request = builder
                    .build(&member_target, &request)
                    .await
                    .map_err(|error| ApplicationError::Internal(error.into()))?;
                match state
                    .execute_processed_json(
                        request,
                        &member.server,
                        ResponseProcessingProfile::Media,
                        proxy_api_key.as_deref(),
                    )
                    .await
                {
                    Ok(response) => {
                        server = member.server;
                        fallback = Some(response);
                        break;
                    }
                    Err(error @ ApplicationError::AccessDenied) => return Err(error),
                    // Eligibility is checked on the initial failure. Once the
                    // fallback search begins, a failed upstream candidate must
                    // not hide another member (or replace the initial error).
                    Err(_) => continue,
                }
            }
            fallback.ok_or(error)?
        }
    };

    if let (true, Some(requested_item_id)) = (merge_media_versions, requested_item_id.as_deref()) {
        merge_media_detail(
            state,
            DetailMergeContext {
                requested_item_id,
                selected_group,
                base_server: &server,
                auth: &input.context.authorization().cloned(),
                access_scope: input.context.access_scope().as_ref(),
                sessions: input.context.backends().sessions.as_deref(),
                original_request: &input.original_request,
                source_generation: source_generation
                    .expect("merged detail has a source generation"),
            },
            proxy_api_key.as_deref(),
            &mut response,
        )
        .await?;
    }

    if let Some(virtual_id) = virtual_library {
        let resolution = state
            .virtual_library_service
            .resolve(&virtual_id, input.context.access_scope().as_ref())
            .await
            .map_err(ApplicationError::Storage)?;
        if let VirtualLibraryResolution::Resolved(resolved) = resolution {
            let name = resolved.name;
            response["Id"] = serde_json::Value::String(virtual_id.clone());
            response["DisplayPreferencesId"] = serde_json::Value::String(virtual_id);
            response["Name"] = serde_json::Value::String(name.clone());
            response["SortName"] = serde_json::Value::String(name.to_lowercase());
        }
    }

    crate::application::response::media_response(
        &state.client_sessions,
        proxy_api_key.as_deref(),
        &response,
    )
    .await;
    Ok(response)
}

fn target_error(error: TargetError) -> ApplicationError {
    match error {
        TargetError::AccessDenied | TargetError::ViewerMismatch => ApplicationError::AccessDenied,
        TargetError::UnavailableOwner => ApplicationError::UnavailableOwner,
    }
}
