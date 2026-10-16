//! Target-bound request construction. This module never selects a backend.
use crate::{
    application::context::AuthorizedTarget, processors::url_processor::UrlProcessor,
    proxy_headers::remove_hop_by_hop_headers, request_preprocessing::JellyfinAuthorization,
    server_storage::Server, url_helper::join_server_url,
    user_authorization_service::AuthorizationSession,
    virtual_library_service::VirtualLibraryAccessScope,
};

#[derive(Debug, thiserror::Error)]
#[error("upstream request body cannot be cloned")]
pub(crate) struct UpstreamBuildError;

pub(crate) struct UpstreamRequestBuilder<'a> {
    urls: &'a UrlProcessor,
    proxy_prefix: Option<&'a str>,
}

impl<'a> UpstreamRequestBuilder<'a> {
    pub(crate) fn new(urls: &'a UrlProcessor, proxy_prefix: Option<&'a str>) -> Self {
        Self { urls, proxy_prefix }
    }

    /// Input must be the normalized client operation, not a previously rewritten
    /// backend URL. Unknown path/query/body extension fields are retained.
    /// Every translated media field must still contain its client/member virtual
    /// ID, including target-specific parents. A backend ID can itself equal a
    /// different virtual ID; an "already resolved" string is not safe input.
    /// JSON bodies are retained here, not translated a second time.
    pub(crate) async fn build(
        &self,
        target: &AuthorizedTarget,
        original: &reqwest::Request,
    ) -> Result<reqwest::Request, UpstreamBuildError> {
        let mut request = original.try_clone().ok_or(UpstreamBuildError)?;
        self.apply(
            &mut request,
            target.server(),
            target.session(),
            target.authorization(),
            target.access_scope(),
        )
        .await;
        Ok(request)
    }

    /// Shared wire mechanics. Historical preprocessing fixtures also use this
    /// entry point; production callers use build with an AuthorizedTarget.
    pub(crate) async fn apply(
        &self,
        request: &mut reqwest::Request,
        server: &Server,
        session: &Option<AuthorizationSession>,
        auth: &Option<JellyfinAuthorization>,
        access_scope: Option<&VirtualLibraryAccessScope>,
    ) {
        remove_hop_by_hop_headers(request.headers_mut());
        if let Some(host) = server.url.host_str() {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(host) {
                request.headers_mut().insert(reqwest::header::HOST, value);
            }
        }
        apply_authorization_header(request, auth);
        let mut url = request.url().clone();
        self.urls
            .client_to_server_url(&mut url, session, access_scope, Some(server.id))
            .await;
        let path = match self.proxy_prefix {
            Some(prefix) => url.path().trim_start_matches(&format!("/{prefix}")),
            None => url.path(),
        };
        let mut destination = join_server_url(&server.url, path);
        destination.set_query(url.query());
        *request.url_mut() = destination;
    }
}

fn apply_authorization_header(
    request: &mut reqwest::Request,
    auth: &Option<JellyfinAuthorization>,
) {
    for name in [
        "authorization",
        "x-emby-authorization",
        "x-emby-token",
        "x-mediabrowser-token",
    ] {
        request.headers_mut().remove(name);
    }
    let Some(auth) = auth else {
        return;
    };
    let (name, value) = match auth {
        JellyfinAuthorization::Authorization(auth)
        | JellyfinAuthorization::XEmbyAuthorization(auth) => {
            (reqwest::header::AUTHORIZATION, auth.to_header_value())
        }
        JellyfinAuthorization::XMediaBrowser(token) => (
            reqwest::header::HeaderName::from_static("x-mediabrowser-token"),
            token.clone(),
        ),
        JellyfinAuthorization::XEmbyToken(token) => (
            reqwest::header::HeaderName::from_static("x-emby-token"),
            token.clone(),
        ),
        JellyfinAuthorization::ApiKey(_) => return,
    };
    if let Ok(value) = reqwest::header::HeaderValue::from_str(&value) {
        request.headers_mut().insert(name, value);
    }
}

