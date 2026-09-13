use super::{
    models::{Capabilities, GeneralCommand, MessageCommand},
    service::ClientSession,
    snapshots, user_has_media_access, valid_session, SessionContext,
};
use crate::AppState;
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

pub fn router() -> Router<AppState> {
    jellyswarrm_macros::lowercase_routes! {
    Router::new()
        .route("/Sessions", get(list))
        .route("/Sessions/", get(list))
        // Playback telemetry continues through the existing media pipeline.
        .route("/Sessions/Playing", post(crate::proxy_handler))
        .route("/Sessions/Playing/Progress", post(crate::proxy_handler))
        .route("/Sessions/Playing/Stopped", post(crate::proxy_handler))
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
    State(state): State<AppState>,
    caller: SessionContext,
    uri: Uri,
) -> ApiResult<Json<Vec<Value>>> {
    let p = Params::new(&uri)?;
    let controllable = p.get("ControllableByUserId");
    if controllable.is_some_and(|id| id != caller.user.id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let active = p.number::<u32>("ActiveWithinSeconds")?;
    let cutoff = active.map(|s| chrono::Utc::now() - chrono::Duration::seconds(i64::from(s)));
    let sessions = snapshots(&state, &caller.user.id)
        .await?
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

async fn target(state: &AppState, caller: &SessionContext, id: &str) -> ApiResult<ClientSession> {
    let target = state
        .client_sessions
        .get(id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    if target.user_id != caller.user.id || !valid_session(state, &target).await? {
        return Err(StatusCode::NOT_FOUND);
    }
    if !state.client_sessions.transport.is_connected(id)
        || !target.capabilities.supports_media_control
    {
        return Err(StatusCode::CONFLICT);
    }
    Ok(target)
}

async fn dispatch(
    State(state): State<AppState>,
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
        state
            .client_sessions
            .capabilities(&caller.session_id, capabilities)
            .await?;
        return Ok(StatusCode::NO_CONTENT);
    }
    if path == "logout" {
        state.syncplay.end_session(&caller.session_id).await;
        state.client_sessions.remove(&caller.session_id).await;
        return Ok(StatusCode::NO_CONTENT);
    }
    if path == "viewing" {
        p.own_id("SessionId", &caller)?;
        let id = item_id(p.required("ItemId")?)?;
        require_media(&state, &caller.user.id, std::slice::from_ref(&id), None).await?;
        state.client_sessions.viewing(&caller.session_id, &id).await;
        return Ok(StatusCode::NO_CONTENT);
    }
    let parts: Vec<_> = path.split('/').collect();
    let [id, action, rest @ ..] = parts.as_slice() else {
        return Err(StatusCode::NOT_IMPLEMENTED);
    };
    let target = target(&state, &caller, id).await?;
    let (message_type, data, playback_mutation) = match (*action, rest) {
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
            for item in &items {
                if let Some(metadata) = state
                    .client_sessions
                    .media_metadata(&target.token, item)
                    .await
                {
                    if let Some(media_type) = metadata["MediaType"].as_str() {
                        if !target
                            .capabilities
                            .playable_media_types
                            .iter()
                            .any(|t| t.eq_ignore_ascii_case(media_type))
                        {
                            return Err(StatusCode::CONFLICT);
                        }
                    }
                }
            }
            let start_index = p.number::<usize>("StartIndex")?;
            if start_index.is_some_and(|index| index >= items.len()) {
                return Err(StatusCode::BAD_REQUEST);
            }
            let source = p.get("MediaSourceId").map(item_id).transpose()?;
            // An explicit source belongs to the selected starting item.
            let mut validation_items = items.clone();
            validation_items.swap(0, start_index.unwrap_or(0));
            require_media(
                &state,
                &target.user_id,
                &validation_items,
                source.as_deref(),
            )
            .await?;
            (
                "Play",
                json!({"PlayCommand": command, "ItemIds": items,
                "StartPositionTicks": p.ticks("StartPositionTicks")?, "StartIndex": start_index,
                "MediaSourceId": source, "AudioStreamIndex": stream_index(&p, "AudioStreamIndex")?,
                "SubtitleStreamIndex": stream_index(&p, "SubtitleStreamIndex")?,
                "ControllingUserId": caller.user.id}),
                true,
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
                true,
            )
        }
        ("command", []) => {
            general(&state, &caller, &target, body::<GeneralCommand>(req).await?).await?
        }
        ("command" | "system", [name]) => {
            general(
                &state,
                &caller,
                &target,
                GeneralCommand {
                    name: name.to_string(),
                    arguments: BTreeMap::new(),
                },
            )
            .await?
        }
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
            general(
                &state,
                &caller,
                &target,
                GeneralCommand {
                    name: "DisplayMessage".into(),
                    arguments,
                },
            )
            .await?
        }
        ("viewing", []) => {
            general(
                &state,
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
            .await?
        }
        _ => return Err(StatusCode::NOT_IMPLEMENTED),
    };
    // Recheck lifecycle and capability state after asynchronous media/credential validation.
    let current = self::target(&state, &caller, id).await?;
    if message_type == "GeneralCommand" && !supports(&current, data["Name"].as_str().unwrap_or(""))
    {
        return Err(StatusCode::CONFLICT);
    }
    state
        .syncplay
        .send_remote_command(id, message_type, &data, playback_mutation)
        .await?;
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
async fn require_media(
    state: &AppState,
    user_id: &str,
    items: &[String],
    source: Option<&str>,
) -> ApiResult<()> {
    if !user_has_media_access(state, user_id, items, source).await? {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(())
}
fn supports(target: &ClientSession, name: &str) -> bool {
    target
        .capabilities
        .supported_commands
        .iter()
        .any(|c| c.eq_ignore_ascii_case(name))
}

async fn general(
    state: &AppState,
    caller: &SessionContext,
    target: &ClientSession,
    command: GeneralCommand,
) -> ApiResult<(&'static str, Value, bool)> {
    let name = canonical(
        &command.name,
        &[
            "MoveUp",
            "MoveDown",
            "MoveLeft",
            "MoveRight",
            "PageUp",
            "PageDown",
            "PreviousLetter",
            "NextLetter",
            "ToggleOsd",
            "ToggleContextMenu",
            "Select",
            "Back",
            "TakeScreenshot",
            "SendKey",
            "SendString",
            "GoHome",
            "GoToSettings",
            "VolumeUp",
            "VolumeDown",
            "Mute",
            "Unmute",
            "ToggleMute",
            "SetVolume",
            "SetAudioStreamIndex",
            "SetSubtitleStreamIndex",
            "ToggleFullscreen",
            "DisplayContent",
            "GoToSearch",
            "DisplayMessage",
            "SetRepeatMode",
            "ChannelUp",
            "ChannelDown",
            "Guide",
            "ToggleStats",
            "SetShuffleQueue",
            "ToggleOsdMenu",
            "SetMaxStreamingBitrate",
            "SetPlaybackOrder",
            "PlayMediaSource",
            "PlayTrailers",
            "PlayState",
            "PlayNext",
            "Play",
        ],
    )?;
    if !supports(target, name) {
        return Err(StatusCode::CONFLICT);
    }
    let arg = |key: &str| {
        command
            .arguments
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    };
    let mut items = Vec::new();
    if let Some(id) = arg("ItemId") {
        items.push(item_id(id)?);
    }
    if let Some(ids) = arg("ItemIds") {
        for id in ids.split(',') {
            items.push(item_id(id.trim())?);
        }
    }
    if matches!(name, "DisplayContent" | "PlayMediaSource" | "PlayTrailers") && items.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(source) = arg("MediaSourceId") {
        if items.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        require_media(state, &target.user_id, &items, Some(&item_id(source)?)).await?;
    } else if !items.is_empty() {
        require_media(state, &target.user_id, &items, None).await?;
    }
    // Navigation and keyboard commands can trigger playback in a client; conservatively
    // reject all except presentation/volume controls while a target belongs to SyncPlay.
    let playback_mutation = !matches!(
        name,
        "VolumeUp"
            | "VolumeDown"
            | "Mute"
            | "Unmute"
            | "ToggleMute"
            | "SetVolume"
            | "DisplayMessage"
            | "ToggleFullscreen"
            | "ToggleStats"
            | "TakeScreenshot"
    );
    Ok((
        "GeneralCommand",
        json!({"Name": name, "Arguments": command.arguments, "ControllingUserId": caller.user.id}),
        playback_mutation,
    ))
}
