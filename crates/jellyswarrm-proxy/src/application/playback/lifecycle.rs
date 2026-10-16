//! Local playback report transitions. Planning never mutates session storage;
//! callers commit only after upstream success and response construction.
use crate::{
    processors::request_analyzer::{PlaybackSessionAction, RequestBodyAnalysisResult},
    server_id::ServerId,
    session_storage::{PlaybackSession, PreparedLiveStream, SessionRevision, SessionStorage},
};

/// The sole application owner of local binding writes. Call only at the
/// operation's documented commit point, never during target planning.
pub(crate) struct PlaybackLifecycle<'a> {
    pub sessions: &'a SessionStorage,
}

impl PlaybackLifecycle<'_> {
    pub async fn establish(&self, session: PlaybackSession) {
        self.sessions.add_session(session).await;
    }

    pub async fn alias(&self, item: &str, session: &str, viewer: &str, server: ServerId) {
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
    pub async fn direct_stream(
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

    pub async fn bind_original(&self, session: &str, viewer: &str, server: ServerId, item: &str) {
        self.sessions
            .bind_original_item(session, viewer, server, item)
            .await;
    }

    pub async fn commit_live(&self, plans: &[std::sync::Arc<PreparedLiveStream>]) -> bool {
        self.sessions.commit_live_streams(plans).await
    }

    #[cfg(test)]
    pub async fn register_live(&self, original: &str, viewer: &str, server: ServerId) -> String {
        self.sessions
            .register_live_stream(original, viewer, server)
            .await
    }

    pub async fn close_live(&self, id: &str, viewer: &str) {
        self.sessions.remove_live_stream(id, viewer).await;
    }

    pub async fn remove_server(&self, server: ServerId) {
        self.sessions.remove_sessions_for_server(server).await;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingPlaybackSessionUpdate {
    pub action: PlaybackSessionAction,
    pub session: PlaybackSession,
    pub revision: Option<SessionRevision>,
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
            PlaybackSessionAction::Start => Some(Self {
                action: PlaybackSessionAction::Start,
                revision: None,
                session: PlaybackSession {
                    session_id: result.requested_play_session_id.clone()?,
                    item_id: result.requested_play_item_id.clone()?,
                    user_id: user_id?.to_string(),
                    server_id,
                    original_item_id: original_item_id.map(str::to_string),
                },
            }),
            action @ (PlaybackSessionAction::Refresh | PlaybackSessionAction::Remove) => {
                Some(Self {
                    action,
                    session: result.authoritative_play_session.clone()?,
                    revision: Some(result.authoritative_play_session_revision?),
                })
            }
        }
    }

    /// Retains revision fencing: a delayed report cannot refresh or remove a
    /// replacement session. Missing revisions remain a no-op, never an upsert.
    pub(crate) async fn commit(self, sessions: &SessionStorage) {
        match self.action {
            PlaybackSessionAction::Start => {
                PlaybackLifecycle { sessions }.establish(self.session).await
            }
            PlaybackSessionAction::Refresh => {
                if let Some(revision) = self.revision {
                    let applied = sessions
                        .refresh_session_for_user(
                            &self.session.session_id,
                            &self.session.user_id,
                            revision,
                        )
                        .await;
                    tracing::debug!(action = "refresh", applied, "Playback report commit");
                }
            }
            PlaybackSessionAction::Remove => {
                if let Some(revision) = self.revision {
                    sessions
                        .remove_session_for_user(
                            &self.session.session_id,
                            &self.session.user_id,
                            revision,
                        )
                        .await;
                    tracing::debug!(action = "remove", "Revision-fenced playback report commit");
                }
            }
        }
    }
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
    async fn planned_report_cannot_remove_replacement_binding() {
        let storage = SessionStorage::new();
        storage.add_session(session("old")).await;
        let (old, revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
        let analysis = RequestBodyAnalysisResult {
            authoritative_play_session: Some(old),
            authoritative_play_session_revision: Some(revision),
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
            .unwrap();
        assert_eq!(current.item_id, "replacement");
        assert_ne!(current_revision, revision);
    }

    #[tokio::test]
    async fn planned_refresh_cannot_touch_replacement_revision() {
        let storage = SessionStorage::new();
        storage.add_session(session("old")).await;
        let (old, revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
        let pending = PendingPlaybackSessionUpdate {
            action: PlaybackSessionAction::Refresh,
            session: old,
            revision: Some(revision),
        };
        storage.add_session(session("replacement")).await;
        let (_, replacement_revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
        pending.commit(&storage).await;
        let (current, current_revision) = storage
            .resolve_report_session("player", "viewer")
            .await
            .unwrap()
            .unwrap();
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
            .unwrap();
        assert_eq!(current.original_item_id.as_deref(), Some("upstream-item"));
    }
}
