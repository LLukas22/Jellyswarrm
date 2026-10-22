//! Parse incoming requests and resolve their local caller identity.
use super::*;

pub struct RequestIdentity {
    pub auth: Option<JellyfinAuthorization>,
    pub user: Option<User>,
    pub device: Option<Device>,
}

pub async fn resolve_request_identity_from_headers_uri(
    headers: &http::HeaderMap,
    uri: &http::Uri,
    state: &AppState,
) -> Result<RequestIdentity> {
    let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let url = url::Url::parse(&format!("http://localhost{path_and_query}"))?;
    let mut request = reqwest::Request::new(reqwest::Method::GET, url);
    request.headers_mut().extend(headers.clone());
    let auth = JellyfinAuthorization::from_request(&request);
    let device = request_device(&request, auth.as_ref());
    let user = get_user_from_request(&auth, state).await?;
    Ok(RequestIdentity { auth, user, device })
}

pub async fn get_user_from_request(
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
) -> Result<Option<User>> {
    let Some(token) = auth.as_ref().and_then(JellyfinAuthorization::token_ref) else {
        return Ok(None);
    };
    Ok(state.user_authorization.get_user_by_token(token).await?)
}
