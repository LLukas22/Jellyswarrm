//! Expose client-session discovery, capabilities, playback reports, and remote-control HTTP routes.
use super::status;
use crate::sessions::{
    models::{Capabilities, GeneralCommand, MessageCommand},
    SessionContext,
};
use crate::{
    application::sessions::SessionOperations, domain::user_id::UserId, http::state::AppState,
};
use axum::{
    body::to_bytes,
    extract::{Path, Request, State},
    http::{Method, StatusCode, Uri},
    routing::{any, get, post},
    Json, Router,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use std::{collections::BTreeMap, str::FromStr};
use uuid::Uuid;

type ApiResult<T> = Result<T, StatusCode>;

pub(crate) fn router() -> Router<AppState> {
    jellyswarrm_macros::lowercase_routes! {
    Router::new()
        .route("/Sessions", get(list))
        .route("/Sessions/", get(list))
        // Playback telemetry continues through the existing media pipeline.
        .route("/Sessions/Playing", post(crate::http::proxy::proxy_handler))
        .route("/Sessions/Playing/Progress", post(crate::http::proxy::proxy_handler))
        .route("/Sessions/Playing/Stopped", post(crate::http::proxy::proxy_handler))
        // All other session operations are local, including unsupported ones.
        .route("/Sessions/{*path}", any(dispatch))
    }
}

pub(super) struct Params(BTreeMap<String, String>);
impl Params {
    fn new(uri: &Uri) -> ApiResult<Self> {
        let mut params = BTreeMap::new();
        for (key, value) in url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()) {
            if params
                .insert(key.to_ascii_lowercase(), value.into_owned())
                .is_some()
            {
                return Err(StatusCode::BAD_REQUEST);
            }
        }
        Ok(Self(params))
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(&key.to_ascii_lowercase()).map(String::as_str)
    }
    fn required(&self, key: &str) -> ApiResult<&str> {
        self.get(key)
            .filter(|v| !v.is_empty())
            .ok_or(StatusCode::BAD_REQUEST)
    }
    fn number<T: FromStr>(&self, key: &str) -> ApiResult<Option<T>> {
        self.get(key)
            .map(|v| v.parse().map_err(|_| StatusCode::BAD_REQUEST))
            .transpose()
    }
    fn ticks(&self, key: &str) -> ApiResult<Option<i64>> {
        let ticks = self.number::<i64>(key)?;
        if ticks.is_some_and(|v| v < 0) {
            return Err(StatusCode::BAD_REQUEST);
        }
        Ok(ticks)
    }
    fn boolean(&self, key: &str, default: bool) -> ApiResult<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(v) if v.eq_ignore_ascii_case("true") => Ok(true),
            Some(v) if v.eq_ignore_ascii_case("false") => Ok(false),
            _ => Err(StatusCode::BAD_REQUEST),
        }
    }
    fn list(&self, key: &str) -> Vec<String> {
        self.get(key)
            .unwrap_or("")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
    fn own_id(&self, key: &str, session: &SessionContext) -> ApiResult<()> {
        if self
            .get(key)
            .is_some_and(|id| !id.is_empty() && id != session.session_id)
        {
            return Err(StatusCode::FORBIDDEN);
        }
        Ok(())
    }
}

