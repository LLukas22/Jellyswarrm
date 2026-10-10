use async_trait::async_trait;
use serde_json::Value;
use tracing::debug;

use crate::{
    processors::{
        field_matcher::{
            DELIVERY_URL_FIELDS, DISABLED_BOOL_FIELDS, MEDIA_ID_ARRAY_FIELDS,
            MEDIA_ID_LIST_PARENT_FIELDS, MEDIA_ID_MAP_KEY_FIELDS, MEDIA_ID_MAP_VALUE_FIELDS,
            MEDIA_ID_NESTED_MAP_KEY_FIELDS, RESPONSE_MEDIA_ID_FIELDS, SERVER_ID_FIELDS,
        },
        json_processor::{JsonProcessingContext, JsonProcessingResult, JsonProcessor},
        url_processor::UrlProcessor,
    },
    server_storage::Server,
    DataContext,
};

pub struct ResponseProcessor {
    pub data_context: DataContext,
    url_processor: UrlProcessor,
}

#[derive(Debug, thiserror::Error)]
pub enum PlaylistResponseError {
    #[error("Playlist sharing cannot be represented completely by local users")]
    IncompleteSharing,
    #[error("Invalid playlist sharing response")]
    InvalidSharing,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

impl ResponseProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self {
            url_processor: UrlProcessor::new(data_context.clone()),
            data_context,
        }
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String, String> {
        self.data_context
            .media_storage
            .get_or_create_media_mapping(id, server)
            .await
            .map(|mapping| mapping.virtual_media_id)
            .map_err(|e| format!("failed to create media mapping for {id}: {e}"))
    }

