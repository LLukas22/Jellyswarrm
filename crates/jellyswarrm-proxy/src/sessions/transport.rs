//! Shared bounded WebSocket delivery for client sessions and SyncPlay.
use axum::http::StatusCode;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use uuid::Uuid;

pub const OUTBOUND_CAPACITY: usize = 64;

#[derive(Clone, Default)]
pub struct ConnectionHub(Arc<Mutex<HashMap<String, Connection>>>);
struct Connection {
    generation: Uuid,
    sender: mpsc::Sender<String>,
}

impl ConnectionHub {
    pub fn register(&self, id: String, sender: mpsc::Sender<String>) -> Uuid {
        let generation = Uuid::new_v4();
        self.0
            .lock()
            .unwrap()
            .insert(id, Connection { generation, sender });
        generation
    }

    pub fn unregister(&self, id: &str, generation: Uuid) -> bool {
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

    pub fn remove(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }

    pub fn is_current(&self, id: &str, generation: Uuid) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|c| c.generation == generation && !c.sender.is_closed())
    }

    pub fn is_connected(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|c| !c.sender.is_closed())
    }

    pub fn send<T: Serialize>(
        &self,
        id: &str,
        message_type: &str,
        data: &T,
    ) -> Result<(), StatusCode> {
        let text = serde_json::to_string(&serde_json::json!({
            "MessageType": message_type, "MessageId": Uuid::new_v4().simple().to_string(), "Data": data
        })).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let mut connections = self.0.lock().unwrap();
        let connection = connections.get(id).ok_or(StatusCode::CONFLICT)?;
        match connection.sender.try_send(text) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => Err(StatusCode::SERVICE_UNAVAILABLE),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                connections.remove(id);
                Err(StatusCode::CONFLICT)
            }
        }
    }
}
