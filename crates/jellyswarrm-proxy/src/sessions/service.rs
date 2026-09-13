use super::{models::Capabilities, transport::ConnectionHub};
use crate::user_authorization_service::{Device, User};
use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::RwLock;
use uuid::Uuid;

const SESSION_TTL_SECONDS: i64 = 30 * 60;

// Never derive Debug/Serialize: the credential is private and only used for local revocation checks.
#[derive(Clone)]
pub struct ClientSession {
    pub id: String,
    pub user_id: String,
    pub user_name: String,
    pub device: Device,
    pub(super) token: String,
    pub capabilities: Capabilities,
    pub last_activity: DateTime<Utc>,
    pub play_state: Value,
    pub now_playing: Value,
    pub now_viewing: Value,
    pub queue: Value,
    pub last_playback: Option<DateTime<Utc>>,
}

impl ClientSession {
    pub fn dto(&self, server_id: &str, transport: &ConnectionHub) -> Value {
        let connected = transport.is_connected(&self.id);
        let controllable = connected && self.capabilities.supports_media_control;
        json!({
            "Id": self.id, "UserId": self.user_id, "UserName": self.user_name,
            "ServerId": server_id, "DeviceId": self.device.device_id,
            "DeviceName": self.device.device, "Client": self.device.client,
            "ApplicationVersion": self.device.version, "LastActivityDate": self.last_activity,
            "LastPlaybackCheckIn": self.last_playback, "IsActive": connected,
            "SupportsMediaControl": controllable, "SupportsRemoteControl": controllable,
            "Capabilities": self.capabilities, "PlayableMediaTypes": self.capabilities.playable_media_types,
            "SupportedCommands": self.capabilities.supported_commands, "AdditionalUsers": [],
            "PlayState": self.play_state, "NowPlayingItem": self.now_playing,
            "NowViewingItem": self.now_viewing, "NowPlayingQueue": self.queue
        })
    }
}

pub struct ClientSessionService {
    entries: RwLock<HashMap<String, ClientSession>>,
    pub transport: ConnectionHub,
    media_metadata: moka::future::Cache<(String, String), Value>,
}

