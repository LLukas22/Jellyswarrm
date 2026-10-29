//! Recognize playback reports and retain test helpers for backend-specific playback routing.
use super::*;
pub(crate) async fn playback_session_action(
    method: &http::Method,
    request_path: &str,
    state: &AppState,
) -> Option<PlaybackSessionAction> {
    let path = state.remove_prefix_from_path(request_path).await;
    crate::processors::request_analyzer::playback_session_action(method, path)
}

#[cfg(test)]
pub(crate) async fn apply_to_request(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
    access_scope: Option<&VirtualLibraryAccessScope>,
) {
    let prefix = state.get_url_prefix().await;
    crate::upstream::jellyfin::request::UpstreamRequestBuilder::new(
        &state.processors.url_processor,
        prefix.as_deref(),
    )
    .apply(
        request,
        server,
        session.as_ref(),
        auth.as_ref(),
        access_scope,
    )
    .await
    .unwrap();
}

// Reports must not use the generic best-server fallback: a wrong destination can
// update another copy's playback state. Only top-level item/source fields route them.
#[cfg(test)]
pub(super) async fn resolve_playback_report_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    analysis: &RequestBodyAnalysisResult,
    state: &AppState,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    crate::application::playback::reports::resolve_server(sessions, analysis, &state.media_storage)
        .await
        .map(|target| (target.server, Some(target.session), true))
}
