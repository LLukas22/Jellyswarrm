//! Parse incoming requests and resolve their local caller identity.
use super::*;

use crate::application::accounts::identity::{self, RequestIdentity};

pub(crate) async fn resolve_request_identity_from_headers_uri(
    headers: &http::HeaderMap,
    uri: &http::Uri,
    state: &crate::auth::UserAuthorizationService,
) -> Result<RequestIdentity> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let url = url::Url::parse(&format!("http://localhost{path_and_query}"))?;
    Ok(identity::resolve(headers, &url, state).await?)
}

pub(crate) async fn get_user_from_request(
    auth: &Option<JellyfinAuthorization>,
    state: &MediaState,
) -> Result<Option<User>> {
    Ok(identity::user_for_credential(auth.as_ref(), &state.user_authorization).await?)
}