impl ClientSessionService {
    pub fn new(transport: ConnectionHub) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            transport,
            media_metadata: moka::future::Cache::builder()
                .max_capacity(2048)
                .time_to_live(std::time::Duration::from_secs(1800))
                .build(),
        }
    }

    /// Remember only public display fields from already-served media responses.
    /// Remote controls must not fetch metadata from an upstream on their own.
    pub async fn cache_media_response(&self, token: &str, payload: &Value) {
        let mut pending = vec![payload];
        let mut retained = 0;
        while let Some(value) = pending.pop() {
            match value {
                Value::Array(items) => pending.extend(items),
                Value::Object(object) => {
                    if let (Some(id), Some(_)) = (
                        object.get("Id").and_then(Value::as_str),
                        object.get("Name").and_then(Value::as_str),
                    ) {
                        if object.contains_key("Type") || object.contains_key("MediaType") {
                            let mut metadata = serde_json::Map::new();
                            for key in [
                                "Id",
                                "Name",
                                "Type",
                                "MediaType",
                                "RunTimeTicks",
                                "IndexNumber",
                                "ParentIndexNumber",
                                "SeriesName",
                                "SeriesId",
                                "Album",
                                "Artists",
                                "ImageTags",
                                "PrimaryImageAspectRatio",
                                "ParentThumbItemId",
                                "ParentThumbImageTag",
                                "SeriesPrimaryImageTag",
                                "IsFolder",
                            ] {
                                if let Some(value) = object.get(key) {
                                    metadata.insert(key.into(), value.clone());
                                }
                            }
                            let metadata = Value::Object(metadata);
                            if metadata.to_string().len() <= 16 * 1024 {
                                self.media_metadata
                                    .insert((token.to_string(), id.to_string()), metadata)
                                    .await;
                                retained += 1;
                            }
                        }
                    }
                    pending.extend(object.values());
                }
                _ => {}
            }
            if retained >= 2048 {
                break;
            }
        }
    }

    pub async fn media_metadata(&self, token: &str, item_id: &str) -> Option<Value> {
        self.media_metadata
            .get(&(token.to_string(), item_id.to_string()))
            .await
    }

    pub async fn ensure(&self, user: &User, token: &str, device: Option<&Device>) -> String {
        let mut entries = self.entries.write().await;
        let now = Utc::now();
        entries.retain(|id, entry| {
            self.transport.is_connected(id)
                || now - entry.last_activity < Duration::seconds(SESSION_TTL_SECONDS)
        });
        let device_id = device.map(|d| d.device_id.as_str()).unwrap_or("");
        let candidates: Vec<_> = entries
            .values()
            .filter(|s| s.user_id == user.id && s.token == token && s.device.device_id == device_id)
            .collect();
        let client = device
            .map(|d| d.client.as_str())
            .filter(|c| *c != "Unknown");
        let existing = if let Some(client) = client {
            candidates
                .iter()
                .find(|s| s.device.client == client)
                .or_else(|| {
                    (candidates.len() == 1 && candidates[0].device.client == "Unknown")
                        .then(|| &candidates[0])
                })
        } else if candidates.len() == 1 {
            candidates.first()
        } else {
            // A DeviceId-only socket cannot disambiguate two named clients using that ID.
            // Keep it separate rather than delivering one client's commands to another.
            candidates.iter().find(|s| s.device.client == "Unknown")
        }
        .map(|s| s.id.clone());
        if let Some(entry) = existing.and_then(|id| entries.get_mut(&id)) {
            entry.last_activity = now;
            // WebSocket requests often contain only DeviceId and no client metadata.
            if let Some(device) = device.filter(|d| d.client != "Unknown" && d.version != "Unknown")
            {
                entry.device = device.clone();
            }
            return entry.id.clone();
        }
        let id = Uuid::new_v4().simple().to_string();
        entries.insert(
            id.clone(),
            ClientSession {
                id: id.clone(),
                user_id: user.id.clone(),
                user_name: user.original_username.clone(),
                token: token.into(),
                device: device.cloned().unwrap_or(Device {
                    client: "Unknown".into(),
                    device: "Unknown".into(),
                    device_id: String::new(),
                    version: "Unknown".into(),
                }),
                capabilities: Capabilities::default(),
                last_activity: now,
                play_state: empty_play_state(),
                now_playing: Value::Null,
                now_viewing: Value::Null,
                queue: json!([]),
                last_playback: None,
            },
        );
        id
    }

    pub async fn get(&self, id: &str) -> Option<ClientSession> {
        self.entries.read().await.get(id).cloned()
    }

    pub async fn for_user(&self, user_id: &str) -> Vec<ClientSession> {
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        entries.retain(|id, entry| {
            self.transport.is_connected(id)
                || now - entry.last_activity < Duration::seconds(SESSION_TTL_SECONDS)
        });
        let mut result: Vec<_> = entries
            .values()
            .filter(|s| s.user_id == user_id)
            .cloned()
            .collect();
        result.sort_by(|a, b| b.last_activity.cmp(&a.last_activity).then(a.id.cmp(&b.id)));
        result
    }

    pub async fn capabilities(
        &self,
        id: &str,
        capabilities: Capabilities,
    ) -> Result<(), StatusCode> {
        self.entries
            .write()
            .await
            .get_mut(id)
            .ok_or(StatusCode::NOT_FOUND)?
            .capabilities = capabilities;
        Ok(())
    }

    pub async fn touch(&self, id: &str) {
        if let Some(s) = self.entries.write().await.get_mut(id) {
            s.last_activity = Utc::now();
        }
    }

    pub async fn remove(&self, id: &str) {
        self.entries.write().await.remove(id);
        self.transport.remove(id);
    }

    pub async fn viewing(&self, id: &str, item_id: &str) {
        if let Some(s) = self.entries.write().await.get_mut(id) {
            s.now_viewing = json!({"Id": item_id});
        }
    }

    pub async fn report(&self, id: &str, stopped: bool, report: &Value) {
        let reported_id = report
            .as_object()
            .and_then(|object| {
                object
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case("ItemId"))
            })
            .and_then(|(_, value)| value.as_str());
        let metadata = if let (Some(item_id), Some(session)) = (reported_id, self.get(id).await) {
            self.media_metadata(&session.token, item_id).await
        } else {
            None
        };
        let mut entries = self.entries.write().await;
        let Some(s) = entries.get_mut(id) else {
            return;
        };
        s.last_activity = Utc::now();
        s.last_playback = Some(s.last_activity);
        let field = |name: &str| {
            report.as_object().and_then(|obj| {
                obj.iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            })
        };
        let Some(item_id) = field("ItemId").and_then(Value::as_str) else {
            return;
        };
        if stopped {
            // Ignore a late stop for the previous item after a new playback has begun.
            if s.now_playing["Id"].as_str() == Some(item_id) {
                s.now_playing = Value::Null;
                s.queue = json!([]);
                s.play_state = empty_play_state();
            }
            return;
        }
        if s.now_playing["Id"].as_str() != Some(item_id) {
            s.now_playing = metadata.clone().unwrap_or_else(|| json!({"Id": item_id}));
            s.play_state = empty_play_state();
        }
        if let Some(metadata) = metadata {
            s.now_playing = metadata;
        }
        for key in [
            "PositionTicks",
            "IsPaused",
            "IsMuted",
            "CanSeek",
            "VolumeLevel",
            "AudioStreamIndex",
            "SubtitleStreamIndex",
            "MediaSourceId",
            "PlayMethod",
            "RepeatMode",
            "PlaybackOrder",
        ] {
            if let Some(value) = field(key) {
                s.play_state[key] = value.clone();
            }
        }
        if let Some(queue) = field("NowPlayingQueue").and_then(Value::as_array) {
            // Copy only the public queue contract, not arbitrary client-supplied fields.
            s.queue = Value::Array(queue.iter().filter_map(|item| Some(json!({"Id": item.get("Id")?.as_str()?, "PlaylistItemId": item.get("PlaylistItemId")}))).collect());
        }
    }
}