pub(crate) fn remap_authorization(
    auth: &Option<JellyfinAuthorization>,
    session: &Option<AuthorizationSession>,
) -> Option<JellyfinAuthorization> {
    let auth = auth.as_ref()?;
    let session = session.as_ref()?;
    Some(match auth {
        JellyfinAuthorization::Authorization(_)
        | JellyfinAuthorization::XEmbyToken(_)
        | JellyfinAuthorization::XEmbyAuthorization(_) => {
            JellyfinAuthorization::Authorization(session.to_authorization())
        }
        JellyfinAuthorization::XMediaBrowser(_) => {
            JellyfinAuthorization::XMediaBrowser(session.jellyfin_token.clone())
        }
        JellyfinAuthorization::ApiKey(_) => {
            JellyfinAuthorization::ApiKey(session.jellyfin_token.clone())
        }
    })
}

#[cfg(test)]
mod tests {
    use hyper as http;
    use std::sync::Arc;

    use super::*;
    use crate::{
        config::{AppConfig, MediaStreamingMode, MIGRATOR},
        media_storage_service::MediaStorageService,
        server_storage::ServerStorageService,
        session_storage::SessionStorage,
        user_authorization_service::UserAuthorizationService,
        virtual_library_service::VirtualLibraryService,
        DataContext,
    };

