use anyhow::Result;
use tracing::debug;

use crate::{
    media_storage_service::MediaMapping,
    server_id::ServerId,
    server_storage::Server,
    url_helper::{contains_id, is_id_like, replace_id},
    user_authorization_service::AuthorizationSession,
    virtual_library_service::{VirtualLibraryAccessScope, VirtualLibraryResolution},
    DataContext,
};

pub static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
    "Audio",
    "Shows",
    "Videos",
    "PlayedItems",
    "FavoriteItems",
    "MediaSegments",
    "PlayingItems",
    "Recordings",
    "Channels",
    "Programs",
    "SeriesTimers",
    "Timers",
    "UserFavoriteItems",
    "UserItems",
    "UserPlayedItems",
];

pub static MEDIA_ID_QUERY_TAGS: &[&str] = &[
    "ParentId",
    "ItemId",
    "SeriesId",
    "MediaSourceId",
    "Tag",
    "SeasonId",
    "startItemId",
    "IDs",
    "PersonIds",
];

pub static USER_ID_PATH_TAGS: &[&str] = &["Users"];
pub static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];
pub static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];

pub struct UrlProcessor {
    data_context: DataContext,
}

impl UrlProcessor {
    pub fn new(data_context: DataContext) -> Self {
        Self { data_context }
    }

    pub async fn client_to_server_url(
        &self,
        url: &mut url::Url,
        session: &Option<AuthorizationSession>,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        self.replace_user_ids_in_path(url, session);
        self.replace_media_ids_in_path(url, access_scope, required_server_id)
            .await;

        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        self.replace_session_query_values(&mut pairs, session);
        self.replace_media_ids_in_query(&mut pairs, access_scope, required_server_id)
            .await;

        url.query_pairs_mut().clear().extend_pairs(pairs);
    }