fn empty_play_state() -> Value {
    json!({"IsPaused": false, "IsMuted": false, "CanSeek": false, "RepeatMode": "RepeatNone", "PlaybackOrder": "Default"})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn user() -> User {
        User {
            id: "user".into(),
            virtual_key: "secret".into(),
            original_username: "Alice".into(),
            local_credential: crate::user_authorization_service::LocalCredential::Passwordless,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
    #[tokio::test]
    async fn expired_disconnected_sessions_are_replaced_but_connected_sessions_survive() {
        let service = ClientSessionService::new(ConnectionHub::default());
        let id = service.ensure(&user(), "secret", None).await;
        service
            .entries
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .last_activity = Utc::now() - Duration::seconds(SESSION_TTL_SECONDS + 1);
        assert!(service.for_user("user").await.is_empty());
        let replacement = service.ensure(&user(), "secret", None).await;
        assert_ne!(id, replacement);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        service.transport.register(replacement.clone(), tx);
        service
            .entries
            .write()
            .await
            .get_mut(&replacement)
            .unwrap()
            .last_activity = Utc::now() - Duration::seconds(SESSION_TTL_SECONDS + 1);
        assert_eq!(service.for_user("user").await.len(), 1);
    }
    #[tokio::test]
    async fn metadata_is_scoped_sanitized_and_late_stops_do_not_clear_new_playback() {
        let service = ClientSessionService::new(ConnectionHub::default());
        let id = service.ensure(&user(), "secret", None).await;
        service.cache_media_response("secret", &json!({"Id":"item", "Name":"Movie", "MediaType":"Video", "RunTimeTicks":123, "Path":"private/backend/path", "AccessToken":"backend-secret"})).await;
        assert!(service.media_metadata("other", "item").await.is_none());
        service
            .report(&id, false, &json!({"ItemId":"item", "PositionTicks":42}))
            .await;
        let current = service.get(&id).await.unwrap();
        assert_eq!(current.now_playing["Name"], "Movie");
        assert_eq!(current.now_playing["RunTimeTicks"], 123);
        assert!(current.now_playing.get("Path").is_none());
        assert!(current.now_playing.get("AccessToken").is_none());
        service
            .report(&id, false, &json!({"ItemId":"new-item"}))
            .await;
        service.report(&id, true, &json!({"ItemId":"item"})).await;
        assert_eq!(
            service.get(&id).await.unwrap().now_playing["Id"],
            "new-item"
        );
    }
}
