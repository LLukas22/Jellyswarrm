//! Client-session authentication, revocation, discovery and remote-control orchestration.
pub(crate) mod access;
#[cfg(test)]
mod tests;
use crate::{
    auth::{service::User, UserAuthorizationService},
    config::SettingsView,
    protocol::{authorization::Authorization, device::Device},
    sessions::{
        models::{Capabilities, GeneralCommand},
        ClientSession, ClientSessionService, SessionContext,
    },
    syncplay::service::{RemoteCommandError, SyncPlayService},
};
use serde_json::{json, Value};

#[derive(thiserror::Error)]
pub(crate) enum SessionOperationError {
    #[error("invalid session command input")]
    InvalidInput,
    #[error("caller is not authenticated")]
    Unauthenticated,
    #[error("caller cannot mutate sessions")]
    ReadOnly,
    #[error("session or accessible media not found")]
    NotFound,
    #[error("target cannot accept this command")]
    NotControllable,
    #[error("session credential lookup failed")]
    Storage(#[from] sqlx::Error),
    #[error("remote command failed")]
    Remote(#[from] RemoteCommandError),
}

impl std::fmt::Debug for SessionOperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

pub(crate) struct SessionOperations<'a> {
    authorization: &'a UserAuthorizationService,
    sessions: &'a ClientSessionService,
    groups: &'a SyncPlayService,
    settings: &'a SettingsView,
    access: access::MediaAccess<'a>,
}

impl<'a> SessionOperations<'a> {
    pub(crate) fn new(
        authorization: &'a UserAuthorizationService,
        sessions: &'a ClientSessionService,
        groups: &'a SyncPlayService,
        settings: &'a SettingsView,
        access: access::MediaAccess<'a>,
    ) -> Self {
        Self {
            authorization,
            sessions,
            groups,
            settings,
            access,
        }
    }

    pub(crate) async fn authenticate(
        &self,
        headers: &http::HeaderMap,
        url: &url::Url,
        mutating: bool,
        device_hint: Option<Device>,
    ) -> Result<SessionContext, SessionOperationError> {
        let identity = super::accounts::resolve_identity(headers, url, self.authorization).await?;
        let user = identity
            .user
            .ok_or(SessionOperationError::Unauthenticated)?;
        let token = identity
            .auth
            .as_ref()
            .and_then(|a| a.token_ref())
            .ok_or(SessionOperationError::Unauthenticated)?;
        if mutating && self.authorization.is_read_only_api_key(token).await? {
            return Err(SessionOperationError::ReadOnly);
        }
        // The cookie never authenticates. Explicit device IDs outrank it.
        let device = identity
            .device
            .clone()
            .filter(|d| Device::has_known_device_id(&d.device_id))
            .or(device_hint)
            .or(identity.device);
        let session_id = self.sessions.ensure(&user, token, device.as_ref()).await;
        Ok(SessionContext { user, session_id })
    }

    pub(crate) async fn valid_session(
        &self,
        session: &ClientSession,
    ) -> Result<bool, SessionOperationError> {
        let user = self
            .authorization
            .get_user_by_token(session.token())
            .await?;
        if user.is_some_and(|user| user.id == session.user_id) {
            return Ok(true);
        }
        self.sessions.remove(&session.id).await;
        self.groups.end_session(&session.id).await;
        Ok(false)
    }

    pub(crate) async fn snapshots(
        &self,
        user_id: &str,
    ) -> Result<Vec<Value>, SessionOperationError> {
        let server_id = self.settings.read().await.server_id.clone();
        let mut result = Vec::new();
        for session in self.sessions.for_user(user_id).await {
            if self.valid_session(&session).await? {
                result.push(session.dto(&server_id, &self.sessions.transport));
            }
        }
        Ok(result)
    }

    pub(crate) async fn target(
        &self,
        caller: &SessionContext,
        id: &str,
    ) -> Result<ClientSession, SessionOperationError> {
        let target = self
            .sessions
            .get(id)
            .await
            .ok_or(SessionOperationError::NotFound)?;
        if target.user_id != caller.user.id || !self.valid_session(&target).await? {
            return Err(SessionOperationError::NotFound);
        }
        if !self.sessions.transport.is_connected(id) || !target.capabilities.supports_media_control
        {
            return Err(SessionOperationError::NotControllable);
        }
        Ok(target)
    }

    pub(crate) async fn capabilities(
        &self,
        caller: &SessionContext,
        capabilities: Capabilities,
    ) -> Result<(), SessionOperationError> {
        self.sessions
            .capabilities(&caller.session_id, capabilities)
            .await
            .ok_or(SessionOperationError::NotFound)
    }

    pub(crate) async fn logout(&self, caller: &SessionContext) {
        self.groups.end_session(&caller.session_id).await;
        self.sessions.remove(&caller.session_id).await;
    }

    pub(crate) async fn viewing(
        &self,
        caller: &SessionContext,
        item: &str,
    ) -> Result<(), SessionOperationError> {
        self.require_media(&caller.user.id, &[item.to_owned()], None)
            .await?;
        self.sessions.viewing(&caller.session_id, item).await;
        Ok(())
    }

