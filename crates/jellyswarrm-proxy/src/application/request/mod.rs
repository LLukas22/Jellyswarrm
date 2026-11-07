//! Per-request steps shared by every workflow, in the order a request uses them:
//!
//! 1. `collect`: caller identity, body analysis and usable backend sessions
//!    (`input::OperationRequest`, `context::RequestContext`);
//! 2. `routing` and `selection`: choose the backend;
//! 3. `containers` and `input::OperationRequest::authorize_target`: check the
//!    choice and bind backend credentials (`context::AuthorizedTarget`);
//! 4. `builder`: encode the upstream request;
//! 5. `response`: execute it and decode JSON.
pub(crate) mod builder;
pub(crate) mod collect;
pub(crate) mod containers;
pub(crate) mod context;
pub(crate) mod input;
pub(crate) mod response;
pub(crate) mod routing;
pub(crate) mod selection;

pub(crate) use builder::{remap_authorization, UpstreamBuildError, UpstreamRequestBuilder};
