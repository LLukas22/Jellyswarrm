//! Maintain proxy-owned client presence, device metadata, playback state, and remote control.
use super::{connections::ConnectionHub, models::Capabilities};
use crate::domain::ids::ServerId;
use crate::domain::user_id::UserId;
use crate::{auth::service::LocalUser, protocol::device::Device};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::sync::RwLock;
use uuid::Uuid;

const SESSION_TTL_SECONDS: i64 = 30 * 60;
const JELLYFIN_MIN_DATE: &str = "0001-01-01T00:00:00.0000000Z";

// Never derive Debug/Serialize: the credential is private and only used for local revocation checks.
#[derive(Clone)]
pub(crate) struct ClientSession {
    pub(crate) id: String,
    pub(crate) user_id: UserId,
    pub(crate) user_name: String,
    pub(crate) device: Device,
    pub(super) token: String,
    pub(crate) capabilities: Capabilities,
    pub(crate) last_activity: DateTime<Utc>,
    pub(crate) play_state: Value,
    pub(crate) now_playing: Value,
    pub(crate) now_viewing: Value,
    pub(crate) queue: Value,
    pub(crate) last_playback: Option<DateTime<Utc>>,
    episode_server: Option<EpisodeServerPreference>,
}

#[derive(Clone)]
struct EpisodeServerPreference {
    series_id: String,
    server_id: ServerId,
}

impl ClientSession {
    /// Used only by application revocation checks and credential-scoped metadata.
    pub(crate) fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn dto(&self, server_id: &str, connections: &ConnectionHub) -> Value {
        let connected = connections.is_connected(&self.id);
        let controllable = connected && self.capabilities.supports_media_control;
        let last_playback = self
            .last_playback
            .map_or_else(|| json!(JELLYFIN_MIN_DATE), |d| json!(d));
        json!({
            "Id": self.id, "UserId": self.user_id, "UserName": self.user_name,
            "ServerId": server_id, "DeviceId": self.device.device_id,
            "DeviceName": self.device.device, "Client": self.device.client,
            "ApplicationVersion": self.device.version, "LastActivityDate": self.last_activity,
            "LastPlaybackCheckIn": last_playback, "IsActive": connected,
            "SupportsMediaControl": controllable, "SupportsRemoteControl": controllable,
            "Capabilities": self.capabilities, "PlayableMediaTypes": self.capabilities.playable_media_types,
            "SupportedCommands": self.capabilities.supported_commands, "AdditionalUsers": [],
            "PlayState": self.play_state, "NowPlayingItem": self.now_playing,
            "NowViewingItem": self.now_viewing, "NowPlayingQueue": self.queue,
            "HasCustomDeviceName": false
        })
    }
}

pub(crate) struct ClientSessionService {
    entries: RwLock<HashMap<String, ClientSession>>,
    pub(crate) connections: ConnectionHub,
    media_metadata: moka::future::Cache<(String, String), Value>,
}

