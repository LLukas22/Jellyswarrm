//! Media segments (intro, outro, recap, …) for Jellyfin 10.10+.
//!
//! Intro Skipper and other segment providers publish their results through
//! Jellyfin's native `/MediaSegments/{itemId}` API, which clients use to show
//! skip buttons. This adapter routes that request to the backend owning the
//! item and translates the IDs in the response.
//!
//! Compiled out, `/MediaSegments/{id}` goes to the catch-all forwarder with
//! the virtual ID untranslated, so the backend finds no segments and clients
//! show no skip button.
use jellyswarrm_plugin_api::{Behavior, Conventions, Method, Plugin, ProxiedRoute};

pub struct IntroSkipper;

pub static PLUGIN: IntroSkipper = IntroSkipper;

const CONVENTIONS: Conventions = Conventions {
    media_id_path_tags: &["MediaSegments"],
    ..Conventions::EMPTY
};

const ROUTES: &[ProxiedRoute] = &[ProxiedRoute {
    method: Method::Get,
    nest: None,
    path: "/MediaSegments/{item_id}",
    behavior: Behavior::ItemList,
}];

impl Plugin for IntroSkipper {
    fn id(&self) -> &'static str {
        "intro-skipper"
    }

    fn name(&self) -> &'static str {
        "Intro Skipper (media segments)"
    }

    fn conventions(&self) -> &'static Conventions {
        &CONVENTIONS
    }

    fn proxied_routes(&self) -> &'static [ProxiedRoute] {
        ROUTES
    }
}
