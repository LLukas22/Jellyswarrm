//! Moonfin's Seerr integration (`SeerrProxyController`), ported from the
//! `moonfin-seerr` branch of asenmitrev/Jellyswarrm.
//!
//! A user signs in to Seerr once with their Jellyswarrm credentials (Seerr
//! must use Jellyswarrm as its Jellyfin server). The Seerr session cookie is
//! kept server-side, encrypted, so every Moonfin client of that user can make
//! Seerr requests through `/Moonfin/Seerr/Api/*`.
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri},
    response::Response,
    routing::{delete, get, post},
    Extension, Json, Router,
};
use jellyswarrm_plugin_api::{Caller, Host, HostError, SecretRevision};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::{error, Failure, LoginUser, User};

/// The `Host` secret holding a user's [`Session`].
const SESSION_KEY: &str = "seerr_session";
/// Seerr gets rechecked at most this often when a client asks for status.
const SESSION_FRESHNESS_MS: i64 = 5 * 60 * 1000;
const CSRF_COOKIES: [&str; 2] = ["XSRF-TOKEN", "_csrf"];

pub(crate) fn routes() -> Router<Arc<dyn Host>> {
    let methods = get(api_proxy)
        .post(api_proxy)
        .put(api_proxy)
        .delete(api_proxy)
        .head(|| async { StatusCode::METHOD_NOT_ALLOWED });
    jellyswarrm_macros::lowercase_routes! {
        Router::new()
            .route("/Config", get(config))
            .route("/Login", post(login))
            .route("/Status", get(status))
            .route("/Validate", get(validate))
            .route("/Logout", delete(logout))
            .nest("/Api", Router::new().route("/{*path}", methods.clone()))
    }
}

/// `[plugins.moonfin]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Settings {
    pub(crate) seerr_url: Option<String>,
    pub(crate) seerr_display_name: Option<String>,
}

/// The configured Seerr instance.
pub(crate) struct Seerr {
    /// The configured base URL; [`Seerr::url`] gives it without a trailing slash.
    url: url::Url,
    display_name: String,
    client: reqwest::Client,
}

impl Seerr {
    /// `None`, logged, when `seerr_url` is missing or not an http(s) URL.
    pub(crate) fn new(settings: &Settings) -> Option<Self> {
        let trimmed = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };
        let Some(configured) = trimmed(&settings.seerr_url) else {
            info!("Moonfin: plugins.moonfin.seerr_url is not set; /Moonfin requests use default routing");
            return None;
        };
        let url = match url::Url::parse(&configured) {
            Ok(url)
                if matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none() =>
            {
                url
            }
            // Don't echo the value: a URL can carry credentials.
            _ => {
                tracing::error!(
                    "Moonfin: plugins.moonfin.seerr_url must be an HTTP(S) base URL without credentials, query or fragment; Moonfin is not mounted"
                );
                return None;
            }
        };
        // Redirects are not followed: a redirected login means the Seerr URL is
        // wrong, and following one would send the session cookie elsewhere.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .user_agent("Jellyswarrm-Moonfin")
            .build()
            .inspect_err(|error| tracing::error!("Moonfin: cannot build the Seerr client: {error}"))
            .ok()?;
        Some(Self {
            url,
            display_name: trimmed(&settings.seerr_display_name)
                .unwrap_or_else(|| "Seerr".to_owned()),
            client,
        })
    }

    pub(crate) fn url(&self) -> &str {
        self.url.as_str().trim_end_matches('/')
    }

    fn api(&self, path: &str) -> url::Url {
        let mut target = self.url.clone();
        target.set_path(&format!(
            "{}/api/v1/{path}",
            self.url.path().trim_end_matches('/')
        ));
        target
    }
}

/// A user's Seerr sign-in, stored as one encrypted record.
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    /// The Seerr the cookie belongs to; it is never sent anywhere else.
    seerr_url: String,
    cookie_name: String,
    cookie_value: String,
    seerr_user_id: i64,
    display_name: Option<String>,
    avatar: Option<String>,
    permissions: i64,
    /// Unix milliseconds.
    created_at: i64,
    last_validated: i64,
}

