//! Process lifecycle and concrete service composition, including shared application state.
pub(crate) mod authorization;
mod bootstrap;
#[cfg(debug_assertions)]
mod debug_initialization;
mod logging;
pub(crate) mod router;
mod runtime;
mod shutdown;
mod state;
pub(crate) use bootstrap::run;
pub use state::{AppState, DataContext};
