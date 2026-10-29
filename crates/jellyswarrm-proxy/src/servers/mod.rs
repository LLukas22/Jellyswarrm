//! Backend server identities, normalized URLs, registry persistence, and health tracking.

pub(crate) mod routing;
pub(crate) mod storage;
pub(crate) mod url;

pub(crate) use storage::{Server, ServerStorageService};
