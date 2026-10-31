//! Shared bounded WebSocket delivery for client sessions and SyncPlay.
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use uuid::Uuid;

pub(crate) const OUTBOUND_CAPACITY: usize = 64;

#[derive(thiserror::Error)]
pub(crate) enum SendError {
    #[error("session message encoding failed")]
    Encoding(#[source] serde_json::Error),
    #[error("session connection is unavailable")]
    Disconnected,
    #[error("session delivery queue is full")]
    Backpressure,
}

impl std::fmt::Debug for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Default)]
pub(crate) struct ConnectionHub(Arc<Mutex<HashMap<String, Connection>>>);
struct Connection {
    generation: Uuid,
    sender: mpsc::Sender<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Envelope<'a> {
    data: &'a serde_json::Value,
    message_id: String,
    message_type: &'a str,
}

impl ConnectionHub {
    pub(crate) fn register(&self, id: String, sender: mpsc::Sender<String>) -> Uuid {
        let generation = Uuid::new_v4();
        self.0
            .lock()
            .unwrap()
            .insert(id, Connection { generation, sender });
        generation
    }

    pub(crate) fn unregister(&self, id: &str, generation: Uuid) -> bool {
        let mut connections = self.0.lock().unwrap();
        if connections
            .get(id)
            .is_some_and(|c| c.generation == generation)
        {
            connections.remove(id);
            true
        } else {
            false
        }
    }

    pub(crate) fn remove(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }

    pub(crate) fn is_current(&self, id: &str, generation: Uuid) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|c| c.generation == generation && !c.sender.is_closed())
    }

    pub(crate) fn is_connected(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|c| !c.sender.is_closed())
    }

    pub(crate) fn send<T: Serialize>(
        &self,
        id: &str,
        message_type: &str,
        data: &T,
    ) -> Result<(), SendError> {
        // Keep the existing Value normalization (including object-key ordering),
        // but propagate serialization failures instead of panicking inside json!.
        let data = serde_json::to_value(data).map_err(SendError::Encoding)?;
        let text = serde_json::to_string(&Envelope {
            data: &data,
            message_id: Uuid::new_v4().simple().to_string(),
            message_type,
        })
        .map_err(SendError::Encoding)?;
        let mut connections = self.0.lock().unwrap();
        let connection = connections.get(id).ok_or(SendError::Disconnected)?;
        match connection.sender.try_send(text) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(SendError::Backpressure),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                connections.remove(id);
                Err(SendError::Disconnected)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encoding_failure_retains_its_cause_without_rendering_private_data() {
        struct Unserializable;
        impl Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("private message data"))
            }
        }
        let error = ConnectionHub::default()
            .send("session", "Play", &Unserializable)
            .unwrap_err();
        assert!(matches!(error, SendError::Encoding(_)));
        assert!(std::error::Error::source(&error)
            .unwrap()
            .is::<serde_json::Error>());
        assert!(!format!("{error:?}").contains("private message data"));
    }

    #[tokio::test]
    async fn bounded_connection_replacement_and_closed_channels() {
        let hub = ConnectionHub::default();
        let (tx, mut rx) = mpsc::channel(1);
        let old = hub.register("session".into(), tx);
        hub.send("session", "Play", &json!({})).unwrap();
        assert!(matches!(
            hub.send("session", "Play", &json!({})),
            Err(SendError::Backpressure)
        ));
        rx.recv().await.unwrap();
        let (tx, rx2) = mpsc::channel(1);
        let new = hub.register("session".into(), tx);
        assert!(!hub.unregister("session", old));
        assert!(hub.is_current("session", new));
        drop(rx2);
        assert!(matches!(
            hub.send("session", "Play", &json!({})),
            Err(SendError::Disconnected)
        ));
        assert!(!hub.is_connected("session"));
    }
}
