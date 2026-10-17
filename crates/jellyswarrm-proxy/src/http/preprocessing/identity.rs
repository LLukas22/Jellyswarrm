//! Resolve incoming authentication, local users, and normalized device identity without selecting a backend.
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

/// Resolve device identity consistently for HTTP and WebSocket requests.
pub(crate) fn request_device(
    request: &reqwest::Request,
    auth: Option<&JellyfinAuthorization>,
) -> Option<Device> {
    let mut device = auth.and_then(|a| a.get_device(request.headers()));
    let explicit_device = matches!(
        auth,
        Some(
            JellyfinAuthorization::Authorization(_) | JellyfinAuthorization::XEmbyAuthorization(_)
        )
    ) && device.as_ref().is_some_and(|d| !d.device_id.is_empty());
    // /Sessions?DeviceId is a target filter, not the caller's device identity.
    if !explicit_device
        && !request
            .url()
            .path()
            .trim_end_matches('/')
            .to_ascii_lowercase()
            .ends_with("/sessions")
    {
        if let Some(device_id) = request
            .url()
            .query_pairs()
            .find_map(|(k, v)| k.eq_ignore_ascii_case("deviceid").then(|| v.into_owned()))
        {
            device = Some(Device {
                device_id,
                client: "Unknown".into(),
                device: "Unknown".into(),
                version: "Unknown".into(),
            });
        }
    }
    device
}

#[derive(Clone)]
pub enum JellyfinAuthorization {
    Authorization(Authorization),
    XMediaBrowser(String),
    ApiKey(String),
    XEmbyToken(String),
    XEmbyAuthorization(Authorization),
}

impl fmt::Debug for JellyfinAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JellyfinAuthorization::Authorization(auth) => {
                f.debug_tuple("Authorization").field(auth).finish()
            }
            JellyfinAuthorization::XMediaBrowser(_) => {
                f.debug_tuple("XMediaBrowser").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::ApiKey(_) => {
                f.debug_tuple("ApiKey").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::XEmbyToken(_) => {
                f.debug_tuple("XEmbyToken").field(&"<redacted>").finish()
            }
            JellyfinAuthorization::XEmbyAuthorization(auth) => {
                f.debug_tuple("XEmbyAuthorization").field(auth).finish()
            }
        }
    }
}

impl JellyfinAuthorization {
    pub fn token_ref(&self) -> Option<&str> {
        match self {
            JellyfinAuthorization::Authorization(auth) => auth.token.as_deref(),
            JellyfinAuthorization::XMediaBrowser(token)
            | JellyfinAuthorization::ApiKey(token)
            | JellyfinAuthorization::XEmbyToken(token) => Some(token),
            JellyfinAuthorization::XEmbyAuthorization(auth) => auth.token.as_deref(),
        }
    }

    pub fn token(&self) -> Option<String> {
        self.token_ref().map(str::to_string)
    }

    pub fn get_device(&self, headers: &http::HeaderMap) -> Option<Device> {
        match self {
            JellyfinAuthorization::Authorization(auth) => Some(Device {
                client: auth.client.clone(),
                device: auth.device.clone(),
                device_id: auth.device_id.clone(),
                version: auth.version.clone(),
            }),
            JellyfinAuthorization::XEmbyAuthorization(auth) => Some(Device {
                client: auth.client.clone(),
                device: auth.device.clone(),
                device_id: auth.device_id.clone(),
                version: auth.version.clone(),
            }),
            JellyfinAuthorization::XMediaBrowser(_) => None,
            JellyfinAuthorization::ApiKey(_) => None,
            JellyfinAuthorization::XEmbyToken(_) => {
                // Try to get device info from User-Agent header
                if let Some(user_agent) = headers.get("user-agent") {
                    if let Ok(ua_str) = user_agent.to_str() {
                        let device = crate::auth::service::Device::from_useragent(ua_str);
                        return Some(Device {
                            client: device.client,
                            device: device.device,
                            device_id: device.device_id,
                            version: device.version,
                        });
                    }
                }
                None
            }
        }
    }

    pub fn from_request(req: &reqwest::Request) -> Option<Self> {
        let headers = req.headers();
        if let Some(auth_header) = headers.get("authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                if let Ok(auth) = Authorization::parse(auth_str) {
                    return Some(JellyfinAuthorization::Authorization(auth));
                }
            }
        }

        if let Some(auth_header) = headers.get("x-emby-authorization") {
            if let Ok(auth_str) = auth_header.to_str() {
                if let Ok(auth) = Authorization::parse(auth_str) {
                    return Some(JellyfinAuthorization::XEmbyAuthorization(auth));
                }
            }
        }

        if let Some(token_header) = headers.get("X-MediaBrowser-Token") {
            if let Ok(token_str) = token_header.to_str() {
                return Some(JellyfinAuthorization::XMediaBrowser(token_str.to_string()));
            }
        }

        if let Some(token_header) = headers.get("x-emby-token") {
            if let Ok(token_str) = token_header.to_str() {
                return Some(JellyfinAuthorization::XEmbyToken(token_str.to_string()));
            }
        }

        if let Some(auth) = req.url().query_pairs().find_map(|(k, v)| {
            if (k == "api_key") | (k == "ApiKey") {
                Some(JellyfinAuthorization::ApiKey(v.to_string()))
            } else {
                None
            }
        }) {
            return Some(auth);
        }

        None
    }
}

pub async fn get_user_from_request(
    auth: &Option<JellyfinAuthorization>,
    state: &AppState,
) -> Result<Option<User>> {
    let Some(auth) = auth else {
        return Ok(None);
    };

    let Some(token) = auth.token() else {
        // No token, return None
        return Ok(None);
    };

    let user = state.user_authorization.get_user_by_token(&token).await?;

    Ok(user)
}