impl Session {
    fn cookie(&self) -> String {
        format!("{}={}", self.cookie_name, self.cookie_value)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SeerrUser {
    id: i64,
    display_name: Option<String>,
    avatar: Option<String>,
    permissions: Option<i64>,
}

impl SeerrUser {
    async fn from_response(response: reqwest::Response) -> Option<Self> {
        response
            .json::<Self>()
            .await
            .ok()
            .filter(|user| user.id > 0)
    }
}

/// A session and the stored value it came from, for conditional updates.
struct Stored {
    revision: SecretRevision,
    session: Session,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The host has already logged the cause.
fn storage_error(_: HostError) -> Failure {
    StatusCode::INTERNAL_SERVER_ERROR.into()
}

fn encode(session: &Session) -> String {
    serde_json::to_string(session).expect("a session always serializes")
}

impl Seerr {
    fn decode_session(&self, value: &str) -> Option<Session> {
        let session: Session = serde_json::from_str(value).ok()?;
        (session.seerr_url == self.url()).then_some(session)
    }

    async fn load(&self, host: &dyn Host, caller: &Caller) -> Result<Option<Stored>, Failure> {
        let Some(secret) = host
            .user_secret(caller, SESSION_KEY)
            .await
            .map_err(storage_error)?
        else {
            return Ok(None);
        };
        Ok(self.decode_session(&secret.value).map(|session| Stored {
            revision: secret.revision,
            session,
        }))
    }

    /// Replace (`Some`) or remove (`None`) `stored`, unless another request
    /// changed it since it was read. Returns whether it applied.
    async fn update(
        &self,
        host: &dyn Host,
        caller: &Caller,
        stored: &Stored,
        next: Option<&Session>,
    ) -> Result<bool, Failure> {
        let next = next.map(encode);
        host.replace_user_secret(caller, SESSION_KEY, &stored.revision, next.as_deref())
            .await
            .map_err(storage_error)
    }

    /// The stored session, confirmed with Seerr when it was last validated
    /// more than `max_age_ms` ago (always when `None`). A dead session is
    /// removed. If a login or logout happened meanwhile, that result wins.
    async fn current(
        &self,
        host: &dyn Host,
        caller: &Caller,
        max_age_ms: Option<i64>,
    ) -> Result<Option<Session>, Failure> {
        let Some(stored) = self.load(host, caller).await? else {
            return Ok(None);
        };
        let now = now_ms();
        if max_age_ms.is_some_and(|max_age| now - stored.session.last_validated < max_age) {
            return Ok(Some(stored.session));
        }
        let next = match self.check(&stored.session).await {
            SessionCheck::Alive(me) => {
                let mut session = stored.session.clone();
                session.seerr_user_id = me.id;
                session.display_name = me.display_name.or(session.display_name);
                session.avatar = me.avatar.or(session.avatar);
                session.permissions = me.permissions.unwrap_or(session.permissions);
                session.last_validated = now;
                Some(session)
            }
            SessionCheck::Dead => None,
            // Seerr being unreachable says nothing about the session itself.
            SessionCheck::Unknown if max_age_ms.is_some() => return Ok(Some(stored.session)),
            SessionCheck::Unknown => return Err(StatusCode::BAD_GATEWAY.into()),
        };
        if self.update(host, caller, &stored, next.as_ref()).await? {
            Ok(next)
        } else {
            Ok(self.load(host, caller).await?.map(|stored| stored.session))
        }
    }

    async fn check(&self, session: &Session) -> SessionCheck {
        let request = self
            .client
            .get(self.api("auth/me"))
            .header(header::COOKIE, session.cookie());
        match request.send().await {
            Ok(response) if response.status().is_success() => SeerrUser::from_response(response)
                .await
                .map_or(SessionCheck::Unknown, SessionCheck::Alive),
            Ok(response)
                if matches!(
                    response.status(),
                    reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
                ) =>
            {
                SessionCheck::Dead
            }
            _ => SessionCheck::Unknown,
        }
    }

    /// Seerr's optional CSRF protection: the token pair comes from cookies set
    /// on any API response and must be echoed back as cookies plus a header.
    async fn csrf(&self) -> CsrfTokens {
        // auth/me answers 403 without a session but still issues the cookies.
        let Ok(response) = self.client.get(self.api("auth/me")).send().await else {
            return CsrfTokens::default();
        };
        let cookies: Vec<(String, String)> = set_cookies(response.headers())
            .into_iter()
            .filter(|(name, _)| CSRF_COOKIES.contains(&name.as_str()))
            .collect();
        let token = cookies
            .iter()
            .find(|(name, _)| name == "XSRF-TOKEN")
            .map(|(_, value)| {
                percent_encoding::percent_decode_str(value)
                    .decode_utf8_lossy()
                    .into_owned()
            });
        CsrfTokens { cookies, token }
    }
}

enum SessionCheck {
    Alive(SeerrUser),
    Dead,
    Unknown,
}

#[derive(Default)]
struct CsrfTokens {
    cookies: Vec<(String, String)>,
    token: Option<String>,
}

impl CsrfTokens {
    fn apply(
        &self,
        mut builder: reqwest::RequestBuilder,
        session: Option<&Session>,
    ) -> reqwest::RequestBuilder {
        if let Some(token) = &self.token {
            builder = builder
                .header("X-XSRF-TOKEN", token)
                .header("X-CSRF-Token", token);
        }
        let cookie = self
            .cookies
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .chain(session.map(Session::cookie))
            .collect::<Vec<_>>()
            .join("; ");
        if !cookie.is_empty() {
            builder = builder.header(header::COOKIE, cookie);
        }
        builder
    }
}

/// `(name, value)` pairs from `Set-Cookie`, values kept verbatim.
fn set_cookies(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| {
            let (name, value) = value.split(';').next()?.trim().split_once('=')?;
            let name = name.trim();
            (!name.is_empty()).then(|| (name.to_owned(), value.trim().to_owned()))
        })
        .collect()
}

/// Stock Seerr names its session cookie `connect.sid`; rebranded builds may
/// rename it, so fall back to the first cookie that is not a CSRF cookie.
fn session_cookie(headers: &HeaderMap) -> Option<(String, String)> {
    let cookies: Vec<_> = set_cookies(headers)
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .collect();
    cookies
        .iter()
        .find(|(name, _)| name == "connect.sid")
        .or_else(|| {
            cookies
                .iter()
                .find(|(name, _)| !CSRF_COOKIES.contains(&name.as_str()))
        })
        .cloned()
}

/// `MoonfinController.GetSeerrConfig`.
async fn config(Extension(seerr): Extension<Arc<Seerr>>, _user: User) -> Json<Value> {
    Json(json!({
        "enabled": true,
        "url": seerr.url(),
        "displayName": seerr.display_name,
        "variant": "seerr",
        "userEnabled": true,
    }))
}

#[derive(Deserialize)]
struct LoginRequest {
    #[serde(alias = "Username")]
    username: Option<String>,
    #[serde(alias = "Password")]
    password: Option<String>,
    #[serde(alias = "AuthType", rename = "authType")]
    auth_type: Option<String>,
}

/// `SeerrProxyController.Login`. A failed login keeps any earlier session.
async fn login(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    LoginUser(caller): LoginUser,
    Json(request): Json<LoginRequest>,
) -> Result<Json<Value>, Failure> {
    let unauthorized = |message: String| {
        error(
            StatusCode::UNAUTHORIZED,
            json!({ "error": message, "success": false }),
        )
    };
    let auth_type = request.auth_type.as_deref().unwrap_or("jellyfin");
    if auth_type.eq_ignore_ascii_case("quickconnect") {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            json!({
                "error": "Quick Connect sign in to Seerr is not supported by Jellyswarrm. Sign in with your password.",
                "errorCode": "sso_unsupported",
                "success": false,
            }),
        ));
    }
    let Some(username) = request.username.filter(|name| !name.trim().is_empty()) else {
        return Err(error(
            StatusCode::BAD_REQUEST,
            json!({ "error": "Username is required" }),
        ));
    };
    let password = request.password.unwrap_or_default();
    let (endpoint, payload) = if auth_type.eq_ignore_ascii_case("local") {
        (
            "auth/local",
            json!({ "email": username, "password": password }),
        )
    } else {
        (
            "auth/jellyfin",
            json!({ "username": username, "password": password }),
        )
    };