    pub(crate) async fn check_playable(
        &self,
        target: &ClientSession,
        items: &[String],
    ) -> Result<(), SessionOperationError> {
        for item in items {
            if let Some(metadata) = self.sessions.media_metadata(target.token(), item).await {
                if let Some(media_type) = metadata["MediaType"].as_str() {
                    if !target
                        .capabilities
                        .playable_media_types
                        .iter()
                        .any(|t| t.eq_ignore_ascii_case(media_type))
                    {
                        return Err(SessionOperationError::NotControllable);
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn require_target_media(
        &self,
        target: &ClientSession,
        items: &[String],
        source: Option<&str>,
    ) -> Result<(), SessionOperationError> {
        self.require_media(&target.user_id, items, source).await
    }

    async fn require_media(
        &self,
        user_id: &str,
        items: &[String],
        source: Option<&str>,
    ) -> Result<(), SessionOperationError> {
        if !self.access.allows(user_id, items, source).await? {
            return Err(SessionOperationError::NotFound);
        }
        Ok(())
    }

    /// Recheck lifecycle/capability state after asynchronous media validation.
    /// SyncPlay serializes its membership gate with enqueue under the group lock.
    pub(crate) async fn send(
        &self,
        caller: &SessionContext,
        id: &str,
        message_type: &str,
        data: &Value,
    ) -> Result<(), SessionOperationError> {
        let current = self.target(caller, id).await?;
        if message_type == "GeneralCommand"
            && !current
                .capabilities
                .supported_commands
                .iter()
                .any(|c| c.eq_ignore_ascii_case(data["Name"].as_str().unwrap_or("")))
        {
            return Err(SessionOperationError::NotControllable);
        }
        let playback_mutation = mutates_playback(message_type, data);
        self.groups
            .send_remote_command(id, message_type, data, playback_mutation)
            .await?;
        Ok(())
    }

    /// Reset local client connections together with an administrative session reset.
    pub(crate) async fn end_user_sessions(&self, user_id: &str) {
        for session in self.sessions.for_user(user_id).await {
            self.groups.end_session(&session.id).await;
            self.sessions.remove(&session.id).await;
        }
    }
}

/// Give password and Quick Connect authentication the same local session DTO.
pub(crate) async fn decorate_authentication(
    sessions: &ClientSessionService,
    server_id: &str,
    user: &User,
    auth: &Authorization,
    response: &mut crate::models::AuthenticateResponse,
) {
    let device = Device {
        client: auth.client.clone(),
        device: auth.device.clone(),
        device_id: auth.device_id.clone(),
        version: auth.version.clone(),
    };
    let id = sessions
        .ensure(user, &user.virtual_key, Some(&device))
        .await;
    if let Some(session) = sessions.get(&id).await {
        response.session_info = serde_json::from_value(session.dto(server_id, &sessions.transport))
            .expect("local SessionInfo DTO");
    }
}

// Remote-command capability, media-access and SyncPlay coexistence policy.
impl SessionOperations<'_> {
    pub(crate) async fn general_command(
        &self,
        caller: &SessionContext,
        target: &ClientSession,
        command: GeneralCommand,
    ) -> Result<(&'static str, Value), SessionOperationError> {
        let choices = [
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
        ];
        let name = choices
            .into_iter()
            .find(|c| c.eq_ignore_ascii_case(&command.name))
            .ok_or(SessionOperationError::InvalidInput)?;
        if !target
            .capabilities
            .supported_commands
            .iter()
            .any(|c| c.eq_ignore_ascii_case(name))
        {
            return Err(SessionOperationError::NotControllable);
        }
        let arg = |key: &str| {
            command
                .arguments
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.as_str())
        };
        let item_id = |value: &str| {
            uuid::Uuid::parse_str(value)
                .map(|id| id.simple().to_string())
                .map_err(|_| SessionOperationError::InvalidInput)
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
        if matches!(name, "DisplayContent" | "PlayMediaSource" | "PlayTrailers") && items.is_empty()
        {
            return Err(SessionOperationError::InvalidInput);
        }
        if let Some(source) = arg("MediaSourceId") {
            if items.is_empty() {
                return Err(SessionOperationError::InvalidInput);
            }
            self.require_target_media(target, &items, Some(&item_id(source)?))
                .await?;
        } else if !items.is_empty() {
            self.require_target_media(target, &items, None).await?;
        }
        Ok((
            "GeneralCommand",
            json!({"Name": name, "Arguments": command.arguments,
            "ControllingUserId": caller.user.id}),
        ))
    }
}

/// Navigation and keyboard commands can trigger playback. Only presentation and
/// volume commands may coexist with SyncPlay; adapters do not choose this gate.
fn mutates_playback(message_type: &str, data: &Value) -> bool {
    message_type != "GeneralCommand"
        || !matches!(
            data["Name"].as_str().unwrap_or(""),
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
        )
}
