use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::server_id::ServerId;

const PLAYBACK_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone)]
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
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);

        if let Some(index) = sessions.iter().position(|tracked| {
            tracked.session.session_id == session.session_id
                && tracked.session.item_id == session.item_id
        }) {
            sessions.remove(index);
        }

        sessions.push(TrackedPlaybackSession {
            session,
            updated_at: now,
        });
    }

    pub async fn get_session(&self, session_id: &str) -> Option<PlaybackSession> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);

        sessions
            .iter()
            .rev()
            .find(|tracked| tracked.session.session_id == session_id)
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_session_by_session_and_item_id(
        &self,
        session_id: &str,
        item_id: &str,
    ) -> Option<PlaybackSession> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);

        sessions
            .iter()
            .rev()
            .find(|tracked| {
                tracked.session.session_id == session_id && tracked.session.item_id == item_id
            })
            .map(|tracked| tracked.session.clone())
    }

    pub async fn get_sessions_by_item_id(&self, item_id: &str) -> Vec<PlaybackSession> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);

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

    pub async fn remove_sessions_for_server(&self, server_id: ServerId) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.server_id != server_id);
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

    #[tokio::test]
    async fn test_add_session_upserts_by_session_and_item_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "item-1".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(1),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "item-1".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(2),
            })
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
    async fn test_same_item_id_requires_matching_session_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "shared-item".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(1),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-2".to_string(),
                item_id: "shared-item".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(2),
            })
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
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "item-1".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(1),
            })
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage
            .get_session_by_session_and_item_id("session-1", "item-1")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn test_remove_sessions_for_server() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "item-1".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(1),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-2".to_string(),
                item_id: "item-2".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(2),
            })
            .await;

        storage.remove_sessions_for_server(ServerId::new(1)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage.get_session("session-2").await.is_some());
    }

    #[tokio::test]
    async fn test_get_sessions_by_item_id_returns_active_matches_newest_first() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "shared-item".to_string(),
                user_id: "user-1".to_string(),
                server_id: ServerId::new(1),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-2".to_string(),
                item_id: "shared-item".to_string(),
                user_id: "user-2".to_string(),
                server_id: ServerId::new(2),
            })
            .await;

        let sessions = storage.get_sessions_by_item_id("shared-item").await;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "session-2");
        assert_eq!(sessions[1].session_id, "session-1");
    }
}
