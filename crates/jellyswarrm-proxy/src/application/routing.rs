//! Backend selection before upstream request encoding. Authorization, availability,
//! and playback authority retain their distinct routing policies.

use anyhow::{anyhow, Result};
use tracing::debug;

use crate::application::playback::reports;
use crate::processors::request_analyzer::RequestBodyAnalysisResult;
use crate::server_storage::Server;
use crate::user_authorization_service::AuthorizationSession;
use crate::virtual_library_service::{compare_virtual_library_routes, VirtualLibraryAccessScope};
use crate::{
    config::AppConfig, media_storage_service::MediaStorageService,
    processors::url_processor::UrlProcessor, server_storage::ServerStorageService,
    session_storage::SessionStorage,
};

pub(crate) struct RoutingServices<'a> {
    pub media_storage: &'a MediaStorageService,
    pub server_storage: &'a ServerStorageService,
    pub play_sessions: &'a SessionStorage,
    pub urls: &'a UrlProcessor,
    pub config: &'a tokio::sync::RwLock<AppConfig>,
}

pub async fn resolve_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    request_body_result: &Option<RequestBodyAnalysisResult>,
    state: &RoutingServices<'_>,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    let config = state.config.read().await;
    let path = crate::url_helper::decoded_path_segments(match &config.url_prefix {
        Some(prefix) => request
            .url()
            .path()
            .trim_start_matches(&format!("/{prefix}")),
        None => request.url().path(),
    });
    drop(config);
    if let Some(target) =
        reports::resolve_cancellation(sessions, state.play_sessions, request, &path, access_scope)
            .await?
    {
        return Ok((target.server, Some(target.session), true));
    }
    if let Some(play_session) = request_body_result
        .as_ref()
        .and_then(|result| result.authoritative_play_session.as_ref())
        .map(|binding| binding.session())
    {
        let server = state
            .server_storage
            .get_server_by_id(play_session.server_id)
            .await?
            .ok_or_else(|| anyhow!("playback session server no longer exists"))?;
        let (session, _) = sessions
            .as_ref()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|(_session, candidate)| candidate.id == server.id)
            })
            .ok_or_else(|| anyhow!("playback session server is unavailable"))?;
        return Ok((server, Some(session.clone()), true));
    }

    if access_scope.is_some_and(|scope| !scope.server_ids().is_empty())
        && sessions.as_ref().is_none_or(|sessions| sessions.is_empty())
    {
        return Err(reports::NoAvailableBackend.into());
    }
    // Choose aggregate representatives only from usable sessions. Keep the
    // full mapped scope on the caller context for inventory reconciliation.
    let routing_scope = access_scope.map(|scope| {
        VirtualLibraryAccessScope::new(
            scope.user_id(),
            sessions.iter().flatten().map(|(_, server)| server.id),
        )
    });
    let mut request_server =
        server_from_request_media_ids(state, request, routing_scope.as_ref()).await?;

    // Body-only routes (for example recording creation) must not silently
    // fall back to an allowed server when their IDs belong to a forbidden one.
    if let (Some(scope), Some(analysis)) = (access_scope, request_body_result) {
        for id in &analysis.found_ids {
            state.urls.validate_media_access(id, scope).await?;
        }
    }

    if request_server.is_none() {
        if let Some(request_body_result) = request_body_result {
            if let Some(found_server) = request_body_result.get_server() {
                if access_scope.is_none_or(|scope| scope.allows(found_server.id)) {
                    debug!(server_id = %found_server.id, selection_reason = "body", "Selected backend");
                    request_server = Some(found_server);
                }
            }
        }
    }

    if request_server.is_none() {
        request_server = server_from_body_media_aggregate_ids(
            state,
            request_body_result.as_ref(),
            routing_scope.as_ref(),
        )
        .await?;
    }

    if let Some(sessions) = sessions {
        if let Some(request_server) = request_server {
            if let Some((session, server)) = sessions
                .iter()
                .find(|(_, server)| request_server.id == server.id)
            {
                debug!(server_id = %server.id, selection_reason = "request", "Selected backend");
                return Ok((server.clone(), Some(session.clone()), true));
            }
            return Err(anyhow!(
                "requested media server has no available authorization session"
            ));
        }

        let Some((session, server)) = sessions.first() else {
            return Err(anyhow!("no authorization sessions available"));
        };
        debug!(server_id = %server.id, selection_reason = "default", "Selected backend");
        return Ok((server.clone(), Some(session.clone()), false));
    }

    if access_scope.is_some() {
        return Err(anyhow!("no authorization sessions available"));
    }

    if let Some(request_server) = request_server {
        debug!(server_id = %request_server.id, selection_reason = "passthrough_request", "Selected backend");
        return Ok((request_server, None, true));
    }

    let server = state.server_storage.get_best_server().await?;
    let server = server.ok_or_else(|| anyhow!("No server available"))?;
    Ok((server, None, false))
}

pub(crate) async fn server_from_body_media_aggregate_ids(
    state: &RoutingServices<'_>,
    analysis: Option<&RequestBodyAnalysisResult>,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<Server>> {
    let Some(analysis) = analysis else {
        return Ok(None);
    };
    for media_id in &analysis.found_ids {
        let members = state
            .media_storage
            .get_media_version_members_by_virtual_id(media_id)
            .await?;
        let mut healthy_members = Vec::new();
        for member in members {
            if access_scope.is_none_or(|scope| scope.allows(member.server.id))
                && state
                    .server_storage
                    .server_status(member.server.id)
                    .await
                    .is_healthy()
            {
                healthy_members.push(member);
            }
        }
        if let Some(member) = healthy_members.into_iter().max_by(|left, right| {
            compare_virtual_library_routes(
                &left.server,
                &left.mapping.original_media_id,
                &right.server,
                &right.mapping.original_media_id,
            )
        }) {
            return Ok(Some(member.server));
        }
    }
    Ok(None)
}

pub(crate) async fn server_from_request_media_ids(
    state: &RoutingServices<'_>,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<Server>> {
    state
        .urls
        .server_from_client_url(request.url(), access_scope)
        .await
}
