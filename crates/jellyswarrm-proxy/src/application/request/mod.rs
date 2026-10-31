//! Per-request steps shared by every workflow: caller context, operation input,
//! backend routing and selection, upstream request building and response decoding.
pub(crate) mod builder;
pub(crate) mod context;
pub(crate) mod input;
pub(crate) mod response;
pub(crate) mod routing;
pub(crate) mod selection;

pub(crate) use builder::{remap_authorization, UpstreamBuildError, UpstreamRequestBuilder};
