//! Resolve a parsed caller credential without incoming-adapter or startup state.
use crate::{
    auth::{service::User, UserAuthorizationService},
    protocol::{
        device::Device,
        identity::{request_device, JellyfinAuthorization},
    },
};

pub(crate) struct RequestIdentity {
    pub(crate) auth: Option<JellyfinAuthorization>,
    pub(crate) user: Option<User>,
    pub(crate) device: Option<Device>,
}

pub(crate) async fn resolve(
    headers: &http::HeaderMap,
    url: &url::Url,
    authorization: &UserAuthorizationService,
) -> Result<RequestIdentity, sqlx::Error> {
    let auth = JellyfinAuthorization::from_headers_url(headers, url);
    let device = request_device(headers, url, auth.as_ref());
    let user = user_for_credential(auth.as_ref(), authorization).await?;
    Ok(RequestIdentity { auth, user, device })
}

pub(crate) async fn user_for_credential(
    auth: Option<&JellyfinAuthorization>,
    authorization: &UserAuthorizationService,
) -> Result<Option<User>, sqlx::Error> {
    let Some(token) = auth.and_then(JellyfinAuthorization::token_ref) else {
        return Ok(None);
    };
    authorization.get_user_by_token(token).await
}
