//! Incoming request preprocessing API and shared fixture types; identity, conversion, and playback routing stay separate.
use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use std::fmt;
use tracing::{debug, error};

#[cfg(test)]
use crate::application::playback::lifecycle::PendingPlaybackSessionUpdate;
use crate::auth::service::{AuthorizationSession, Device, User};
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::models::Authorization;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{
    PlaybackSessionAction, RequestAnalysisContext, RequestBodyAnalysisResult,
};
use crate::servers::storage::Server;
use crate::AppState;

mod identity;
mod playback;
mod request;
pub(crate) use identity::{
    get_user_from_request, request_device, resolve_request_identity_from_headers_uri,
    JellyfinAuthorization,
};
#[cfg(test)]
pub(crate) use playback::apply_to_request;
#[cfg(test)]
use playback::resolve_playback_report_server;
pub(crate) use playback::{playback_session_action, MediaAccessDenied, NoAvailableBackend};
pub(crate) use request::{body_to_json, extract_request_infos};
// Historical unit fixtures only. Production routes use raw application inputs.
#[cfg(test)]
#[allow(dead_code)]
#[derive(Debug)]
pub struct PreprocessedRequest {
    pub request: reqwest::Request,
    pub original_request: reqwest::Request,
    pub user: Option<User>,
    pub sessions: Option<Vec<(AuthorizationSession, Server)>>,
    pub server: Server,
    pub auth: Option<JellyfinAuthorization>,
    pub session: Option<AuthorizationSession>,
    pub new_auth: Option<JellyfinAuthorization>,
    pub access_scope: Option<VirtualLibraryAccessScope>,
    pub server_matched_request: bool,
    pub(crate) pending_playback_session_update: Option<PendingPlaybackSessionUpdate>,
}

#[cfg(test)]
#[path = "../../unit_tests/preprocessing.rs"]
mod tests;
