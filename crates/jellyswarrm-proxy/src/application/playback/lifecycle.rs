//! Local playback report transitions. Planning never mutates session storage;
//! callers commit only after upstream success and response construction.
use crate::{
    application::{details::sources::version_name, error::ApplicationError},
    domain::ids::ServerId,
    media::storage::{MediaStorageService, MediaVersionSourceObservation},
    models::MediaSource,
    playback::{PlaybackSession, PreparedLiveStream, RevisionedPlaybackSession, SessionStorage},
    processors::request_analyzer::{PlaybackSessionAction, RequestBodyAnalysisResult},
    servers::storage::Server,
};

/// The sole application owner of local binding writes. Call only at the
/// operation's documented commit point, never during target planning.
pub(crate) struct PlaybackLifecycle<'a> {
    sessions: &'a SessionStorage,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("live stream reservation was invalidated before publication")]
pub(crate) struct InvalidatedReservation;

impl<'a> PlaybackLifecycle<'a> {
    pub(crate) fn new(sessions: &'a SessionStorage) -> Self {
        Self { sessions }
    }

    pub(crate) async fn establish(&self, session: PlaybackSession) {
        self.sessions.add_session(session).await;
    }

    pub(crate) async fn alias(&self, item: &str, session: &str, viewer: &str, server: ServerId) {
        self.establish(PlaybackSession {
            session_id: session.into(),
            item_id: item.into(),
            user_id: viewer.into(),
            server_id: server,
            original_item_id: None,
        })
        .await;
    }

    /// Direct streams bind after an accepted response/redirect, before exposing
    /// it to the client. Stream consumption is not the commit condition.
    pub(crate) async fn direct_stream(
        &self,
        status: hyper::StatusCode,
        session: Option<&str>,
        item: Option<&str>,
        viewer: Option<&str>,
        server: ServerId,
        original_item: Option<String>,
    ) {
        if status.is_success() || status.is_redirection() {
            if let (Some(session), Some(item), Some(viewer)) = (session, item, viewer) {
                self.establish(PlaybackSession {
                    session_id: session.into(),
                    item_id: item.into(),
                    user_id: viewer.into(),
                    server_id: server,
                    original_item_id: original_item,
                })
                .await;
            }
        }
    }

    pub(crate) async fn bind_original(
        &self,
        session: &str,
        viewer: &str,
        server: ServerId,
        item: &str,
    ) {
        self.sessions
            .bind_original_item(session, viewer, server, item)
            .await;
    }

    pub(crate) async fn commit_live(
        &self,
        plans: &[std::sync::Arc<PreparedLiveStream>],
    ) -> Result<(), InvalidatedReservation> {
        if self.sessions.commit_live_streams(plans).await {
            Ok(())
        } else {
            Err(InvalidatedReservation)
        }
    }

    #[cfg(test)]
    pub(crate) async fn register_live(
        &self,
        original: &str,
        viewer: &str,
        server: ServerId,
    ) -> String {
        self.sessions
            .register_live_stream(original, viewer, server)
            .await
    }

    pub(crate) async fn close_live(
        &self,
        id: &crate::domain::ids::VirtualLiveStreamId,
        viewer: &crate::domain::ids::ViewerId,
    ) {
        self.sessions.close_live_binding(id, viewer).await;
    }

    pub(crate) async fn remove_server(&self, server: ServerId) {
        self.sessions.remove_sessions_for_server(server).await;
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PendingPlaybackSessionUpdate {
    Start(PlaybackSession),
    Refresh(RevisionedPlaybackSession),
    Remove(RevisionedPlaybackSession),
}

#[cfg(test)]
mod reservation_tests {
    use super::*;

    #[tokio::test]
    async fn live_commit_reports_invalidation_without_consuming_shared_reservations() {
        let storage = SessionStorage::new();
        let lifecycle = PlaybackLifecycle::new(&storage);
        let first = storage
            .prepare_live_stream("backend-live", "viewer", ServerId::new(1))
            .await;
        let shared = storage
            .prepare_live_stream("backend-live", "viewer", ServerId::new(1))
            .await;
        assert!(std::sync::Arc::ptr_eq(&first, &shared));
        assert_eq!(
            lifecycle.commit_live(std::slice::from_ref(&first)).await,
            Ok(())
        );
        assert_eq!(
            lifecycle.commit_live(std::slice::from_ref(&shared)).await,
            Ok(())
        );
        lifecycle
            .close_live(&first.virtual_id().into(), &"viewer".into())
            .await;
        assert_eq!(
            lifecycle.commit_live(&[shared]).await,
            Err(InvalidatedReservation)
        );
        assert!(storage
            .live_stream(first.virtual_id(), "viewer")
            .await
            .is_none());
    }
}

impl PendingPlaybackSessionUpdate {
    pub(crate) fn plan(
        action: Option<PlaybackSessionAction>,
        analysis: Option<&RequestBodyAnalysisResult>,
        user_id: Option<&str>,
        server_id: ServerId,
        original_item_id: Option<&str>,
    ) -> Option<Self> {
        let result = analysis?;
        match action? {
            PlaybackSessionAction::Start => Some(Self::Start(PlaybackSession {
                session_id: result.requested_play_session_id.clone()?,
                item_id: result.requested_play_item_id.clone()?,
                user_id: user_id?.into(),
                server_id,
                original_item_id: original_item_id.map(str::to_string),
            })),
            PlaybackSessionAction::Refresh => {
                Some(Self::Refresh(result.authoritative_play_session.clone()?))
            }
            PlaybackSessionAction::Remove => {
                Some(Self::Remove(result.authoritative_play_session.clone()?))
            }
        }
    }

    /// Retains revision fencing: a delayed report cannot refresh or remove a
    /// replacement session. Every refresh/remove carries a storage-issued revision.
    pub(crate) async fn commit(self, sessions: &SessionStorage) {
        match self {
            Self::Start(session) => PlaybackLifecycle::new(sessions).establish(session).await,
            Self::Refresh(binding) => {
                let applied = sessions.refresh_revisioned_session(&binding).await;
                tracing::debug!(action = "refresh", applied, "Playback report commit");
            }
            Self::Remove(binding) => {
                sessions.remove_revisioned_session(&binding).await;
                tracing::debug!(action = "remove", "Revision-fenced playback report commit");
            }
        }
    }
}

/// Record negotiated playback sources and retain revision-fenced routability.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session(item_id: &str) -> PlaybackSession {
        PlaybackSession {
            session_id: "player".into(),
            item_id: item_id.into(),
            user_id: "viewer".into(),
            server_id: ServerId::new(1),
            original_item_id: Some(format!("upstream-{item_id}")),
        }
    }

