//! Playback report and transcoder cancellation authority. These routes must not
//! fall back to another backend when their selected owner is unavailable.

use super::target::MediaTargetResolver;
use anyhow::{anyhow, Result};

use crate::{
    media_storage_service::{MediaMapping, MediaStorageService},
    processors::request_analyzer::RequestBodyAnalysisResult,
    server_storage::Server,
    session_storage::SessionStorage,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::VirtualLibraryAccessScope,
};

#[derive(Debug, thiserror::Error)]
#[error("no authorized backend is currently available")]
pub(crate) struct NoAvailableBackend;

#[derive(Debug)]
pub(crate) struct ResolvedReportTarget {
    pub server: Server,
    pub session: AuthorizationSession,
}

// Reports must not use the generic best-server fallback: a wrong destination can
// update another copy's playback state. Only top-level item/source fields route them.
pub(crate) async fn resolve_server(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    analysis: &RequestBodyAnalysisResult,
    media_storage: &MediaStorageService,
) -> Result<ResolvedReportTarget> {
    let sessions = sessions
        .as_ref()
        .ok_or_else(|| anyhow!("no authorization sessions available"))?;
    let item_id = analysis
        .requested_play_item_id
        .as_deref()
        .ok_or_else(|| anyhow!("playback report requires an item"))?;
    let resolver = MediaTargetResolver {
        media: media_storage,
    };
    let mut candidates = resolver
        .owners(
            item_id,
            analysis.requested_play_source_id.as_deref(),
            analysis
                .authoritative_play_session
                .as_ref()
                .map(|binding| binding.session()),
        )
        .await?;
    candidates.retain(|id| sessions.iter().any(|(_, server)| server.id == *id));
    candidates.sort_by_key(|id| id.as_i64());
    candidates.dedup();
    if candidates.len() != 1 {
        return Err(anyhow!(
            "playback item/source route is unavailable or ambiguous"
        ));
    }
    let (session, server) = sessions
        .iter()
        .find(|(_, server)| server.id == candidates[0])
        .ok_or_else(|| anyhow!("playback server is unavailable"))?;
    Ok(ResolvedReportTarget {
        server: server.clone(),
        session: session.clone(),
    })
}

pub(crate) async fn resolve_member(
    analysis: &RequestBodyAnalysisResult,
    media_storage: &MediaStorageService,
    server: &Server,
) -> Result<MediaMapping> {
    let item_id = analysis
        .requested_play_item_id
        .as_deref()
        .ok_or_else(|| anyhow!("playback report requires an item"))?;
    Ok(MediaTargetResolver {
        media: media_storage,
    }
    .member(
        item_id,
        analysis.requested_play_source_id.as_deref(),
        analysis
            .authoritative_play_session
            .as_ref()
            .map(|binding| binding.session()),
        server.id,
    )
    .await?)
}

