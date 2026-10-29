//! Session routes, identity-cookie extraction and WebSocket frame handling.
mod authentication;
mod error;
mod identity;
mod routes;
#[cfg(test)]
mod tests;
mod websocket;

pub(crate) use authentication::{authentication_response, AuthenticationResponse};
pub(crate) use routes::router;
pub(crate) use websocket::websocket;
