//! Choose authorized playback routes using owner, version, and session evidence.
#[cfg(test)]
use super::target::{MediaTargetResolver, ResolutionError};
use crate::application::error::ApplicationError;
#[cfg(test)]
use hyper::StatusCode;
#[cfg(test)]
use tracing::debug;

#[cfg(test)]
use crate::application::context::{AuthorizedTarget, RequestContext, TargetError};
#[cfg(test)]
use crate::AppState;
use crate::{
    application::details::sources::version_name,
    media::storage::{MediaStorageService, MediaVersionSourceObservation},
    models::MediaSource,
    servers::storage::Server,
};

#[cfg(test)]
pub(crate) enum PlaybackRouteDecision {
    Original,
    Rerouted(Box<ReroutedPlaybackRoute>),
    InvalidSelectedSource,
    SelectedSourceUnavailable,
}

#[cfg(test)]
pub(crate) struct ReroutedPlaybackRoute {
    pub target: AuthorizedTarget,
    pub member_virtual_id: String,
}

pub(crate) async fn record_playback_sources(
    media: &MediaStorageService,
    aggregate_id: &str,
    server: &Server,
    original_item_id: &str,
    source_generation: crate::media::storage::ReconciliationGeneration,
    sources: &mut Vec<MediaSource>,
) -> Result<(), ApplicationError> {
    let Some(group) = media
        .get_media_version_group(aggregate_id)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return Ok(());
    };
    let Some(member) = media
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?
        .into_iter()
        .find(|member| {
            member.mapping.server_id == server.id
                && member.mapping.original_media_id == original_item_id
        })
    else {
        return Ok(());
    };
    let observations = sources
        .iter()
        .map(|source| MediaVersionSourceObservation {
            member_mapping_id: member.mapping.id,
            source_virtual_id: source.id.clone(),
        })
        .collect::<Vec<_>>();
    // Keep negotiated names identical to the versions advertised by details.
    for source in sources.iter_mut() {
        source.name = Some(version_name(source, server));
    }
    let sources_recorded = media
        .observe_media_version_sources(
            group.id,
            source_generation,
            &[member.mapping.id],
            &observations,
        )
        .await
        .map_err(ApplicationError::Storage)?;
    if !sources_recorded {
        let mut routable_sources = Vec::new();
        for source in sources.drain(..) {
            if media
                .get_media_version_source_route(group.id, &source.id)
                .await
                .map_err(ApplicationError::Storage)?
                .is_some()
            {
                routable_sources.push(source);
            }
        }
        *sources = routable_sources;
    }
    Ok(())
}

/// Test-only compatibility adapter for the pre-extraction reliability scenarios
/// and source-selection characterization tests. Production entry points all use
/// `target::MediaTargetResolver::plan`; no production routing lives here.
#[cfg(test)]
pub(crate) async fn resolve_playback_route(
    state: &AppState,
    context: &RequestContext,
    requested_item_id: &str,
    selected_source_id: Option<&str>,
) -> Result<PlaybackRouteDecision, StatusCode> {
    let Some(group) = state
        .media_storage
        .get_media_version_group(requested_item_id)
        .await
        .map_err(storage_error)?
    else {
        return Ok(PlaybackRouteDecision::Original);
    };
    let Some(selected_source_id) = selected_source_id.filter(|id| !id.trim().is_empty()) else {
        return Ok(PlaybackRouteDecision::Original);
    };
    let resolved = match (MediaTargetResolver {
        media: &state.media_storage,
    })
    .selected(context, requested_item_id, selected_source_id)
    .await
    {
        Ok(resolved) => resolved,
        Err(ResolutionError::Target(TargetError::UnavailableOwner)) => {
            return Ok(PlaybackRouteDecision::SelectedSourceUnavailable)
        }
        Err(ResolutionError::Target(TargetError::ViewerMismatch)) => {
            return Err(StatusCode::FORBIDDEN)
        }
        Err(ResolutionError::Storage(error)) => return Err(storage_error(error)),
        Err(_) => return Ok(PlaybackRouteDecision::InvalidSelectedSource),
    };
    if !state
        .server_storage
        .server_status(resolved.backend.server().id)
        .await
        .is_healthy()
    {
        return Ok(PlaybackRouteDecision::SelectedSourceUnavailable);
    }
    let target = resolved.backend;
    debug!(
        "Routing aggregate media {} source {} to server {}",
        group.virtual_media_id,
        selected_source_id,
        target.server().name
    );
    Ok(PlaybackRouteDecision::Rerouted(Box::new(
        ReroutedPlaybackRoute {
            target,
            member_virtual_id: resolved
                .member
                .expect("selected source member")
                .virtual_media_id,
        },
    )))
}

#[cfg(test)]
fn storage_error(_error: sqlx::Error) -> StatusCode {
    StatusCode::INTERNAL_SERVER_ERROR
}