/// `path` is the decoded, prefix-stripped path supplied by the HTTP adapter.
/// Unrelated requests return `None`; invalid cancellation requests remain errors.
pub(crate) async fn resolve_cancellation(
    sessions: &Option<Vec<(AuthorizationSession, Server)>>,
    play_sessions: &SessionStorage,
    request: &reqwest::Request,
    path: &[String],
    access_scope: Option<&VirtualLibraryAccessScope>,
) -> Result<Option<ResolvedReportTarget>> {
    if request.method() != reqwest::Method::DELETE
        || !matches!(path, [videos, action] if videos.eq_ignore_ascii_case("Videos") && action.eq_ignore_ascii_case("ActiveEncodings"))
    {
        return Ok(None);
    }
    let scope =
        access_scope.ok_or_else(|| anyhow!("transcoder cancellation requires authentication"))?;
    let session_id = crate::processors::url_processor::play_session_id(request.url())?
        .ok_or_else(|| anyhow!("transcoder cancellation requires a playback session ID"))?;
    // Jellyfin only recognizes PlaySessionId on this endpoint. Accepting
    // SessionId alone would turn a targeted stop into a device-wide stop.
    if !request
        .url()
        .query_pairs()
        .any(|(key, value)| key.eq_ignore_ascii_case("PlaySessionId") && !value.is_empty())
    {
        return Err(anyhow!("transcoder cancellation requires PlaySessionId"));
    }
    let binding = super::target::owned_session(
        play_sessions,
        &session_id.as_str().into(),
        &scope.user_id().into(),
    )
    .await?
    .ok_or_else(|| anyhow!("unknown transcoder playback session"))?;
    let play_session = binding.session();
    if !scope.allows(play_session.server_id) {
        return Err(anyhow!("playback session server is no longer authorized"));
    }
    let (session, server) = sessions
        .as_ref()
        .and_then(|sessions| {
            sessions
                .iter()
                .find(|(_, server)| server.id == play_session.server_id)
        })
        .ok_or(NoAvailableBackend)?;
    Ok(Some(ResolvedReportTarget {
        server: server.clone(),
        session: session.clone(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{MediaStreamingMode, MIGRATOR},
        server_id::ServerId,
        server_storage::ServerStorageService,
        session_storage::PlaybackSession,
        user_authorization_service::Device,
    };

    fn binding(server_id: ServerId, original_item_id: &str) -> PlaybackSession {
        PlaybackSession {
            session_id: "player".into(),
            item_id: "virtual-item".into(),
            user_id: "viewer".into(),
            server_id,
            original_item_id: Some(original_item_id.into()),
        }
    }

    fn authorization(server: &Server) -> AuthorizationSession {
        let now = chrono::Utc::now();
        AuthorizationSession {
            id: server.id.as_i64(),
            user_id: "viewer".into(),
            mapping_id: server.id.as_i64(),
            server_url: server.url.to_string(),
            device: Device::from_useragent("Test"),
            jellyfin_token: "token".into(),
            original_user_id: "upstream-viewer".into(),
            expires_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn report_member_rejects_session_member_conflict_and_unavailable_owner() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool);
        let id = servers
            .add_server(
                "owner",
                "http://owner.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = servers.get_server_by_id(id).await.unwrap().unwrap();
        let item = media
            .get_or_create_media_mapping("item", &server)
            .await
            .unwrap();
        let mut analysis = RequestBodyAnalysisResult {
            requested_play_item_id: Some(item.virtual_media_id.clone()),
            authoritative_play_session: Some(binding(id, "different-member").into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_member(&analysis, &media, &server)
                .await
                .unwrap_err()
                .to_string(),
            "conflicting playback session and item/source authority"
        );
        analysis.authoritative_play_session = Some(binding(id, "item").into());
        assert_eq!(
            resolve_member(&analysis, &media, &server).await.unwrap().id,
            item.id
        );
        assert_eq!(
            resolve_server(&Some(vec![]), &analysis, &media)
                .await
                .unwrap_err()
                .to_string(),
            "playback item/source route is unavailable or ambiguous"
        );
        let sessions = Some(vec![(authorization(&server), server.clone())]);
        assert_eq!(
            resolve_server(&sessions, &analysis, &media)
                .await
                .unwrap()
                .server
                .id,
            id
        );
        let other_id = servers
            .add_server(
                "other",
                "http://other.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let other = servers.get_server_by_id(other_id).await.unwrap().unwrap();
        let source = media
            .get_or_create_media_mapping("source", &other)
            .await
            .unwrap();
        analysis.requested_play_source_id = Some(source.virtual_media_id);
        assert_eq!(
            resolve_server(&sessions, &analysis, &media)
                .await
                .unwrap_err()
                .to_string(),
            "conflicting playback item and source servers"
        );
    }

    #[tokio::test]
    async fn cancellation_preserves_validation_and_unavailable_owner_errors() {
        let storage = SessionStorage::new();
        let id = ServerId::new(1);
        storage.add_session(binding(id, "item")).await;
        let scope = VirtualLibraryAccessScope::new("viewer", [id]);
        let path = vec!["Videos".into(), "ActiveEncodings".into()];
        for (query, expected) in [
            ("", "transcoder cancellation requires a playback session ID"),
            (
                "SessionId=player",
                "transcoder cancellation requires PlaySessionId",
            ),
            (
                "PlaySessionId=unknown",
                "unknown transcoder playback session",
            ),
        ] {
            let request = reqwest::Request::new(
                reqwest::Method::DELETE,
                format!("http://proxy/Videos/ActiveEncodings?{query}")
                    .parse()
                    .unwrap(),
            );
            assert_eq!(
                resolve_cancellation(&None, &storage, &request, &path, Some(&scope))
                    .await
                    .unwrap_err()
                    .to_string(),
                expected
            );
        }
        let request = reqwest::Request::new(
            reqwest::Method::DELETE,
            "http://proxy/Videos/ActiveEncodings?PlaySessionId=player"
                .parse()
                .unwrap(),
        );
        let error = resolve_cancellation(&None, &storage, &request, &path, Some(&scope))
            .await
            .unwrap_err();
        assert!(error.is::<NoAvailableBackend>());
        let foreign = VirtualLibraryAccessScope::new("other-viewer", [id]);
        assert!(
            resolve_cancellation(&None, &storage, &request, &path, Some(&foreign))
                .await
                .is_err()
        );
        assert!(
            resolve_cancellation(&None, &storage, &request, &[], Some(&scope))
                .await
                .unwrap()
                .is_none()
        );
        assert!(storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .is_some());
    }
}