    async fn fixture() -> (UrlProcessor, MediaStorageService, Server, Server) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let servers = ServerStorageService::new(pool.clone());
        let mut backends = Vec::new();
        for (name, url, priority) in [
            ("Preferred sibling", "http://sibling.example", 200),
            (
                "Chosen owner",
                "http://chosen.example/backend/jellyfin",
                100,
            ),
        ] {
            let id = servers
                .add_server(name, url, priority, MediaStreamingMode::Redirect)
                .await
                .unwrap();
            backends.push(servers.get_server_by_id(id).await.unwrap().unwrap());
        }
        let media = MediaStorageService::new(pool.clone());
        let libraries = VirtualLibraryService::new(pool.clone(), servers.clone(), media.clone());
        let urls = UrlProcessor::new(DataContext {
            user_authorization: Arc::new(UserAuthorizationService::new(pool)),
            server_storage: Arc::new(servers),
            media_storage: Arc::new(media.clone()),
            virtual_library_service: Arc::new(libraries),
            play_sessions: Arc::new(SessionStorage::new()),
            config: Arc::new(tokio::sync::RwLock::new(AppConfig::default())),
        });
        (urls, media, backends.remove(0), backends.remove(0))
    }

    #[tokio::test]
    async fn build_preserves_original_and_extensions_while_translating_ids_once() {
        let (urls, media, sibling, chosen) = fixture().await;
        // Deliberately make the upstream item ID another valid virtual ID. A
        // second translation would incorrectly turn it into "translated-twice".
        let decoy = media
            .get_or_create_media_mapping("translated-twice", &chosen)
            .await
            .unwrap();
        let item = media
            .get_or_create_media_mapping(&decoy.virtual_media_id, &chosen)
            .await
            .unwrap();
        let source_decoy = media
            .get_or_create_media_mapping("source-translated-twice", &chosen)
            .await
            .unwrap();
        let source = media
            .get_or_create_media_mapping(&source_decoy.virtual_media_id, &chosen)
            .await
            .unwrap();
        let foreign = media
            .get_or_create_media_mapping("sibling-only", &sibling)
            .await
            .unwrap();
        let target = AuthorizedTarget::new(
            chosen.clone(),
            None,
            Some(JellyfinAuthorization::XMediaBrowser("chosen-token".into())),
            Some(VirtualLibraryAccessScope::new(
                "viewer",
                [chosen.id, sibling.id],
            )),
        )
        .unwrap();
        let raw_url = format!(
            "http://proxy.example/jelly/Videos/{}/stream?MediaSourceId={}&ItemId={}&Extension=one%2Ftwo&Extension=three&IDs={}",
            item.virtual_media_id, source.virtual_media_id, item.virtual_media_id, foreign.virtual_media_id);
        let body = format!(
            r#"{{"ItemId":"{}","Extension":{{"opaque":[1,2,3]}}}}"#,
            item.virtual_media_id
        );
        #[derive(Clone, Debug, PartialEq)]
        struct ExtensionMarker(&'static str);
        let raw = http::Request::builder()
            .method(http::Method::POST)
            .uri(raw_url.as_str())
            .version(http::Version::HTTP_11)
            .extension(ExtensionMarker("opaque request metadata"))
            .body(body.clone())
            .unwrap();
        let mut original = reqwest::Request::try_from(raw).unwrap();
        for (name, value) in [
            ("host", "proxy.example"),
            ("connection", "keep-alive"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "proxy-secret"),
            ("authorization", "client-secret"),
            ("x-emby-authorization", "legacy-secret"),
            ("x-emby-token", "client-token"),
            ("x-mediabrowser-token", "old-token"),
            ("x-extension", "opaque-header"),
            ("content-type", "application/json"),
        ] {
            original.headers_mut().insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        let original_headers = original.headers().clone();
        let builder = UpstreamRequestBuilder::new(&urls, Some("jelly"));
        for _ in 0..2 {
            let built = builder.build(&target, &original).await.unwrap();
            assert_eq!(built.url().host_str(), Some("chosen.example"));
            assert_eq!(
                built.url().path(),
                format!("/backend/jellyfin/Videos/{}/stream", decoy.virtual_media_id)
            );
            let pairs: Vec<_> = built.url().query_pairs().collect();
            assert_eq!(
                pairs
                    .iter()
                    .find(|(key, _)| key == "MediaSourceId")
                    .unwrap()
                    .1,
                source_decoy.virtual_media_id
            );
            assert_eq!(
                pairs.iter().find(|(key, _)| key == "ItemId").unwrap().1,
                decoy.virtual_media_id
            );
            assert_eq!(
                pairs
                    .iter()
                    .filter(|(key, _)| key == "Extension")
                    .map(|(_, value)| value.as_ref())
                    .collect::<Vec<_>>(),
                ["one/two", "three"]
            );
            // A real ID-valued query field owned only by a sibling must not
            // silently be rewritten using that sibling's mapping.
            assert_eq!(
                pairs.iter().find(|(key, _)| key == "IDs").unwrap().1,
                foreign.virtual_media_id
            );
            assert_eq!(built.method(), reqwest::Method::POST);
            assert_eq!(built.version(), reqwest::Version::HTTP_11);
            assert_eq!(built.body().unwrap().as_bytes().unwrap(), body.as_bytes());
            assert_eq!(built.headers()["x-extension"], "opaque-header");
            assert_eq!(built.headers()["content-type"], "application/json");
            assert_eq!(built.headers()["host"], "chosen.example");
            assert_eq!(built.headers()["x-mediabrowser-token"], "chosen-token");
            for name in [
                "connection",
                "keep-alive",
                "proxy-authorization",
                "authorization",
                "x-emby-authorization",
                "x-emby-token",
            ] {
                assert!(!built.headers().contains_key(name), "must remove {name}");
            }
            let built: http::Request<reqwest::Body> = built.try_into().unwrap();
            assert_eq!(
                built.extensions().get::<ExtensionMarker>(),
                Some(&ExtensionMarker("opaque request metadata"))
            );
        }
        assert_eq!(original.url().as_str(), raw_url);
        assert_eq!(original.headers(), &original_headers);
        assert_eq!(
            original.body().unwrap().as_bytes().unwrap(),
            body.as_bytes()
        );
        let original: http::Request<reqwest::Body> = original.try_into().unwrap();
        assert_eq!(
            original.extensions().get::<ExtensionMarker>(),
            Some(&ExtensionMarker("opaque request metadata"))
        );
    }

    #[tokio::test]
    async fn build_without_proxy_prefix_keeps_client_path_under_backend_base_path() {
        let (urls, _, _, chosen) = fixture().await;
        let target = AuthorizedTarget::new(chosen, None, None, None).unwrap();
        let mut original = reqwest::Request::new(
            reqwest::Method::GET,
            "http://proxy.example/jelly/System/Info?Extension=kept"
                .parse()
                .unwrap(),
        );
        for name in [
            "authorization",
            "x-emby-authorization",
            "x-emby-token",
            "x-mediabrowser-token",
        ] {
            original.headers_mut().insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                "client-secret".parse().unwrap(),
            );
        }
        let built = UpstreamRequestBuilder::new(&urls, None)
            .build(&target, &original)
            .await
            .unwrap();
        assert_eq!(
            built.url().as_str(),
            "http://chosen.example/backend/jellyfin/jelly/System/Info?Extension=kept"
        );
        for name in [
            "authorization",
            "x-emby-authorization",
            "x-emby-token",
            "x-mediabrowser-token",
        ] {
            assert!(!built.headers().contains_key(name));
        }
    }
}
