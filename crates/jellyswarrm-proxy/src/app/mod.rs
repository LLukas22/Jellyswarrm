//! Composition root: startup wiring, router assembly and process lifecycle.
pub(crate) mod authorization;
mod bootstrap;
mod composition;
#[cfg(debug_assertions)]
mod debug_initialization;
mod logging;
pub(crate) mod router;
mod runtime;
mod shutdown;
pub(crate) use bootstrap::run;
