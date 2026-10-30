//! Incoming request extraction; workflows own backend selection.
use axum::extract::{OriginalUri, Request};

use anyhow::{anyhow, Result};
use axum::http;
use http_body_util::BodyExt;
use tracing::{debug, error};

use crate::application::services::MediaServices;
use crate::auth::service::AuthorizationSession;
use crate::libraries::service::VirtualLibraryAccessScope;
use crate::processors::analyze_json;
use crate::processors::request_analyzer::{
    PlaybackSessionAction, RequestAnalysisContext, RequestBodyAnalysisResult,
};
#[cfg(test)]
use crate::protocol::device::Device;
use crate::protocol::identity::{request_device, JellyfinAuthorization};
use crate::servers::storage::Server;

mod identity;
#[cfg(test)]
mod playback;
mod request;
pub(crate) use identity::resolve_request_identity_from_headers_uri;
#[cfg(test)]
pub(crate) use playback::apply_to_request;
#[cfg(test)]
use playback::resolve_playback_report_server;
pub(crate) use request::extract_request_infos;
#[cfg(test)]
#[path = "../../unit_tests/preprocessing.rs"]
mod tests;