    #[tokio::test]
    async fn refresh_and_remove_require_a_revisioned_lookup_and_commit_current_binding() {
        let storage = SessionStorage::new();
        let mut analysis = RequestBodyAnalysisResult {
            requested_play_session_id: Some("player".into()),
            requested_play_item_id: Some("item".into()),
            ..Default::default()
        };
        for action in [
            PlaybackSessionAction::Refresh,
            PlaybackSessionAction::Remove,
        ] {
            assert!(PendingPlaybackSessionUpdate::plan(
                Some(action),
                Some(&analysis),
                Some("viewer"),
                ServerId::new(1),
                None
            )
            .is_none());
        }
        storage.add_session(session("item")).await;
        analysis.authoritative_play_session = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap();
        let before = analysis
            .authoritative_play_session
            .as_ref()
            .unwrap()
            .revision();
        let refresh = PendingPlaybackSessionUpdate::plan(
            Some(PlaybackSessionAction::Refresh),
            Some(&analysis),
            Some("viewer"),
            ServerId::new(1),
            None,
        )
        .unwrap();
        assert!(matches!(refresh, PendingPlaybackSessionUpdate::Refresh(_)));
        refresh.commit(&storage).await;
        assert_eq!(
            storage
                .resolve_report_session("player", "viewer")
                .await
                .unwrap()
                .unwrap()
                .revision(),
            before
        );
        let remove = PendingPlaybackSessionUpdate::plan(
            Some(PlaybackSessionAction::Remove),
            Some(&analysis),
            Some("viewer"),
            ServerId::new(1),
            None,
        )
        .unwrap();
        assert!(matches!(remove, PendingPlaybackSessionUpdate::Remove(_)));
        remove.commit(&storage).await;
        assert!(storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn planned_report_cannot_remove_replacement_binding() {
        let storage = SessionStorage::new();
        storage.add_session(session("old")).await;
        let old = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
        let revision = old.revision();
        let analysis = RequestBodyAnalysisResult {
            authoritative_play_session: Some(old),
            ..Default::default()
        };
        let pending = PendingPlaybackSessionUpdate::plan(
            Some(PlaybackSessionAction::Remove),
            Some(&analysis),
            Some("viewer"),
            ServerId::new(1),
            None,
        )
        .unwrap();

        storage.add_session(session("replacement")).await;
        pending.commit(&storage).await;

        let (current, current_revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(current.item_id, "replacement");
        assert_ne!(current_revision, revision);
    }

    #[tokio::test]
    async fn planned_refresh_cannot_touch_replacement_revision() {
        let storage = SessionStorage::new();
        storage.add_session(session("old")).await;
        let old = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
        let revision = old.revision();
        let pending = PendingPlaybackSessionUpdate::Refresh(old);
        storage.add_session(session("replacement")).await;
        let (_, replacement_revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        pending.commit(&storage).await;
        let (current, current_revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(current.item_id, "replacement");
        assert_eq!(current_revision, replacement_revision);
        assert_ne!(current_revision, revision);
    }

    #[tokio::test]
    async fn planning_start_does_not_publish_binding_before_commit() {
        let storage = SessionStorage::new();
        let analysis = RequestBodyAnalysisResult {
            requested_play_session_id: Some("player".into()),
            requested_play_item_id: Some("item".into()),
            ..Default::default()
        };
        let pending = PendingPlaybackSessionUpdate::plan(
            Some(PlaybackSessionAction::Start),
            Some(&analysis),
            Some("viewer"),
            ServerId::new(1),
            Some("upstream-item"),
        )
        .unwrap();
        assert!(storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .is_none());
        pending.commit(&storage).await;
        let (current, _) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(current.original_item_id.as_deref(), Some("upstream-item"));
    }
}