    pub async fn server_to_client_delivery_url(
        &self,
        value: &str,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<Option<String>> {
        let Some((mut url, style)) = parse_delivery_url(value) else {
            return Ok(None);
        };

        self.remap_delivery_url_path(&mut url, server).await?;
        self.remap_delivery_url_query(&mut url, server, proxy_api_key)
            .await?;

        Ok(Some(format_delivery_url(url, style)))
    }

    pub async fn server_from_client_url(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some(server) = self.server_from_path_media_ids(url, access_scope).await? {
            return Ok(Some(server));
        }

        self.server_from_query_media_ids(url, access_scope).await
    }

    fn replace_user_ids_in_path(&self, url: &mut url::Url, session: &Option<AuthorizationSession>) {
        let Some(session) = session else {
            return;
        };

        for &path_segment in USER_ID_PATH_TAGS {
            if let Some(user_id) = contains_id(url, path_segment) {
                debug!(
                    "Replacing user ID in path: {} -> {}",
                    user_id, session.original_user_id
                );
                *url = replace_id(url.clone(), &user_id, &session.original_user_id);
            }
        }
    }

    async fn replace_media_ids_in_path(
        &self,
        url: &mut url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        for &path_segment in MEDIA_ID_PATH_TAGS {
            if let Some(media_id) = contains_id(url, path_segment) {
                if let Some(media_mapping) = self
                    .client_media_mapping(&media_id, access_scope, required_server_id)
                    .await
                {
                    debug!(
                        "Replacing media ID in path: {} -> {}",
                        media_id, media_mapping.original_media_id
                    );
                    *url = replace_id(url.clone(), &media_id, &media_mapping.original_media_id);
                }
            }
        }
    }

    fn replace_session_query_values(
        &self,
        pairs: &mut [(String, String)],
        session: &Option<AuthorizationSession>,
    ) {
        let Some(session) = session else {
            return;
        };

        for (name, value) in pairs {
            if matches_case_insensitive(name, USER_ID_QUERY_TAGS) {
                *value = session.original_user_id.clone();
            } else if matches_case_insensitive(name, API_KEY_QUERY_TAGS) {
                *value = session.jellyfin_token.clone();
            }
        }
    }

    async fn replace_media_ids_in_query(
        &self,
        pairs: &mut [(String, String)],
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) {
        for (name, value) in pairs {
            if !matches_case_insensitive(name, MEDIA_ID_QUERY_TAGS) {
                continue;
            }

            if let Some(resolved_value) = self
                .resolve_client_media_id_list(value, access_scope, required_server_id)
                .await
            {
                *value = resolved_value;
            }
        }
    }

    async fn resolve_client_media_id_list(
        &self,
        value: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Option<String> {
        let mut changed = false;
        let mut resolved_ids = Vec::new();

        for raw_id in value.split(',') {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(media_mapping) = self
                .client_media_mapping(trimmed, access_scope, required_server_id)
                .await
            {
                debug!(
                    "Replacing media ID in query: {} -> {}",
                    trimmed, media_mapping.original_media_id
                );
                resolved_ids.push(media_mapping.original_media_id);
                changed = true;
            } else {
                resolved_ids.push(trimmed.to_string());
            }
        }

        changed.then(|| resolved_ids.join(","))
    }

    async fn remap_delivery_url_path(&self, url: &mut url::Url, server: &Server) -> Result<()> {
        let Some(segments) = url.path_segments() else {
            return Ok(());
        };

        let mut changed = false;
        let mut remapped_segments = Vec::new();

        for segment in segments {
            if is_id_like(segment) {
                remapped_segments.push(self.virtual_media_id(segment, server).await?);
                changed = true;
            } else {
                remapped_segments.push(segment.to_string());
            }
        }

        if changed {
            url.set_path(&remapped_segments.join("/"));
        }

        Ok(())
    }

    async fn remap_delivery_url_query(
        &self,
        url: &mut url::Url,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<()> {
        let Some(query) = url.query() else {
            return Ok(());
        };

        let mut changed = false;
        let mut pairs = Vec::new();

        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let value = if matches_case_insensitive(&key, API_KEY_QUERY_TAGS) {
                if let Some(proxy_api_key) = proxy_api_key {
                    changed = true;
                    proxy_api_key.to_string()
                } else {
                    value.into_owned()
                }
            } else if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                let remapped = self.remap_delivery_url_query_value(&value, server).await?;
                if remapped != value {
                    changed = true;
                }
                remapped
            } else {
                value.into_owned()
            };

            pairs.push((key.into_owned(), value));
        }

        if changed {
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }

        Ok(())
    }

    async fn remap_delivery_url_query_value(&self, value: &str, server: &Server) -> Result<String> {
        let mut changed = false;
        let mut remapped_ids = Vec::new();

        for raw_id in value.split(',') {
            let id = raw_id.trim();
            if id.is_empty() {
                continue;
            }

            if is_id_like(id) {
                remapped_ids.push(self.virtual_media_id(id, server).await?);
                changed = true;
            } else {
                remapped_ids.push(id.to_string());
            }
        }

        if changed {
            Ok(remapped_ids.join(","))
        } else {
            Ok(value.to_string())
        }
    }

    async fn virtual_media_id(&self, id: &str, server: &Server) -> Result<String> {
        self.data_context
            .media_storage
            .get_or_create_media_mapping(id, server)
            .await
            .map(|mapping| mapping.virtual_media_id)
            .map_err(Into::into)
    }

    async fn client_media_mapping(
        &self,
        virtual_media_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Option<MediaMapping> {
        if let Some(mapping) = self
            .data_context
            .media_storage
            .get_media_mapping_by_virtual(virtual_media_id)
            .await
            .unwrap_or_default()
        {
            if !server_is_allowed(mapping.server_id, access_scope, required_server_id) {
                return None;
            }
            return Some(mapping);
        }

        self.data_context
            .virtual_library_service
            .routing_target(virtual_media_id, access_scope, required_server_id)
            .await
            .ok()
            .flatten()
            .map(|target| target.mapping)
    }

    async fn server_from_path_media_ids(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        for &path_segment in MEDIA_ID_PATH_TAGS {
            if let Some(media_id) = contains_id(url, path_segment) {
                debug!("Found {} ID in request: {}", path_segment, media_id);
                if let Some(server) = self
                    .server_from_client_media_id(&media_id, access_scope)
                    .await?
                {
                    debug!(
                        "Found server for {} ID {}: {} ({})",
                        path_segment, media_id, server.name, server.url
                    );
                    return Ok(Some(server));
                }
                debug!("No server found for {} ID: {}", path_segment, media_id);
            }
        }

        Ok(None)
    }

    async fn server_from_query_media_ids(
        &self,
        url: &url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        for (param_name, param_value) in url.query_pairs() {
            if !matches_case_insensitive(&param_name, MEDIA_ID_QUERY_TAGS) {
                continue;
            }

            debug!("Found {} in query: {}", param_name, param_value);
            for raw_id in param_value.split(',') {
                let media_id = raw_id.trim();
                if media_id.is_empty() {
                    continue;
                }

                if let Some(server) = self
                    .server_from_client_media_id(media_id, access_scope)
                    .await?
                {
                    debug!(
                        "Found server for {} {}: {} ({})",
                        param_name, media_id, server.name, server.url
                    );
                    return Ok(Some(server));
                }
                debug!("No server found for {}: {}", param_name, media_id);
            }
        }

        Ok(None)
    }

    async fn server_from_client_media_id(
        &self,
        media_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) -> Result<Option<Server>> {
        if let Some((_mapping, server)) = self
            .data_context
            .media_storage
            .get_media_mapping_with_server(media_id)
            .await?
        {
            if !server_is_allowed(server.id, access_scope, None) {
                return Err(anyhow::anyhow!(
                    "media ID is not available in the current user's server scope"
                ));
            }
            return Ok(Some(server));
        }

        let target = self
            .data_context
            .virtual_library_service
            .routing_target(media_id, access_scope, None)
            .await?;
        if let Some(target) = target {
            return Ok(Some(target.server));
        }

        match self
            .data_context
            .virtual_library_service
            .resolve(media_id, access_scope)
            .await?
        {
            VirtualLibraryResolution::Unknown | VirtualLibraryResolution::Empty(_) => Ok(None),
            VirtualLibraryResolution::Resolved(_) => Err(anyhow::anyhow!(
                "failed to select a routing target for the resolved virtual library"
            )),
        }
    }
}

fn server_is_allowed(
    server_id: ServerId,
    access_scope: Option<&VirtualLibraryAccessScope>,
    required_server_id: Option<ServerId>,
) -> bool {
    required_server_id.is_none_or(|required| required == server_id)
        && access_scope.is_none_or(|scope| scope.allows(server_id))
}

#[derive(Clone, Copy)]
enum DeliveryUrlStyle {
    Absolute,
    RootRelative,
    Relative,
}

fn parse_delivery_url(value: &str) -> Option<(url::Url, DeliveryUrlStyle)> {
    if let Ok(url) = url::Url::parse(value) {
        return Some((url, DeliveryUrlStyle::Absolute));
    }

    let (path, style) = if value.starts_with('/') {
        (value.to_string(), DeliveryUrlStyle::RootRelative)
    } else {
        (format!("/{value}"), DeliveryUrlStyle::Relative)
    };

    url::Url::parse(&format!("http://localhost{path}"))
        .ok()
        .map(|url| (url, style))
}

fn format_delivery_url(url: url::Url, style: DeliveryUrlStyle) -> String {
    match style {
        DeliveryUrlStyle::Absolute => url.to_string(),
        DeliveryUrlStyle::RootRelative => relative_url_from_parts(&url),
        DeliveryUrlStyle::Relative => relative_url_from_parts(&url)
            .strip_prefix('/')
            .unwrap_or(url.path())
            .to_string(),
    }
}

fn relative_url_from_parts(url: &url::Url) -> String {
    let mut value = url.path().to_string();
    if let Some(query) = url.query() {
        value.push('?');
        value.push_str(query);
    }
    if let Some(fragment) = url.fragment() {
        value.push('#');
        value.push_str(fragment);
    }
    value
}

pub fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        config::{AppConfig, MIGRATOR},
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        virtual_library_service::VirtualLibraryService,
    };

    #[tokio::test]
    async fn empty_virtual_library_does_not_force_a_routing_server() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let library = virtual_libraries
            .create_group("Empty library")
            .await
            .unwrap();
        let processor = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        let scope = VirtualLibraryAccessScope::new("user", [ServerId::new(1)]);
        let url = url::Url::parse(&format!(
            "http://localhost/Users/user/Items?ParentId={}",
            library.virtual_id
        ))
        .unwrap();

        let server = processor
            .server_from_client_url(&url, Some(&scope))
            .await
            .unwrap();

        assert!(server.is_none());
    }
}