    // Never return a partial permission list that a client could save back as
    // a replacement. Either every recipient is translated or the read fails.
    pub(crate) async fn remap_playlist_users(
        &self,
        payload: &mut Value,
        server: &Server,
    ) -> Result<bool, PlaylistResponseError> {
        if let Value::Array(permissions) = payload {
            return self
                .remap_playlist_permission_list(permissions, server)
                .await;
        }
        let mut modified = false;
        let mut has_permission_list = false;
        let object = payload
            .as_object_mut()
            .ok_or(PlaylistResponseError::InvalidSharing)?;
        for (key, value) in object.iter_mut() {
            if key.eq_ignore_ascii_case("Shares") || key.eq_ignore_ascii_case("Users") {
                has_permission_list = true;
                let permissions = value
                    .as_array_mut()
                    .ok_or(PlaylistResponseError::InvalidSharing)?;
                modified |= self
                    .remap_playlist_permission_list(permissions, server)
                    .await?;
            }
        }
        if payload
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key.eq_ignore_ascii_case("UserId")))
        {
            modified |= self.remap_playlist_permission(payload, server).await?;
        } else if !has_permission_list {
            return Err(PlaylistResponseError::InvalidSharing);
        }
        Ok(modified)
    }

    async fn remap_playlist_permission_list(
        &self,
        permissions: &mut [Value],
        server: &Server,
    ) -> Result<bool, PlaylistResponseError> {
        let mut modified = false;
        for permission in permissions {
            modified |= self.remap_playlist_permission(permission, server).await?;
        }
        Ok(modified)
    }

    async fn remap_playlist_permission(
        &self,
        permission: &mut Value,
        server: &Server,
    ) -> Result<bool, PlaylistResponseError> {
        let (_, value) = permission
            .as_object_mut()
            .and_then(|object| {
                object
                    .iter_mut()
                    .find(|(key, _)| key.eq_ignore_ascii_case("UserId"))
            })
            .ok_or(PlaylistResponseError::InvalidSharing)?;
        let original_id = value
            .as_str()
            .ok_or(PlaylistResponseError::InvalidSharing)?;
        let virtual_id = self
            .data_context
            .user_authorization
            .virtual_user_id_for_backend(server.id, original_id)
            .await?
            .ok_or(PlaylistResponseError::IncompleteSharing)?;
        *value = Value::String(virtual_id);
        Ok(true)
    }

    async fn remap_delivery_url(
        &self,
        value: &str,
        context: &ResponseProcessingContext,
    ) -> Result<Option<String>, String> {
        self.url_processor
            .server_to_client_delivery_url(value, &context.server, context.proxy_api_key.as_deref())
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct ResponseProcessingContext {
    pub server: Server,
    pub proxy_server_id: String,
    pub proxy_api_key: Option<String>,
    pub profile: ResponseProcessingProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseProcessingProfile {
    Media,
    BestEffortMedia,
    PlaylistPermissions,
    Disabled,
}

impl ResponseProcessingProfile {
    pub fn for_proxy_request(method: &hyper::Method, path: &str) -> Self {
        let segments = crate::url_helper::decoded_path_segments(path.trim_end_matches('/'));
        let permission_read = match segments.as_slice() {
            [tag, _] => tag.eq_ignore_ascii_case("Playlists"),
            [tag, _, users] | [tag, _, users, _] => {
                tag.eq_ignore_ascii_case("Playlists") && users.eq_ignore_ascii_case("Users")
            }
            _ => false,
        };
        if method == hyper::Method::GET && permission_read {
            Self::PlaylistPermissions
        } else {
            Self::BestEffortMedia
        }
    }

    pub fn requires_json(self) -> bool {
        matches!(self, Self::PlaylistPermissions)
    }
}

impl ResponseProcessingContext {
    fn rewrites_media_fields(&self) -> bool {
        matches!(
            self.profile,
            ResponseProcessingProfile::Media
                | ResponseProcessingProfile::BestEffortMedia
                | ResponseProcessingProfile::PlaylistPermissions
        )
    }
}

#[async_trait]
impl JsonProcessor<ResponseProcessingContext> for ResponseProcessor {
    async fn process(
        &self,
        json_context: &JsonProcessingContext,
        value: &mut Value,
        context: &ResponseProcessingContext,
    ) -> JsonProcessingResult {
        let mut result = JsonProcessingResult::new();

        if context.profile == ResponseProcessingProfile::Disabled {
            return result;
        }

        if context.rewrites_media_fields()
            && json_context.is_array_item
            && (MEDIA_ID_ARRAY_FIELDS.contains(last_segment(&json_context.parent_path))
                || MEDIA_ID_LIST_PARENT_FIELDS.contains(last_segment(&json_context.parent_path)))
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response array media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_value(&json_context.parent_path) {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!("Replacing response map media ID {} -> {}", id, virtual_id);
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
            return result;
        }

        if context.rewrites_media_fields() && should_remap_map_key(&json_context.parent_path) {
            match self
                .virtual_media_id(&json_context.key, &context.server)
                .await
            {
                Ok(virtual_id) => {
                    debug!(
                        "Replacing response map key media ID {} -> {}",
                        json_context.key, virtual_id
                    );
                    result = result.rename_key(virtual_id);
                }
                Err(e) => result = result.add_error(e),
            }
            return result;
        }

        if context.rewrites_media_fields()
            && RESPONSE_MEDIA_ID_FIELDS.contains(&json_context.key)
            && !is_legacy_unmapped_media_id_field(json_context)
        {
            if let Some(id) = value.as_str().map(str::to_string) {
                match self.virtual_media_id(&id, &context.server).await {
                    Ok(virtual_id) => {
                        debug!(
                            "Replacing response media ID {} -> {} for field {}",
                            id, virtual_id, json_context.key
                        );
                        *value = Value::String(virtual_id);
                        result = result.mark_modified();
                    }
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if DELIVERY_URL_FIELDS.contains(&json_context.key) {
            if let Some(delivery_url) = value.as_str().map(str::to_string) {
                match self.remap_delivery_url(&delivery_url, context).await {
                    Ok(Some(remapped)) => {
                        *value = Value::String(remapped);
                        result = result.mark_modified();
                    }
                    Ok(None) => {}
                    Err(e) => result = result.add_error(e),
                }
            }
        } else if context.rewrites_media_fields()
            && DISABLED_BOOL_FIELDS.contains(&json_context.key)
        {
            let is_playlist = json_context
                .parent_object
                .as_ref()
                .and_then(|object| object.get("Type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("Playlist"));
            if value.is_boolean()
                && !(json_context.key.eq_ignore_ascii_case("CanDelete") && is_playlist)
            {
                *value = Value::Bool(false);
                result = result.mark_modified();
            }
        } else if context.rewrites_media_fields()
            && SERVER_ID_FIELDS.contains(&json_context.key)
            && value.is_string()
        {
            *value = Value::String(context.proxy_server_id.clone());
            result = result.mark_modified();
        }

        result
    }
}

fn should_remap_map_value(parent_path: &str) -> bool {
    MEDIA_ID_MAP_VALUE_FIELDS.contains(last_segment(parent_path))
}

fn should_remap_map_key(parent_path: &str) -> bool {
    MEDIA_ID_MAP_KEY_FIELDS.contains(last_segment(parent_path))
        || parent_contains_nested_map_key_field(parent_path)
}

fn parent_contains_nested_map_key_field(parent_path: &str) -> bool {
    let mut seen_nested_map = false;
    for segment in path_segments(parent_path) {
        if MEDIA_ID_NESTED_MAP_KEY_FIELDS.contains(segment) {
            seen_nested_map = true;
            continue;
        }

        if seen_nested_map {
            return true;
        }
    }

    false
}

fn is_legacy_unmapped_media_id_field(json_context: &JsonProcessingContext) -> bool {
    let is_user_data_item_id = json_context.key.eq_ignore_ascii_case("ItemId")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("UserData"));
    let is_media_source_etag = json_context.key.eq_ignore_ascii_case("Etag")
        && path_segments(&json_context.parent_path)
            .any(|segment| segment.eq_ignore_ascii_case("MediaSources"));

    is_user_data_item_id || is_media_source_etag
}

fn last_segment(path: &str) -> &str {
    path.rsplit('.')
        .next()
        .map(strip_array_index)
        .unwrap_or(path)
}

fn path_segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('.').map(strip_array_index)
}

fn strip_array_index(segment: &str) -> &str {
    segment.split('[').next().unwrap_or(segment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_playlist_permission_reads_require_complete_json_processing() {
        for path in [
            "/Playlists/id",
            "/Playlists/id/Users",
            "/Playlists/id/Users/user/",
            "/%50laylists/id/%55sers",
        ] {
            let profile = ResponseProcessingProfile::for_proxy_request(&hyper::Method::GET, path);
            assert_eq!(profile, ResponseProcessingProfile::PlaylistPermissions);
            assert!(profile.requires_json());
        }
        for (method, path) in [
            (hyper::Method::POST, "/Playlists/id"),
            (hyper::Method::GET, "/Playlists/id/Items"),
            (hyper::Method::GET, "/Playlists/id/InstantMix"),
            (hyper::Method::GET, "/Collections/id"),
        ] {
            let profile = ResponseProcessingProfile::for_proxy_request(&method, path);
            assert_eq!(profile, ResponseProcessingProfile::BestEffortMedia);
            assert!(!profile.requires_json());
        }
    }
}