    let csrf = seerr.csrf().await;
    let builder = seerr
        .client
        .post(seerr.api(endpoint))
        .header(header::ORIGIN, seerr.url.origin().ascii_serialization())
        .header(header::REFERER, format!("{}/", seerr.url()))
        .json(&payload);
    let response = match csrf.apply(builder, None).send().await {
        Ok(response) => response,
        Err(cause) => {
            let cause = cause.without_url();
            warn!("Moonfin: cannot reach Seerr: {cause}");
            return Err(unauthorized(format!("Cannot reach Seerr: {cause}")));
        }
    };
    let status = response.status();
    if status.is_redirection() {
        return Err(unauthorized(
            "Seerr redirected the login request. Verify the Seerr URL in Jellyswarrm matches its public address (https and any sub-path).".into(),
        ));
    }
    if !status.is_success() {
        warn!("Moonfin: Seerr login failed for {username}: {status}");
        return Err(unauthorized(if status == reqwest::StatusCode::FORBIDDEN {
            "Access denied. Make sure you have a Seerr account.".into()
        } else {
            format!("Authentication failed: {status}")
        }));
    }
    let Some((cookie_name, cookie_value)) = session_cookie(response.headers()) else {
        return Err(unauthorized("No session cookie received from Seerr".into()));
    };
    let user = SeerrUser::from_response(response).await.ok_or_else(|| {
        error(
            StatusCode::BAD_GATEWAY,
            json!({ "error": "Invalid user in Seerr login response", "success": false }),
        )
    })?;
    let now = now_ms();
    let session = Session {
        seerr_url: seerr.url().to_owned(),
        cookie_name,
        cookie_value,
        seerr_user_id: user.id,
        display_name: user.display_name.or(Some(username)),
        avatar: user.avatar,
        permissions: user.permissions.unwrap_or_default(),
        created_at: now,
        last_validated: now,
    };
    host.set_user_secret(&caller, SESSION_KEY, &encode(&session))
        .await
        .map_err(storage_error)?;
    info!(
        "Moonfin: Seerr session created for {} (Seerr user {})",
        caller.user_name, session.seerr_user_id
    );
    Ok(Json(json!({
        "success": true,
        "seerrUserId": session.seerr_user_id,
        "jellyseerrUserId": session.seerr_user_id,
        "displayName": session.display_name,
        "avatar": session.avatar,
        "permissions": session.permissions,
    })))
}

