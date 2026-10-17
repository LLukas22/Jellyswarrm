//! Local credentials, users, API keys, authorization sessions, and backend account mappings.

pub(crate) mod encryption;
pub(crate) mod federation;
pub(crate) mod mapping_authentication;
pub(crate) mod service;

pub(crate) use service::UserAuthorizationService;

pub(crate) mod credentials;
pub(crate) mod models;
