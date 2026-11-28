//! The Moonfin companion plugin API (`/Moonfin/*`), served by Jellyswarrm.
//!
//! Moonfin clients look for the plugin with `GET /Moonfin/Ping` and only enable
//! features it advertises. Its data is per user, so forwarding to one backend
//! would be wrong; Jellyswarrm answers itself. Contract:
//! <https://github.com/Moonfin-Client/Plugin> (`Jellyfin/backend/Api`).
//!
//! Served: Ping, Discovery, and the Seerr integration (`/Moonfin/Seerr/*` and
//! its legacy `/Moonfin/Jellyseerr/*` alias), which needs Seerr to use
//! Jellyswarrm as its Jellyfin server. Moonfin mounts only when
//! `[plugins.moonfin] seerr_url` is set; otherwise every `/Moonfin/*` request
//! takes default routing, as in a build without the plugin. Paths not listed
//! here always take default routing.
use std::sync::Arc;

use axum::{
    extract::{FromRequestParts, State},
    http::{header, request::Parts, HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use jellyswarrm_plugin_api::{Caller, CredentialKind, Host, Plugin};
use serde_json::{json, Value};

mod seerr;
#[cfg(test)]
mod tests;

use seerr::Seerr;

pub struct Moonfin;

pub static PLUGIN: Moonfin = Moonfin;

impl Plugin for Moonfin {
    fn id(&self) -> &'static str {
        "moonfin"
    }

    fn name(&self) -> &'static str {
        "Moonfin"
    }

    fn router(&self, host: Arc<dyn Host>) -> Option<Router> {
        let settings = match host.settings_as::<seerr::Settings>() {
            Ok(settings) => settings.unwrap_or_default(),
            Err(cause) => {
                tracing::error!(
                    "Moonfin: invalid [plugins.moonfin] settings: {cause}; Moonfin is not mounted"
                );
                return None;
            }
        };
        let seerr = Arc::new(Seerr::new(&settings)?);
        Some(
            jellyswarrm_macros::lowercase_routes! {
                Router::new().nest("/Moonfin", Router::new()
                    .route("/Ping", get(ping))
                    .route("/Discovery", get(discovery))
                    .route("/Discovery/discover", get(discovery))
                    .nest("/Seerr", seerr::routes())
                    .nest("/Jellyseerr", seerr::routes()))
            }
            .layer(Extension(seerr))
            .with_state(host),
        )
    }
}

/// The signed-in caller. Moonfin only shows a reason for a failed request when
/// the body carries `error`, so this rejects with one.
struct User(Caller);

impl FromRequestParts<Arc<dyn Host>> for User {
    type Rejection = Failure;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<dyn Host>,
    ) -> Result<Self, Self::Rejection> {
        match Caller::from_request_parts(parts, state).await {
            Ok(caller) => Ok(Self(caller)),
            Err(StatusCode::UNAUTHORIZED) => Err(error(
                StatusCode::UNAUTHORIZED,
                json!({ "error": "User not authenticated", "success": false }),
            )),
            Err(status) => Err(status.into()),
        }
    }
}

/// Routes that act on a user's stored Seerr session require their Jellyswarrm login.
struct LoginUser(Caller);

impl FromRequestParts<Arc<dyn Host>> for LoginUser {
    type Rejection = Failure;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<dyn Host>) -> Result<Self, Failure> {
        let User(caller) = User::from_request_parts(parts, state).await?;
        if caller.credential == CredentialKind::Login {
            Ok(Self(caller))
        } else {
            Err(error(
                StatusCode::FORBIDDEN,
                json!({ "error": "A user login is required", "success": false }),
            ))
        }
    }
}

/// A failed request: a status and, for Moonfin to show, an optional JSON body.
struct Failure {
    status: StatusCode,
    body: Option<Value>,
}

impl From<StatusCode> for Failure {
    fn from(status: StatusCode) -> Self {
        Self { status, body: None }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self.body {
            Some(body) => (self.status, Json(body)).into_response(),
            None => self.status.into_response(),
        }
    }
}

fn error(status: StatusCode, body: Value) -> Failure {
    Failure {
        status,
        body: Some(body),
    }
}

/// `MoonfinController.Ping`.
async fn ping(
    State(host): State<Arc<dyn Host>>,
    Extension(seerr): Extension<Arc<Seerr>>,
    _user: User,
) -> Json<Value> {
    // `settingsSyncEnabled` is deliberately absent: clients treat an explicit
    // `false` as "plugin unavailable" and switch every Moonfin feature off.
    Json(json!({
        "installed": true,
        "version": env!("CARGO_PKG_VERSION"),
        "serverName": host.server_name().await,
        "seerrEnabled": true,
        "seerrUrl": seerr.url(),
        "mdblistAvailable": false,
        "tmdbAvailable": false,
        "messagesSupported": false,
        "recommendationsSupported": false,
    }))
}

/// `MoonfinDiscoveryController`: the address this server was reached at, for
/// Moonfin Web. Anonymous, like the Jellyfin plugin.
async fn discovery(
    State(host): State<Arc<dyn Host>>,
    headers: HeaderMap,
    uri: Uri,
) -> impl IntoResponse {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };
    let scheme = header("x-forwarded-proto").unwrap_or("http");
    let authority = header("x-forwarded-host")
        .or_else(|| header(header::HOST.as_str()))
        .or_else(|| uri.authority().map(|authority| authority.as_str()));
    let prefix = host
        .url_prefix()
        .map(|prefix| format!("/{prefix}"))
        .unwrap_or_default();
    let address = authority.map(|authority| format!("{scheme}://{authority}{prefix}"));
    (
        [
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
        ],
        Json(json!([{
            "id": "jellyfin-local",
            "name": host.server_name().await,
            "address": address,
            "type": "Jellyfin",
        }])),
    )
}