/// `SeerrProxyController.Status`.
async fn status(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    LoginUser(caller): LoginUser,
) -> Result<Json<Value>, Failure> {
    let session = seerr
        .current(&*host, &caller, Some(SESSION_FRESHNESS_MS))
        .await?;
    Ok(Json(json!({
        "enabled": true,
        "authenticated": session.is_some(),
        "url": seerr.url(),
        "seerrUserId": session.as_ref().map(|s| s.seerr_user_id),
        "jellyseerrUserId": session.as_ref().map(|s| s.seerr_user_id),
        "displayName": session.as_ref().and_then(|s| s.display_name.clone()),
        "avatar": session.as_ref().and_then(|s| s.avatar.clone()),
        "permissions": session.as_ref().map_or(0, |s| s.permissions),
        "sessionCreated": session.as_ref().map(|s| s.created_at),
        "lastValidated": session.as_ref().map(|s| s.last_validated),
    })))
}

/// `SeerrProxyController.Validate`: always asks Seerr.
async fn validate(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    LoginUser(caller): LoginUser,
) -> Result<Json<Value>, Failure> {
    let session = seerr.current(&*host, &caller, None).await?;
    Ok(Json(json!({
        "valid": session.is_some(),
        "lastValidated": session.map(|s| s.last_validated),
    })))
}

/// `SeerrProxyController.Logout`.
async fn logout(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    LoginUser(caller): LoginUser,
) -> Result<Json<Value>, Failure> {
    let removed = host
        .take_user_secret(&caller, SESSION_KEY)
        .await
        .map_err(storage_error)?;
    if let Some(session) = removed.and_then(|secret| seerr.decode_session(&secret.value)) {
        let csrf = seerr.csrf().await;
        let builder = seerr.client.post(seerr.api("auth/logout"));
        if let Err(cause) = csrf.apply(builder, Some(&session)).send().await {
            warn!(
                "Moonfin: Seerr logout request failed: {}",
                cause.without_url()
            );
        }
    }
    Ok(Json(
        json!({ "success": true, "message": "Logged out from Seerr" }),
    ))
}

