use std::time::{Duration, Instant};

use tokio::sync::RwLock;

use crate::server_storage::Server;

const PLAYBACK_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

#[derive(Clone)]
pub struct PlaybackSession {
    pub session_id: String, // Unique identifier for the session
    pub item_id: String,    // ID of the media item being played
    pub server: Server,
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

        if let Some(index) = sessions
            .iter()
            .position(|tracked| tracked.session.session_id == session.session_id)
        {
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

    pub async fn get_session_by_item_id(&self, item_id: &str) -> Option<PlaybackSession> {
        let now = Instant::now();
        let mut sessions = self.sessions.write().await;
        Self::prune_stale_sessions(&mut sessions, self.session_ttl, now);

        sessions
            .iter()
            .rev()
            .find(|tracked| tracked.session.item_id == item_id)
            .map(|tracked| tracked.session.clone())
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|tracked| tracked.session.session_id != session_id);
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
    use crate::{config::MediaStreamingMode, server_id::ServerId, server_url::ServerUrl};

    fn test_server(id: i64, name: &str) -> Server {
        let now = chrono::Utc::now();
        Server {
            id: ServerId::new(id),
            name: name.to_string(),
            url: ServerUrl::parse(&format!("http://server{id}.example")).unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn test_add_session_upserts_by_session_id() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "old-item".to_string(),
                server: test_server(1, "old"),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "new-item".to_string(),
                server: test_server(2, "new"),
            })
            .await;

        let session = storage.get_session("session-1").await.unwrap();
        assert_eq!(session.server.id, ServerId::new(2));
        assert!(storage.get_session_by_item_id("old-item").await.is_none());
        assert_eq!(
            storage
                .get_session_by_item_id("new-item")
                .await
                .unwrap()
                .server
                .id,
            ServerId::new(2)
        );
    }

    #[tokio::test]
    async fn test_get_session_by_item_id_prefers_newest_match() {
        let storage = SessionStorage::new();

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "shared-item".to_string(),
                server: test_server(1, "old"),
            })
            .await;
        storage
            .add_session(PlaybackSession {
                session_id: "session-2".to_string(),
                item_id: "shared-item".to_string(),
                server: test_server(2, "new"),
            })
            .await;

        let session = storage.get_session_by_item_id("shared-item").await.unwrap();
        assert_eq!(session.server.id, ServerId::new(2));
    }

    #[tokio::test]
    async fn test_stale_sessions_expire() {
        let storage = SessionStorage::with_session_ttl(Duration::from_millis(1));

        storage
            .add_session(PlaybackSession {
                session_id: "session-1".to_string(),
                item_id: "item-1".to_string(),
                server: test_server(1, "server"),
            })
            .await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(storage.get_session("session-1").await.is_none());
        assert!(storage.get_session_by_item_id("item-1").await.is_none());
    }
}
