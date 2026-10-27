//! Incoming request extraction; workflows own backend selection.
use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use tracing::{debug, error};

use crate::auth::service::{AuthorizationSession, Device, User};
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{
    PlaybackSessionAction, RequestAnalysisContext, RequestBodyAnalysisResult,
};
use crate::servers::storage::Server;
use crate::AppState;

mod identity;
mod playback;
mod request;
pub(crate) use identity::{get_user_from_request, resolve_request_identity_from_headers_uri};
// Compatibility for incoming adapters and historical fixtures.
pub(crate) use crate::auth::identity::{request_device, JellyfinAuthorization};
#[cfg(test)]
pub(crate) use playback::apply_to_request;
pub(crate) use playback::playback_session_action;
#[cfg(test)]
use playback::resolve_playback_report_server;
pub(crate) use request::extract_request_infos;
#[cfg(test)]
#[path = "../../unit_tests/preprocessing.rs"]
mod tests;
