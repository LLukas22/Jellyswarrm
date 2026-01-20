use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::debug;

use crate::server_storage::Server;

/// Default session TTL: 4 hours
const DEFAULT_SESSION_TTL: Duration = Duration::from_secs(4 * 60 * 60);

/// Cleanup interval: run cleanup every 5 minutes
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone)]
pub struct PlaybackSession {
    pub session_id: String,
    pub item_id: String,
    pub server: Server,
    /// When this session was created
    created_at: Instant,
    /// When this session was last accessed
    last_accessed: Instant,
    /// Time-to-live for this session
    ttl: Duration,
}

impl PlaybackSession {
    pub fn new(session_id: String, item_id: String, server: Server) -> Self {
        let now = Instant::now();
        Self {
            session_id,
            item_id,
            server,
            created_at: now,
            last_accessed: now,
            ttl: DEFAULT_SESSION_TTL,
        }
    }

    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Check if this session has expired
    pub fn is_expired(&self) -> bool {
        self.last_accessed.elapsed() > self.ttl
    }

    /// Touch the session to update last_accessed time
    pub fn touch(&mut self) {
        self.last_accessed = Instant::now();
    }

    /// Get the age of this session
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }
}

pub struct SessionStorage {
    sessions: RwLock<Vec<PlaybackSession>>,
    last_cleanup: RwLock<Instant>,
}

impl Default for SessionStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStorage {
    pub fn new() -> Self {
        SessionStorage {
            sessions: RwLock::new(Vec::new()),
            last_cleanup: RwLock::new(Instant::now()),
        }
    }

    /// Add a new session
    pub async fn add_session(&self, session: PlaybackSession) {
        // Run cleanup if needed before adding
        self.maybe_cleanup().await;

        let mut sessions = self.sessions.write().await;
        // Remove any existing session with the same ID to avoid duplicates
        sessions.retain(|s| s.session_id != session.session_id);
        sessions.push(session);
        debug!("Added session, total sessions: {}", sessions.len());
    }

    /// Get a session by session ID, touching it to extend its lifetime
    pub async fn get_session(&self, session_id: &str) -> Option<PlaybackSession> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.iter_mut().find(|s| s.session_id == session_id) {
            if session.is_expired() {
                return None;
            }
            session.touch();
            Some(session.clone())
        } else {
            None
        }
    }

    /// Get a session by item ID, touching it to extend its lifetime
    pub async fn get_session_by_item_id(&self, item_id: &str) -> Option<PlaybackSession> {
        let mut sessions = self.sessions.write().await;
        if let Some(session) = sessions.iter_mut().find(|s| s.item_id == item_id) {
            if session.is_expired() {
                return None;
            }
            session.touch();
            Some(session.clone())
        } else {
            None
        }
    }

    /// Remove a session by ID
    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|s| s.session_id != session_id);
        let removed = before - sessions.len();
        if removed > 0 {
            debug!("Removed {} session(s), total sessions: {}", removed, sessions.len());
        }
    }

    /// Get the current number of sessions (including expired ones not yet cleaned)
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }

    /// Get the number of active (non-expired) sessions
    pub async fn active_session_count(&self) -> usize {
        self.sessions
            .read()
            .await
            .iter()
            .filter(|s| !s.is_expired())
            .count()
    }

    /// Force cleanup of expired sessions
    pub async fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let before = sessions.len();
        sessions.retain(|s| !s.is_expired());
        let removed = before - sessions.len();
        if removed > 0 {
            debug!(
                "Cleaned up {} expired sessions, {} remaining",
                removed,
                sessions.len()
            );
        }
        removed
    }

    /// Run cleanup if enough time has passed since last cleanup
    async fn maybe_cleanup(&self) {
        let should_cleanup = {
            let last = self.last_cleanup.read().await;
            last.elapsed() > CLEANUP_INTERVAL
        };

        if should_cleanup {
            self.cleanup_expired().await;
            let mut last = self.last_cleanup.write().await;
            *last = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn make_test_server() -> Server {
        Server {
            id: 1,
            name: "Test Server".to_string(),
            url: Url::parse("http://localhost:8096").unwrap(),
            priority: 1,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn test_add_and_get_session() {
        let storage = SessionStorage::new();
        let session = PlaybackSession::new(
            "session1".to_string(),
            "item1".to_string(),
            make_test_server(),
        );

        storage.add_session(session).await;

        let retrieved = storage.get_session("session1").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().item_id, "item1");
    }

    #[tokio::test]
    async fn test_get_session_by_item_id() {
        let storage = SessionStorage::new();
        let session = PlaybackSession::new(
            "session1".to_string(),
            "item1".to_string(),
            make_test_server(),
        );

        storage.add_session(session).await;

        let retrieved = storage.get_session_by_item_id("item1").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().session_id, "session1");
    }

    #[tokio::test]
    async fn test_remove_session() {
        let storage = SessionStorage::new();
        let session = PlaybackSession::new(
            "session1".to_string(),
            "item1".to_string(),
            make_test_server(),
        );

        storage.add_session(session).await;
        assert_eq!(storage.session_count().await, 1);

        storage.remove_session("session1").await;
        assert_eq!(storage.session_count().await, 0);
    }

    #[tokio::test]
    async fn test_expired_session_not_returned() {
        let storage = SessionStorage::new();
        // Create a session with a very short TTL
        let session = PlaybackSession::new(
            "session1".to_string(),
            "item1".to_string(),
            make_test_server(),
        )
        .with_ttl(Duration::from_millis(1));

        storage.add_session(session).await;

        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(10)).await;

        let retrieved = storage.get_session("session1").await;
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_expired() {
        let storage = SessionStorage::new();

        // Add an expired session
        let expired = PlaybackSession::new(
            "expired".to_string(),
            "item1".to_string(),
            make_test_server(),
        )
        .with_ttl(Duration::from_millis(1));

        // Add a valid session
        let valid = PlaybackSession::new(
            "valid".to_string(),
            "item2".to_string(),
            make_test_server(),
        )
        .with_ttl(Duration::from_secs(3600));

        storage.add_session(expired).await;
        storage.add_session(valid).await;

        // Wait for first session to expire
        tokio::time::sleep(Duration::from_millis(10)).await;

        let removed = storage.cleanup_expired().await;
        assert_eq!(removed, 1);
        assert_eq!(storage.session_count().await, 1);

        // The valid session should still be there
        let retrieved = storage.get_session("valid").await;
        assert!(retrieved.is_some());
    }

    #[tokio::test]
    async fn test_duplicate_session_id_replaces() {
        let storage = SessionStorage::new();

        let session1 = PlaybackSession::new(
            "session1".to_string(),
            "item1".to_string(),
            make_test_server(),
        );

        let session2 = PlaybackSession::new(
            "session1".to_string(),
            "item2".to_string(),
            make_test_server(),
        );

        storage.add_session(session1).await;
        storage.add_session(session2).await;

        assert_eq!(storage.session_count().await, 1);

        let retrieved = storage.get_session("session1").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().item_id, "item2");
    }
}
