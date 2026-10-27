//! Target-bound URL rewriting. Backend selection belongs to application routing.
use super::delivery::{relative_url_from_parts, DeliveryUrlTranslator};
use anyhow::Result;
use tracing::debug;

use crate::{
    auth::service::AuthorizationSession,
    auth::UserAuthorizationService,
    config::SettingsView,
    libraries::service::VirtualLibraryAccessScope,
    libraries::VirtualLibraryService,
    media::storage::{MediaMapping, MediaVersionMember},
    media::MediaStorageService,
    playback::sessions::SessionStorage,
    servers::id::ServerId,
    servers::storage::Server,
    upstream::url::{contains_id, decoded_path_segments, is_id_like, replace_id},
};
use std::sync::Arc;

pub static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
    "Items/RemoteSearch/Apply",
    "Audio",
    "Shows",
    "Videos",
    "Playlists",
    "Collections",
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

pub use super::field_matcher::{matches_case_insensitive, API_KEY_QUERY_TAGS, MEDIA_ID_QUERY_TAGS};

pub static USER_ID_PATH_TAGS: &[&str] = &["Users"];
pub static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];

fn path_media_source_id(url: &url::Url) -> Option<String> {
    decoded_path_segments(url.path())
        .windows(4)
        .find_map(|parts| {
            (parts[0].eq_ignore_ascii_case("Videos")
                && is_id_like(&parts[1])
                && is_id_like(&parts[2])
                && (parts[3].eq_ignore_ascii_case("Subtitles")
                    || parts[3].eq_ignore_ascii_case("Attachments")))
            .then(|| parts[2].clone())
        })
}

/// Path and query source selections have identical authority. Reject conflicting
/// selections before choosing a server or rewriting any IDs.
pub(crate) fn selected_media_source(url: &url::Url) -> Result<Option<String>> {
    let mut selected = None;
    let query_sources = url
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case("MediaSourceId"))
        .flat_map(|(_, value)| value.split(',').map(str::to_string).collect::<Vec<_>>());
    for source in path_media_source_id(url).into_iter().chain(query_sources) {
        let source = source.trim();
        if source.is_empty() {
            continue;
        }
        let source = crate::media::storage::MediaStorageService::normalize_uuid(source);
        if selected
            .as_ref()
            .is_some_and(|previous| previous != &source)
        {
            anyhow::bail!("conflicting media source selections");
        }
        selected = Some(source);
    }
    Ok(selected)
}

pub(crate) fn play_session_id(url: &url::Url) -> Result<Option<String>> {
    let mut selected = None;
    for (key, value) in url.query_pairs() {
        if !(key.eq_ignore_ascii_case("PlaySessionId") || key.eq_ignore_ascii_case("SessionId"))
            || value.is_empty()
        {
            continue;
        }
        if selected
            .as_deref()
            .is_some_and(|previous| previous != value)
        {
            anyhow::bail!("conflicting playback session IDs");
        }
        selected = Some(value.into_owned());
    }
    Ok(selected)
}

#[derive(Clone)]
pub struct UrlProcessor {
    media_storage: Arc<MediaStorageService>,
    virtual_library_service: Arc<VirtualLibraryService>,
    play_sessions: Arc<SessionStorage>,
    user_ids: super::user_ids::BackendUserIds,
    config: SettingsView,
}

impl UrlProcessor {
    pub(crate) fn new(
        media_storage: Arc<MediaStorageService>,
        virtual_library_service: Arc<VirtualLibraryService>,
        play_sessions: Arc<SessionStorage>,
        authorization: Arc<UserAuthorizationService>,
        config: SettingsView,
    ) -> Self {
        Self {
            media_storage,
            virtual_library_service,
            play_sessions,
            user_ids: super::user_ids::BackendUserIds::new(authorization),
            config,
        }
    }

    pub async fn client_to_server_url(
        &self,
        url: &mut url::Url,
        session: Option<&AuthorizationSession>,
        access_scope: Option<&VirtualLibraryAccessScope>,
        server_id: ServerId,
    ) -> Result<()> {
        let required_server_id = Some(server_id);
        self.replace_user_ids_in_path(url, session, required_server_id)
            .await?;
        self.replace_media_ids_in_path(url, access_scope, required_server_id)
            .await?;

        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();

        self.replace_session_query_values(&mut pairs, session, required_server_id)
            .await?;
        self.replace_media_ids_in_query(&mut pairs, access_scope, required_server_id)
            .await?;

        url.query_pairs_mut().clear().extend_pairs(pairs);
        Ok(())
    }