async fn list(
    State(operations): State<SessionOperations>,
    caller: SessionContext,
    uri: Uri,
) -> ApiResult<Json<Vec<Value>>> {
    let p = Params::new(&uri)?;
    let controllable = p.get("ControllableByUserId");
    if controllable.is_some_and(|id| UserId::new(id) != caller.user.id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let active = p.number::<u32>("ActiveWithinSeconds")?;
    let cutoff = active.map(|s| chrono::Utc::now() - chrono::Duration::seconds(i64::from(s)));
    let sessions = operations
        .snapshots(&caller.user.id)
        .await
        .map_err(status)?
        .into_iter()
        .filter(|s| {
            (controllable.is_none() || s["SupportsRemoteControl"] == true)
                && p.get("DeviceId").is_none_or(|id| s["DeviceId"] == id)
                && cutoff.is_none_or(|cutoff| {
                    s["LastActivityDate"]
                        .as_str()
                        .and_then(|d| chrono::DateTime::parse_from_rfc3339(d).ok())
                        .is_some_and(|d| d >= cutoff)
                })
        })
        .collect();
    Ok(Json(sessions))
}

async fn dispatch(
    State(operations): State<SessionOperations>,
    caller: SessionContext,
    Path(path): Path<String>,
    req: Request,
) -> ApiResult<StatusCode> {
    if req.method() != Method::POST {
        return Err(StatusCode::NOT_IMPLEMENTED);
    }
    let p = Params::new(req.uri())?;
    let path = path.trim_end_matches('/').to_ascii_lowercase();
    let path = path.as_str();
    if matches!(path, "capabilities" | "capabilities/full") {
        p.own_id("Id", &caller)?;
        let mut capabilities = if path.ends_with("/full") {
            body::<Capabilities>(req).await?
        } else {
            Capabilities {
                playable_media_types: p.list("PlayableMediaTypes"),
                supported_commands: p.list("SupportedCommands"),
                supports_media_control: p.boolean("SupportsMediaControl", false)?,
                supports_persistent_identifier: p.boolean("SupportsPersistentIdentifier", true)?,
            }
        };
        for media_type in &mut capabilities.playable_media_types {
            *media_type = canonical(media_type, &["Audio", "Video", "Book", "Photo"])?.into();
        }
        // Keep client-advertised extensions, but command dispatch only accepts known contracts.
        if capabilities.supported_commands.len() > 256
            || capabilities
                .supported_commands
                .iter()
                .any(|c| c.len() > 128)
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        operations
            .capabilities(&caller, capabilities)
            .await
            .map_err(status)?;
        return Ok(StatusCode::NO_CONTENT);
    }
    if path == "logout" {
        operations.logout(&caller).await;
        return Ok(StatusCode::NO_CONTENT);
    }
    if path == "viewing" {
        p.own_id("SessionId", &caller)?;
        let id = item_id(p.required("ItemId")?)?;
        operations.viewing(&caller, &id).await.map_err(status)?;
        return Ok(StatusCode::NO_CONTENT);
    }
    let parts: Vec<_> = path.split('/').collect();
    let [id, action, rest @ ..] = parts.as_slice() else {
        return Err(StatusCode::NOT_IMPLEMENTED);
    };
    let target = operations.target(&caller, id).await.map_err(status)?;
    let (message_type, data) = match (*action, rest) {
        ("playing", []) => {
            let command = canonical(
                p.required("PlayCommand")?,
                &["PlayNow", "PlayNext", "PlayLast"],
            )?;
            let items: Vec<_> = p
                .list("ItemIds")
                .iter()
                .map(|id| item_id(id))
                .collect::<ApiResult<_>>()?;
            if items.is_empty() || items.len() > 1000 {
                return Err(StatusCode::BAD_REQUEST);
            }
            operations
                .check_playable(&target, &items)
                .await
                .map_err(status)?;
            let start_index = p.number::<usize>("StartIndex")?;
            if start_index.is_some_and(|index| index >= items.len()) {
                return Err(StatusCode::BAD_REQUEST);
            }
            let source = p.get("MediaSourceId").map(item_id).transpose()?;
            // An explicit source belongs to the selected starting item.
            let mut validation_items = items.clone();
            validation_items.swap(0, start_index.unwrap_or(0));
            operations
                .require_target_media(&target, &validation_items, source.as_deref())
                .await
                .map_err(status)?;
            (
                "Play",
                json!({"PlayCommand": command, "ItemIds": items,
                "StartPositionTicks": p.ticks("StartPositionTicks")?, "StartIndex": start_index,
                "MediaSourceId": source, "AudioStreamIndex": stream_index(&p, "AudioStreamIndex")?,
                "SubtitleStreamIndex": stream_index(&p, "SubtitleStreamIndex")?,
                "ControllingUserId": caller.user.id}),
            )
        }
        ("playing", [command]) => {
            let command = canonical(
                command,
                &[
                    "Stop",
                    "Pause",
                    "Unpause",
                    "NextTrack",
                    "PreviousTrack",
                    "Seek",
                    "Rewind",
                    "FastForward",
                    "PlayPause",
                ],
            )?;
            let position = p.ticks("SeekPositionTicks")?;
            if command == "Seek" && position.is_none() {
                return Err(StatusCode::BAD_REQUEST);
            }
            if command == "Seek" && target.play_state["CanSeek"] != true {
                return Err(StatusCode::CONFLICT);
            }
            (
                "Playstate",
                json!({"Command": command, "SeekPositionTicks": position, "ControllingUserId": caller.user.id}),
            )
        }
        ("command", []) => operations
            .general_command(&caller, &target, body::<GeneralCommand>(req).await?)
            .await
            .map_err(status)?,
        ("command" | "system", [name]) => operations
            .general_command(
                &caller,
                &target,
                GeneralCommand {
                    name: name.to_string(),
                    arguments: BTreeMap::new(),
                },
            )
            .await
            .map_err(status)?,
        ("message", []) => {
            let message = body::<MessageCommand>(req).await?;
            let mut arguments = BTreeMap::from([
                (
                    "Header".into(),
                    if message.header.trim().is_empty() {
                        "Message from Server".into()
                    } else {
                        message.header
                    },
                ),
                ("Text".into(), message.text),
            ]);
            if let Some(timeout) = message.timeout_ms {
                arguments.insert("TimeoutMs".into(), timeout.to_string());
            }
            operations
                .general_command(
                    &caller,
                    &target,
                    GeneralCommand {
                        name: "DisplayMessage".into(),
                        arguments,
                    },
                )
                .await
                .map_err(status)?
        }
        ("viewing", []) => operations
            .general_command(
                &caller,
                &target,
                GeneralCommand {
                    name: "DisplayContent".into(),
                    arguments: BTreeMap::from([
                        ("ItemId".into(), p.required("ItemId")?.into()),
                        ("ItemType".into(), p.required("ItemType")?.into()),
                        ("ItemName".into(), p.required("ItemName")?.into()),
                    ]),
                },
            )
            .await
            .map_err(status)?,
        _ => return Err(StatusCode::NOT_IMPLEMENTED),
    };
    operations
        .send(&caller, id, message_type, &data)
        .await
        .map_err(status)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn body<T: DeserializeOwned>(request: Request) -> ApiResult<T> {
    let bytes = to_bytes(request.into_body(), 64 * 1024)
        .await
        .map_err(|_| StatusCode::PAYLOAD_TOO_LARGE)?;
    serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)
}
fn canonical<'a>(value: &str, choices: &[&'a str]) -> ApiResult<&'a str> {
    choices
        .iter()
        .copied()
        .find(|c| c.eq_ignore_ascii_case(value))
        .ok_or(StatusCode::BAD_REQUEST)
}
fn item_id(value: &str) -> ApiResult<String> {
    Uuid::parse_str(value)
        .map(|id| id.simple().to_string())
        .map_err(|_| StatusCode::BAD_REQUEST)
}
fn stream_index(p: &Params, key: &str) -> ApiResult<Option<i32>> {
    let index = p.number::<i32>(key)?;
    if index.is_some_and(|i| i < -1) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(index)
}
