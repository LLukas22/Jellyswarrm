//! Trickplay (scrubbing previews) for Jellyfin 10.9+.
//!
//! Items carry a `Trickplay` object keyed by media source ID, and clients fetch
//! tiles from `/Videos/{id}/Trickplay/{width}/…`. This adapter rewrites those
//! keys to virtual IDs and forwards tile requests to the owning backend as
//! opaque resources.
//!
//! Compiled out, the keys stay upstream IDs and tile requests fall through to
//! the generic `/Videos/{id}/{*path}` video resource route, so previews stop
//! working.
use jellyswarrm_plugin_api::{Behavior, Conventions, Method, Plugin, ProxiedRoute};

pub struct Trickplay;

pub static PLUGIN: Trickplay = Trickplay;

const CONVENTIONS: Conventions = Conventions {
    media_id_map_key_fields: &["Trickplay"],
    ..Conventions::EMPTY
};

const ROUTES: &[ProxiedRoute] = &[ProxiedRoute {
    method: Method::Get,
    nest: Some("/Videos"),
    path: "/{stream_id}/Trickplay/{*path}",
    behavior: Behavior::Forward,
}];

impl Plugin for Trickplay {
    fn id(&self) -> &'static str {
        "trickplay"
    }

    fn name(&self) -> &'static str {
        "Trickplay"
    }

    fn conventions(&self) -> &'static Conventions {
        &CONVENTIONS
    }

    fn proxied_routes(&self) -> &'static [ProxiedRoute] {
        ROUTES
    }
}
