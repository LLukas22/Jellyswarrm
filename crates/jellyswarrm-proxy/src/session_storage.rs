use std::time::{Duration, Instant};

use tokio::sync::{RwLock, RwLockWriteGuard};

use crate::server_id::ServerId;

const PLAYBACK_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRevision(uuid::Uuid);

#[derive(Debug, Clone)]
pub struct PlaybackSession {
    pub session_id: String, // Unique identifier for the session
    pub item_id: String,    // ID of the media item being played
    pub user_id: String,
    pub server_id: ServerId,
}

pub struct SessionStorage {
    sessions: RwLock<Vec<TrackedPlaybackSession>>,
    session_ttl: Duration,
}

struct TrackedPlaybackSession {
    session: PlaybackSession,
    revision: SessionRevision,
    updated_at: Instant,
}

impl Default for SessionStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStorage {
    pub fn new() -> Self {
        Self::with_session_ttl(PLAYBACK_SESSION_TTL)
    }

    pub fn with_session_ttl(session_ttl: Duration) -> Self {
        SessionStorage {
            sessions: RwLock::new(Vec::new()),
            session_ttl,
        }
    }

    pub async fn add_session(&self, session: PlaybackSession) {
        let mut sessions = self.live_sessions().await;
        let revision = SessionRevision(uuid::Uuid::new_v4());

        sessions.retain(|tracked| {
            let same_authority = tracked.session.session_id == session.session_id
                && tracked.session.user_id == session.user_id;
            !same_authority
                || (tracked.session.server_id == session.server_id
                    && tracked.session.item_id != session.item_id)
        });

        // Retained item aliases share one binding revision, even on identical upserts.
        for tracked in sessions.iter_mut().filter(|tracked| {
            tracked.session.session_id == session.session_id
                && tracked.session.user_id == session.user_id
        }) {
            tracked.revision = revision;
        }
        sessions.push(TrackedPlaybackSession {
            session,
            revision,
            updated_at: Instant::now(),
        });
    }

