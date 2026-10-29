//! Test helpers for target-bound wire preparation and playback-report routing.
use super::*;

#[cfg(test)]
pub(crate) async fn apply_to_request(
    request: &mut reqwest::Request,
    server: &Server,
    session: &Option<AuthorizationSession>,
    auth: &Option<JellyfinAuthorization>,
    state: &MediaState,
    access_scope: Option<&VirtualLibraryAccessScope>,
) {
    let prefix = state.paths.url_prefix();
    crate::application::request::UpstreamRequestBuilder::new(
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
    state: &MediaState,
) -> Result<(Server, Option<AuthorizationSession>, bool)> {
    crate::application::playback::reports::resolve_server(sessions, analysis, &state.media_storage)
        .await
        .map(|target| (target.server, Some(target.session), true))
}
