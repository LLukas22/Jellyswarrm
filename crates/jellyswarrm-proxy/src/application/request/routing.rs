//! Backend selection before upstream request encoding. Authorization, availability,
//! and playback authority retain their distinct routing policies.

type Result<T> = std::result::Result<T, RoutingError>;
use crate::application::error::ApplicationError;
use tracing::debug;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RoutingError {
    #[error("routing storage failed")]
    Storage(#[from] sqlx::Error),
    #[error("requested media is outside the caller's authorized scope")]
    AccessDenied,
    #[error("selected backend is unavailable")]
    UnavailableOwner,
    #[error("invalid routing selection")]
    InvalidSelection(#[source] anyhow::Error),
}

impl From<RoutingError> for ApplicationError {
    fn from(error: RoutingError) -> Self {
        match error {
            RoutingError::Storage(cause) => Self::Storage(cause),
            RoutingError::AccessDenied => Self::AccessDenied,
            RoutingError::UnavailableOwner => Self::UnavailableOwner,
            RoutingError::InvalidSelection(cause) => Self::InvalidRouting(cause),
        }
    }
}

use crate::application::playback::reports;
use crate::application::services::MediaServices;
use crate::auth::service::AuthorizationSession;
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::processors::request_analyzer::RequestBodyAnalysisResult;
use crate::servers::compare_routes;
use crate::servers::storage::Server;

pub(crate) async fn resolve_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    request_body_result: &Option<RequestBodyAnalysisResult>,
    state: &MediaServices,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    let path =
        crate::protocol::url::decoded_path_segments(state.remove_prefix(request.url().path()));
    if let Some(target) = reports::resolve_cancellation(
        sessions,
        &state.playback_registry,
        request,
        &path,
        access_scope,
    )
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
            .ok_or(RoutingError::UnavailableOwner)?;
        let (session, _) = sessions
            .as_ref()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|(_session, candidate)| candidate.id == server.id)
            })
            .ok_or(RoutingError::UnavailableOwner)?;
        return Ok((server, Some(session.clone()), true));
    }

    if access_scope.is_some_and(|scope| !scope.server_ids().is_empty())
        && sessions.as_ref().is_none_or(|sessions| sessions.is_empty())
    {
        return Err(RoutingError::UnavailableOwner);
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
            state.url_selection.validate_media_access(id, scope).await?;
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
            return Err(RoutingError::UnavailableOwner);
        }

        let Some((session, server)) = sessions.first() else {
            return Err(RoutingError::UnavailableOwner);
        };
        debug!(server_id = %server.id, selection_reason = "default", "Selected backend");
        return Ok((server.clone(), Some(session.clone()), false));
    }

    if access_scope.is_some() {
        return Err(RoutingError::UnavailableOwner);
    }

    if let Some(request_server) = request_server {
        debug!(server_id = %request_server.id, selection_reason = "passthrough_request", "Selected backend");
        return Ok((request_server, None, true));
    }

    let server = state.server_storage.get_best_server().await?;
    let server = server.ok_or(RoutingError::UnavailableOwner)?;
    Ok((server, None, false))
}

pub(crate) async fn server_from_body_media_aggregate_ids(
    state: &MediaServices,
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
            compare_routes(
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
    state: &MediaServices,
    request: &reqwest::Request,
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<Server>> {
    state
        .url_selection
        .server_from_client_url(request.url(), access_scope)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::error::ApplicationError;
    use std::error::Error;

    #[tokio::test]
    async fn storage_failure_is_not_an_invalid_selection_or_a_missing_mapping() {
        let (state, pool, _, _) = crate::application::catalog::tests::setup().await;
        let unknown = "0123456789abcdef0123456789abcdef";
        assert!(state
            .url_selection
            .client_media_mapping(unknown, None, None)
            .await
            .unwrap()
            .is_none());
        pool.close().await;

        assert!(matches!(
            state
                .url_selection
                .client_media_mapping(unknown, None, None)
                .await,
            Err(sqlx::Error::PoolClosed)
        ));
        let request = reqwest::Request::new(
            reqwest::Method::GET,
            format!("http://proxy.example/Items/{unknown}")
                .parse()
                .unwrap(),
        );
        let error = resolve_server(&None, &None, &state.services, &request, None)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RoutingError::Storage(sqlx::Error::PoolClosed)
        ));
        let error = ApplicationError::from(error);
        assert!(matches!(error, ApplicationError::Storage(_)));
        assert!(error.source().unwrap().is::<sqlx::Error>());
    }

    #[tokio::test]
    async fn conflicting_selections_remain_invalid_input() {
        let (state, _, _, _) = crate::application::catalog::tests::setup().await;
        let request = reqwest::Request::new(
            reqwest::Method::GET,
            "http://proxy.example/Items?MediaSourceId=one&MediaSourceId=two"
                .parse()
                .unwrap(),
        );
        let error = resolve_server(&None, &None, &state.services, &request, None)
            .await
            .unwrap_err();
        assert!(matches!(error, RoutingError::InvalidSelection(_)));
    }
}