impl ClientSessionService {
    pub(crate) fn new(connections: ConnectionHub) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            connections,
            media_metadata: moka::future::Cache::builder()
                .max_capacity(2048)
                .time_to_live(std::time::Duration::from_secs(1800))
                .build(),
        }
    }

    /// Remember only public display fields from already-served media responses.
    /// Remote controls must not fetch metadata from an upstream on their own.
    pub(crate) async fn cache_media_response(&self, token: &str, payload: &Value) {
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

    pub(crate) async fn media_metadata(&self, token: &str, item_id: &str) -> Option<Value> {
        self.media_metadata
            .get(&(token.to_string(), item_id.to_string()))
            .await
    }

    pub(crate) async fn ensure(
        &self,
        user: &LocalUser,
        token: &str,
        device: Option<&Device>,
    ) -> String {
        let mut entries = self.entries.write().await;
        let now = Utc::now();
        entries.retain(|id, entry| {
            self.connections.is_connected(id)
                || now - entry.last_activity < Duration::seconds(SESSION_TTL_SECONDS)
        });
        let device_id = device.map(|d| d.device_id.as_str()).unwrap_or("");
        let candidates: Vec<_> = entries
            .values()
            .filter(|s| s.user_id == user.id && s.token == token && s.device.device_id == device_id)
            .collect();
        let client = device
            .map(|d| d.client.as_str())
            .filter(|c| !c.is_empty() && *c != "Unknown");
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
            if let Some(device) = device {
                for (stored, incoming) in [
                    (&mut entry.device.client, &device.client),
                    (&mut entry.device.device, &device.device),
                    (&mut entry.device.version, &device.version),
                ] {
                    if !incoming.is_empty() && incoming != "Unknown" {
                        *stored = incoming.clone();
                    }
                }
            }
            return entry.id.clone();
        }
        let device = device.map(|d| Device {
            client: if d.client.is_empty() {
                "Unknown".into()
            } else {
                d.client.clone()
            },
            device: if d.device.is_empty() {
                "Unknown".into()
            } else {
                d.device.clone()
            },
            version: if d.version.is_empty() {
                "Unknown".into()
            } else {
                d.version.clone()
            },
            device_id: d.device_id.clone(),
        });
        let id = Uuid::new_v4().simple().to_string();
        entries.insert(
            id.clone(),
            ClientSession {
                id: id.clone(),
                user_id: user.id.clone(),
                user_name: user.original_username.clone(),
                token: token.into(),
                device: device.unwrap_or(Device {
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
                episode_server: None,
            },
        );
        id
    }

    pub(crate) async fn get(&self, id: &str) -> Option<ClientSession> {
        self.entries.read().await.get(id).cloned()
    }

    pub(crate) async fn preferred_episode_server(
        &self,
        id: &str,
        series_id: &str,
    ) -> Option<ServerId> {
        let entries = self.entries.read().await;
        let preference = entries.get(id)?.episode_server.as_ref()?;
        (preference.series_id == series_id).then_some(preference.server_id)
    }

    /// Keep continuity across the stop/next transition, but never share it with
    /// another client session or expose it as a client-supplied routing authority.
    pub(crate) async fn remember_episode_server(
        &self,
        id: &str,
        series_id: &str,
        server_id: ServerId,
    ) {
        if let Some(session) = self.entries.write().await.get_mut(id) {
            session.episode_server = Some(EpisodeServerPreference {
                series_id: series_id.into(),
                server_id,
            });
        }
    }

    pub(crate) async fn for_user(&self, user_id: &str) -> Vec<ClientSession> {
        let user_id = UserId::new(user_id);
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        entries.retain(|id, entry| {
            self.connections.is_connected(id)
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

    pub(crate) async fn capabilities(&self, id: &str, capabilities: Capabilities) -> Option<()> {
        self.entries.write().await.get_mut(id)?.capabilities = capabilities;
        Some(())
    }

    pub(crate) async fn touch(&self, id: &str) {
        if let Some(s) = self.entries.write().await.get_mut(id) {
            s.last_activity = Utc::now();
        }
    }

    pub(crate) async fn remove(&self, id: &str) {
        self.entries.write().await.remove(id);
        self.connections.remove(id);
    }

    pub(crate) async fn viewing(&self, id: &str, item_id: &str) {
        if let Some(s) = self.entries.write().await.get_mut(id) {
            s.now_viewing = json!({"Id": item_id});
        }
    }

    pub(crate) async fn report(&self, id: &str, stopped: bool, report: &Value) {
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
    fn user() -> LocalUser {
        LocalUser {
            id: "user".into(),
            virtual_key: "secret".into(),
            original_username: "Alice".into(),
            local_credential: crate::auth::service::LocalCredential::Passwordless,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
    #[tokio::test]
    async fn episode_server_preference_stays_private_scoped_to_series_and_expires_with_the_client()
    {
        let service = ClientSessionService::new(ConnectionHub::default());
        let device = Device {
            client: "Web".into(),
            device: "TV".into(),
            device_id: "tv".into(),
            version: "1".into(),
        };
        let id = service.ensure(&user(), "secret", Some(&device)).await;
        let server = crate::domain::ids::ServerId::new(2);
        service.remember_episode_server(&id, "show", server).await;
        assert_eq!(
            service.preferred_episode_server(&id, "show").await,
            Some(server)
        );
        assert_eq!(
            service.preferred_episode_server(&id, "another-show").await,
            None
        );
        let another_client = Device {
            client: "Another client".into(),
            ..device.clone()
        };
        let other = service
            .ensure(&user(), "secret", Some(&another_client))
            .await;
        assert_ne!(other, id);
        assert_eq!(service.preferred_episode_server(&other, "show").await, None);
        let other = service
            .ensure(&user(), "another-token", Some(&device))
            .await;
        assert_eq!(service.preferred_episode_server(&other, "show").await, None);
        let dto = service
            .get(&id)
            .await
            .unwrap()
            .dto("proxy", &service.connections);
        assert!(!dto.to_string().contains("episode_server"));

        service
            .entries
            .write()
            .await
            .get_mut(&id)
            .unwrap()
            .last_activity = Utc::now() - Duration::seconds(SESSION_TTL_SECONDS + 1);
        let replacement = service.ensure(&user(), "secret", Some(&device)).await;
        assert_ne!(replacement, id);
        assert_eq!(
            service.preferred_episode_server(&replacement, "show").await,
            None
        );
    }

    #[tokio::test]
    async fn partial_metadata_can_be_enriched_without_replacing_the_session() {
        let service = ClientSessionService::new(ConnectionHub::default());
        let partial = Device {
            client: String::new(),
            device: String::new(),
            version: String::new(),
            device_id: "device".into(),
        };
        let id = service.ensure(&user(), "secret", Some(&partial)).await;
        let full = Device {
            client: "Streamyfin".into(),
            device: "iPhone".into(),
            version: "0.55.2".into(),
            device_id: partial.device_id.clone(),
        };
        assert_eq!(service.ensure(&user(), "secret", Some(&full)).await, id);
        assert_eq!(service.ensure(&user(), "secret", Some(&partial)).await, id);
        let entry = service.get(&id).await.unwrap();
        assert_eq!(entry.device.client, full.client);
        assert_eq!(entry.device.device, full.device);
        assert_eq!(entry.device.version, full.version);
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
        service.connections.register(replacement.clone(), tx);
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