    pub async fn get_session(&self, session_id: &str) -> Option<PlaybackSession> {
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| tracked.session.session_id == session_id)
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Option<PlaybackSession> {
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| {
                tracked.session.session_id == session_id && tracked.session.user_id == user_id
            })
            .map(|tracked| tracked.session.clone())
    }

    pub async fn resolve_report_session(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> anyhow::Result<Option<(PlaybackSession, SessionRevision)>> {
        let sessions = self.live_sessions().await;
        let matching = sessions
            .iter()
            .rev()
            .filter(|tracked| tracked.session.session_id == session_id);
        if let Some(tracked) = matching
            .clone()
            .find(|tracked| tracked.session.user_id == user_id)
        {
            return Ok(Some((tracked.session.clone(), tracked.revision)));
        }
        if matching.count() != 0 {
            anyhow::bail!("playback session does not belong to the user");
        }
        Ok(None)
    }

    pub async fn refresh_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
        revision: SessionRevision,
    ) -> bool {
        let mut sessions = self.live_sessions().await;
        let now = Instant::now();
        let mut refreshed = false;
        for tracked in sessions.iter_mut().filter(|tracked| {
            tracked.session.session_id == session_id
                && tracked.session.user_id == user_id
                && tracked.revision == revision
        }) {
            tracked.updated_at = now;
            refreshed = true;
        }
        refreshed
    }

    pub async fn get_session_by_session_and_item_id(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Option<PlaybackSession> {
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .find(|tracked| {
                tracked.session.session_id == session_id && tracked.session.item_id == item_id
            })
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_sessions_by_item_id(&self, item_id: &str) -> Vec<PlaybackSession> {
        let sessions = self.live_sessions().await;

        sessions
            .iter()
            .rev()
            .filter(|tracked| tracked.session.item_id == item_id)
            .map(|tracked| tracked.session.clone())
            .collect()
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.session_id != session_id);
    }

    pub async fn remove_session_for_user(
        &self,
        session_id: &str,
        user_id: &str,
        revision: SessionRevision,
    ) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| {
            tracked.session.session_id != session_id
                || tracked.session.user_id != user_id
                || tracked.revision != revision
        });
    }

    pub async fn remove_sessions_for_server(&self, server_id: ServerId) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.server_id != server_id);
    }

    async fn live_sessions(&self) -> RwLockWriteGuard<'_, Vec<TrackedPlaybackSession>> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);
        sessions
    }

    fn prune_stale_sessions(
        sessions: &mut Vec<TrackedPlaybackSession>,
        session_ttl: Duration,
        now: Instant,
    ) {
        sessions.retain(|tracked| now.duration_since(tracked.updated_at) <= session_ttl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playback_session(
        session_id: &str,
        item_id: &str,
        user_id: &str,
        server_id: i64,
    ) -> PlaybackSession {
        PlaybackSession {
            session_id: session_id.into(),
            item_id: item_id.into(),
            user_id: user_id.into(),
            server_id: ServerId::new(server_id),
        }
    }

    #[tokio::test]
    async fn stale_reports_leave_replacement_bindings_and_aliases_unchanged() {
        for (case, server_id, item_id) in [
            ("identical upsert", ServerId::new(1), "item"),
            (
                "same-server item alias",
                ServerId::new(1),
                "replacement-item",
            ),
            ("server replacement", ServerId::new(2), "item"),
            (
                "server and item replacement",
                ServerId::new(2),
                "replacement-item",
            ),
        ] {
            let storage = SessionStorage::new();
            storage
                .add_session(playback_session("session", "item", "user", 1))
                .await;
            let (_, old_revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap();
            storage
                .add_session(playback_session(
                    "session",
                    item_id,
                    "user",
                    server_id.as_i64(),
                ))
                .await;
            assert!(
                !storage
                    .refresh_session_for_user("session", "user", old_revision)
                    .await,
                "{case}: stale refresh before alias"
            );
            storage
                .remove_session_for_user("session", "user", old_revision)
                .await;
            let (replacement, replacement_revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap();
            assert_ne!(old_revision, replacement_revision, "{case}");
            assert_eq!(replacement.server_id, server_id, "{case}");
            assert_eq!(replacement.item_id, item_id, "{case}");
            storage
                .add_session(playback_session(
                    "session",
                    "alias",
                    "user",
                    server_id.as_i64(),
                ))
                .await;
            let (_, revision) = storage
                .resolve_report_session("session", "user")
                .await
                .unwrap()
                .unwrap();
            assert_ne!(old_revision, revision, "{case}");
            let before: Vec<_> = storage
                .sessions
                .read()
                .await
                .iter()
                .map(|tracked| tracked.updated_at)
                .collect();

            assert!(
                !storage
                    .refresh_session_for_user("session", "user", old_revision)
                    .await,
                "{case}: stale refresh after alias"
            );
            storage
                .remove_session_for_user("session", "user", old_revision)
                .await;
            let sessions = storage.sessions.read().await;
            assert_eq!(
                sessions
                    .iter()
                    .map(|tracked| tracked.updated_at)
                    .collect::<Vec<_>>(),
                before,
                "{case}: stale reports must preserve timestamps"
            );
            assert!(
                sessions.iter().all(|tracked| {
                    tracked.revision == revision && tracked.session.server_id == server_id
                }),
                "{case}: alias authority"
            );
            assert!(
                sessions
                    .iter()
                    .any(|tracked| tracked.session.item_id == item_id),
                "{case}: replacement retained"
            );
            assert!(
                sessions
                    .iter()
                    .any(|tracked| tracked.session.item_id == "alias"),
                "{case}: alias retained"
            );
            drop(sessions);

            assert!(
                storage
                    .refresh_session_for_user("session", "user", revision)
                    .await,
                "{case}: current refresh"
            );
            let sessions = storage.sessions.read().await;
            assert!(
                sessions.iter().all(|tracked| {
                    tracked.updated_at == sessions[0].updated_at && tracked.revision == revision
                }),
                "{case}: aliases refreshed together"
            );
            drop(sessions);
            storage
                .remove_session_for_user("session", "user", revision)
                .await;
            assert!(
                storage.sessions.read().await.is_empty(),
                "{case}: current removal"
            );
        }
    }

    #[tokio::test]
    async fn report_lookup_distinguishes_unknown_and_other_user_sessions() {
        let storage = SessionStorage::new();
        assert!(storage
            .resolve_report_session("session", "caller")
            .await
            .unwrap()
            .is_none());
        storage
            .add_session(playback_session("session", "item", "owner", 1))
            .await;
        assert!(storage
            .resolve_report_session("session", "caller")
            .await
            .is_err());
        assert!(storage
            .resolve_report_session("session", "owner")
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn test_add_session_upserts_by_session_and_item_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 2))
            .await;

        let session = storage.get_session("session-1").await.unwrap();
        assert_eq!(session.server_id, ServerId::new(2));
        assert_eq!(
            storage
                .get_session_by_session_and_item_id("session-1", "item-1")
                .await
                .unwrap()
                .server_id,
            ServerId::new(2)
        );
    }

    #[tokio::test]
    async fn reused_user_session_id_has_one_server_authority() {
        let storage = SessionStorage::new();
        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-1", "item-2", "user-1", 2))
            .await;

        assert!(storage
            .get_session_by_session_and_item_id("session-1", "item-1")
            .await
            .is_none());
        assert_eq!(
            storage
                .get_session_for_user("session-1", "user-1")
                .await
                .unwrap()
                .server_id,
            ServerId::new(2)
        );
    }

    #[tokio::test]
    async fn test_same_item_id_requires_matching_session_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "shared-item", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "shared-item", "user-1", 2))
            .await;

        let session = storage
            .get_session_by_session_and_item_id("session-1", "shared-item")
            .await
            .unwrap();
        assert_eq!(session.server_id, ServerId::new(1));

        let session = storage
            .get_session_by_session_and_item_id("session-2", "shared-item")
            .await
            .unwrap();
        assert_eq!(session.server_id, ServerId::new(2));

        assert!(storage
            .get_session_by_session_and_item_id("missing-session", "shared-item")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_stale_sessions_expire() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage
            .get_session_by_session_and_item_id("session-1", "item-1")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_add_session_prunes_stale_sessions() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));

        storage
            .add_session(playback_session("stale-session", "stale-item", "user-1", 1))
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        storage
            .add_session(playback_session("fresh-session", "fresh-item", "user-1", 2))
            .await;

        assert!(storage.get_session("stale-session").await.is_none());
        assert!(storage.get_session("fresh-session").await.is_some());
    }

    #[tokio::test]
    async fn test_remove_sessions_for_server() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "item-1", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "item-2", "user-1", 2))
            .await;

        storage.remove_sessions_for_server(ServerId::new(1)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage.get_session("session-2").await.is_some());
    }

    #[tokio::test]
    async fn test_get_sessions_by_item_id_returns_active_matches_newest_first() {
        let storage = SessionStorage::new();

        storage
            .add_session(playback_session("session-1", "shared-item", "user-1", 1))
            .await;
        storage
            .add_session(playback_session("session-2", "shared-item", "user-2", 2))
            .await;

        let sessions = storage.get_sessions_by_item_id("shared-item").await;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "session-2");
        assert_eq!(sessions[1].session_id, "session-1");
    }
}
