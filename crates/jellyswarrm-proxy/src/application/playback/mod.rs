//! Playback entry points, in request order. See `../../../DATA_FLOW.md` for the full map.
//!
//! `negotiation` handles PlaybackInfo; `resources` handles subsequent media requests;
//! `live` handles live open/refresh/close. They share target selection and lifecycle
//! commits. `application::media_services` holds their dependencies.
pub(crate) mod continuity;
pub(crate) mod lifecycle;
pub(crate) mod live;
pub(crate) mod negotiation;
pub(crate) mod reports;
pub(crate) mod resources;
pub(crate) mod response;
pub(crate) mod target;
