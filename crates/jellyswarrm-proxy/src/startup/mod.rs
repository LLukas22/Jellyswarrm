//! Composition root: startup wiring, background runtime and process lifecycle.
mod bootstrap;
mod composition;
#[cfg(debug_assertions)]
mod debug_initialization;
mod runtime;
pub(crate) use bootstrap::run;