    pub async fn server_to_client_delivery_url(
        &self,
        value: &str,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<Option<String>> {
        DeliveryUrlTranslator::new(self.media_storage.clone())
            .translate(value, server, proxy_api_key)
            .await
    }

    /// HLS references are resolved against the upstream playlist, then returned
    /// as proxy-root paths. Never send a proxy credential to an external origin.
    pub(crate) async fn server_to_client_hls_url(
        &self,
        value: &str,
        playlist: &url::Url,
        server: &Server,
        proxy_api_key: Option<&str>,
        viewer: Option<&str>,
    ) -> Result<String> {
        let mut url = playlist.join(value)?;
        if url.origin() != server.url.origin() {
            return Ok(value.to_string());
        }
        self.remap_delivery_url_path(&mut url, server).await?;
        self.remap_delivery_url_query(&mut url, server, proxy_api_key)
            .await?;

        let mut pairs = Vec::new();
        for (key, value) in url.query_pairs() {
            let value = if key.eq_ignore_ascii_case("LiveStreamId") {
                match viewer {
                    Some(user) => self
                        .play_sessions
                        .register_live_binding(&value.as_ref().into(), &user.into(), server.id)
                        .await
                        .as_str()
                        .to_owned(),
                    None => continue,
                }
            } else {
                value.into_owned()
            };
            pairs.push((key.into_owned(), value));
        }
        if let Some(key) = proxy_api_key {
            if !pairs
                .iter()
                .any(|(key, _)| matches_case_insensitive(key, API_KEY_QUERY_TAGS))
            {
                pairs.push(("ApiKey".into(), key.into()));
            }
        }
        url.query_pairs_mut().clear().extend_pairs(pairs);

        let backend_prefix = server.url.path().trim_end_matches('/');
        let path = url
            .path()
            .strip_prefix(backend_prefix)
            .filter(|path| path.is_empty() || path.starts_with('/'))
            .unwrap_or(url.path());
        let prefix = self
            .config
            .read()
            .await
            .url_prefix
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let path = if prefix.trim_matches('/').is_empty() {
            path.to_string()
        } else {
            format!(
                "/{}/{}",
                prefix.trim_matches('/'),
                path.trim_start_matches('/')
            )
        };
        url.set_path(&path);
        Ok(relative_url_from_parts(&url))
    }

    pub async fn upstream_user_id(
        &self,
        user_id: &str,
        session: Option<&AuthorizationSession>,
        server_id: ServerId,
    ) -> Result<String> {
        self.user_ids
            .resolve(user_id, session, server_id)
            .await
            .map_err(|error| match error {
                super::user_ids::UserIdError::Storage(cause) => cause.into(),
                super::user_ids::UserIdError::MissingMapping => error.into(),
            })
    }

    pub async fn validate_container_request(
        &self,
        url: &url::Url,
        session: Option<&AuthorizationSession>,
        scope: Option<&VirtualLibraryAccessScope>,
        server_id: ServerId,
        body_media_ids: &[String],
    ) -> Result<()> {
        let segments = decoded_path_segments(url.path());
        let is_playlist = segments
            .iter()
            .any(|part| part.eq_ignore_ascii_case("Playlists"));
        let is_collection = segments
            .iter()
            .any(|part| part.eq_ignore_ascii_case("Collections"));
        if !is_playlist && !is_collection {
            return Ok(());
        }
        let unavailable = if is_playlist {
            "Playlist track or entry is unavailable on the selected server; mixed-server playlists are unsupported"
        } else {
            "Collection item is unavailable on the selected server; mixed-server collections are unsupported"
        };
        // Check container IDs even when they are malformed, since contains_id
        // intentionally only matches UUID-like path segments.
        for pair in segments.windows(2) {
            if (pair[0].eq_ignore_ascii_case("Playlists")
                || pair[0].eq_ignore_ascii_case("Collections"))
                && self
                    .client_media_mapping(&pair[1], scope, Some(server_id))
                    .await?
                    .is_none()
            {
                anyhow::bail!(unavailable);
            }
        }
        for tag in MEDIA_ID_PATH_TAGS {
            if let Some(id) = contains_id(url, tag) {
                if self
                    .client_media_mapping(&id, scope, Some(server_id))
                    .await?
                    .is_none()
                {
                    anyhow::bail!(unavailable);
                }
            }
        }
        for (key, value) in url.query_pairs() {
            if matches_case_insensitive(&key, USER_ID_QUERY_TAGS) {
                self.upstream_user_id(&value, session, server_id).await?;
            }
            if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
                for id in value.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                    if self
                        .client_media_mapping(id, scope, Some(server_id))
                        .await?
                        .is_none()
                    {
                        anyhow::bail!(unavailable);
                    }
                }
            }
        }
        for id in body_media_ids {
            if self
                .client_media_mapping(id, scope, Some(server_id))
                .await?
                .is_none()
            {
                anyhow::bail!(unavailable);
            }
        }
        // Validate even malformed recipient IDs: contains_id deliberately skips
        // non-UUID path values, which must not bypass sharing validation.
        if let Some(pair) = segments
            .windows(2)
            .find(|pair| pair[0].eq_ignore_ascii_case("Users"))
        {
            self.upstream_user_id(&pair[1], session, server_id).await?;
        }
        Ok(())
    }

    async fn replace_user_ids_in_path(
        &self,
        url: &mut url::Url,
        session: Option<&AuthorizationSession>,
        server_id: Option<ServerId>,
    ) -> Result<()> {
        for &tag in USER_ID_PATH_TAGS {
            if let (Some(user_id), Some(server_id)) = (contains_id(url, tag), server_id) {
                match self.user_ids.resolve(&user_id, session, server_id).await {
                    Ok(upstream) => *url = replace_id(url.clone(), &user_id, &upstream),
                    Err(super::user_ids::UserIdError::Storage(error)) => return Err(error.into()),
                    Err(super::user_ids::UserIdError::MissingMapping) => {}
                }
            }
        }
        Ok(())
    }

    async fn replace_media_ids_in_path(
        &self,
        url: &mut url::Url,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<()> {
        // These endpoints carry a second ID after the item. Capture it before
        // rewriting the item (item/source IDs may be identical on the backend).
        let source_id = path_media_source_id(url);
        let selected_source = selected_media_source(url)?;
        for &path_segment in MEDIA_ID_PATH_TAGS {
            if let Some(media_id) = contains_id(url, path_segment) {
                if let Some(source) = selected_source.as_deref() {
                    if let Some(group) = self
                        .media_storage
                        .get_media_version_group(&media_id)
                        .await?
                    {
                        if let Some(route) = self
                            .media_storage
                            .get_media_version_source_route(group.id, source)
                            .await?
                        {
                            if server_is_allowed(
                                route.member_mapping.server_id,
                                access_scope,
                                required_server_id,
                            ) {
                                *url = replace_id(
                                    url.clone(),
                                    &media_id,
                                    &route.member_mapping.original_media_id,
                                );
                                continue;
                            }
                        }
                    }
                }
                if let Some(media_mapping) = self
                    .client_media_mapping(&media_id, access_scope, required_server_id)
                    .await?
                {
                    debug!(
                        "Replacing media ID in path: {} -> {}",
                        media_id, media_mapping.original_media_id
                    );
                    *url = replace_id(url.clone(), &media_id, &media_mapping.original_media_id);
                }
            }
        }
        if let Some(source_id) = source_id {
            if let Some(mapping) = self
                .client_media_mapping(&source_id, access_scope, required_server_id)
                .await?
            {
                *url = replace_id(url.clone(), &source_id, &mapping.original_media_id);
            }
        }
        Ok(())
    }

    async fn replace_session_query_values(
        &self,
        pairs: &mut [(String, String)],
        session: Option<&AuthorizationSession>,
        server_id: Option<ServerId>,
    ) -> Result<()> {
        let session_option = session;
        let Some(session) = session else {
            return Ok(());
        };

        for (name, value) in pairs {
            if matches_case_insensitive(name, USER_ID_QUERY_TAGS) {
                if let Some(server_id) = server_id {
                    match self
                        .user_ids
                        .resolve(value, session_option, server_id)
                        .await
                    {
                        Ok(upstream) => *value = upstream,
                        Err(super::user_ids::UserIdError::Storage(error)) => {
                            return Err(error.into())
                        }
                        Err(super::user_ids::UserIdError::MissingMapping) => {}
                    }
                }
            } else if matches_case_insensitive(name, API_KEY_QUERY_TAGS) {
                *value = session.jellyfin_token.clone();
            } else if name.eq_ignore_ascii_case("LiveStreamId") {
                if let Some(binding) = self
                    .play_sessions
                    .live_binding(&value.as_str().into(), &session.user_id.as_str().into())
                    .await
                {
                    if Some(binding.server_id) == server_id {
                        *value = binding.original_id.as_str().to_owned();
                    }
                }
            }
        }
        Ok(())
    }

    async fn replace_media_ids_in_query(
        &self,
        pairs: &mut [(String, String)],
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<()> {
        for (name, value) in pairs {
            if !matches_case_insensitive(name, MEDIA_ID_QUERY_TAGS) {
                continue;
            }

            if let Some(resolved_value) = self
                .resolve_client_media_id_list(value, access_scope, required_server_id)
                .await?
            {
                *value = resolved_value;
            }
        }
        Ok(())
    }

    async fn resolve_client_media_id_list(
        &self,
        value: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> Result<Option<String>> {
        let mut changed = false;
        let mut resolved_ids = Vec::new();

        for raw_id in value.split(',') {
            let trimmed = raw_id.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(media_mapping) = self
                .client_media_mapping(trimmed, access_scope, required_server_id)
                .await?
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

        Ok(changed.then(|| resolved_ids.join(",")))
    }

    async fn remap_delivery_url_path(&self, url: &mut url::Url, server: &Server) -> Result<()> {
        DeliveryUrlTranslator::new(self.media_storage.clone())
            .remap_path(url, server)
            .await
    }

    async fn remap_delivery_url_query(
        &self,
        url: &mut url::Url,
        server: &Server,
        proxy_api_key: Option<&str>,
    ) -> Result<()> {
        DeliveryUrlTranslator::new(self.media_storage.clone())
            .remap_query(url, server, proxy_api_key)
            .await
    }

    pub(crate) async fn client_media_mapping(
        &self,
        virtual_media_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> std::result::Result<Option<MediaMapping>, sqlx::Error> {
        // Translation never selects a backend. An already resolved target is
        // required even when the incoming identity is an aggregate/library.
        let Some(server_id) = required_server_id else {
            return Ok(None);
        };
        let required_server_id = Some(server_id);
        if let Some(mapping) = self
            .media_storage
            .get_media_mapping_by_virtual(virtual_media_id)
            .await?
        {
            if !server_is_allowed(mapping.server_id, access_scope, required_server_id) {
                return Ok(None);
            }
            return Ok(Some(mapping));
        }

        if let Some(member) = self
            .media_version_member(virtual_media_id, access_scope, required_server_id)
            .await?
        {
            return Ok(Some(member.mapping));
        }

        Ok(self
            .virtual_library_service
            .routing_target(virtual_media_id, access_scope, required_server_id)
            .await?
            .map(|target| target.mapping))
    }

    async fn media_version_member(
        &self,
        group_virtual_id: &str,
        access_scope: Option<&VirtualLibraryAccessScope>,
        required_server_id: Option<ServerId>,
    ) -> std::result::Result<Option<MediaVersionMember>, sqlx::Error> {
        let Some(server_id) = required_server_id else {
            return Ok(None);
        };
        Ok(self
            .media_storage
            .get_media_version_members_by_virtual_id(group_virtual_id)
            .await?
            .into_iter()
            .filter(|member| {
                member.server.id == server_id
                    && server_is_allowed(server_id, access_scope, Some(server_id))
            })
            // Multiple copies on the selected server keep the historical
            // stable mapping tie-break, without consulting availability.
            .max_by(|left, right| {
                left.mapping
                    .original_media_id
                    .cmp(&right.mapping.original_media_id)
            }))
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

#[cfg(test)]
mod tests {
    use crate::{
        auth::{service::User, user_id::UserId},
        models::Authorization,
        DataContext,
    };

    #[tokio::test]
    async fn translation_distinguishes_unknown_ids_from_storage_failure() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let media = MediaStorageService::new(pool.clone());
        let processor = UrlProcessor::new(
            Arc::new(media.clone()),
            Arc::new(VirtualLibraryService::new(pool.clone(), servers, media)),
            Arc::new(SessionStorage::new()),
            Arc::new(UserAuthorizationService::new(pool.clone())),
            SettingsView::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default()))),
        );
        let id = "0123456789abcdef0123456789abcdef";
        let server = ServerId::new(1);
        assert!(processor
            .client_media_mapping(id, None, Some(server))
            .await
            .unwrap()
            .is_none());
        pool.close().await;
        let error = processor
            .client_media_mapping(id, None, Some(server))
            .await
            .unwrap_err();
        assert!(matches!(error, sqlx::Error::PoolClosed));
        for path in [format!("/Items/{id}"), format!("/Items?Ids={id}")] {
            let mut url = url::Url::parse(&format!("http://proxy.example{path}")).unwrap();
            let error = processor
                .client_to_server_url(&mut url, None, None, server)
                .await
                .unwrap_err();
            assert!(error.is::<sqlx::Error>());
        }
    }

    async fn user_mapping_fixture() -> (UrlProcessor, User, Vec<(AuthorizationSession, Server)>) {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let media_storage = MediaStorageService::new(pool.clone());
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let user_authorization = UserAuthorizationService::new(pool);
        let user = user_authorization
            .create_user("viewer", &"password".into())
            .await
            .unwrap();
        for name in ["first", "second"] {
            let id = server_storage
                .add_server(
                    name,
                    &format!("http://{name}.example"),
                    100,
                    MediaStreamingMode::Redirect,
                )
                .await
                .unwrap();
            let server = server_storage.get_server_by_id(id).await.unwrap().unwrap();
            user_authorization
                .add_server_mapping(
                    &user.id,
                    &server,
                    "viewer",
                    &"password".into(),
                    Some(&user.local_credential.mapping_key()),
                )
                .await
                .unwrap();
            let upstream_id = uuid::Uuid::new_v4();
            let upstream_id = if name == "first" {
                upstream_id.simple().to_string()
            } else {
                upstream_id.hyphenated().to_string().to_uppercase()
            };
            user_authorization
                .store_authorization_session(
                    &user.id,
                    &server,
                    &Authorization {
                        client: "Wholphin".into(),
                        device: "Android TV".into(),
                        device_id: "test-device".into(),
                        version: "test".into(),
                        token: None,
                    },
                    format!("{name}-token"),
                    upstream_id,
                    None,
                )
                .await
                .unwrap();
        }
        let sessions = user_authorization
            .get_user_sessions_by_user_id(&user.id)
            .await
            .unwrap()
            .unwrap()
            .1;
        let processor = UrlProcessor::from(DataContext {
            user_authorization: Arc::new(user_authorization),
            server_storage: Arc::new(server_storage),
            media_storage: Arc::new(media_storage),
            virtual_library_service: Arc::new(virtual_libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        (processor, user, sessions)
    }

    fn uuid_variants(value: &str) -> Vec<String> {
        let id = uuid::Uuid::parse_str(value).unwrap();
        vec![
            id.simple().to_string(),
            id.hyphenated().to_string(),
            id.simple().to_string().to_uppercase(),
            id.hyphenated().to_string().to_uppercase(),
        ]
    }

    #[tokio::test]
    async fn user_uuid_formats_are_remapped_for_each_upstream() {
        let (processor, user, sessions) = user_mapping_fixture().await;
        assert_eq!(sessions.len(), 2);
        assert_ne!(
            sessions[0].0.original_user_id,
            sessions[1].0.original_user_id
        );
        for (session, server) in sessions {
            for id in uuid_variants(&user.id)
                .into_iter()
                .chain(uuid_variants(&session.original_user_id))
            {
                for endpoint in [
                    "/UserViews",
                    "/UserItems/Resume",
                    "/UserImage",
                    "/DisplayPreferences/default",
                ] {
                    for key in ["userId", "UserId", "USERID"] {
                        let mut url = url::Url::parse(&format!("http://localhost{endpoint}?{key}={id}&fields=Overview&fields=SortName&client=Wholphin")).unwrap();
                        processor
                            .client_to_server_url(&mut url, Some(&session), None, server.id)
                            .await
                            .unwrap();
                        let pairs = url.query_pairs().collect::<Vec<_>>();
                        assert_eq!(pairs[0].0, key);
                        assert_eq!(
                            pairs[0].1, session.original_user_id,
                            "{endpoint}: {key}={id}"
                        );
                        assert_eq!(
                            &pairs[1..],
                            &[
                                ("fields".into(), "Overview".into()),
                                ("fields".into(), "SortName".into()),
                                ("client".into(), "Wholphin".into())
                            ]
                        );
                    }
                }
                let mut url =
                    url::Url::parse(&format!("http://localhost/Users/{id}/Views")).unwrap();
                processor
                    .client_to_server_url(&mut url, Some(&session), None, server.id)
                    .await
                    .unwrap();
                assert_eq!(
                    url.path(),
                    format!("/Users/{}/Views", session.original_user_id)
                );
            }
        }
    }

    #[tokio::test]
    async fn recipient_uuid_formats_keep_server_specific_mapping_and_reject_unknown_users() {
        let (processor, user, sessions) = user_mapping_fixture().await;
        for (session, server) in sessions {
            let mut caller = session.clone();
            caller.user_id = UserId::new(uuid::Uuid::new_v4().simple().to_string());
            caller.original_user_id = "caller-upstream-id".into();
            for id in uuid_variants(&user.id) {
                assert_eq!(
                    processor
                        .upstream_user_id(&id, Some(&caller), server.id)
                        .await
                        .unwrap(),
                    session.original_user_id
                );
                let mut url = url::Url::parse(&format!(
                    "http://localhost/Playlists/list/Users/{id}?userId={id}"
                ))
                .unwrap();
                processor
                    .client_to_server_url(&mut url, Some(&caller), None, server.id)
                    .await
                    .unwrap();
                assert_eq!(
                    url.path(),
                    format!("/Playlists/list/Users/{}", session.original_user_id)
                );
                assert_eq!(
                    url.query_pairs().next().unwrap().1,
                    session.original_user_id
                );
                assert!(processor
                    .upstream_user_id(&id, Some(&caller), ServerId::new(i64::MAX))
                    .await
                    .is_err());
            }
            for unknown in [
                "unknown".to_string(),
                "not-a-valid-uuid".to_string(),
                uuid::Uuid::new_v4().hyphenated().to_string(),
            ] {
                assert!(processor
                    .upstream_user_id(&unknown, Some(&caller), server.id)
                    .await
                    .is_err());
            }
            assert_eq!(
                processor
                    .upstream_user_id("caller-upstream-id", Some(&caller), server.id)
                    .await
                    .unwrap(),
                "caller-upstream-id"
            );
        }
    }
    use std::sync::Arc;

    use sqlx::SqlitePool;

    use super::*;
    use crate::{
        auth::service::UserAuthorizationService,
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        libraries::service::VirtualLibraryService,
        media::storage::MediaStorageService,
        playback::sessions::SessionStorage,
        servers::storage::ServerStorageService,
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
        let processor = crate::application::url_selection::UrlSelection::new(
            Arc::new(media_storage),
            Arc::new(server_storage),
            Arc::new(virtual_libraries),
        );
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

    #[tokio::test]
    async fn image_tag_remains_opaque_while_item_id_is_remapped() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-id", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(
            Arc::new(media_storage),
            Arc::new(virtual_libraries),
            Arc::new(SessionStorage::new()),
            Arc::new(UserAuthorizationService::new(pool)),
            SettingsView::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default()))),
        );
        let mut url = url::Url::parse(&format!(
            "http://localhost/Items/{}/Images/Primary?tag={}",
            mapping.virtual_media_id, mapping.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, None, None, server_id)
            .await
            .unwrap();

        assert_eq!(url.path(), "/Items/upstream-id/Images/Primary");
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("tag"))
                .map(|(_, value)| value.into_owned()),
            Some(mapping.virtual_media_id)
        );
    }

    #[tokio::test]
    async fn artist_ids_query_param_is_remapped_to_real_upstream_id() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let mapping = media_storage
            .get_or_create_media_mapping("upstream-id", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = UrlProcessor::new(
            Arc::new(media_storage),
            Arc::new(virtual_libraries),
            Arc::new(SessionStorage::new()),
            Arc::new(UserAuthorizationService::new(pool)),
            SettingsView::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default()))),
        );
        let mut url = url::Url::parse(&format!(
            "http://localhost/Items?ContributingArtistIds={}",
            mapping.virtual_media_id
        ))
        .unwrap();

        processor
            .client_to_server_url(&mut url, None, None, server_id)
            .await
            .unwrap();

        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("ContributingArtistIds"))
                .map(|(_, value)| value.into_owned()),
            Some(mapping.original_media_id)
        );
    }

    #[tokio::test]
    async fn explicit_media_source_selects_server_before_path_item() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let first_id = server_storage
            .add_server(
                "First",
                "http://first.example",
                200,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let second_id = server_storage
            .add_server(
                "Second",
                "http://second.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let first = server_storage
            .get_server_by_id(first_id)
            .await
            .unwrap()
            .unwrap();
        let second = server_storage
            .get_server_by_id(second_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let item = media_storage
            .get_or_create_media_mapping("item", &first)
            .await
            .unwrap();
        let source = media_storage
            .get_or_create_media_mapping("source", &second)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let processor = crate::application::url_selection::UrlSelection::new(
            Arc::new(media_storage),
            Arc::new(server_storage),
            Arc::new(virtual_libraries),
        );
        let url = url::Url::parse(&format!(
            "http://localhost/Videos/{}/stream?MediaSourceId={}",
            item.virtual_media_id, source.virtual_media_id
        ))
        .unwrap();

        let selected = processor
            .server_from_client_url(&url, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(selected.id, second.id);
    }

    #[tokio::test]
    async fn playlist_path_and_entry_ids_are_remapped_to_upstream_ids() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let server_storage = ServerStorageService::new(pool.clone());
        let server_id = server_storage
            .add_server(
                "Server",
                "http://server.example",
                100,
                MediaStreamingMode::Redirect,
            )
            .await
            .unwrap();
        let server = server_storage
            .get_server_by_id(server_id)
            .await
            .unwrap()
            .unwrap();
        let media_storage = MediaStorageService::new(pool.clone());
        let playlist = media_storage
            .get_or_create_media_mapping("upstream-playlist", &server)
            .await
            .unwrap();
        let entry = media_storage
            .get_or_create_media_mapping("upstream-entry", &server)
            .await
            .unwrap();
        let virtual_libraries =
            VirtualLibraryService::new(pool.clone(), server_storage.clone(), media_storage.clone());
        let selection = crate::application::url_selection::UrlSelection::new(
            Arc::new(media_storage.clone()),
            Arc::new(server_storage.clone()),
            Arc::new(virtual_libraries.clone()),
        );
        let processor = UrlProcessor::new(
            Arc::new(media_storage),
            Arc::new(virtual_libraries),
            Arc::new(SessionStorage::new()),
            Arc::new(UserAuthorizationService::new(pool)),
            SettingsView::new(Arc::new(tokio::sync::RwLock::new(AppConfig::default()))),
        );

        // Routing must find the owning server from the playlist path ID.
        let url = url::Url::parse(&format!(
            "http://localhost/Playlists/{}/Items?UserId=user",
            playlist.virtual_media_id
        ))
        .unwrap();
        let selected = selection
            .server_from_client_url(&url, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selected.id, server.id);

        // Path and EntryIds query values must be rewritten to upstream IDs.
        let mut url = url::Url::parse(&format!(
            "http://localhost/Playlists/{}/Items?EntryIds={}",
            playlist.virtual_media_id, entry.virtual_media_id
        ))
        .unwrap();
        processor
            .client_to_server_url(&mut url, None, None, server_id)
            .await
            .unwrap();
        assert_eq!(url.path(), "/Playlists/upstream-playlist/Items");
        assert_eq!(
            url.query_pairs()
                .find(|(key, _)| key.eq_ignore_ascii_case("EntryIds"))
                .map(|(_, value)| value.into_owned()),
            Some("upstream-entry".to_string())
        );
    }
}
