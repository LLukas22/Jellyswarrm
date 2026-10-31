//! Caller identity and backend availability, independent of a selected target.
//!
//! Credentials deliberately have no derived Debug/Serialize implementation.
use crate::{
    auth::service::{AuthorizationSession, LocalUser},
    domain::ids::ServerId,
    libraries::service::VirtualLibraryAccessScope,
    protocol::device::Device,
    protocol::identity::{request_device, JellyfinAuthorization},
    servers::storage::Server,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Principal {
    ProxyUser,
    BackendPassthrough,
    Anonymous,
}

pub(crate) struct BackendAccessSnapshot {
    /// Usable device sessions; authorization lives separately in access_scope.
    pub(crate) sessions: Option<Vec<(AuthorizationSession, Server)>>,
}

pub(crate) struct RequestContext {
    identity: CallerIdentity,
    /// Anonymous requests can retain legacy user metadata without authenticating.
    legacy_user: Option<LocalUser>,
    device: Option<Device>,
    access_scope: Option<VirtualLibraryAccessScope>,
    backends: BackendAccessSnapshot,
}

enum CallerIdentity {
    Anonymous,
    BackendPassthrough {
        authorization: JellyfinAuthorization,
    },
    ProxyUser {
        user: LocalUser,
        authorization: JellyfinAuthorization,
    },
}

impl RequestContext {
    pub(crate) fn new(
        request: &reqwest::Request,
        auth: Option<JellyfinAuthorization>,
        user: Option<LocalUser>,
        sessions: Option<Vec<(AuthorizationSession, Server)>>,
        access_scope: Option<VirtualLibraryAccessScope>,
    ) -> Self {
        let device = request_device(request.headers(), request.url(), auth.as_ref());
        let (identity, legacy_user) = match (auth, user) {
            (Some(authorization), Some(user)) => (
                CallerIdentity::ProxyUser {
                    user,
                    authorization,
                },
                None,
            ),
            (Some(authorization), None) => {
                (CallerIdentity::BackendPassthrough { authorization }, None)
            }
            (None, user) => (CallerIdentity::Anonymous, user),
        };
        Self {
            identity,
            legacy_user,
            device,
            access_scope,
            backends: BackendAccessSnapshot { sessions },
        }
    }

    pub(crate) fn principal(&self) -> Principal {
        match self.identity {
            CallerIdentity::Anonymous => Principal::Anonymous,
            CallerIdentity::BackendPassthrough { .. } => Principal::BackendPassthrough,
            CallerIdentity::ProxyUser { .. } => Principal::ProxyUser,
        }
    }

    /// Includes legacy metadata. Use principal()/authorization() to establish
    /// authentication, never the presence of this metadata alone.
    pub(crate) fn user(&self) -> Option<&LocalUser> {
        match &self.identity {
            CallerIdentity::ProxyUser { user, .. } => Some(user),
            _ => self.legacy_user.as_ref(),
        }
    }

    pub(crate) fn authorization(&self) -> Option<&JellyfinAuthorization> {
        match &self.identity {
            CallerIdentity::Anonymous => None,
            CallerIdentity::BackendPassthrough { authorization }
            | CallerIdentity::ProxyUser { authorization, .. } => Some(authorization),
        }
    }

    pub(crate) fn device(&self) -> Option<&Device> {
        self.device.as_ref()
    }
    pub(crate) fn access_scope(&self) -> Option<&VirtualLibraryAccessScope> {
        self.access_scope.as_ref()
    }
    pub(crate) fn backends(&self) -> &BackendAccessSnapshot {
        &self.backends
    }

    /// Validate client-owned media selections before any target is built or
    /// traffic is emitted. Availability checks remain operation-specific.
    pub(crate) async fn validate_media_ids(
        &self,
        url: &url::Url,
        analysis: Option<&crate::processors::request_analyzer::RequestBodyAnalysisResult>,
        urls: &super::selection::UrlSelection,
    ) -> Result<(), super::routing::RoutingError> {
        use crate::protocol::routes::{MEDIA_ID_PATH_TAGS, MEDIA_ID_QUERY_TAGS};
        if let Some(scope) = &self.access_scope {
            for tag in MEDIA_ID_PATH_TAGS {
                if let Some(id) = crate::protocol::url::contains_id(url, tag) {
                    urls.validate_media_access(&id, scope).await?;
                }
            }
            for (key, value) in url.query_pairs() {
                if MEDIA_ID_QUERY_TAGS
                    .iter()
                    .any(|tag| key.eq_ignore_ascii_case(tag))
                {
                    for id in value.split(',') {
                        urls.validate_media_access(id, scope).await?;
                    }
                }
            }
            if let Some(analysis) = analysis {
                for id in &analysis.found_ids {
                    urls.validate_media_access(id, scope).await?;
                }
            }
        }
        Ok(())
    }

    /// Resolve exactly the requested owner. Missing availability never changes
    /// authorization and never selects an authorized sibling.
    pub(crate) fn target(&self, server_id: ServerId) -> Result<AuthorizedTarget, TargetError> {
        if self
            .access_scope
            .as_ref()
            .is_some_and(|scope| !scope.allows(server_id))
        {
            return Err(TargetError::AccessDenied);
        }
        let (session, server) = self
            .backends
            .sessions
            .as_ref()
            .and_then(|sessions| sessions.iter().find(|(_, server)| server.id == server_id))
            .ok_or(TargetError::UnavailableOwner)?;
        AuthorizedTarget::new(
            server.clone(),
            Some(session.clone()),
            super::remap_authorization(self.authorization(), Some(session)),
            self.access_scope.clone(),
        )
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum TargetError {
    #[error("requested backend is outside the caller's authorized scope")]
    AccessDenied,
    #[error("selected backend is unavailable")]
    UnavailableOwner,
    #[error("backend session belongs to a different viewer")]
    ViewerMismatch,
}

/// A concrete, access-checked backend plus opaque, already-remapped credentials.
/// The builder consumes it without consulting a routing/default-server policy.
pub(crate) struct AuthorizedTarget {
    server: Server,
    session: Option<AuthorizationSession>,
    auth: Option<JellyfinAuthorization>,
    access_scope: Option<VirtualLibraryAccessScope>,
}

impl AuthorizedTarget {
    pub(crate) fn new(
        server: Server,
        session: Option<AuthorizationSession>,
        auth: Option<JellyfinAuthorization>,
        access_scope: Option<VirtualLibraryAccessScope>,
    ) -> Result<Self, TargetError> {
        if let Some(scope) = &access_scope {
            if !scope.allows(server.id) {
                return Err(TargetError::AccessDenied);
            }
            if session
                .as_ref()
                .is_some_and(|session| session.user_id.as_str() != scope.user_id())
            {
                return Err(TargetError::ViewerMismatch);
            }
        }
        Ok(Self {
            server,
            session,
            auth,
            access_scope,
        })
    }

    pub(crate) fn server(&self) -> &Server {
        &self.server
    }
    pub(crate) fn session(&self) -> Option<&AuthorizationSession> {
        self.session.as_ref()
    }
    pub(crate) fn authorization(&self) -> Option<&JellyfinAuthorization> {
        self.auth.as_ref()
    }
    pub(crate) fn access_scope(&self) -> Option<&VirtualLibraryAccessScope> {
        self.access_scope.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{auth::service::LocalCredential, config::MediaStreamingMode};

    fn user() -> LocalUser {
        LocalUser {
            id: "viewer".into(),
            virtual_key: "proxy-token".into(),
            original_username: "Viewer".into(),
            local_credential: LocalCredential::Passwordless,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn backend(id: i64) -> (AuthorizationSession, Server) {
        let server = Server {
            id: ServerId::new(id),
            name: format!("Backend {id}"),
            url: crate::servers::url::ServerUrl::parse(&format!("http://backend-{id}.example"))
                .unwrap(),
            priority: 100,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let session = AuthorizationSession {
            id,
            user_id: "viewer".into(),
            mapping_id: id,
            server_url: server.url.to_string(),
            device: Device {
                client: "Test client".into(),
                device: "Test device".into(),
                device_id: format!("device-{id}"),
                version: "1".into(),
            },
            jellyfin_token: format!("backend-token-{id}"),
            original_user_id: format!("backend-viewer-{id}"),
            expires_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        (session, server)
    }

    fn request() -> reqwest::Request {
        reqwest::Request::new(
            reqwest::Method::GET,
            "http://proxy.example/Items".parse().unwrap(),
        )
    }

    #[test]
    fn principal_distinguishes_anonymous_passthrough_and_proxy_user() {
        for (auth, user, expected) in [
            (None, None, Principal::Anonymous),
            (
                Some(JellyfinAuthorization::XEmbyToken("token".into())),
                None,
                Principal::BackendPassthrough,
            ),
            (
                Some(JellyfinAuthorization::XEmbyToken("token".into())),
                Some(user()),
                Principal::ProxyUser,
            ),
        ] {
            let context = RequestContext::new(&request(), auth, user, None, None);
            assert_eq!(context.principal(), expected);
        }
    }

    #[test]
    fn body_derived_user_without_authentication_remains_anonymous() {
        let mut request = request();
        *request.body_mut() = Some(r#"{"UserId":"viewer","Extension":{"Enabled":true}}"#.into());
        let context = RequestContext::new(
            &request,
            None,
            Some(user()),
            Some(vec![backend(1)]),
            Some(VirtualLibraryAccessScope::new("viewer", [ServerId::new(1)])),
        );

        assert_eq!(context.principal(), Principal::Anonymous);
        assert_eq!(context.user().unwrap().id.as_str(), "viewer");
        assert!(context.authorization().is_none());
    }

    #[test]
    fn selected_owner_unavailable_never_falls_back_to_authorized_sibling() {
        let context = RequestContext::new(
            &request(),
            Some(JellyfinAuthorization::XEmbyToken("proxy-token".into())),
            Some(user()),
            Some(vec![backend(2)]),
            Some(VirtualLibraryAccessScope::new(
                "viewer",
                [ServerId::new(1), ServerId::new(2)],
            )),
        );

        assert!(matches!(
            context.target(ServerId::new(1)),
            Err(TargetError::UnavailableOwner)
        ));
        assert!(context
            .access_scope
            .as_ref()
            .unwrap()
            .allows(ServerId::new(1)));
        assert_eq!(
            context.target(ServerId::new(2)).unwrap().server().id,
            ServerId::new(2)
        );
    }

    #[test]
    fn access_denial_precedes_backend_availability() {
        for sessions in [None, Some(vec![]), Some(vec![backend(1)])] {
            let context = RequestContext::new(
                &request(),
                None,
                None,
                sessions,
                Some(VirtualLibraryAccessScope::new("viewer", [ServerId::new(2)])),
            );
            assert!(matches!(
                context.target(ServerId::new(1)),
                Err(TargetError::AccessDenied)
            ));
        }
    }

    #[test]
    fn exact_target_remaps_credentials_from_its_own_session() {
        let context = RequestContext::new(
            &request(),
            Some(JellyfinAuthorization::XEmbyToken("proxy-token".into())),
            Some(user()),
            Some(vec![backend(1), backend(2)]),
            Some(VirtualLibraryAccessScope::new(
                "viewer",
                [ServerId::new(1), ServerId::new(2)],
            )),
        );
        let target = context.target(ServerId::new(2)).unwrap();

        assert_eq!(target.server().id, ServerId::new(2));
        assert_eq!(
            target.session().unwrap().original_user_id,
            "backend-viewer-2"
        );
        assert_eq!(
            target.authorization().unwrap().token_ref(),
            Some("backend-token-2")
        );
        match target.authorization().unwrap() {
            JellyfinAuthorization::Authorization(auth) => assert_eq!(auth.device_id, "device-2"),
            _ => panic!("token authorization must be rebuilt from the selected session"),
        }
    }

    #[test]
    fn authorized_target_rejects_session_for_another_viewer() {
        let (mut session, server) = backend(1);
        session.user_id = "other-viewer".into();
        assert!(matches!(
            AuthorizedTarget::new(
                server,
                Some(session),
                None,
                Some(VirtualLibraryAccessScope::new("viewer", [ServerId::new(1)]))
            ),
            Err(TargetError::ViewerMismatch)
        ));
    }
}