/// `SeerrProxyController.Proxy*`: `/Api/{path}` → Seerr `/api/v1/{path}`
/// with the user's session.
async fn api_proxy(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    LoginUser(caller): LoginUser,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Failure> {
    let Some(stored) = seerr.load(&*host, &caller).await? else {
        return Err(error(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Not authenticated with Seerr", "code": "NO_SESSION" }),
        ));
    };

    // Nesting strips the mount point while preserving the encoded resource path.
    let mut target = seerr.api(uri.path().trim_start_matches('/'));
    if !target.path().starts_with(seerr.api("").path()) {
        return Err(StatusCode::BAD_REQUEST.into());
    }
    if let Some(query) = uri.query() {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            // Jellyswarrm's own credential is for Jellyswarrm, not Seerr.
            if !key.eq_ignore_ascii_case("api_key") && !key.eq_ignore_ascii_case("apikey") {
                target.query_pairs_mut().append_pair(&key, &value);
            }
        }
    }
    let csrf = if method == Method::GET {
        CsrfTokens::default()
    } else {
        seerr.csrf().await
    };
    let mut builder = seerr.client.request(method, target);
    if !body.is_empty() {
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .cloned()
            .unwrap_or_else(|| HeaderValue::from_static("application/json"));
        builder = builder
            .header(header::CONTENT_TYPE, content_type)
            .body(body);
    }
    if let Some(accept) = headers.get(header::ACCEPT) {
        builder = builder.header(header::ACCEPT, accept.clone());
    }
    let response = match csrf.apply(builder, Some(&stored.session)).send().await {
        Ok(response) => response,
        Err(cause) => {
            let cause = cause.without_url();
            warn!("Moonfin: failed to proxy a Seerr request: {cause}");
            return Err(error(
                StatusCode::BAD_GATEWAY,
                json!({ "error": format!("Cannot reach Seerr: {cause}") }),
            ));
        }
    };

    let status = response.status();
    // Seerr answers 403 both for permission denials and for sessions it no
    // longer knows; only drop the session once Seerr confirms it is dead.
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) && matches!(seerr.check(&stored.session).await, SessionCheck::Dead)
    {
        seerr.update(&*host, &caller, &stored, None).await?;
        return Err(error(
            StatusCode::UNAUTHORIZED,
            json!({ "error": "Seerr session expired", "code": "SESSION_EXPIRED" }),
        ));
    }
    if status.is_success() {
        if let Some((name, value)) = session_cookie(response.headers()) {
            if name == stored.session.cookie_name && value != stored.session.cookie_value {
                let rotated = Session {
                    cookie_value: value,
                    ..stored.session.clone()
                };
                // Best effort: the request itself succeeded either way.
                let _ = seerr.update(&*host, &caller, &stored, Some(&rotated)).await;
            }
        }
    }

    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = response.bytes().await.map_err(|cause| {
        warn!(
            "Moonfin: failed to read a Seerr response: {}",
            cause.without_url()
        );
        Failure::from(StatusCode::BAD_GATEWAY)
    })?;
    let mut proxied = Response::new(Body::from(bytes));
    *proxied.status_mut() = status;
    if let Some(content_type) = content_type {
        proxied
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    Ok(proxied)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn session_cookie_prefers_connect_sid_and_skips_csrf() {
        let mut headers = HeaderMap::new();
        for value in [
            "XSRF-TOKEN=a; Path=/",
            "custom.sid=b; Path=/",
            "connect.sid=c; Path=/; HttpOnly",
        ] {
            headers.append(header::SET_COOKIE, value.parse().unwrap());
        }
        assert_eq!(
            session_cookie(&headers),
            Some(("connect.sid".into(), "c".into()))
        );

        headers.remove(header::SET_COOKIE);
        headers.append(header::SET_COOKIE, "_csrf=x".parse().unwrap());
        headers.append(header::SET_COOKIE, "sb.sid=y".parse().unwrap());
        assert_eq!(
            session_cookie(&headers),
            Some(("sb.sid".into(), "y".into()))
        );
    }

    #[test]
    fn seerr_url_is_normalized_and_validated() {
        let seerr = |url: &str| {
            Seerr::new(&Settings {
                seerr_url: Some(url.into()),
                ..Settings::default()
            })
        };
        assert_eq!(
            seerr("http://seerr:5055/").unwrap().url(),
            "http://seerr:5055"
        );
        assert_eq!(
            seerr("https://example.com/seerr/").unwrap().url(),
            "https://example.com/seerr"
        );
        for invalid in [
            "",
            "  ",
            "seerr:5055",
            "ftp://seerr",
            "https://user:pw@seerr",
            "https://seerr/?key=x",
            "https://seerr/#fragment",
        ] {
            assert!(seerr(invalid).is_none(), "{invalid:?}");
        }
        assert!(Seerr::new(&Settings::default()).is_none());
    }
}
