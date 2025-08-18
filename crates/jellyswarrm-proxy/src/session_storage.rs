use tokio::sync::RwLock;

use crate::server_storage::Server;

#[derive(Clone)]
pub struct PlaybackSession {
    pub session_id: String, // Unique identifier for the session
    pub item_id: String,    // ID of the media item being played
    pub server: Server,
}

pub struct SessionStorage {
    pub sessions: RwLock<Vec<PlaybackSession>>,
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
        }
    }

    pub async fn add_session(&self, session: PlaybackSession) {
        let mut sessions = self.sessions.write().await;
        sessions.push(session);
    }

    pub async fn get_session(&self, session_id: &str) -> Option<PlaybackSession> {
        let sessions = self.sessions.read().await;
        sessions
            .iter()
            .find(|s| s.session_id == session_id)
            .cloned()
    }

    pub async fn get_session_by_item_id(&self, item_id: &str) -> Option<PlaybackSession> {
        let sessions = self.sessions.read().await;
        sessions.iter().find(|s| s.item_id == item_id).cloned()
    }

    pub async fn remove_session(&self, session_id: &str) {
        let mut sessions = self.sessions.write().await;
        sessions.retain(|s| s.session_id != session_id);
    }
}
